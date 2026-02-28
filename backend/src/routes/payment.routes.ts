/**
 * Payment Routes  (/api/payment)
 *
 * Orchestrates Stellar disbursements and repayment verification.
 *
 * POST /api/payment/disburse
 *   Body: { invoiceId, advanceAmount, recipientAddress, assetCode, anchorDomain? }
 *   → disburse advance_amount to SME via XLM / USDC / SEP-31 anchor
 *   → returns DisbursementResult
 *
 * POST /api/payment/repay
 *   Body: { invoiceId, payerAddress, amount, assetCode, stellarTxHash, nullifier }
 *   → verify Horizon tx → lumina-core.repay() → registry update_state(Repaid)
 *   → returns RepaymentResult
 *
 * GET /api/payment/anchor/toml?domain=<domain>
 *   → fetch & return the anchor's stellar.toml (5-min cached)
 *
 * GET /api/payment/status/:invoiceId
 *   → lumina-core.get_invoice() on-chain state
 */

import { Router, Request, Response } from 'express';
import * as StellarSdk from '@stellar/stellar-sdk';
import { PaymentService }   from '../services/payment.service';
import { AnchorService }    from '../services/anchor.service';
import { StellarService }   from '../services/stellar.service';
import { requireContractId } from '../stellar/contractIds';

const router          = Router();
const paymentService  = new PaymentService();
const anchorService   = new AnchorService();
const stellar         = new StellarService();

// ── Validation helpers ────────────────────────────────────────────────────────

const HEX32_RE = /^[0-9a-fA-F]{64}$/;
function isHex32(s: unknown): s is string {
  return typeof s === 'string' && HEX32_RE.test(s);
}

function isStellarAddress(s: unknown): s is string {
  return typeof s === 'string' && /^G[A-Z2-7]{55}$/.test(s);
}

// ── POST /api/payment/disburse ────────────────────────────────────────────────
//
// Body (JSON):
//   invoiceId        number   — on-chain u64 invoice ID
//   advanceAmount    number   — amount in stroops (7-decimal Stellar amounts)
//   recipientAddress string   — SME's Stellar G-address
//   assetCode        string   — "XLM" | "USDC"
//   anchorDomain?    string   — e.g. "anchor.example.com" for SEP-31 payout

interface DisburseBody {
  invoiceId:        number;
  advanceAmount:    number;
  recipientAddress: string;
  assetCode:        string;
  anchorDomain?:    string;
}

router.post('/disburse', async (req: Request, res: Response): Promise<void> => {
  try {
    const b = req.body as Partial<DisburseBody>;

    if (typeof b.invoiceId !== 'number' || b.invoiceId <= 0) {
      res.status(400).json({ success: false, error: 'invoiceId must be a positive number' });
      return;
    }
    if (typeof b.advanceAmount !== 'number' || b.advanceAmount <= 0) {
      res.status(400).json({ success: false, error: 'advanceAmount must be a positive number (stroops)' });
      return;
    }
    if (!isStellarAddress(b.recipientAddress)) {
      res.status(400).json({ success: false, error: 'recipientAddress must be a valid Stellar G-address' });
      return;
    }
    if (typeof b.assetCode !== 'string' || !['XLM', 'USDC'].includes(b.assetCode)) {
      res.status(400).json({ success: false, error: 'assetCode must be "XLM" or "USDC"' });
      return;
    }
    if (b.anchorDomain !== undefined && typeof b.anchorDomain !== 'string') {
      res.status(400).json({ success: false, error: 'anchorDomain must be a string' });
      return;
    }

    const body = b as DisburseBody;

    const result = await paymentService.disburseFunds({
      invoiceId:        body.invoiceId,
      advanceAmount:    body.advanceAmount,
      recipientAddress: body.recipientAddress,
      assetCode:        body.assetCode,
      anchorDomain:     body.anchorDomain,
    });

    if (!result.success) {
      res.status(502).json({ success: false, error: 'Disbursement failed', detail: result });
      return;
    }

    res.status(200).json({ success: true, data: result });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── POST /api/payment/repay ───────────────────────────────────────────────────
//
// Body (JSON):
//   invoiceId      number  — on-chain u64 invoice ID
//   payerAddress   string  — Stellar G-address of the debtor
//   amount         number  — expected repayment in stroops
//   assetCode      string  — "XLM" | "USDC"
//   stellarTxHash  string  — Horizon transaction hash to verify
//   nullifier      string  — 32-byte hex; used to update registry state

interface RepayBody {
  invoiceId:      number;
  payerAddress:   string;
  amount:         number;
  assetCode:      string;
  stellarTxHash:  string;
  nullifier:      string;
}

router.post('/repay', async (req: Request, res: Response): Promise<void> => {
  try {
    const b = req.body as Partial<RepayBody>;

    if (typeof b.invoiceId !== 'number' || b.invoiceId <= 0) {
      res.status(400).json({ success: false, error: 'invoiceId must be a positive number' });
      return;
    }
    if (!isStellarAddress(b.payerAddress)) {
      res.status(400).json({ success: false, error: 'payerAddress must be a valid Stellar G-address' });
      return;
    }
    if (typeof b.amount !== 'number' || b.amount <= 0) {
      res.status(400).json({ success: false, error: 'amount must be a positive number (stroops)' });
      return;
    }
    if (typeof b.assetCode !== 'string' || !['XLM', 'USDC'].includes(b.assetCode)) {
      res.status(400).json({ success: false, error: 'assetCode must be "XLM" or "USDC"' });
      return;
    }
    if (typeof b.stellarTxHash !== 'string' || !b.stellarTxHash) {
      res.status(400).json({ success: false, error: 'stellarTxHash is required' });
      return;
    }
    if (!isHex32(b.nullifier)) {
      res.status(400).json({ success: false, error: 'nullifier must be a 32-byte hex string (64 hex chars)' });
      return;
    }

    const body = b as RepayBody;

    const result = await paymentService.collectRepayment({
      invoiceId:      body.invoiceId,
      payerAddress:   body.payerAddress,
      amount:         body.amount,
      assetCode:      body.assetCode,
      stellarTxHash:  body.stellarTxHash,
      nullifier:      body.nullifier,
    });

    if (!result.success) {
      res.status(result.verified_on_horizon ? 409 : 404).json({
        success: false,
        error:   result.message ?? 'Repayment processing failed',
        data:    result,
      });
      return;
    }

    res.status(200).json({ success: true, data: result });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── GET /api/payment/anchor/toml ──────────────────────────────────────────────
//
// Query params:
//   domain  string  — anchor domain (e.g. "anchor.example.com")
//
// Returns the parsed stellar.toml fields (AUTH_SERVER, DIRECT_PAYMENT_SERVER, etc.)
// Results are served from the 5-minute in-process cache.

router.get('/anchor/toml', async (req: Request, res: Response): Promise<void> => {
  try {
    const domain = req.query['domain'];
    if (typeof domain !== 'string' || !domain) {
      res.status(400).json({ success: false, error: 'domain query parameter is required' });
      return;
    }

    const toml = await anchorService.getStellarToml(domain);
    res.status(200).json({ success: true, data: toml });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── GET /api/payment/status/:invoiceId ────────────────────────────────────────
//
// Path param:
//   invoiceId  number  — on-chain u64 invoice ID
//
// Returns the lumina-core invoice record (state, advance_amount, apr_bps, etc.)

router.get('/status/:invoiceId', async (req: Request, res: Response): Promise<void> => {
  try {
    const rawId     = req.params['invoiceId'] as string;
    const invoiceId = parseInt(rawId, 10);
    if (isNaN(invoiceId) || invoiceId <= 0) {
      res.status(400).json({
        success: false,
        error: 'invoiceId must be a positive integer (the on-chain u64 invoice ID)',
      });
      return;
    }

    const luminaCore = requireContractId('luminaCore');

    const raw = await stellar.queryContract(
      luminaCore,
      'get_invoice',
      [StellarService.u64(invoiceId)],
    );

    if (!raw) {
      res.status(404).json({
        success: false,
        error: `Invoice #${invoiceId} not found on-chain.`,
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

export default router;
