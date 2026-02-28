/**
 * ZkProverService
 *
 * Orchestrates the RISC Zero proof generation by spawning the Rust host binary
 * synchronously. Automatically selects local or Bonsai mode based on whether
 * BONSAI_API_KEY is present in the environment.
 *
 * Local mode  — risc0_zkvm::default_prover() on this machine (slow, no key needed)
 * Bonsai mode — remote proving via Bonsai API (fast, requires BONSAI_API_KEY)
 */

import { spawnSync, SpawnSyncReturns } from 'child_process';
import * as path from 'path';

// ── Types ─────────────────────────────────────────────────────────────────────

/**
 * Invoice data passed to the ZK guest circuit.
 *
 * `invoice_hash` and `debtor_id` are `[u8; 32]` in Rust — serialised by
 * serde_json as plain arrays of integers (0-255). Use `hexToBytes32` to
 * convert hex strings from the API layer.
 *
 * `current_timestamp` is injected by this service at call time; callers do
 * not need to provide it.
 *
 * zkTLS fields (`data_source`, `tls_proof_hash`, `source_timestamp`) are
 * optional — the service defaults them to manual (0) values when omitted.
 */
export interface InvoiceProofInput {
  invoice_hash: number[];          // 32-byte array
  amount: number;                  // u64 (smallest currency unit)
  debtor_id: number[];             // 32-byte array
  due_date: number;                // u64 Unix timestamp
  payment_history_score: number;   // u8  0-100
  country_cds_spread: number;      // u16 basis points
  sector_risk: number;             // u8  0-100

  // ── zkTLS source attestation (optional; defaults to manual) ──────────────
  /** 0=manual (default), 1=xero, 2=quickbooks */
  data_source?: number;
  /** SHA-256 of the zkTLS session response body. 32-byte array; zeros if manual. */
  tls_proof_hash?: number[];
  /** Unix seconds when data was fetched from the source API. 0 if manual. */
  source_timestamp?: number;
}

/** Structured result returned after a successful proof. */
export interface ZkProofResult {
  nullifier: string;           // hex-encoded SHA-256(invoice_hash || debtor_id)
  invoice_hash: string;        // hex-encoded — same value as input, for binding
  risk_score: number;          // 0-100 composite risk score (bonus applied if verified)
  is_valid: boolean;           // true when all circuit constraints are satisfied
  is_verified_source: boolean; // true when data came from a verified accounting API
  data_source: number;         // 0=manual, 1=xero, 2=quickbooks
  receipt_bytes: string;       // hex-encoded bincode-serialised RISC Zero Receipt
  image_id: string;            // hex-encoded RISC Zero guest image ID
  mode: 'local' | 'bonsai';
}

/** Raw JSON printed to stdout by zk-prover/host/src/main.rs (ProofOutput). */
interface HostOutput {
  receipt_bytes: string;
  nullifier: string;
  invoice_hash: string;
  risk_score: number;
  is_valid: boolean;
  is_verified_source: boolean;
  data_source: number;
  image_id: string;
  mode: string;
}

// ── Service ───────────────────────────────────────────────────────────────────

export class ZkProverService {
  /**
   * Absolute path to `zk-prover/host/Cargo.toml`.
   * Resolved from `backend/src/services` → project root (3 levels up).
   */
  private readonly manifestPath: string;

  constructor() {
    const projectRoot = path.resolve(__dirname, '../../..');
    this.manifestPath = path.join(
      projectRoot,
      'zk-prover', 'host', 'Cargo.toml'
    );
  }

  /**
   * Synchronously generate a RISC Zero proof for the given invoice input.
   *
   * ⚠️  This call blocks the event loop for the duration of the proof:
   *   - Local mode:  up to ~5 minutes (depends on hardware)
   *   - Bonsai mode: up to ~2 minutes (remote, parallel proving)
   * In production, move this into a job queue or worker thread.
   *
   * @throws if cargo fails to start, exits non-zero, or produces invalid JSON.
   */
  prove(input: InvoiceProofInput): ZkProofResult {
    // Inject the current Unix timestamp — the circuit needs it for due_date
    // comparison; the zkVM has no system clock.
    // Fill in zkTLS defaults for manual entries.
    const fullInput = {
      ...input,
      current_timestamp: Math.floor(Date.now() / 1000),
      data_source:       input.data_source    ?? 0,
      tls_proof_hash:    input.tls_proof_hash ?? new Array(32).fill(0) as number[],
      source_timestamp:  input.source_timestamp ?? 0,
    };

    // Forward BONSAI_API_KEY only when set; its absence triggers local mode
    // in the Rust host (env::var("BONSAI_API_KEY").is_ok()).
    const childEnv: NodeJS.ProcessEnv = { ...process.env };
    if (!process.env['BONSAI_API_KEY']) {
      delete childEnv['BONSAI_API_KEY'];
    }

    const result: SpawnSyncReturns<string> = spawnSync(
      'cargo',
      [
        'run',
        '--manifest-path', this.manifestPath,
        '--',
        JSON.stringify(fullInput),
      ],
      {
        env: childEnv,
        encoding: 'utf8',
        timeout: 5 * 60 * 1_000,  // 5-minute hard ceiling
      }
    );

    if (result.error) {
      throw new Error(
        `ZK prover process failed to start: ${result.error.message}\n` +
          'Ensure the RISC Zero toolchain is installed: ' +
          'curl -L https://risczero.com/install | bash && rzup install'
      );
    }

    if (result.status !== 0) {
      const stderr = result.stderr?.trim() ?? '(no stderr)';
      throw new Error(
        `ZK prover exited with code ${result.status ?? 'signal'}:\n${stderr}`
      );
    }

    const stdout = result.stdout.trim();
    let raw: HostOutput;
    try {
      raw = JSON.parse(stdout) as HostOutput;
    } catch {
      throw new Error(
        `ZK prover produced non-JSON output: ${stdout.slice(0, 300)}`
      );
    }

    return {
      nullifier:           raw.nullifier,
      invoice_hash:        raw.invoice_hash,
      risk_score:          raw.risk_score,
      is_valid:            raw.is_valid,
      is_verified_source:  raw.is_verified_source ?? false,
      data_source:         raw.data_source ?? 0,
      receipt_bytes:       raw.receipt_bytes,
      image_id:            raw.image_id,
      mode:                raw.mode === 'bonsai' ? 'bonsai' : 'local',
    };
  }

  // ── Utilities ──────────────────────────────────────────────────────────────

  /**
   * Convert a 64-character hex string (32 bytes) to a `number[]` array
   * compatible with serde_json's default serialisation of `[u8; 32]`.
   */
  static hexToBytes32(hex: string): number[] {
    if (!/^[0-9a-fA-F]{64}$/.test(hex)) {
      throw new Error(
        `Expected a 32-byte hex string (64 hex chars), got: "${hex}"`
      );
    }
    return Array.from(Buffer.from(hex, 'hex'));
  }
}
