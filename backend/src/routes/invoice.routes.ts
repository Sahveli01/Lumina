/**
 * Invoice Routes  (/api/invoice)
 *
 * POST /api/invoice/submit
 *   Full orchestration: ZK proof → nullifier check → register → risk_score →
 *   submit_invoice. Returns all three tx hashes + risk_score.
 *
 * POST /api/invoice/factor/:invoiceId
 *   Read risk_score from oracle → factor_invoice on lumina-core.
 *   Returns advance_amount + apr_bps.
 *
 * GET /api/invoice/:invoiceId
 *   Read invoice state from lumina-core. Returns the on-chain InvoiceState.
 */

import { Router, Request, Response } from 'express';
import * as StellarSdk from '@stellar/stellar-sdk';
import { ZkProverService }  from '../services/zkProver.service';
import { StellarService }   from '../services/stellar.service';
import { PaymentService }   from '../services/payment.service';
import { requireContractId } from '../stellar/contractIds';

const router         = Router();
const zkProver       = new ZkProverService();
const stellar        = new StellarService();
const paymentService = new PaymentService();

// ── Validation helpers ────────────────────────────────────────────────────────

const HEX32_RE = /^[0-9a-fA-F]{64}$/;

function isHex32(s: unknown): s is string {
  return typeof s === 'string' && HEX32_RE.test(s);
}

// ── POST /api/invoice/submit ──────────────────────────────────────────────────

interface SubmitBody {
  invoice_hash: string;          // 32-byte hex (SHA-256 of invoice document)
  amount: number;                // u64 — face value in smallest currency unit
  debtor_id: string;             // 32-byte hex — opaque debtor identifier
  due_date: number;              // u64 Unix timestamp of invoice due date
  payment_history_score: number; // u8  0-100
  country_cds_spread: number;    // u16 basis points (e.g. 150 = 1.5 %)
  sector_risk: number;           // u8  0-100
}

router.post('/submit', async (req: Request, res: Response): Promise<void> => {
  try {
    const b = req.body as Partial<SubmitBody>;

    // ── Input validation ────────────────────────────────────────────────────
    const requiredFields: (keyof SubmitBody)[] = [
      'invoice_hash', 'amount', 'debtor_id', 'due_date',
      'payment_history_score', 'country_cds_spread', 'sector_risk',
    ];
    const missing = requiredFields.filter((k) => b[k] === undefined);
    if (missing.length > 0) {
      res.status(400).json({
        success: false,
        error: `Missing required fields: ${missing.join(', ')}`,
      });
      return;
    }

    if (!isHex32(b.invoice_hash)) {
      res.status(400).json({
        success: false,
        error: 'invoice_hash must be a 32-byte hex string (64 hex chars)',
      });
      return;
    }
    if (!isHex32(b.debtor_id)) {
      res.status(400).json({
        success: false,
        error: 'debtor_id must be a 32-byte hex string (64 hex chars)',
      });
      return;
    }
    if (typeof b.amount !== 'number' || b.amount <= 0) {
      res.status(400).json({ success: false, error: 'amount must be a positive number' });
      return;
    }

    const body = b as SubmitBody;

    // Contract IDs — will throw with a clear message if not configured.
    const nullifierRegistry = requireContractId('nullifierRegistry');
    const riskOracle        = requireContractId('riskOracle');
    const luminaCore        = requireContractId('luminaCore');
    const adminKeypair      = StellarService.adminKeypair();

    // ── Step 1: Generate ZK proof ──────────────────────────────────────────
    // spawnSync — blocks until proof is ready (up to 5 min).
    const proof = zkProver.prove({
      invoice_hash:          ZkProverService.hexToBytes32(body.invoice_hash),
      amount:                body.amount,
      debtor_id:             ZkProverService.hexToBytes32(body.debtor_id),
      due_date:              body.due_date,
      payment_history_score: body.payment_history_score,
      country_cds_spread:    body.country_cds_spread,
      sector_risk:           body.sector_risk,
    });

    if (!proof.is_valid) {
      res.status(400).json({
        success: false,
        error:
          'Invoice failed ZK validation. ' +
          'Ensure amount > 0 and due_date is in the future.',
        risk_score: proof.risk_score,
      });
      return;
    }

    // ── Step 2: Nullifier double-spend check ───────────────────────────────
    const nullifierParam = StellarService.bytes(proof.nullifier);

    const isUsedRaw = await stellar.queryContract(
      nullifierRegistry, 'is_used', [nullifierParam]
    );
    const isUsed = isUsedRaw
      ? (StellarSdk.scValToNative(isUsedRaw) as boolean)
      : false;

    if (isUsed) {
      res.status(409).json({
        success: false,
        error:
          'Nullifier already registered — this invoice has already been factored.',
        nullifier: proof.nullifier,
      });
      return;
    }

    // ── Step 3: Register nullifier in nullifier-registry ──────────────────
    const { txHash: nullifierTxHash } = await stellar.invokeContract(
      nullifierRegistry,
      'register_nullifier',
      [
        StellarService.address(adminKeypair.publicKey()),  // caller
        nullifierParam,                                     // nullifier
        StellarService.bytes(body.invoice_hash),           // invoice_hash
        StellarService.u64(body.due_date),                 // due_date
      ],
      adminKeypair,
    );

    // ── Step 4: Publish risk score + expiry in risk-oracle ────────────────
    const { txHash: riskScoreTxHash } = await stellar.invokeContract(
      riskOracle,
      'set_risk_score',
      [
        StellarService.bytes(body.invoice_hash),   // key: invoice_hash
        StellarService.u32(proof.risk_score),      // risk score 0-100
        StellarService.u64(body.due_date),         // expiry = due_date
      ],
      adminKeypair,
    );

    // ── Step 5: Submit invoice to lumina-core ──────────────────────────────
    const { txHash: invoiceTxHash } = await stellar.invokeContract(
      luminaCore,
      'submit_invoice',
      [
        StellarService.address(adminKeypair.publicKey()),  // company
        StellarService.bytes(body.invoice_hash),           // invoice_hash
        StellarService.i128(body.amount),                  // amount (i128)
        StellarService.address(adminKeypair.publicKey()),  // debtor (admin as placeholder)
        StellarService.u64(body.due_date),                 // due_date
        nullifierParam,                                    // nullifier
      ],
      adminKeypair,
    );

    // Warn when the risk score came from a manually entered (unverified) invoice.
    // Verified sources (Xero / QuickBooks) receive a 10% risk score reduction
    // in the ZK circuit — so unverified invoices objectively carry more risk.
    const unverifiedWarning = !proof.is_verified_source
      ? 'Unverified data source. Consider using Xero or QuickBooks integration ' +
        'for a cryptographically attested risk score with a 10% bonus reduction.'
      : undefined;

    res.status(201).json({
      success: true,
      ...(unverifiedWarning ? { warning: unverifiedWarning } : {}),
      data: {
        invoice_hash:       body.invoice_hash,
        nullifier:          proof.nullifier,
        risk_score:         proof.risk_score,
        is_verified_source: proof.is_verified_source,
        data_source:        proof.data_source,
        proof_mode:         proof.mode,
        nullifier_tx:       nullifierTxHash,
        risk_score_tx:      riskScoreTxHash,
        invoice_tx:         invoiceTxHash,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── POST /api/invoice/factor/:invoiceId ───────────────────────────────────────
//
// Path param:
//   invoiceId        string — 32-byte hex invoice_hash (used as risk-oracle key)
//
// Body (JSON):
//   invoiceNumericId  number  — on-chain u64 invoice ID (from submit response)
//   recipientAddress  string  — SME's Stellar G-address for automatic disbursement
//   assetCode         string  — "XLM" | "USDC"
//   anchorDomain?     string  — SEP-31 anchor domain for local-currency payout
//
// Flow:
//   a. Read risk_score from risk-oracle (keyed by invoice_hash)
//   b. factor_invoice(invoice_hash, risk_score) → { advance_amount, apr_bps }
//   c. disburseFunds(invoiceNumericId, advance_amount, recipient, asset, anchor?)
//   → returns { factoring: {...}, disbursement: DisbursementResult }

interface FactorBody {
  invoiceNumericId:  number;  // u64 on-chain ID returned from /submit
  recipientAddress:  string;  // SME Stellar G-address
  assetCode:         string;  // "XLM" | "USDC"
  anchorDomain?:     string;  // optional SEP-31 anchor
}

router.post('/factor/:invoiceId', async (req: Request, res: Response): Promise<void> => {
  try {
    const invoiceId = req.params['invoiceId'] as string;
    if (!isHex32(invoiceId)) {
      res.status(400).json({
        success: false,
        error: 'invoiceId path param must be a 32-byte hex string (64 hex chars)',
      });
      return;
    }

    // ── Validate body ─────────────────────────────────────────────────────
    const b = req.body as Partial<FactorBody>;

    if (typeof b.invoiceNumericId !== 'number' || b.invoiceNumericId <= 0) {
      res.status(400).json({
        success: false,
        error: 'invoiceNumericId must be a positive number (the on-chain u64 invoice ID)',
      });
      return;
    }
    if (typeof b.recipientAddress !== 'string' || !/^G[A-Z2-7]{55}$/.test(b.recipientAddress)) {
      res.status(400).json({
        success: false,
        error: 'recipientAddress must be a valid Stellar G-address',
      });
      return;
    }
    if (typeof b.assetCode !== 'string' || !['XLM', 'USDC'].includes(b.assetCode)) {
      res.status(400).json({
        success: false,
        error: 'assetCode must be "XLM" or "USDC"',
      });
      return;
    }
    if (b.anchorDomain !== undefined && typeof b.anchorDomain !== 'string') {
      res.status(400).json({ success: false, error: 'anchorDomain must be a string' });
      return;
    }

    const body = b as FactorBody;

    const riskOracle   = requireContractId('riskOracle');
    const luminaCore   = requireContractId('luminaCore');
    const adminKeypair = StellarService.adminKeypair();

    const invoiceHashParam = StellarService.bytes(invoiceId);

    // ── Step a: Read the on-chain risk score ──────────────────────────────
    const riskRaw = await stellar.queryContract(
      riskOracle, 'get_risk_score', [invoiceHashParam]
    );
    if (!riskRaw) {
      res.status(404).json({
        success: false,
        error:
          'No risk score found for this invoice. ' +
          'Ensure the invoice was submitted via POST /api/invoice/submit first.',
      });
      return;
    }
    const riskScore = StellarSdk.scValToNative(riskRaw) as number;

    // ── Step b: Factor the invoice ────────────────────────────────────────
    // factor_invoice(invoice_id: u64, risk_score: u32) — uses numeric ID, not hash
    const { txHash, returnValue } = await stellar.invokeContract(
      luminaCore,
      'factor_invoice',
      [StellarService.u64(body.invoiceNumericId), StellarService.u32(riskScore)],
      adminKeypair,
    );

    // Decode the contract's return map { advance_amount: u64, apr_bps: u32 }
    const factorResult = StellarSdk.scValToNative(returnValue) as {
      advance_amount?: bigint;
      apr_bps?: number;
    };

    const advanceAmount = Number(factorResult.advance_amount ?? 0n);

    // ── Step c: Disburse funds to SME ─────────────────────────────────────
    const disbursement = await paymentService.disburseFunds({
      invoiceId:        body.invoiceNumericId,
      advanceAmount,
      recipientAddress: body.recipientAddress,
      assetCode:        body.assetCode,
      anchorDomain:     body.anchorDomain,
    });

    res.status(200).json({
      success: true,
      data: {
        factoring: {
          invoice_hash:   invoiceId,
          risk_score:     riskScore,
          advance_amount: advanceAmount.toString(),
          apr_bps:        factorResult.apr_bps ?? 0,
          tx_hash:        txHash,
        },
        disbursement,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── GET /api/invoice/:invoiceId ───────────────────────────────────────────────

router.get('/:invoiceId', async (req: Request, res: Response): Promise<void> => {
  try {
    const invoiceId = req.params['invoiceId'] as string;
    if (!isHex32(invoiceId)) {
      res.status(400).json({
        success: false,
        error: 'invoiceId path param must be a 32-byte hex string (64 hex chars)',
      });
      return;
    }

    const luminaCore = requireContractId('luminaCore');

    const raw = await stellar.queryContract(
      luminaCore, 'get_invoice', [StellarService.bytes(invoiceId)]
    );
    if (!raw) {
      res.status(404).json({
        success: false,
        error: `Invoice ${invoiceId} not found on-chain.`,
      });
      return;
    }

    res.status(200).json({
      success: true,
      data: StellarSdk.scValToNative(raw),
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── POST /api/invoice/repay/:invoiceId ────────────────────────────────────────
//
// Body (JSON): { nullifier: "<64-hex-char string>" }
//   nullifier — SHA-256(invoice_hash ‖ debtor_id); passed to nullifier-registry
//               for the Active→Funded→Repaid state transition.
//
// Flow:
//   1. lumina-core.repay(invoice_id)              — state → Repaid on-chain
//   2. nullifier-registry.update_state(nullifier, Repaid) — registry sync

interface RepayBody {
  nullifier: string; // 32-byte hex
}

router.post('/repay/:invoiceId', async (req: Request, res: Response): Promise<void> => {
  try {
    const rawId = req.params['invoiceId'] as string;
    const invoiceId = parseInt(rawId, 10);
    if (isNaN(invoiceId) || invoiceId <= 0) {
      res.status(400).json({
        success: false,
        error: 'invoiceId must be a positive integer (the on-chain u64 invoice ID)',
      });
      return;
    }

    const b = req.body as Partial<RepayBody>;
    if (!isHex32(b.nullifier)) {
      res.status(400).json({
        success: false,
        error: 'nullifier must be a 32-byte hex string (64 hex chars)',
      });
      return;
    }
    const { nullifier } = b as RepayBody;

    const luminaCore        = requireContractId('luminaCore');
    const nullifierRegistry = requireContractId('nullifierRegistry');
    const adminKeypair      = StellarService.adminKeypair();

    // Step 1: lumina-core.repay(invoice_id: u64)
    const { txHash: repayTxHash, returnValue } = await stellar.invokeContract(
      luminaCore,
      'repay',
      [StellarService.u64(invoiceId)],
      adminKeypair,
    );

    const ok = StellarSdk.scValToNative(returnValue) as boolean;
    if (!ok) {
      res.status(409).json({
        success: false,
        error: `repay() returned false — invoice #${invoiceId} may not be in Funded state.`,
        tx_hash: repayTxHash,
      });
      return;
    }

    // Step 2: nullifier-registry.update_state(nullifier, Repaid)
    const { txHash: stateTxHash } = await stellar.invokeContract(
      nullifierRegistry,
      'update_state',
      [
        StellarService.bytes(nullifier),
        StellarService.symbol('Repaid'),
      ],
      adminKeypair,
    );

    res.status(200).json({
      success: true,
      data: {
        invoice_id:    invoiceId,
        nullifier,
        repay_tx:      repayTxHash,
        registry_tx:   stateTxHash,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── POST /api/invoice/mark-default/:invoiceId ─────────────────────────────────
//
// Body (JSON): { nullifier: "<64-hex>", senior_loss: <number> }
//   senior_loss — i128 kayıp tutarı; liquidity-pools.trigger_default_protection'a geçilir.
//
// Flow:
//   1. lumina-core.mark_defaulted(invoice_id)
//   2. nullifier-registry.update_state(nullifier, Disputed)
//   3. nullifier-registry.update_state(nullifier, Defaulted)   ← tek adımda Defaulted
//      NOT: Funded → Disputed → Defaulted geçişi on-chain iki tx gerektirir.
//           Bu endpoint ikisini de sırayla çağırır.
//   4. liquidity-pools.trigger_default_protection(invoice_id, senior_loss)

interface MarkDefaultBody {
  nullifier:   string; // 32-byte hex
  senior_loss: number; // i128 — sigorta rezervinden karşılanacak kayıp tutarı
}

router.post('/mark-default/:invoiceId', async (req: Request, res: Response): Promise<void> => {
  try {
    const rawId = req.params['invoiceId'] as string;
    const invoiceId = parseInt(rawId, 10);
    if (isNaN(invoiceId) || invoiceId <= 0) {
      res.status(400).json({
        success: false,
        error: 'invoiceId must be a positive integer',
      });
      return;
    }

    const b = req.body as Partial<MarkDefaultBody>;
    if (!isHex32(b.nullifier)) {
      res.status(400).json({
        success: false,
        error: 'nullifier must be a 32-byte hex string (64 hex chars)',
      });
      return;
    }
    if (typeof b.senior_loss !== 'number' || b.senior_loss < 0) {
      res.status(400).json({
        success: false,
        error: 'senior_loss must be a non-negative number',
      });
      return;
    }
    const { nullifier, senior_loss } = b as MarkDefaultBody;

    const luminaCore        = requireContractId('luminaCore');
    const nullifierRegistry = requireContractId('nullifierRegistry');
    const liquidityPools    = requireContractId('liquidityPools');
    const adminKeypair      = StellarService.adminKeypair();

    const nullifierParam = StellarService.bytes(nullifier);

    // Step 1: lumina-core.mark_defaulted(invoice_id)
    const { txHash: markTxHash } = await stellar.invokeContract(
      luminaCore,
      'mark_defaulted',
      [StellarService.u64(invoiceId)],
      adminKeypair,
    );

    // Step 2: nullifier-registry — Funded → Disputed
    const { txHash: disputedTxHash } = await stellar.invokeContract(
      nullifierRegistry,
      'update_state',
      [nullifierParam, StellarService.symbol('Disputed')],
      adminKeypair,
    );

    // Step 3: nullifier-registry — Disputed → Defaulted
    const { txHash: defaultedTxHash } = await stellar.invokeContract(
      nullifierRegistry,
      'update_state',
      [nullifierParam, StellarService.symbol('Defaulted')],
      adminKeypair,
    );

    // Step 4: liquidity-pools.trigger_default_protection(invoice_id, senior_loss)
    const { txHash: protectionTxHash } = await stellar.invokeContract(
      liquidityPools,
      'trigger_default_protection',
      [
        StellarService.u64(invoiceId),
        StellarService.u64(senior_loss),   // i128 → u64 encoding (fits in practice)
      ],
      adminKeypair,
    );

    res.status(200).json({
      success: true,
      data: {
        invoice_id:      invoiceId,
        nullifier,
        mark_tx:         markTxHash,
        disputed_tx:     disputedTxHash,
        defaulted_tx:    defaultedTxHash,
        protection_tx:   protectionTxHash,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── GET /api/registry/stats ───────────────────────────────────────────────────
//
// nullifier-registry'nin toplam istatistiklerini döndürür.
// Herkese açık (auth gerektirmez).
//
// Döndürür: { total, active, funded, disputed, repaid, defaulted }

const registryRouter = Router();

registryRouter.get('/stats', async (_req: Request, res: Response): Promise<void> => {
  try {
    const nullifierRegistry = requireContractId('nullifierRegistry');

    const raw = await stellar.queryContract(
      nullifierRegistry,
      'get_registry_stats',
      [],
    );

    if (!raw) {
      res.status(503).json({
        success: false,
        error: 'Registry stats could not be retrieved. Contract may not be initialized.',
      });
      return;
    }

    res.status(200).json({
      success: true,
      data: StellarSdk.scValToNative(raw),
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── GET /api/registry/query/:nullifier ───────────────────────────────────────
//
// Nullifier bazlı privacy-preserving invoice durumu sorgusu.
// Vergi dairesi / banka entegrasyonu bu endpoint'i kullanır.
// invoice_hash açıklanmaz; yalnızca { nullifier, state, due_date, funded_at }.

registryRouter.get('/query/:nullifier', async (req: Request, res: Response): Promise<void> => {
  try {
    const nullifier = req.params['nullifier'] as string;
    if (!isHex32(nullifier)) {
      res.status(400).json({
        success: false,
        error: 'nullifier must be a 32-byte hex string (64 hex chars)',
      });
      return;
    }

    const nullifierRegistry = requireContractId('nullifierRegistry');

    const raw = await stellar.queryContract(
      nullifierRegistry,
      'query_state',
      [StellarService.bytes(nullifier)],
    );

    if (!raw) {
      res.status(404).json({
        success: false,
        error: `No registry entry found for nullifier: ${nullifier}`,
      });
      return;
    }

    res.status(200).json({
      success: true,
      data: StellarSdk.scValToNative(raw),
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

export { registryRouter };
export default router;
