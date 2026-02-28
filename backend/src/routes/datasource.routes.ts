/**
 * Datasource Routes  (/api/datasource)
 *
 * Provides OAuth2 + invoice-fetch flows for Xero and QuickBooks,
 * plus a manual-entry preparation endpoint.  Each flow ends by running
 * the RISC Zero ZK proof and returning the full ZkProofResult — including
 * the new `is_verified_source` and `data_source` fields.
 *
 * GET  /api/datasource/xero/auth
 *   → 302 redirect to Xero OAuth2 consent page
 *
 * GET  /api/datasource/xero/callback?code=<code>
 *   → exchange code for tokens, store in session, redirect to /connected
 *
 * POST /api/datasource/xero/fetch-invoice
 *   Body: { access_token, invoice_id, country_cds_spread?, sector_risk? }
 *   → fetch from Xero, transform, prove, return ZkProofResult
 *
 * POST /api/datasource/quickbooks/fetch-invoice
 *   Body: { access_token, realm_id, invoice_id, country_cds_spread?, sector_risk? }
 *   → same flow for QuickBooks, data_source: 2
 *
 * POST /api/datasource/manual/prepare
 *   Body: standard InvoiceProofInput fields (minus current_timestamp)
 *   → prove with data_source: 0, is_verified_source will be false
 */

import { Router, Request, Response } from 'express';
import {
  XeroAuthService,
  XeroInvoiceService,
  QuickBooksAuthService,
  QuickBooksInvoiceService,
} from '../services/xero.service';
import { ZkProverService, InvoiceProofInput } from '../services/zkProver.service';

const router = Router();

const xeroAuth    = new XeroAuthService();
const xeroInvoice = new XeroInvoiceService();
const qbAuth      = new QuickBooksAuthService();
const qbInvoice   = new QuickBooksInvoiceService();
const zkProver    = new ZkProverService();

// ── In-memory token session store ────────────────────────────────────────────
// Production: replace with Redis / signed cookies.
const tokenStore = new Map<string, {
  access_token:  string;
  refresh_token: string;
  expires_at:    number;
  source:        'xero' | 'quickbooks';
  realm_id?:     string; // QuickBooks only
}>();

// ── Validation helpers ────────────────────────────────────────────────────────

const HEX32_RE = /^[0-9a-fA-F]{64}$/;
function isHex32(s: unknown): s is string {
  return typeof s === 'string' && HEX32_RE.test(s);
}

// ═════════════════════════════════════════════════════════════════════════════
// XERO
// ═════════════════════════════════════════════════════════════════════════════

// GET /api/datasource/xero/auth
router.get('/xero/auth', (_req: Request, res: Response): void => {
  const authUrl = xeroAuth.getAuthUrl();
  res.redirect(authUrl);
});

// GET /api/datasource/xero/callback?code=<code>
router.get('/xero/callback', async (req: Request, res: Response): Promise<void> => {
  try {
    const code = req.query['code'];
    if (typeof code !== 'string' || !code) {
      res.status(400).json({ success: false, error: 'Missing OAuth2 code parameter' });
      return;
    }

    const tokens = await xeroAuth.exchangeCode(code);

    // Store tokens keyed by access_token prefix (simplified session key)
    const sessionKey = tokens.access_token.slice(0, 32);
    tokenStore.set(sessionKey, {
      access_token:  tokens.access_token,
      refresh_token: tokens.refresh_token,
      expires_at:    Math.floor(Date.now() / 1000) + tokens.expires_in,
      source:        'xero',
    });

    // In a browser flow this would redirect; API clients get the key to reuse
    res.status(200).json({
      success:     true,
      session_key: sessionKey,
      message:     'Xero connected. Use access_token in POST /xero/fetch-invoice.',
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// GET /api/datasource/xero/connected (landing after callback redirect)
router.get('/xero/connected', (_req: Request, res: Response): void => {
  res.status(200).json({ success: true, message: 'Xero OAuth2 flow complete.' });
});

// POST /api/datasource/xero/fetch-invoice
interface XeroFetchBody {
  access_token:        string;
  invoice_id:          string;
  country_cds_spread?: number; // u16 bps  — defaults to 150 (1.5%)
  sector_risk?:        number; // u8 0-100 — defaults to 50
}

router.post('/xero/fetch-invoice', async (req: Request, res: Response): Promise<void> => {
  try {
    const b = req.body as Partial<XeroFetchBody>;

    if (typeof b.access_token !== 'string' || !b.access_token) {
      res.status(400).json({ success: false, error: 'access_token is required' });
      return;
    }
    if (typeof b.invoice_id !== 'string' || !b.invoice_id) {
      res.status(400).json({ success: false, error: 'invoice_id is required' });
      return;
    }

    const body = b as XeroFetchBody;

    // ── Fetch from Xero ───────────────────────────────────────────────────
    const xeroData = await xeroInvoice.fetchInvoice(body.access_token, body.invoice_id);
    const rawBody  = JSON.stringify(xeroData);

    // ── Transform to ZK input ─────────────────────────────────────────────
    const partial = xeroInvoice.transformToInvoiceInput(xeroData, rawBody);

    const proofInput: InvoiceProofInput = {
      invoice_hash:          partial.invoice_hash          ?? new Array(32).fill(0) as number[],
      amount:                partial.amount                ?? 0,
      debtor_id:             partial.debtor_id             ?? new Array(32).fill(0) as number[],
      due_date:              partial.due_date              ?? 0,
      payment_history_score: partial.payment_history_score ?? 70,
      country_cds_spread:    body.country_cds_spread       ?? 150,
      sector_risk:           body.sector_risk              ?? 50,
      data_source:           partial.data_source,          // 1 = xero
      tls_proof_hash:        partial.tls_proof_hash        ?? new Array(32).fill(0) as number[],
      source_timestamp:      partial.source_timestamp      ?? 0,
    };

    // ── Generate ZK proof ─────────────────────────────────────────────────
    const proof = zkProver.prove(proofInput);

    if (!proof.is_valid) {
      res.status(400).json({
        success: false,
        error:   'Invoice failed ZK validation (amount ≤ 0 or already expired).',
        proof,
      });
      return;
    }

    res.status(200).json({
      success: true,
      source:  'xero',
      data: {
        invoice_id:         body.invoice_id,
        risk_score:         proof.risk_score,
        is_verified_source: proof.is_verified_source,
        data_source:        proof.data_source,
        nullifier:          proof.nullifier,
        invoice_hash:       proof.invoice_hash,
        is_valid:           proof.is_valid,
        receipt_bytes:      proof.receipt_bytes,
        image_id:           proof.image_id,
        mode:               proof.mode,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ═════════════════════════════════════════════════════════════════════════════
// QUICKBOOKS
// ═════════════════════════════════════════════════════════════════════════════

// GET /api/datasource/quickbooks/auth
router.get('/quickbooks/auth', (_req: Request, res: Response): void => {
  const authUrl = qbAuth.getAuthUrl();
  res.redirect(authUrl);
});

// GET /api/datasource/quickbooks/callback?code=<code>&realmId=<id>
router.get('/quickbooks/callback', async (req: Request, res: Response): Promise<void> => {
  try {
    const code    = req.query['code'];
    const realmId = req.query['realmId'];

    if (typeof code !== 'string' || !code) {
      res.status(400).json({ success: false, error: 'Missing code parameter' });
      return;
    }
    if (typeof realmId !== 'string' || !realmId) {
      res.status(400).json({ success: false, error: 'Missing realmId parameter' });
      return;
    }

    const result = await qbAuth.exchangeCode(code, realmId);
    const sessionKey = result.access_token.slice(0, 32);

    tokenStore.set(sessionKey, {
      access_token:  result.access_token,
      refresh_token: result.refresh_token,
      expires_at:    Math.floor(Date.now() / 1000) + result.expires_in,
      source:        'quickbooks',
      realm_id:      result.realm_id,
    });

    res.status(200).json({
      success:     true,
      session_key: sessionKey,
      realm_id:    realmId,
      message:     'QuickBooks connected.',
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// POST /api/datasource/quickbooks/fetch-invoice
interface QBFetchBody {
  access_token:        string;
  realm_id:            string;
  invoice_id:          string;
  country_cds_spread?: number;
  sector_risk?:        number;
}

router.post('/quickbooks/fetch-invoice', async (req: Request, res: Response): Promise<void> => {
  try {
    const b = req.body as Partial<QBFetchBody>;

    if (typeof b.access_token !== 'string' || !b.access_token) {
      res.status(400).json({ success: false, error: 'access_token is required' });
      return;
    }
    if (typeof b.realm_id !== 'string' || !b.realm_id) {
      res.status(400).json({ success: false, error: 'realm_id is required' });
      return;
    }
    if (typeof b.invoice_id !== 'string' || !b.invoice_id) {
      res.status(400).json({ success: false, error: 'invoice_id is required' });
      return;
    }

    const body = b as QBFetchBody;

    // ── Fetch from QuickBooks ─────────────────────────────────────────────
    const qbData  = await qbInvoice.fetchInvoice(body.access_token, body.realm_id, body.invoice_id);
    const rawBody = JSON.stringify(qbData);

    // ── Transform to ZK input ─────────────────────────────────────────────
    const partial = qbInvoice.transformToInvoiceInput(qbData, rawBody);

    const proofInput: InvoiceProofInput = {
      invoice_hash:          partial.invoice_hash          ?? new Array(32).fill(0) as number[],
      amount:                partial.amount                ?? 0,
      debtor_id:             partial.debtor_id             ?? new Array(32).fill(0) as number[],
      due_date:              partial.due_date              ?? 0,
      payment_history_score: partial.payment_history_score ?? 70,
      country_cds_spread:    body.country_cds_spread       ?? 150,
      sector_risk:           body.sector_risk              ?? 50,
      data_source:           partial.data_source,          // 2 = quickbooks
      tls_proof_hash:        partial.tls_proof_hash        ?? new Array(32).fill(0) as number[],
      source_timestamp:      partial.source_timestamp      ?? 0,
    };

    // ── Generate ZK proof ─────────────────────────────────────────────────
    const proof = zkProver.prove(proofInput);

    if (!proof.is_valid) {
      res.status(400).json({
        success: false,
        error:   'Invoice failed ZK validation.',
        proof,
      });
      return;
    }

    res.status(200).json({
      success: true,
      source:  'quickbooks',
      data: {
        invoice_id:         body.invoice_id,
        realm_id:           body.realm_id,
        risk_score:         proof.risk_score,
        is_verified_source: proof.is_verified_source,
        data_source:        proof.data_source,
        nullifier:          proof.nullifier,
        invoice_hash:       proof.invoice_hash,
        is_valid:           proof.is_valid,
        receipt_bytes:      proof.receipt_bytes,
        image_id:           proof.image_id,
        mode:               proof.mode,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ═════════════════════════════════════════════════════════════════════════════
// MANUAL
// ═════════════════════════════════════════════════════════════════════════════

// POST /api/datasource/manual/prepare
// Body mirrors the standard invoice submit fields (hex strings for hashes).
// data_source is forced to 0; is_verified_source will always be false.
interface ManualPrepareBody {
  invoice_hash:          string; // 32-byte hex
  amount:                number;
  debtor_id:             string; // 32-byte hex
  due_date:              number;
  payment_history_score: number;
  country_cds_spread:    number;
  sector_risk:           number;
}

router.post('/manual/prepare', async (req: Request, res: Response): Promise<void> => {
  try {
    const b = req.body as Partial<ManualPrepareBody>;

    if (!isHex32(b.invoice_hash)) {
      res.status(400).json({ success: false, error: 'invoice_hash must be a 32-byte hex string' });
      return;
    }
    if (!isHex32(b.debtor_id)) {
      res.status(400).json({ success: false, error: 'debtor_id must be a 32-byte hex string' });
      return;
    }
    const requiredNums: (keyof ManualPrepareBody)[] = [
      'amount', 'due_date', 'payment_history_score', 'country_cds_spread', 'sector_risk',
    ];
    const missing = requiredNums.filter((k) => typeof b[k] !== 'number');
    if (missing.length > 0) {
      res.status(400).json({ success: false, error: `Missing or invalid fields: ${missing.join(', ')}` });
      return;
    }

    const body = b as ManualPrepareBody;

    const proofInput: InvoiceProofInput = {
      invoice_hash:          ZkProverService.hexToBytes32(body.invoice_hash),
      amount:                body.amount,
      debtor_id:             ZkProverService.hexToBytes32(body.debtor_id),
      due_date:              body.due_date,
      payment_history_score: body.payment_history_score,
      country_cds_spread:    body.country_cds_spread,
      sector_risk:           body.sector_risk,
      data_source:           0,  // manual
      tls_proof_hash:        new Array(32).fill(0) as number[],
      source_timestamp:      0,
    };

    const proof = zkProver.prove(proofInput);

    res.status(200).json({
      success: true,
      source:  'manual',
      warning: !proof.is_verified_source
        ? 'Unverified data source. Consider using Xero or QuickBooks integration for a lower risk score.'
        : undefined,
      data: {
        risk_score:         proof.risk_score,
        is_verified_source: proof.is_verified_source,
        data_source:        proof.data_source,
        nullifier:          proof.nullifier,
        invoice_hash:       proof.invoice_hash,
        is_valid:           proof.is_valid,
        receipt_bytes:      proof.receipt_bytes,
        image_id:           proof.image_id,
        mode:               proof.mode,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

export default router;
