//! Lumina ZK Guest Program — RISC Zero Invoice Validation Circuit
//!
//! Runs inside the RISC Zero zkVM (no_std + std feature).
//! Validates private invoice data, computes a risk score, and commits
//! public outputs to the journal.
//!
//! Private inputs (NOT revealed on-chain):
//!   invoice_hash            — SHA-256 of the raw invoice document
//!   debtor_id               — opaque debtor identifier bytes
//!   payment_history_score   — 0-100, higher = better credit history
//!   country_cds_spread      — sovereign CDS spread in basis points
//!   sector_risk             — 0-100, higher = riskier sector
//!   current_timestamp       — Unix seconds at proof generation time
//!   data_source             — 0=manual, 1=xero, 2=quickbooks
//!   tls_proof_hash          — SHA-256 of the zkTLS session (zeros if manual)
//!   source_timestamp        — Unix seconds when data was fetched from source
//!
//! Public outputs (journal — visible on-chain):
//!   nullifier          — SHA-256(invoice_hash || debtor_id)
//!   invoice_hash       — bound to this specific invoice
//!   risk_score         — 0-100 composite risk score (with verified-source bonus)
//!   is_valid           — all constraints satisfied
//!   is_verified_source — true when data came from a verified API source
//!   data_source        — 0=manual, 1=xero, 2=quickbooks

#![no_main]

use risc0_zkvm::guest::env;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

risc0_zkvm::guest::entry!(main);

// ── Input / Output types ──────────────────────────────────────────────────────

/// All data supplied by the host prover.
/// `current_timestamp` is required because the zkVM has no system clock.
#[derive(Debug, Deserialize)]
pub struct InvoiceInput {
    /// SHA-256 of the raw invoice document (binds proof to exact invoice).
    pub invoice_hash: [u8; 32],
    /// Face value in smallest currency unit (e.g. USD cents).
    pub amount: u64,
    /// Opaque debtor identifier (e.g. SHA-256 of debtor name + company secret).
    pub debtor_id: [u8; 32],
    /// Unix timestamp of invoice due date.
    pub due_date: u64,
    /// Payment history score: 0 (worst) – 100 (best). Weight: 50%.
    pub payment_history_score: u8,
    /// Sovereign CDS spread in basis points (e.g. 150 = 1.5%). Weight: 20%.
    pub country_cds_spread: u16,
    /// Sector risk score: 0 (safest) – 100 (riskiest). Weight: 30%.
    pub sector_risk: u8,
    /// Current Unix timestamp — supplied by prover, not clock-read.
    pub current_timestamp: u64,

    // ── zkTLS source attestation ─────────────────────────────────────────────

    /// Data source identifier:
    ///   0 = manual entry (unverified)
    ///   1 = Xero accounting API
    ///   2 = QuickBooks API
    pub data_source: u8,
    /// SHA-256 of the zkTLS session response (ties proof to exact TLS exchange).
    /// All-zeros for manual entries.
    pub tls_proof_hash: [u8; 32],
    /// Unix timestamp when the invoice data was fetched from the source API.
    /// 0 for manual entries.
    pub source_timestamp: u64,
}

/// Public outputs committed to the RISC Zero journal.
#[derive(Debug, Serialize)]
pub struct InvoiceJournal {
    /// SHA-256(invoice_hash || debtor_id) — unique per (invoice, debtor).
    pub nullifier: [u8; 32],
    /// Binds this proof to the specific invoice document.
    pub invoice_hash: [u8; 32],
    /// Composite risk score 0-100 (lower = safer).
    /// Includes a 10% reduction bonus if is_verified_source == true.
    pub risk_score: u8,
    /// True when all validity constraints are satisfied.
    pub is_valid: bool,
    /// True when data_source ≠ 0 AND source was fetched within the last hour.
    /// Verified-source invoices receive a 10% risk score reduction.
    pub is_verified_source: bool,
    /// Mirrors the input data_source field (public for on-chain audit).
    pub data_source: u8,
}

// ── Guest main ─────────────────────────────────────────────────────────────────

fn main() {
    // Read all private inputs from the prover via stdin.
    let input: InvoiceInput = env::read();

    // ── Nullifier ──────────────────────────────────────────────────────────────
    // nullifier = SHA-256(invoice_hash || debtor_id)
    // Unique per (invoice, debtor) pair — prevents double factoring.
    let nullifier: [u8; 32] = {
        let mut h = Sha256::new();
        h.update(input.invoice_hash);
        h.update(input.debtor_id);
        h.finalize().into()
    };

    // ── Risk Score ────────────────────────────────────────────────────────────
    //
    // risk_score = payment_history_score * 0.5
    //            + sector_risk           * 0.3
    //            + normalize(cds)        * 0.2
    //
    // All arithmetic is integer-only (no f32/f64 in zkVM).
    // Intermediate values are u32 to prevent overflow.
    //
    // CDS normalisation: clamp to [0, 2000 bps] range → [0, 100].
    // 2000 bps (20%) is used as the "maximum distress" reference.
    let normalized_cds: u32 =
        ((input.country_cds_spread as u32).saturating_mul(100) / 2000).min(100);

    // Weighted sum (×100 to keep precision before final division).
    let risk_score_raw: u32 = (input.payment_history_score as u32) * 50   // 0.50 weight
        + (input.sector_risk as u32) * 30                                  // 0.30 weight
        + normalized_cds * 20;                                             // 0.20 weight

    // Divide by 100 to get a value in [0, 100].
    let base_score: u32 = (risk_score_raw / 100).min(100);

    // ── zkTLS Source Verification ─────────────────────────────────────────────
    //
    // A source is considered "verified" when:
    //   1. data_source is not manual (0)
    //   2. source_timestamp is non-zero (data was actually fetched)
    //   3. The fetch happened within the last hour (staleness guard)
    //
    // is_verified_source is a public journal output — the on-chain contract
    // (or a future policy layer) can enforce stricter terms for unverified data.
    let age_secs = input.current_timestamp.saturating_sub(input.source_timestamp);
    let is_verified_source: bool = input.data_source != 0
        && input.source_timestamp > 0
        && age_secs < 3600;

    // ── Verified-Source Risk Bonus ────────────────────────────────────────────
    // Invoices from verified accounting APIs carry less information risk.
    // Reward them with a 10% reduction in computed risk score.
    let risk_score: u8 = if is_verified_source {
        (base_score * 90 / 100) as u8
    } else {
        base_score as u8
    };

    // ── Validity Constraints ──────────────────────────────────────────────────

    // 1. Amount must be strictly positive.
    let amount_valid = input.amount > 0;

    // 2. Invoice must not have already expired at proof time.
    let not_expired = input.due_date > input.current_timestamp;

    let is_valid = amount_valid && not_expired;

    // ── Commit Journal (public outputs) ───────────────────────────────────────
    env::commit(&InvoiceJournal {
        nullifier,
        invoice_hash: input.invoice_hash,
        risk_score,
        is_valid,
        is_verified_source,
        data_source: input.data_source,
    });
}
