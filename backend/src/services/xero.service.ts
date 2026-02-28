/**
 * Xero + QuickBooks Integration Services
 *
 * Provides:
 *   XeroAuthService      — OAuth2 URL construction, code exchange, token refresh
 *   XeroInvoiceService   — fetch invoice from Xero API + transform to ZK input
 *   QuickBooksInvoiceService — same for QuickBooks API
 *
 * Both services compute tls_proof_hash = SHA-256(response_body + source_timestamp)
 * and inject it into the InvoiceProofInput so the ZK circuit can attest to the
 * exact data used in the proof.
 */

import * as crypto from 'crypto';
import { InvoiceProofInput } from './zkProver.service';

// ── Shared helpers ─────────────────────────────────────────────────────────────

/** SHA-256 of a UTF-8 string → 32-byte number[]. */
function sha256Bytes(data: string): number[] {
  return Array.from(crypto.createHash('sha256').update(data, 'utf8').digest());
}

/**
 * Parse Xero's /Date(ms+tz)/ format to Unix seconds.
 * Falls back to Date.parse for ISO strings.
 */
function parseXeroDate(raw: string): number {
  const m = /\/Date\((\d+)/.exec(raw);
  if (m) return Math.floor(parseInt(m[1]!, 10) / 1000);
  const ts = Date.parse(raw);
  return isNaN(ts) ? 0 : Math.floor(ts / 1000);
}

/** Compute payment_history_score (0-100) from a Xero invoice status. */
function xeroPaymentScore(status: string, overdueFlag?: boolean): number {
  if (overdueFlag) return 35;
  switch (status.toUpperCase()) {
    case 'PAID':        return 90;
    case 'AUTHORISED':  return 70;
    case 'DRAFT':       return 55;
    case 'VOIDED':
    case 'DELETED':     return 20;
    default:            return 60;
  }
}

// ── Xero ──────────────────────────────────────────────────────────────────────

export interface XeroTokens {
  access_token:  string;
  refresh_token: string;
  expires_in:    number;
  token_type:    string;
}

/**
 * Minimal shape of a Xero Invoice API response.
 * Only the fields we actually use are typed; the rest are unknown.
 */
export interface XeroInvoiceData {
  Invoices: Array<{
    InvoiceID:   string;
    Status:      string;
    AmountDue:   number;
    DueDate:     string;
    Contact: {
      ContactID: string;
      Name?:     string;
    };
    Payments?: Array<{ Amount: number; Date: string }>;
    CurrencyCode?: string;
  }>;
}

// ── XeroAuthService ───────────────────────────────────────────────────────────

export class XeroAuthService {
  private static readonly TOKEN_URL = 'https://identity.xero.com/connect/token';
  private static readonly AUTH_BASE  = 'https://login.xero.com/identity/connect/authorize';

  private readonly clientId:     string;
  private readonly clientSecret: string;
  private readonly redirectUri:  string;

  constructor() {
    this.clientId     = process.env['XERO_CLIENT_ID']     ?? '';
    this.clientSecret = process.env['XERO_CLIENT_SECRET'] ?? '';
    this.redirectUri  = process.env['XERO_REDIRECT_URI']  ?? 'http://localhost:4000/api/datasource/xero/callback';

    if (!this.clientId) {
      console.warn('[XeroAuth] XERO_CLIENT_ID not set — Xero integration will fail at runtime');
    }
  }

  /**
   * Build the Xero OAuth2 authorisation URL to redirect the user to.
   * scope: accounting.transactions.read + accounting.contacts.read
   */
  getAuthUrl(): string {
    const params = new URLSearchParams({
      response_type: 'code',
      client_id:     this.clientId,
      redirect_uri:  this.redirectUri,
      scope:         'accounting.transactions.read accounting.contacts.read offline_access',
      state:         crypto.randomBytes(16).toString('hex'),
    });
    return `${XeroAuthService.AUTH_BASE}?${params.toString()}`;
  }

  /** Exchange an authorisation code for access + refresh tokens. */
  async exchangeCode(code: string): Promise<XeroTokens> {
    return this.tokenRequest({
      grant_type:   'authorization_code',
      code,
      redirect_uri: this.redirectUri,
    });
  }

  /** Refresh an expired access token. */
  async refreshToken(refresh_token: string): Promise<XeroTokens> {
    return this.tokenRequest({
      grant_type:    'refresh_token',
      refresh_token,
    });
  }

  private async tokenRequest(body: Record<string, string>): Promise<XeroTokens> {
    const credentials = Buffer.from(
      `${this.clientId}:${this.clientSecret}`
    ).toString('base64');

    const resp = await fetch(XeroAuthService.TOKEN_URL, {
      method:  'POST',
      headers: {
        'Authorization': `Basic ${credentials}`,
        'Content-Type':  'application/x-www-form-urlencoded',
      },
      body: new URLSearchParams(body).toString(),
    });

    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`Xero token request failed (${resp.status}): ${text}`);
    }

    return resp.json() as Promise<XeroTokens>;
  }
}

// ── XeroInvoiceService ────────────────────────────────────────────────────────

export class XeroInvoiceService {
  private static readonly API_BASE = 'https://api.xero.com/api.xro/2.0';

  /**
   * Fetch a single invoice from the Xero API.
   *
   * @param access_token - Bearer token (from XeroAuthService.exchangeCode)
   * @param invoice_id   - Xero InvoiceID GUID
   * @returns raw Xero invoice response
   */
  async fetchInvoice(access_token: string, invoice_id: string): Promise<XeroInvoiceData> {
    const url = `${XeroInvoiceService.API_BASE}/Invoices/${encodeURIComponent(invoice_id)}`;

    const resp = await fetch(url, {
      headers: {
        'Authorization': `Bearer ${access_token}`,
        'Accept':        'application/json',
        'Xero-tenant-id': process.env['XERO_TENANT_ID'] ?? '',
      },
    });

    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`Xero API error (${resp.status}) for invoice ${invoice_id}: ${text}`);
    }

    return resp.json() as Promise<XeroInvoiceData>;
  }

  /**
   * Transform a Xero invoice into an `InvoiceProofInput` partial.
   *
   * Computes:
   *   invoice_hash    = SHA-256(InvoiceID + AmountDue + DueDate)
   *   debtor_id       = SHA-256(ContactID)
   *   tls_proof_hash  = SHA-256(raw response body + source_timestamp string)
   *   data_source     = 1 (xero)
   */
  transformToInvoiceInput(
    xeroData: XeroInvoiceData,
    rawResponseBody: string,
  ): Partial<InvoiceProofInput> & { data_source: number } {
    const invoice = xeroData.Invoices[0];
    if (!invoice) throw new Error('Xero response contained no invoices');

    const sourceTimestamp = Math.floor(Date.now() / 1000);
    const dueDate         = parseXeroDate(invoice.DueDate);

    // invoice_hash: SHA-256(InvoiceID + AmountDue + DueDate)
    const hashInput = `${invoice.InvoiceID}|${invoice.AmountDue}|${invoice.DueDate}`;

    // tls_proof_hash: SHA-256(raw_body + source_timestamp) — ties proof to exact fetch
    const tlsInput = rawResponseBody + String(sourceTimestamp);

    // payment_history_score: infer from status
    const now = Math.floor(Date.now() / 1000);
    const isOverdue = dueDate > 0 && dueDate < now && invoice.Status !== 'PAID';
    const paymentScore = xeroPaymentScore(invoice.Status, isOverdue);

    return {
      invoice_hash:          sha256Bytes(hashInput),
      // AmountDue in Xero is decimal (e.g. 1234.56 USD) → convert to micros (×1_000_000)
      amount:                Math.round(invoice.AmountDue * 1_000_000),
      debtor_id:             sha256Bytes(invoice.Contact.ContactID),
      due_date:              dueDate,
      payment_history_score: paymentScore,
      // CDS spread and sector risk are not available from Xero — caller must supply them
      // or they default to conservative values in the route handler.
      data_source:           1,
      tls_proof_hash:        sha256Bytes(tlsInput),
      source_timestamp:      sourceTimestamp,
    };
  }
}

// ── QuickBooks ─────────────────────────────────────────────────────────────────

export interface QuickBooksTokens {
  access_token:  string;
  refresh_token: string;
  expires_in:    number;
  token_type:    string;
}

/** Minimal QuickBooks Invoice response shape. */
export interface QuickBooksInvoiceData {
  Invoice: {
    Id:            string;
    Balance:       number;
    DueDate:       string; // ISO 8601: "2025-03-31"
    CustomerRef: {
      value: string;
      name?: string;
    };
    TotalAmt:      number;
    EmailStatus?:  string;
  };
  time: string;
}

// ── QuickBooksAuthService ─────────────────────────────────────────────────────

export class QuickBooksAuthService {
  private static readonly TOKEN_URL = 'https://oauth.platform.intuit.com/op/v2/token';
  private static readonly AUTH_BASE  = 'https://appcenter.intuit.com/connect/oauth2';

  private readonly clientId:     string;
  private readonly clientSecret: string;
  private readonly redirectUri:  string;

  constructor() {
    this.clientId     = process.env['QB_CLIENT_ID']     ?? '';
    this.clientSecret = process.env['QB_CLIENT_SECRET'] ?? '';
    this.redirectUri  = process.env['QB_REDIRECT_URI']  ?? 'http://localhost:4000/api/datasource/quickbooks/callback';

    if (!this.clientId) {
      console.warn('[QuickBooksAuth] QB_CLIENT_ID not set — QuickBooks integration will fail at runtime');
    }
  }

  /** Build the QuickBooks OAuth2 authorisation URL. */
  getAuthUrl(): string {
    const params = new URLSearchParams({
      client_id:     this.clientId,
      scope:         'com.intuit.quickbooks.accounting',
      redirect_uri:  this.redirectUri,
      response_type: 'code',
      state:         crypto.randomBytes(16).toString('hex'),
    });
    return `${QuickBooksAuthService.AUTH_BASE}?${params.toString()}`;
  }

  async exchangeCode(code: string, realmId: string): Promise<QuickBooksTokens & { realm_id: string }> {
    const tokens = await this.tokenRequest({ grant_type: 'authorization_code', code });
    return { ...tokens, realm_id: realmId };
  }

  async refreshToken(refresh_token: string): Promise<QuickBooksTokens> {
    return this.tokenRequest({ grant_type: 'refresh_token', refresh_token });
  }

  private async tokenRequest(body: Record<string, string>): Promise<QuickBooksTokens> {
    const credentials = Buffer.from(`${this.clientId}:${this.clientSecret}`).toString('base64');
    const resp = await fetch(QuickBooksAuthService.TOKEN_URL, {
      method:  'POST',
      headers: {
        'Authorization': `Basic ${credentials}`,
        'Content-Type':  'application/x-www-form-urlencoded',
        'Accept':        'application/json',
      },
      body: new URLSearchParams(body).toString(),
    });

    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`QuickBooks token request failed (${resp.status}): ${text}`);
    }
    return resp.json() as Promise<QuickBooksTokens>;
  }
}

// ── QuickBooksInvoiceService ──────────────────────────────────────────────────

export class QuickBooksInvoiceService {
  private static readonly API_BASE = 'https://quickbooks.api.intuit.com/v3/company';

  /**
   * Fetch a single invoice from the QuickBooks API.
   *
   * @param access_token - Bearer token
   * @param realm_id     - QuickBooks company/realm ID
   * @param invoice_id   - QB Invoice entity ID
   */
  async fetchInvoice(
    access_token: string,
    realm_id:     string,
    invoice_id:   string,
  ): Promise<QuickBooksInvoiceData> {
    const url = `${QuickBooksInvoiceService.API_BASE}/${encodeURIComponent(realm_id)}/invoice/${encodeURIComponent(invoice_id)}?minorversion=65`;

    const resp = await fetch(url, {
      headers: {
        'Authorization': `Bearer ${access_token}`,
        'Accept':        'application/json',
      },
    });

    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`QuickBooks API error (${resp.status}) for invoice ${invoice_id}: ${text}`);
    }

    return resp.json() as Promise<QuickBooksInvoiceData>;
  }

  /**
   * Transform a QuickBooks invoice into an `InvoiceProofInput` partial.
   * data_source = 2 (quickbooks)
   */
  transformToInvoiceInput(
    qbData:          QuickBooksInvoiceData,
    rawResponseBody: string,
  ): Partial<InvoiceProofInput> & { data_source: number } {
    const inv = qbData.Invoice;
    const sourceTimestamp = Math.floor(Date.now() / 1000);
    const dueDate = Math.floor(Date.parse(inv.DueDate) / 1000);

    const hashInput = `${inv.Id}|${inv.Balance}|${inv.DueDate}`;
    const tlsInput  = rawResponseBody + String(sourceTimestamp);

    const now       = Math.floor(Date.now() / 1000);
    const isOverdue = dueDate > 0 && dueDate < now;
    const paymentScore = isOverdue ? 35 : 70;

    return {
      invoice_hash:          sha256Bytes(hashInput),
      // Balance is the outstanding amount in account currency decimal
      amount:                Math.round(inv.Balance * 1_000_000),
      debtor_id:             sha256Bytes(inv.CustomerRef.value),
      due_date:              dueDate,
      payment_history_score: paymentScore,
      data_source:           2,
      tls_proof_hash:        sha256Bytes(tlsInput),
      source_timestamp:      sourceTimestamp,
    };
  }
}
