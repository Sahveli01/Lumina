# Lumina 2.0 — Programmable Trade Credit Layer

Lumina turns unpaid invoices into on-chain, privacy-preserving credit instruments on the Stellar blockchain.  An SME uploads an invoice, a RISC Zero zero-knowledge circuit validates it and produces a risk score, and a set of Soroban smart contracts convert it into a funded, trackable debt position — without ever leaking debtor identity or invoice details on-chain.

Three design pillars:

- **ZK Privacy** — The ZK guest circuit validates invoice authenticity, computes a nullifier (`SHA-256(invoice_hash ∥ debtor_id)`) and a risk score.  Only the public journal (nullifier, risk score, validity flag) touches the blockchain; raw debtor data stays off-chain.
- **Compliance** — `nullifier-registry` tracks every funded position as a state machine (Active → Funded → Repaid / Defaulted) and prevents double-spending the same invoice via nullifier uniqueness checks.
- **Risk Pricing** — `risk-oracle` stores ZK-verified scores; `lumina-core` applies a banded APR (5 %–25 %) and a time-to-maturity discount to compute advance amounts automatically.

---

## Project Structure

```
Lumina/
├── contracts/                   # Soroban (Rust) smart contracts
│   ├── lumina-core/             #   Factor logic, advance calculation
│   ├── nullifier-registry/      #   ZK double-spend prevention + state registry
│   ├── risk-oracle/             #   ZK proof verification + score storage
│   └── liquidity-pools/         #   Senior / junior tranche pools + insurance
├── zk-prover/                   # RISC Zero zkVM proof system
│   ├── guest/src/main.rs        #   Circuit: validates invoice, computes outputs
│   └── host/src/main.rs         #   Orchestrator: local prover or Bonsai API
├── backend/                     # Node.js + Express + TypeScript API (port 4000)
│   └── src/
│       ├── app.ts               #   Express app factory
│       ├── index.ts             #   HTTP server entry point
│       ├── routes/              #   invoice, pool, datasource, registry, payment, health
│       └── services/            #   zkProver, stellar (contract invocation)
├── frontend/                    # Next.js 15 App Router + Tailwind CSS
│   ├── app/
│   │   ├── page.tsx             #   Landing / hero
│   │   ├── dashboard/page.tsx   #   Invoice management
│   │   └── pools/page.tsx       #   Liquidity deposit / withdraw
│   ├── components/              #   Navbar, WalletProvider, InvoiceCard, …
│   └── lib/                     #   api.ts, freighter.ts, stellar.ts
├── scripts/
│   ├── deploy.sh                #   Testnet / mainnet contract deployment
│   ├── health-check.sh          #   Pre-flight checks (tools, env, ports, network)
│   └── e2e-test.sh              #   End-to-end integration test suite
└── .env.example                 # Required environment variables
```

---

## Quick Start

### 1. Prerequisites

| Tool | Minimum | Install |
|------|---------|---------|
| Node.js | 20 | https://nodejs.org |
| Rust + Cargo | stable | https://rustup.rs |
| wasm32 target | — | `rustup target add wasm32-unknown-unknown` |
| Stellar CLI | latest | `cargo install --locked stellar-cli --features opt` |
| RISC Zero (`rzup`) | latest | `curl -L https://risczero.com/install \| bash && rzup install` |
| jq | any | `apt install jq` / `brew install jq` |

### 2. Environment

```bash
cp .env.example .env
# Edit .env — fill in ADMIN_SECRET_KEY and contract IDs after deploy
```

### 3. Install dependencies

```bash
npm install
```

### 4. Pre-flight check

```bash
npm run health        # bash scripts/health-check.sh
```

### 5. Start development servers

```bash
npm run dev           # starts backend :4000 and frontend :3000 concurrently
```

### 6. Deploy contracts (testnet)

```bash
bash scripts/deploy.sh testnet
```

### 7. Run E2E tests

```bash
npm run e2e           # bash scripts/e2e-test.sh
```

---

## API Documentation

All endpoints are served at `http://localhost:4000`.

### Health

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Service status, version, deployed contract IDs |

### Invoice

| Method | Path | Body / Params | Description |
|--------|------|---------------|-------------|
| POST | `/api/invoice/submit` | `invoice_hash, amount, debtor_id, due_date, payment_history_score, country_cds_spread, sector_risk` | Generate ZK proof, register nullifier, store risk score, submit to chain |
| POST | `/api/invoice/factor/:invoiceHash` | `invoiceNumericId, recipientAddress, assetCode` | Factor invoice: compute advance, disburse payment |
| GET | `/api/invoice/:invoiceHash` | — | Fetch on-chain invoice state by hash |

**Submit response** (`201`):
```json
{
  "success": true,
  "data": {
    "nullifier_tx": "...",
    "risk_score_tx": "...",
    "invoice_tx": "...",
    "risk_score": 72,
    "nullifier": "abc123..."
  }
}
```

### Registry

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/registry/query/:nullifier` | Fetch public state entry (state, due_date, funded_at) |
| GET | `/api/registry/stats` | Aggregated counts: total, active, funded, repaid, defaulted |

### Pool

| Method | Path | Body | Description |
|--------|------|------|-------------|
| POST | `/api/pool/deposit` | `tranche ("senior"\|"junior"), amount, depositorAddress` | Deposit into tranche |
| GET | `/api/pool/balance/:tranche/:depositor` | — | Get depositor balance |

### Datasource

| Method | Path | Body | Description |
|--------|------|------|-------------|
| POST | `/api/datasource/manual/prepare` | Same fields as submit | Run ZK proof without blockchain write (preview / testing) |

---

## ZK Circuit

**Guest** (`zk-prover/guest/src/main.rs`) — runs inside the RISC Zero zkVM.

### Input (`InvoiceInput`)

| Field | Type | Description |
|-------|------|-------------|
| `invoice_hash` | `[u8; 32]` | SHA-256 of raw invoice document |
| `amount` | `u64` | Invoice face value (stroops) |
| `debtor_id` | `[u8; 32]` | Hash of debtor identity (never revealed) |
| `due_date` | `u64` | Unix timestamp |
| `payment_history_score` | `u8` | 0–100 |
| `country_cds_spread` | `u16` | Basis points |
| `sector_risk` | `u8` | 0–100 |
| `current_timestamp` | `u64` | Host-provided `now` |

### Public output (`InvoiceJournal`)

| Field | Type | Description |
|-------|------|-------------|
| `nullifier` | `[u8; 32]` | `SHA-256(invoice_hash ∥ debtor_id)` |
| `invoice_hash` | `[u8; 32]` | Unchanged from input |
| `risk_score` | `u8` | 0–100 composite score |
| `is_valid` | `bool` | `amount > 0 && due_date > current_timestamp` |

### Risk Score Formula

```
norm_cds  = min(country_cds_spread × 100 / 2000, 100)   // 2000 bps reference
risk_score = (payment_history_score × 50
            + sector_risk            × 30
            + norm_cds               × 20) / 100
```

All arithmetic is integer-only (no floating point in the zkVM guest).

---

## Lumina State Registry

`nullifier-registry` tracks every invoice as a finite state machine.

### State Transitions

```
  submit_invoice
       │
       ▼
  ┌─────────┐   factor_invoice   ┌────────┐
  │  Active │ ─────────────────► │ Funded │
  └─────────┘                    └────────┘
                                      │
                          ┌───────────┼───────────┐
                          ▼                       ▼
                       ┌──────┐            ┌──────────┐
                       │Repaid│            │Disputed  │
                       └──────┘            └──────────┘
                                                │
                                                ▼
                                          ┌─────────┐
                                          │Defaulted│
                                          └─────────┘
```

Allowed transitions (enforced on-chain):

| From | To |
|------|----|
| Active | Funded |
| Funded | Repaid, Disputed |
| Disputed | Defaulted, Repaid |

### Query endpoint

```
GET /api/registry/query/:nullifier

{
  "success": true,
  "data": {
    "nullifier": "...",
    "state": "Active",
    "due_date": 1740000000,
    "funded_at": 0
  }
}
```

---

## On-Chain Insurance Primitive

`liquidity-pools` provides a two-tranche insurance model:

| Tranche | Risk | Return |
|---------|------|--------|
| Senior | Lower — protected by junior first | Lower yield |
| Junior | Higher — absorbs first losses | Higher yield |

Key mechanisms:

- **Premium collection** — Each factored invoice contributes 1.5 % of its face value to the insurance reserve (`collect_premium`).
- **Default protection** — In a default event, `trigger_default_protection` draws from the junior reserve to make senior depositors whole.
- **Pool risk score** — An admin-managed 0–100 score; new deposits inherit a risk tier based on a weighted average.

---

## Advance Rate Table (lumina-core)

| Risk Score | APR (bps) | Advance |
|------------|-----------|---------|
| 0–20 | 500 (5 %) | Highest |
| 21–40 | 800 (8 %) | High |
| 41–60 | 1200 (12 %) | Medium |
| 61–80 | 1800 (18 %) | Low |
| 81–100 | 2500 (25 %) | Lowest |

```
advance_amount = amount − (amount × apr_bps × days_to_maturity) / (365 × 10 000)
days_to_maturity = (due_date − now) / 86 400
```

---

## Testnet Deploy

```bash
# 1. Build WASM artefacts
npm run build:contracts

# 2. Deploy all four contracts and write IDs to .env
bash scripts/deploy.sh testnet

# 3. Verify the deployment
npm run health
npm run e2e
```

The deploy script also funds the admin account from Friendbot if the balance is below 10 XLM.

---

## Developer Commands

| Command | Description |
|---------|-------------|
| `npm run dev` | Start backend + frontend concurrently |
| `npm run health` | Pre-flight health check |
| `npm run e2e` | End-to-end integration tests |
| `npm run build:contracts` | Compile Soroban contracts to WASM |
| `npm run check` | TypeScript + Cargo type-check (no emit) |
| `npm run check:backend` | `tsc --noEmit` for backend only |
| `npm run check:frontend` | `tsc --noEmit` for frontend only |
| `npm run check:contracts` | `cargo check --tests` for all contracts |
| `bash scripts/deploy.sh testnet` | Full testnet deploy |

---

## Security Notes

- **Debtor privacy** — `debtor_id` is an opaque 32-byte hash of `SHA-256(debtor_name ∥ company_secret)`.  It is passed to the zkVM as a private witness and never stored on-chain.  The nullifier leaks only the *commitment* to the pair (invoice, debtor), not the debtor's identity.
- **Double-spend prevention** — `nullifier-registry` enforces nullifier uniqueness.  Submitting the same invoice twice fails at the contract level.
- **Admin key** — `ADMIN_SECRET_KEY` authorises `update_state`, `trigger_default_protection`, and `update_pool_risk_score`.  Rotate via a Stellar multi-sig threshold signer in production.
- **ZK proof integrity** — `risk-oracle` stores the RISC Zero `GUEST_ID` (image ID) and rejects receipts generated by a different circuit version.
- **No floating point** — Risk and advance calculations use integer arithmetic only, ensuring deterministic results across all environments.
