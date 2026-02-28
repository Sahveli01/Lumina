/**
 * PaymentService — Lumina disbursement & repayment orchestration
 *
 * Bridges the on-chain `advance_amount` from lumina-core's factor_invoice
 * to a real Stellar payment (or SEP-31 cross-border transfer via an anchor).
 *
 * Supported flows:
 *
 *   XLM    — native Stellar payment directly to the SME
 *   USDC   — Circle USDC (GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN)
 *   Anchor — SEP-31 cross-border transfer for local-currency payouts
 */

import * as StellarSdk from '@stellar/stellar-sdk';
import { StellarService }               from './stellar.service';
import { AnchorService, stroopsToAmount } from './anchor.service';
import { requireContractId }             from '../stellar/contractIds';

// ── Constants ─────────────────────────────────────────────────────────────────

/** Circle USDC issuer on Stellar mainnet / testnet (task-specified address). */
const USDC_ISSUER = 'GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN';

/** Horizon base URL — used for transaction lookup during repayment verification. */
const HORIZON_URL =
  process.env['STELLAR_NETWORK_PASSPHRASE'] === StellarSdk.Networks.TESTNET ||
  !process.env['STELLAR_NETWORK_PASSPHRASE']
    ? 'https://horizon-testnet.stellar.org'
    : 'https://horizon.stellar.org';

// ── Return types ──────────────────────────────────────────────────────────────

export interface DisbursementResult {
  success:                boolean;
  stellar_tx_hash?:       string;
  anchor_transaction_id?: string;
  amount_sent:            string;   // human-readable, e.g. "1234.5000000"
  asset:                  string;   // "XLM", "USDC", or "USDC+anchor"
  estimated_arrival?:     string;   // ISO timestamp estimate for anchor flows
}

export interface RepaymentResult {
  success:               boolean;
  invoice_id:            number;
  stellar_tx_hash:       string;
  repay_tx_hash?:        string;  // lumina-core repay() on-chain tx
  registry_tx_hash?:     string;  // nullifier-registry update_state tx
  verified_on_horizon:   boolean;
  message?:              string;
}

// ── PaymentService ────────────────────────────────────────────────────────────

export class PaymentService {
  private readonly stellar: StellarService;
  private readonly anchor:  AnchorService;

  constructor() {
    this.stellar = new StellarService();
    this.anchor  = new AnchorService();
  }

  // ── disburseFunds ──────────────────────────────────────────────────────────

  /**
   * Disburse the advance amount to the SME after a successful factor_invoice.
   *
   * @param invoiceId        on-chain u64 invoice ID (for logging)
   * @param advanceAmount    amount in stroops (1 XLM / USDC = 10,000,000)
   * @param recipientAddress SME's Stellar account address
   * @param assetCode        "XLM" | "USDC"
   * @param anchorDomain     optional anchor domain for local-currency payout
   */
  async disburseFunds(params: {
    invoiceId:        number;
    advanceAmount:    number;
    recipientAddress: string;
    assetCode:        string;
    anchorDomain?:    string;
  }): Promise<DisbursementResult> {
    const { invoiceId, advanceAmount, recipientAddress, assetCode, anchorDomain } = params;

    const amountStr = stroopsToAmount(advanceAmount);
    const adminKeypair = StellarService.adminKeypair();

    // ── Flow 3: Local-currency anchor payout (SEP-31) ──────────────────────
    if (anchorDomain) {
      const jwt = await this.anchor.getSep10Token(anchorDomain, adminKeypair);

      const assetIssuer = assetCode === 'XLM' ? '' : USDC_ISSUER;

      const anchorTxId = await this.anchor.sendSep31Payment({
        anchorDomain,
        jwt,
        amount:          amountStr,
        assetCode:       assetCode === 'XLM' ? 'XLM' : assetCode,
        assetIssuer,
        receiverAccount: recipientAddress,
        fields:          { transaction: {} },
      });

      console.info(
        `[PaymentService] invoice #${invoiceId} — SEP-31 anchor tx=${anchorTxId} domain=${anchorDomain}`
      );

      // Estimated arrival: 1 business day for cross-border
      const eta = new Date(Date.now() + 24 * 60 * 60 * 1000).toISOString();

      return {
        success:               true,
        anchor_transaction_id: anchorTxId,
        amount_sent:           amountStr,
        asset:                 `${assetCode}+${anchorDomain}`,
        estimated_arrival:     eta,
      };
    }

    // ── Build the Stellar payment operation ───────────────────────────────
    let asset: StellarSdk.Asset;

    if (assetCode === 'XLM') {
      // Flow 1: Native XLM payment
      asset = StellarSdk.Asset.native();
    } else {
      // Flow 2: USDC payment via Circle issuer
      asset = new StellarSdk.Asset(assetCode, USDC_ISSUER);
    }

    const paymentOp = StellarSdk.Operation.payment({
      destination: recipientAddress,
      asset,
      amount: amountStr,
    });

    const { txHash } = await this.stellar.buildAndSubmitTx(paymentOp, adminKeypair);

    console.info(
      `[PaymentService] invoice #${invoiceId} — Stellar payment tx=${txHash} ` +
      `amount=${amountStr} ${assetCode} → ${recipientAddress}`
    );

    return {
      success:         true,
      stellar_tx_hash: txHash,
      amount_sent:     amountStr,
      asset:           assetCode,
    };
  }

  // ── collectRepayment ───────────────────────────────────────────────────────

  /**
   * Verify that a Stellar repayment transaction has settled on Horizon,
   * then call lumina-core repay() and update the nullifier-registry.
   *
   * @param invoiceId    on-chain u64 invoice ID
   * @param payerAddress Stellar address of the debtor
   * @param amount       expected repayment amount in stroops
   * @param assetCode    "XLM" | "USDC"
   * @param stellarTxHash Horizon transaction hash to verify
   * @param nullifier    hex-encoded nullifier for registry update
   */
  async collectRepayment(params: {
    invoiceId:       number;
    payerAddress:    string;
    amount:          number;
    assetCode:       string;
    stellarTxHash:   string;
    nullifier:       string;
  }): Promise<RepaymentResult> {
    const { invoiceId, stellarTxHash, nullifier } = params;

    // ── Step 1: Verify the Stellar transaction on Horizon ─────────────────
    const horizonVerified = await this.verifyHorizonTx(stellarTxHash);
    if (!horizonVerified) {
      return {
        success:             false,
        invoice_id:          invoiceId,
        stellar_tx_hash:     stellarTxHash,
        verified_on_horizon: false,
        message:             `Transaction ${stellarTxHash} not found or not yet settled on Horizon.`,
      };
    }

    const adminKeypair = StellarService.adminKeypair();

    // ── Step 2: lumina-core.repay(invoice_id) ─────────────────────────────
    const luminaCore = requireContractId('luminaCore');
    const { txHash: repayTxHash, returnValue } = await this.stellar.invokeContract(
      luminaCore,
      'repay',
      [StellarService.u64(invoiceId)],
      adminKeypair,
    );

    const repayOk = StellarSdk.scValToNative(returnValue) as boolean;
    if (!repayOk) {
      return {
        success:             false,
        invoice_id:          invoiceId,
        stellar_tx_hash:     stellarTxHash,
        repay_tx_hash:       repayTxHash,
        verified_on_horizon: true,
        message:             `lumina-core repay() returned false — invoice #${invoiceId} may not be in Funded state.`,
      };
    }

    // ── Step 3: nullifier-registry.update_state(nullifier, Repaid) ────────
    const nullifierRegistry = requireContractId('nullifierRegistry');
    const { txHash: registryTxHash } = await this.stellar.invokeContract(
      nullifierRegistry,
      'update_state',
      [
        StellarService.bytes(nullifier),
        StellarService.symbol('Repaid'),
      ],
      adminKeypair,
    );

    console.info(
      `[PaymentService] repayment verified — invoice #${invoiceId} ` +
      `repay_tx=${repayTxHash} registry_tx=${registryTxHash}`
    );

    return {
      success:             true,
      invoice_id:          invoiceId,
      stellar_tx_hash:     stellarTxHash,
      repay_tx_hash:       repayTxHash,
      registry_tx_hash:    registryTxHash,
      verified_on_horizon: true,
    };
  }

  // ── Private helpers ────────────────────────────────────────────────────────

  /**
   * Check Horizon for a submitted transaction by hash.
   * Returns true if the transaction is found with status "success".
   */
  private async verifyHorizonTx(txHash: string): Promise<boolean> {
    try {
      const url  = `${HORIZON_URL}/transactions/${encodeURIComponent(txHash)}`;
      const resp = await fetch(url);
      if (!resp.ok) return false;

      const body = await resp.json() as { successful?: boolean };
      return body.successful === true;
    } catch {
      return false;
    }
  }
}
