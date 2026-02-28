//! Lumina ZK Host — Proof Orchestrator
//!
//! Drives RISC Zero proof generation for invoice validation.
//! Selects mode at runtime:
//!
//!   Local  (default)        — `risc0_zkvm::default_prover()` on this machine.
//!   Bonsai (BONSAI_API_KEY) — remote proving via Bonsai cloud service.
//!
//! Usage:
//!   cargo run                         # demo data, local mode
//!   cargo run -- '<json>'             # custom InvoiceInput JSON, local mode
//!   BONSAI_API_KEY=<key> cargo run    # demo data, Bonsai mode
//!
//! Prints a JSON ProofOutput to stdout.

use anyhow::{Context, Result};
use risc0_zkvm::{default_prover, ExecutorEnv, Receipt};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{info, warn};

// ── Include generated ELF + image ID ─────────────────────────────────────────
// `methods.rs` is emitted by risc0_build::embed_methods() in build.rs.
// Exposes: GUEST_ELF: &[u8], GUEST_ID: [u32; 8]
include!(concat!(env!("OUT_DIR"), "/methods.rs"));

// ── Types ─────────────────────────────────────────────────────────────────────

/// Must mirror `InvoiceInput` in guest/src/main.rs exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvoiceInput {
    pub invoice_hash: [u8; 32],
    pub amount: u64,
    pub debtor_id: [u8; 32],
    pub due_date: u64,
    pub payment_history_score: u8,
    pub country_cds_spread: u16,
    pub sector_risk: u8,
    pub current_timestamp: u64,
    // zkTLS source attestation
    pub data_source: u8,
    pub tls_proof_hash: [u8; 32],
    pub source_timestamp: u64,
}

/// Must mirror `InvoiceJournal` in guest/src/main.rs exactly.
#[derive(Debug, Serialize, Deserialize)]
pub struct InvoiceJournal {
    pub nullifier: [u8; 32],
    pub invoice_hash: [u8; 32],
    pub risk_score: u8,
    pub is_valid: bool,
    pub is_verified_source: bool,
    pub data_source: u8,
}

/// JSON output written to stdout after a successful proof.
#[derive(Debug, Serialize)]
pub struct ProofOutput {
    /// Hex-encoded bincode-serialized Receipt.
    pub receipt_bytes: String,
    /// Hex-encoded nullifier (SHA-256(invoice_hash || debtor_id)).
    pub nullifier: String,
    /// Hex-encoded invoice_hash (binds proof to document).
    pub invoice_hash: String,
    /// Composite risk score 0-100 (with verified-source bonus applied).
    pub risk_score: u8,
    /// Whether all invoice constraints were satisfied.
    pub is_valid: bool,
    /// True when data came from a verified accounting API (Xero/QuickBooks).
    pub is_verified_source: bool,
    /// 0=manual, 1=xero, 2=quickbooks.
    pub data_source: u8,
    /// Hex-encoded RISC Zero image ID for on-chain verification.
    pub image_id: String,
    /// "local" or "bonsai".
    pub mode: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Encode the guest image ID ([u32; 8]) as a hex string.
fn image_id_hex() -> String {
    hex::encode(
        LUMINA_GUEST_ID
            .iter()
            .flat_map(|w: &u32| w.to_le_bytes())
            .collect::<Vec<u8>>(),
    )
}

// ── Local mode ────────────────────────────────────────────────────────────────

fn prove_locally(input: &InvoiceInput) -> Result<(Receipt, InvoiceJournal)> {
    info!("Local mode: building executor environment");

    let exec_env = ExecutorEnv::builder()
        .write(input)
        .context("Failed to write inputs to ExecutorEnv")?
        .build()
        .context("Failed to build ExecutorEnv")?;

    let prover = default_prover();

    info!("Local mode: generating proof (this may take a while)");
    let receipt = prover
        .prove(exec_env, LUMINA_GUEST_ELF)
        .context("Proof generation failed")?
        .receipt;

    // Verify the proof locally — catches any corruption before publishing.
    receipt
        .verify(LUMINA_GUEST_ID)
        .context("Local receipt verification failed")?;

    let journal: InvoiceJournal = receipt
        .journal
        .decode()
        .context("Failed to decode journal")?;

    info!(
        "Local mode: proof OK — is_valid={}, risk_score={}, is_verified_source={}, nullifier={}",
        journal.is_valid,
        journal.risk_score,
        journal.is_verified_source,
        hex::encode(journal.nullifier),
    );

    Ok((receipt, journal))
}

// ── Bonsai mode ───────────────────────────────────────────────────────────────

/// Maximum number of status-poll attempts before giving up.
const BONSAI_MAX_POLLS: u32 = 10;
/// Seconds between each Bonsai status poll.
const BONSAI_POLL_INTERVAL_SECS: u64 = 5;

async fn prove_via_bonsai(input: &InvoiceInput) -> Result<(Vec<u8>, InvoiceJournal)> {
    use bonsai_sdk::non_blocking::Client;

    // Build client from BONSAI_API_KEY + BONSAI_API_URL env vars.
    let client = Client::from_env(risc0_zkvm::VERSION)
        .context("Failed to create Bonsai client (check BONSAI_API_KEY)")?;

    let image_id = image_id_hex();

    // Upload guest ELF (idempotent — Bonsai deduplicates by image_id).
    info!("Bonsai: uploading guest ELF (image_id={})", &image_id[..16]);
    client
        .upload_img(&image_id, LUMINA_GUEST_ELF.to_vec())
        .await
        .context("Failed to upload guest ELF to Bonsai")?;

    // Serialize input using risc0's word-oriented serde, then convert to bytes.
    let input_words =
        risc0_zkvm::serde::to_vec(input).context("Failed to serialize InvoiceInput")?;
    let input_bytes: Vec<u8> = input_words
        .iter()
        .flat_map(|w: &u32| w.to_le_bytes())
        .collect();

    info!("Bonsai: uploading input ({} bytes)", input_bytes.len());
    let input_id = client
        .upload_input(input_bytes)
        .await
        .context("Failed to upload input to Bonsai")?;

    // Create the remote proving session.
    let session = client
        .create_session(image_id, input_id, vec![], false)
        .await
        .context("Failed to create Bonsai session")?;

    info!("Bonsai: session created — uuid={}", session.uuid);

    // Poll until SUCCEEDED or terminal failure (max BONSAI_MAX_POLLS attempts).
    for attempt in 1..=BONSAI_MAX_POLLS {
        tokio::time::sleep(Duration::from_secs(BONSAI_POLL_INTERVAL_SECS)).await;

        let status = session
            .status(&client)
            .await
            .context("Failed to poll Bonsai session status")?;

        info!(
            "Bonsai: attempt {}/{} — status={}",
            attempt, BONSAI_MAX_POLLS, status.status
        );

        match status.status.as_str() {
            "SUCCEEDED" => {
                let receipt_url = status
                    .receipt_url
                    .context("Bonsai reported SUCCEEDED but no receipt_url")?;

                info!("Bonsai: downloading receipt from {}", receipt_url);
                let receipt_bytes: Vec<u8> = reqwest::get(&receipt_url)
                    .await
                    .context("Failed to GET receipt URL")?
                    .bytes()
                    .await
                    .context("Failed to read receipt bytes")?
                    .to_vec();

                let receipt: Receipt = bincode::deserialize(&receipt_bytes)
                    .context("Failed to deserialize Bonsai receipt")?;

                // Verify the downloaded receipt before trusting it.
                receipt
                    .verify(LUMINA_GUEST_ID)
                    .context("Bonsai receipt verification failed")?;

                let journal: InvoiceJournal = receipt
                    .journal
                    .decode()
                    .context("Failed to decode journal from Bonsai receipt")?;

                info!(
                    "Bonsai: proof OK — is_valid={}, risk_score={}, is_verified_source={}",
                    journal.is_valid, journal.risk_score, journal.is_verified_source,
                );

                return Ok((receipt_bytes, journal));
            }

            "FAILED" | "TIMED_OUT" => {
                anyhow::bail!(
                    "Bonsai session {} reached terminal state: {}",
                    session.uuid,
                    status.status
                );
            }

            // RUNNING, PENDING, etc. — keep polling.
            _ => continue,
        }
    }

    anyhow::bail!(
        "Bonsai session {} did not complete after {} polls ({} s each)",
        session.uuid,
        BONSAI_MAX_POLLS,
        BONSAI_POLL_INTERVAL_SECS,
    )
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::fmt()
        .with_writer(std::io::stderr)
        .init();

    // Input: first CLI argument as JSON, or hardcoded demo data.
    let args: Vec<String> = std::env::args().collect();

    let input: InvoiceInput = if args.len() > 1 {
        serde_json::from_str(&args[1]).context("Failed to parse InvoiceInput JSON from arg[1]")?
    } else {
        warn!("No CLI argument provided — using hardcoded test invoice data");
        InvoiceInput {
            // Simulated SHA-256 of an invoice PDF.
            invoice_hash: [0xab; 32],
            // $100,000.00 expressed in USD cents.
            amount: 10_000_000,
            // Simulated opaque debtor identifier.
            debtor_id: [0xcd; 32],
            // Far-future due date (year ~2286 in Unix seconds).
            due_date: 9_999_999_999,
            // Solid payment history.
            payment_history_score: 80,
            // 1.5% sovereign CDS spread.
            country_cds_spread: 150,
            // Moderate sector risk.
            sector_risk: 40,
            // Current wall-clock time supplied by the host.
            current_timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("System time before Unix epoch")?
                .as_secs(),
            // Demo: manual entry (no zkTLS attestation).
            data_source: 0,
            tls_proof_hash: [0u8; 32],
            source_timestamp: 0,
        }
    };

    let use_bonsai = std::env::var("BONSAI_API_KEY").is_ok();

    let output: ProofOutput = if use_bonsai {
        info!("Mode: Bonsai");
        let (receipt_bytes, journal) = prove_via_bonsai(&input).await?;
        ProofOutput {
            receipt_bytes:      hex::encode(&receipt_bytes),
            nullifier:          hex::encode(journal.nullifier),
            invoice_hash:       hex::encode(journal.invoice_hash),
            risk_score:         journal.risk_score,
            is_valid:           journal.is_valid,
            is_verified_source: journal.is_verified_source,
            data_source:        journal.data_source,
            image_id:           image_id_hex(),
            mode:               "bonsai".to_string(),
        }
    } else {
        info!("Mode: local");
        let (receipt, journal) = prove_locally(&input)?;
        let receipt_bytes = bincode::serialize(&receipt).context("Failed to serialize receipt")?;
        ProofOutput {
            receipt_bytes:      hex::encode(&receipt_bytes),
            nullifier:          hex::encode(journal.nullifier),
            invoice_hash:       hex::encode(journal.invoice_hash),
            risk_score:         journal.risk_score,
            is_valid:           journal.is_valid,
            is_verified_source: journal.is_verified_source,
            data_source:        journal.data_source,
            image_id:           image_id_hex(),
            mode:               "local".to_string(),
        }
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
