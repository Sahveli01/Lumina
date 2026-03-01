/**
 * Invoice Routes  (/api/invoice)
 *
 * POST /api/invoice/prepare
 *   Compute invoice_hash, debtor_id, nullifier, risk score, APR, advance
 *   without touching the blockchain. Used by the frontend before on-chain submit.
 *
 * POST /api/invoice/submit
 *   Full on-chain orchestration: nullifier check → register → risk-oracle →
 *   lumina-core.submit_invoice. Returns all tx hashes.
 *
 * POST /api/invoice/factor/:invoiceId
 * GET  /api/invoice/:invoiceId
 * POST /api/invoice/repay/:invoiceId
 * POST /api/invoice/mark-default/:invoiceId
 */

import { Router, Request, Response } from 'express';
import { createHash, randomUUID }    from 'crypto';
import * as StellarSdk               from '@stellar/stellar-sdk';
import { ZkProverService }           from '../services/zkProver.service';
import { StellarService }            from '../services/stellar.service';
import { PaymentService }            from '../services/payment.service';
import { requireContractId }         from '../stellar/contractIds';

const router         = Router();
const zkProver       = new ZkProverService();
const stellar        = new StellarService();
const paymentService = new PaymentService();

// ── Validation helpers ────────────────────────────────────────────────────────

const HEX32_RE = /^[0-9a-fA-F]{64}$/;

function isHex32(s: unknown): s is string {
  return typeof s === 'string' && HEX32_RE.test(s);
}

/** Compute risk score from the three risk factors. */
function calcRiskScore(
  paymentHistoryScore: number,
  sectorRisk:          number,
  cdsSpreadBps:        number,
): number {
  const norm_cds = Math.min((cdsSpreadBps * 100) / 2000, 100);
  return Math.round(
    (paymentHistoryScore * 50 + sectorRisk * 30 + norm_cds * 20) / 100
  );
}

/** Map risk score to APR in basis points. */
function calcAprBps(risk_score: number): number {
  if (risk_score <= 25) return  800;
  if (risk_score <= 40) return 1000;
  if (risk_score <= 55) return 1200;
  if (risk_score <= 70) return 1500;
  if (risk_score <= 85) return 1800;
  return 2200;
}

/** SHA-256(invoice_hash_bytes ‖ debtor_id_bytes) → hex */
function calcNullifier(invoiceHash: string, debtorId: string): string {
  return createHash('sha256')
    .update(Buffer.concat([
      Buffer.from(invoiceHash, 'hex'),
      Buffer.from(debtorId,    'hex'),
    ]))
    .digest('hex');
}

// ── POST /api/invoice/prepare ─────────────────────────────────────────────────
//
// Compute all deterministic values without touching the blockchain.
// The frontend calls this first, shows the result, then calls /submit.

interface PrepareBody {
  invoice_number:        string;
  amount_usd:            number;
  due_date:              string;  // ISO date string e.g. "2026-06-01"
  debtor_name:           string;
  debtor_tax_id:         string;
  payment_history_score: number;  // 0-100
  sector_risk:           number;  // 0-100
  cds_spread_bps:        number;  // basis points e.g. 150
  wallet_address?:       string;
}

router.post('/prepare', async (req: Request, res: Response): Promise<void> => {
  try {
    const b = req.body as Partial<PrepareBody>;

    const requiredFields: (keyof PrepareBody)[] = [
      'invoice_number', 'amount_usd', 'due_date',
      'debtor_name', 'debtor_tax_id',
      'payment_history_score', 'sector_risk', 'cds_spread_bps',
    ];
    const missing = requiredFields.filter((k) => b[k] === undefined || b[k] === '');
    if (missing.length > 0) {
      res.status(400).json({ success: false, error: `Missing fields: ${missing.join(', ')}` });
      return;
    }

    const body = b as PrepareBody;

    // ── 1. invoice_hash ────────────────────────────────────────────────────
    const invoice_hash = createHash('sha256')
      .update(`${body.invoice_number}:${body.amount_usd}:${body.due_date}`)
      .digest('hex');

    // ── 2. debtor_id ───────────────────────────────────────────────────────
    const debtor_id = createHash('sha256')
      .update(`${body.debtor_name}:${body.debtor_tax_id}`)
      .digest('hex');

    // ── 3. nullifier ───────────────────────────────────────────────────────
    const nullifier = calcNullifier(invoice_hash, debtor_id);

    // ── 4. amount_stroops ──────────────────────────────────────────────────
    const amount_stroops = Math.round(body.amount_usd * 10_000_000);

    // ── 5. risk_score ──────────────────────────────────────────────────────
    const risk_score = calcRiskScore(
      body.payment_history_score,
      body.sector_risk,
      body.cds_spread_bps,
    );

    // ── 6. APR ─────────────────────────────────────────────────────────────
    const apr_bps = calcAprBps(risk_score);

    // ── 7. advance_usd ─────────────────────────────────────────────────────
    const due_date_ms   = new Date(body.due_date).getTime();
    const now_ms        = Date.now();
    const days_to_due   = Math.max(1, Math.round((due_date_ms - now_ms) / 86_400_000));
    const advance_usd   = body.amount_usd * (1 - (apr_bps / 10_000) * (days_to_due / 365));

    // ── 8. ZK proof ────────────────────────────────────────────────────────
    let proof_id:       string;
    let is_zk_verified: boolean;
    let zk_status:      string;

    const bonsaiKey = process.env['BONSAI_API_KEY'];
    if (bonsaiKey && bonsaiKey.trim() !== '') {
      try {
        const proof = zkProver.prove({
          invoice_hash:          ZkProverService.hexToBytes32(invoice_hash),
          amount:                amount_stroops,
          debtor_id:             ZkProverService.hexToBytes32(debtor_id),
          due_date:              Math.floor(due_date_ms / 1000),
          payment_history_score: body.payment_history_score,
          country_cds_spread:    body.cds_spread_bps,
          sector_risk:           body.sector_risk,
        });
        proof_id       = proof.receipt_bytes.slice(0, 36);
        is_zk_verified = proof.is_valid;
        zk_status      = proof.is_valid ? 'VERIFIED' : 'INVALID';
      } catch {
        proof_id       = randomUUID();
        is_zk_verified = false;
        zk_status      = 'BONSAI_ERROR';
      }
    } else {
      proof_id       = randomUUID();
      is_zk_verified = false;
      zk_status      = 'BONSAI_PENDING';
    }

    res.status(200).json({
      success: true,
      data: {
        invoice_hash,
        debtor_id,
        nullifier,
        amount_stroops,
        risk_score,
        apr_bps,
        apr_percent:     (apr_bps / 100).toFixed(2),
        advance_usd:     Math.round(advance_usd * 100) / 100,
        advance_stroops: Math.round(advance_usd * 10_000_000),
        days_to_due,
        proof_id,
        is_zk_verified,
        zk_status,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── POST /api/invoice/submit ──────────────────────────────────────────────────
//
// On-chain orchestration. Takes pre-computed values from /prepare.
// Backend re-derives nullifier and risk_score for security (never trusts client).

interface SubmitBody {
  invoice_hash:          string;  // 64-char hex (from /prepare)
  debtor_id:             string;  // 64-char hex (from /prepare)
  amount_stroops:        number;  // from /prepare
  apr_bps:               number;  // from /prepare
  due_date:              string;  // ISO date string
  wallet_address:        string;  // Stellar G-address
  proof_id:              string;  // from /prepare
  payment_history_score: number;
  sector_risk:           number;
  cds_spread_bps:        number;
}

router.post('/submit', async (req: Request, res: Response): Promise<void> => {
  try {
    const b = req.body as Partial<SubmitBody>;

    const requiredFields: (keyof SubmitBody)[] = [
      'invoice_hash', 'debtor_id', 'amount_stroops', 'apr_bps',
      'due_date', 'wallet_address', 'proof_id',
      'payment_history_score', 'sector_risk', 'cds_spread_bps',
    ];
    const missing = requiredFields.filter((k) => b[k] === undefined);
    if (missing.length > 0) {
      res.status(400).json({ success: false, error: `Missing fields: ${missing.join(', ')}` });
      return;
    }

    if (!isHex32(b.invoice_hash)) {
      res.status(400).json({ success: false, error: 'invoice_hash must be 64 hex chars' });
      return;
    }
    if (!isHex32(b.debtor_id)) {
      res.status(400).json({ success: false, error: 'debtor_id must be 64 hex chars' });
      return;
    }

    const body = b as SubmitBody;

    // Re-derive nullifier and risk_score server-side (do not trust client values)
    const nullifier  = calcNullifier(body.invoice_hash, body.debtor_id);
    const risk_score = calcRiskScore(
      body.payment_history_score,
      body.sector_risk,
      body.cds_spread_bps,
    );
    const due_date_unix = Math.floor(new Date(body.due_date).getTime() / 1000);

    const nullifierRegistry = requireContractId('nullifierRegistry');
    const riskOracle        = requireContractId('riskOracle');
    const luminaCore        = requireContractId('luminaCore');
    const adminKeypair      = StellarService.adminKeypair();

    const nullifierParam = StellarService.bytes(nullifier);

    // ── Step 1: Nullifier double-spend check ───────────────────────────────
    const isUsedRaw = await stellar.queryContract(
      nullifierRegistry, 'is_used', [nullifierParam]
    );
    const isUsed = isUsedRaw
      ? (StellarSdk.scValToNative(isUsedRaw) as boolean)
      : false;

    if (isUsed) {
      res.status(409).json({
        success: false,
        error:   'Nullifier already registered — this invoice has already been factored.',
        nullifier,
      });
      return;
    }

    // ── Step 2: Register nullifier ─────────────────────────────────────────
    const { txHash: nullifierTxHash } = await stellar.invokeContract(
      nullifierRegistry,
      'register_nullifier',
      [
        StellarService.address(adminKeypair.publicKey()),
        nullifierParam,
        StellarService.bytes(body.invoice_hash),
        StellarService.u64(due_date_unix),
      ],
      adminKeypair,
    );

    // ── Step 3: Publish risk score to risk-oracle ──────────────────────────
    const { txHash: riskScoreTxHash } = await stellar.invokeContract(
      riskOracle,
      'set_risk_score',
      [
        StellarService.bytes(body.invoice_hash),
        StellarService.u32(risk_score),
        StellarService.u64(due_date_unix),
      ],
      adminKeypair,
    );

    // ── Step 4: Submit invoice to lumina-core ──────────────────────────────
    // Use wallet_address as the company if it's a valid G-address, else admin.
    const companyAddress = /^G[A-Z2-7]{55}$/.test(body.wallet_address)
      ? body.wallet_address
      : adminKeypair.publicKey();

    const { txHash: invoiceTxHash } = await stellar.invokeContract(
      luminaCore,
      'submit_invoice',
      [
        StellarService.address(companyAddress),
        StellarService.bytes(body.invoice_hash),
        StellarService.i128(body.amount_stroops),
        StellarService.address(adminKeypair.publicKey()),
        StellarService.u64(due_date_unix),
        nullifierParam,
      ],
      adminKeypair,
    );

    res.status(201).json({
      success: true,
      data: {
        invoice_hash:  body.invoice_hash,
        nullifier,
        risk_score,
        nullifier_tx:  nullifierTxHash,
        risk_score_tx: riskScoreTxHash,
        invoice_tx:    invoiceTxHash,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── POST /api/invoice/factor/:invoiceId ───────────────────────────────────────

interface FactorBody {
  invoiceNumericId:  number;
  recipientAddress:  string;
  assetCode:         string;
  anchorDomain?:     string;
}

router.post('/factor/:invoiceId', async (req: Request, res: Response): Promise<void> => {
  try {
    const invoiceId = req.params['invoiceId'] as string;
    if (!isHex32(invoiceId)) {
      res.status(400).json({ success: false, error: 'invoiceId must be 64 hex chars' });
      return;
    }

    const b = req.body as Partial<FactorBody>;
    if (typeof b.invoiceNumericId !== 'number' || b.invoiceNumericId <= 0) {
      res.status(400).json({ success: false, error: 'invoiceNumericId must be a positive number' });
      return;
    }
    if (typeof b.recipientAddress !== 'string' || !/^G[A-Z2-7]{55}$/.test(b.recipientAddress)) {
      res.status(400).json({ success: false, error: 'recipientAddress must be a valid Stellar G-address' });
      return;
    }
    if (typeof b.assetCode !== 'string' || !['XLM', 'USDC'].includes(b.assetCode)) {
      res.status(400).json({ success: false, error: 'assetCode must be "XLM" or "USDC"' });
      return;
    }

    const body = b as FactorBody;

    const riskOracle   = requireContractId('riskOracle');
    const luminaCore   = requireContractId('luminaCore');
    const adminKeypair = StellarService.adminKeypair();

    const invoiceHashParam = StellarService.bytes(invoiceId);

    const riskRaw = await stellar.queryContract(riskOracle, 'get_risk_score', [invoiceHashParam]);
    if (!riskRaw) {
      res.status(404).json({
        success: false,
        error:   'No risk score found for this invoice. Submit via POST /api/invoice/submit first.',
      });
      return;
    }
    const riskScore = StellarSdk.scValToNative(riskRaw) as number;

    const { txHash, returnValue } = await stellar.invokeContract(
      luminaCore,
      'factor_invoice',
      [StellarService.u64(body.invoiceNumericId), StellarService.u32(riskScore)],
      adminKeypair,
    );

    const factorResult = StellarSdk.scValToNative(returnValue) as {
      advance_amount?: bigint;
      apr_bps?: number;
    };
    const advanceAmount = Number(factorResult.advance_amount ?? 0n);

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
      res.status(400).json({ success: false, error: 'invoiceId must be 64 hex chars' });
      return;
    }

    const luminaCore = requireContractId('luminaCore');
    const raw = await stellar.queryContract(luminaCore, 'get_invoice', [StellarService.bytes(invoiceId)]);

    if (!raw) {
      res.status(404).json({ success: false, error: `Invoice ${invoiceId} not found on-chain.` });
      return;
    }

    res.status(200).json({ success: true, data: StellarSdk.scValToNative(raw) });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── POST /api/invoice/repay/:invoiceId ────────────────────────────────────────

interface RepayBody { nullifier: string; }

router.post('/repay/:invoiceId', async (req: Request, res: Response): Promise<void> => {
  try {
    const rawId     = req.params['invoiceId'] as string;
    const invoiceId = parseInt(rawId, 10);
    if (isNaN(invoiceId) || invoiceId <= 0) {
      res.status(400).json({ success: false, error: 'invoiceId must be a positive integer' });
      return;
    }

    const b = req.body as Partial<RepayBody>;
    if (!isHex32(b.nullifier)) {
      res.status(400).json({ success: false, error: 'nullifier must be 64 hex chars' });
      return;
    }
    const { nullifier } = b as RepayBody;

    const luminaCore        = requireContractId('luminaCore');
    const nullifierRegistry = requireContractId('nullifierRegistry');
    const adminKeypair      = StellarService.adminKeypair();

    const { txHash: repayTxHash, returnValue } = await stellar.invokeContract(
      luminaCore, 'repay', [StellarService.u64(invoiceId)], adminKeypair,
    );

    const ok = StellarSdk.scValToNative(returnValue) as boolean;
    if (!ok) {
      res.status(409).json({
        success: false,
        error:   `repay() returned false — invoice #${invoiceId} may not be in Funded state.`,
        tx_hash: repayTxHash,
      });
      return;
    }

    const { txHash: stateTxHash } = await stellar.invokeContract(
      nullifierRegistry,
      'update_state',
      [StellarService.bytes(nullifier), StellarService.symbol('Repaid')],
      adminKeypair,
    );

    res.status(200).json({
      success: true,
      data: { invoice_id: invoiceId, nullifier, repay_tx: repayTxHash, registry_tx: stateTxHash },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── POST /api/invoice/mark-default/:invoiceId ─────────────────────────────────

interface MarkDefaultBody { nullifier: string; senior_loss: number; }

router.post('/mark-default/:invoiceId', async (req: Request, res: Response): Promise<void> => {
  try {
    const rawId     = req.params['invoiceId'] as string;
    const invoiceId = parseInt(rawId, 10);
    if (isNaN(invoiceId) || invoiceId <= 0) {
      res.status(400).json({ success: false, error: 'invoiceId must be a positive integer' });
      return;
    }

    const b = req.body as Partial<MarkDefaultBody>;
    if (!isHex32(b.nullifier)) {
      res.status(400).json({ success: false, error: 'nullifier must be 64 hex chars' });
      return;
    }
    if (typeof b.senior_loss !== 'number' || b.senior_loss < 0) {
      res.status(400).json({ success: false, error: 'senior_loss must be a non-negative number' });
      return;
    }
    const { nullifier, senior_loss } = b as MarkDefaultBody;

    const luminaCore        = requireContractId('luminaCore');
    const nullifierRegistry = requireContractId('nullifierRegistry');
    const liquidityPools    = requireContractId('liquidityPools');
    const adminKeypair      = StellarService.adminKeypair();

    const nullifierParam = StellarService.bytes(nullifier);

    const { txHash: markTxHash } = await stellar.invokeContract(
      luminaCore, 'mark_defaulted', [StellarService.u64(invoiceId)], adminKeypair,
    );
    const { txHash: disputedTxHash } = await stellar.invokeContract(
      nullifierRegistry, 'update_state',
      [nullifierParam, StellarService.symbol('Disputed')], adminKeypair,
    );
    const { txHash: defaultedTxHash } = await stellar.invokeContract(
      nullifierRegistry, 'update_state',
      [nullifierParam, StellarService.symbol('Defaulted')], adminKeypair,
    );
    const { txHash: protectionTxHash } = await stellar.invokeContract(
      liquidityPools, 'trigger_default_protection',
      [StellarService.u64(invoiceId), StellarService.u64(senior_loss)], adminKeypair,
    );

    res.status(200).json({
      success: true,
      data: {
        invoice_id: invoiceId, nullifier,
        mark_tx: markTxHash, disputed_tx: disputedTxHash,
        defaulted_tx: defaultedTxHash, protection_tx: protectionTxHash,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── Registry sub-router ───────────────────────────────────────────────────────

const registryRouter = Router();

registryRouter.get('/stats', async (_req: Request, res: Response): Promise<void> => {
  try {
    const nullifierRegistry = requireContractId('nullifierRegistry');
    const raw = await stellar.queryContract(nullifierRegistry, 'get_registry_stats', []);

    if (!raw) {
      res.status(503).json({ success: false, error: 'Registry stats unavailable.' });
      return;
    }
    res.status(200).json({ success: true, data: StellarSdk.scValToNative(raw) });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

registryRouter.get('/query/:nullifier', async (req: Request, res: Response): Promise<void> => {
  try {
    const nullifier = req.params['nullifier'] as string;
    if (!isHex32(nullifier)) {
      res.status(400).json({ success: false, error: 'nullifier must be 64 hex chars' });
      return;
    }

    const nullifierRegistry = requireContractId('nullifierRegistry');
    const raw = await stellar.queryContract(
      nullifierRegistry, 'query_state', [StellarService.bytes(nullifier)]
    );

    if (!raw) {
      res.status(404).json({ success: false, error: `No entry for nullifier: ${nullifier}` });
      return;
    }
    res.status(200).json({ success: true, data: StellarSdk.scValToNative(raw) });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

export { registryRouter };
export default router;
