/**
 * AnchorService — Stellar SEP-10 / SEP-24 / SEP-31
 *
 * Implements the three SEPs needed for Lumina's cross-border disbursement:
 *
 *   SEP-10  Authentication    — challenge/response JWT with anchor
 *   SEP-24  Interactive flow  — anchor-hosted withdrawal UI + polling
 *   SEP-31  Direct payment    — server-to-server cross-border transfer
 *
 * All network calls use the global fetch (Node 18+, @types/node 25).
 */

import * as StellarSdk from '@stellar/stellar-sdk';

// ── Types ─────────────────────────────────────────────────────────────────────

/** Relevant subset of a Stellar TOML file. */
export interface StellarTomlData {
  /** Base URL of the SEP-24 Transfer Server. */
  TRANSFER_SERVER_SEP0024: string;
  /** Base URL of the SEP-10 Authentication Server. */
  AUTH_SERVER: string;
  /** Base URL of the SEP-31 Direct Payment Server (optional). */
  DIRECT_PAYMENT_SERVER?: string;
  /** Anchor signing key (used to verify SEP-10 challenge). */
  SIGNING_KEY?: string;
}

/** Status shape for a SEP-24 or SEP-31 transaction. */
export interface Sep24Transaction {
  id:                       string;
  status:                   string;   // "pending_anchor" | "completed" | "error" | ...
  amount_in?:               string;
  amount_out?:              string;
  amount_fee?:              string;
  stellar_transaction_id?:  string;
  external_transaction_id?: string;
  more_info_url?:           string;
  to?:                      string;
  from?:                    string;
  message?:                 string;
  refunded?:                boolean;
}

// ── TOML cache entry ──────────────────────────────────────────────────────────

interface TomlCacheEntry {
  data:      StellarTomlData;
  expiresAt: number;  // Unix seconds
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/** Extract a string value from a Stellar TOML document using a regex approach. */
function parseTomlString(toml: string, key: string): string {
  // Matches:  KEY = "value"   (with optional spaces around =)
  const re = new RegExp(`^${key}\\s*=\\s*"([^"\\r\\n]*)"`, 'm');
  return toml.match(re)?.[1] ?? '';
}

/** Convert a Stellar amount string to a number (stroops × 10^7). */
export function stroopsToAmount(stroops: number): string {
  return (stroops / 10_000_000).toFixed(7);
}

// ── AnchorService ─────────────────────────────────────────────────────────────

export class AnchorService {
  private readonly networkPassphrase: string;
  private readonly tomlCache = new Map<string, TomlCacheEntry>();
  private static readonly TOML_TTL_SECS = 300; // 5 minutes

  constructor() {
    this.networkPassphrase =
      process.env['STELLAR_NETWORK_PASSPHRASE'] ?? StellarSdk.Networks.TESTNET;
  }

  // ── SEP-1: TOML ────────────────────────────────────────────────────────────

  /**
   * Fetch and parse the anchor's stellar.toml.
   * Results are cached for 5 minutes.
   */
  async getStellarToml(anchorDomain: string): Promise<StellarTomlData> {
    const now = Math.floor(Date.now() / 1000);
    const cached = this.tomlCache.get(anchorDomain);
    if (cached && cached.expiresAt > now) return cached.data;

    const url = `https://${anchorDomain}/.well-known/stellar.toml`;
    const resp = await fetch(url);
    if (!resp.ok) {
      throw new Error(
        `Failed to fetch stellar.toml from ${anchorDomain}: HTTP ${resp.status}`
      );
    }

    const raw = await resp.text();

    const data: StellarTomlData = {
      TRANSFER_SERVER_SEP0024: parseTomlString(raw, 'TRANSFER_SERVER_SEP0024'),
      AUTH_SERVER:             parseTomlString(raw, 'AUTH_SERVER'),
      DIRECT_PAYMENT_SERVER:   parseTomlString(raw, 'DIRECT_PAYMENT_SERVER') || undefined,
      SIGNING_KEY:             parseTomlString(raw, 'SIGNING_KEY') || undefined,
    };

    if (!data.TRANSFER_SERVER_SEP0024) {
      throw new Error(
        `stellar.toml at ${anchorDomain} is missing TRANSFER_SERVER_SEP0024`
      );
    }
    if (!data.AUTH_SERVER) {
      throw new Error(`stellar.toml at ${anchorDomain} is missing AUTH_SERVER`);
    }

    this.tomlCache.set(anchorDomain, {
      data,
      expiresAt: now + AnchorService.TOML_TTL_SECS,
    });

    return data;
  }

  // ── SEP-10: Authentication ─────────────────────────────────────────────────

  /**
   * Complete the SEP-10 challenge/response flow and return a JWT.
   *
   * Flow:
   *   1. GET  <AUTH_SERVER>/auth?account=<publicKey>  → challenge XDR
   *   2. Sign the challenge transaction with `keypair`
   *   3. POST <AUTH_SERVER>/auth { transaction: <signedXDR> } → JWT
   */
  async getSep10Token(anchorDomain: string, keypair: StellarSdk.Keypair): Promise<string> {
    const { AUTH_SERVER } = await this.getStellarToml(anchorDomain);

    // Step 1: Fetch challenge
    const challengeUrl = `${AUTH_SERVER}/auth?account=${keypair.publicKey()}`;
    const challengeResp = await fetch(challengeUrl);
    if (!challengeResp.ok) {
      const text = await challengeResp.text();
      throw new Error(`SEP-10 challenge request failed (${challengeResp.status}): ${text}`);
    }

    const challengeBody = await challengeResp.json() as { transaction?: string; error?: string };
    if (challengeBody.error || !challengeBody.transaction) {
      throw new Error(`SEP-10 challenge error: ${challengeBody.error ?? 'no transaction field'}`);
    }

    // Step 2: Sign the challenge transaction
    const challengeTx = new StellarSdk.Transaction(
      challengeBody.transaction,
      this.networkPassphrase
    );
    challengeTx.sign(keypair);
    const signedXdr = challengeTx.toEnvelope().toXDR('base64');

    // Step 3: Submit signed challenge for JWT
    const authResp = await fetch(`${AUTH_SERVER}/auth`, {
      method:  'POST',
      headers: { 'Content-Type': 'application/json' },
      body:    JSON.stringify({ transaction: signedXdr }),
    });

    if (!authResp.ok) {
      const text = await authResp.text();
      throw new Error(`SEP-10 auth submission failed (${authResp.status}): ${text}`);
    }

    const authBody = await authResp.json() as { token?: string; error?: string };
    if (authBody.error || !authBody.token) {
      throw new Error(`SEP-10 JWT error: ${authBody.error ?? 'no token field'}`);
    }

    return authBody.token;
  }

  // ── SEP-24: Interactive Withdrawal ────────────────────────────────────────

  /**
   * Initiate a SEP-24 interactive withdrawal.
   *
   * Returns the anchor's `transaction_id` and the `url` for the customer to
   * complete the interactive flow (KYC, bank details, etc.).
   */
  async initiateWithdrawal(params: {
    anchorDomain: string;
    jwt:          string;
    assetCode:    string;
    assetIssuer:  string;
    amount:       string;
    account:      string;
    memo?:        string;
  }): Promise<Sep24Transaction> {
    const { TRANSFER_SERVER_SEP0024 } = await this.getStellarToml(params.anchorDomain);

    const body = new URLSearchParams({
      asset_code:   params.assetCode,
      asset_issuer: params.assetIssuer,
      amount:       params.amount,
      account:      params.account,
    });
    if (params.memo) body.set('memo', params.memo);

    const resp = await fetch(
      `${TRANSFER_SERVER_SEP0024}/transactions/withdraw/interactive`,
      {
        method:  'POST',
        headers: {
          'Authorization': `Bearer ${params.jwt}`,
          'Content-Type':  'application/x-www-form-urlencoded',
        },
        body: body.toString(),
      }
    );

    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`SEP-24 withdraw initiation failed (${resp.status}): ${text}`);
    }

    const data = await resp.json() as {
      id?:    string;
      type?:  string;
      url?:   string;
      error?: string;
    };

    if (data.error || !data.id) {
      throw new Error(`SEP-24 withdraw error: ${data.error ?? 'no id returned'}`);
    }

    return {
      id:           data.id,
      status:       'pending_anchor',
      more_info_url: data.url,
    };
  }

  /**
   * Poll a SEP-24 (or SEP-31) transaction until it completes or errors.
   *
   * Attempts up to `maxAttempts` polls with a 3-second interval.
   * Returns the final transaction status object.
   */
  async pollTransactionStatus(
    anchorDomain:    string,
    jwt:             string,
    transactionId:   string,
    maxAttempts:     number = 20,
  ): Promise<Sep24Transaction> {
    const { TRANSFER_SERVER_SEP0024 } = await this.getStellarToml(anchorDomain);
    const pollUrl = `${TRANSFER_SERVER_SEP0024}/transaction?id=${encodeURIComponent(transactionId)}`;

    for (let attempt = 1; attempt <= maxAttempts; attempt++) {
      await sleep(3_000);

      const resp = await fetch(pollUrl, {
        headers: { 'Authorization': `Bearer ${jwt}` },
      });

      if (!resp.ok) {
        // Transient network error — keep trying until maxAttempts
        console.warn(
          `[AnchorService] poll attempt ${attempt}/${maxAttempts} — HTTP ${resp.status}`
        );
        continue;
      }

      const body = await resp.json() as { transaction?: Sep24Transaction; error?: string };
      const tx   = body.transaction;

      if (!tx) {
        throw new Error(`SEP-24 poll: no transaction in response — ${body.error ?? ''}`);
      }

      console.info(
        `[AnchorService] poll ${attempt}/${maxAttempts} — id=${transactionId} status=${tx.status}`
      );

      if (tx.status === 'completed' || tx.status === 'error' || tx.status === 'refunded') {
        return tx;
      }
    }

    // Return last known state as a timeout indicator
    return {
      id:      transactionId,
      status:  'pending_anchor',
      message: `Polling timed out after ${maxAttempts} attempts (${maxAttempts * 3}s)`,
    };
  }

  // ── SEP-31: Cross-border Direct Payment ───────────────────────────────────

  /**
   * Send a SEP-31 direct payment.
   *
   * Flow:
   *   1. POST <DIRECT_PAYMENT_SERVER>/transactions → get anchor's Stellar account + memo
   *   2. Build a Stellar payment from admin → anchor's stellar_account_id
   *   3. Submit on-chain; return anchor transaction_id
   *
   * @returns anchor `transaction_id`
   */
  async sendSep31Payment(params: {
    anchorDomain:    string;
    jwt:             string;
    amount:          string;
    assetCode:       string;
    assetIssuer:     string;
    receiverAccount: string;
    fields: {
      transaction: {
        routing_number?: string;
        account_number?: string;
      };
    };
  }): Promise<string> {
    const toml = await this.getStellarToml(params.anchorDomain);

    const directPaymentServer = toml.DIRECT_PAYMENT_SERVER;
    if (!directPaymentServer) {
      throw new Error(
        `Anchor ${params.anchorDomain} does not support SEP-31 (no DIRECT_PAYMENT_SERVER in TOML)`
      );
    }

    // Step 1: Create the SEP-31 transaction record at the anchor
    const resp = await fetch(`${directPaymentServer}/transactions`, {
      method:  'POST',
      headers: {
        'Authorization': `Bearer ${params.jwt}`,
        'Content-Type':  'application/json',
      },
      body: JSON.stringify({
        amount:          params.amount,
        asset_code:      params.assetCode,
        asset_issuer:    params.assetIssuer,
        receiver_id:     params.receiverAccount,
        fields:          params.fields,
      }),
    });

    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`SEP-31 transaction creation failed (${resp.status}): ${text}`);
    }

    const body = await resp.json() as {
      id?:                  string;
      stellar_account_id?:  string;
      stellar_memo_type?:   string;
      stellar_memo?:        string;
      error?:               string;
    };

    if (body.error || !body.id) {
      throw new Error(`SEP-31 error: ${body.error ?? 'no id returned'}`);
    }

    const anchorTxId        = body.id;
    const anchorAccountId   = body.stellar_account_id;
    const anchorMemoType    = body.stellar_memo_type;
    const anchorMemo        = body.stellar_memo;

    // Step 2: Send the Stellar payment to the anchor's account
    if (anchorAccountId) {
      const adminKeypair = StellarSdk.Keypair.fromSecret(
        process.env['ADMIN_SECRET_KEY'] ?? ''
      );
      const rpcUrl = process.env['STELLAR_RPC_URL'] ?? 'https://soroban-testnet.stellar.org';
      const rpc    = new StellarSdk.rpc.Server(rpcUrl, { allowHttp: false });

      const account  = await rpc.getAccount(adminKeypair.publicKey());
      const asset    = params.assetCode === 'XLM'
        ? StellarSdk.Asset.native()
        : new StellarSdk.Asset(params.assetCode, params.assetIssuer);

      const builder = new StellarSdk.TransactionBuilder(account, {
        fee:              StellarSdk.BASE_FEE,
        networkPassphrase: this.networkPassphrase,
      }).addOperation(
        StellarSdk.Operation.payment({
          destination: anchorAccountId,
          asset,
          amount: params.amount,
        })
      ).setTimeout(30);

      // Attach memo if the anchor requires it
      if (anchorMemo && anchorMemoType) {
        switch (anchorMemoType.toLowerCase()) {
          case 'text': builder.addMemo(StellarSdk.Memo.text(anchorMemo)); break;
          case 'id':   builder.addMemo(StellarSdk.Memo.id(anchorMemo));   break;
          case 'hash': builder.addMemo(StellarSdk.Memo.hash(anchorMemo)); break;
        }
      }

      const tx = builder.build();
      const preparedTx = await rpc.prepareTransaction(tx);
      preparedTx.sign(adminKeypair);

      const sendResult = await rpc.sendTransaction(preparedTx);
      if (sendResult.status === 'ERROR') {
        throw new Error(`SEP-31 Stellar payment failed: ${JSON.stringify(sendResult.errorResult)}`);
      }
    }

    return anchorTxId;
  }
}

// ── Utility ───────────────────────────────────────────────────────────────────

function sleep(ms: number): Promise<void> {
  return new Promise<void>((resolve) => setTimeout(resolve, ms));
}
