#!/usr/bin/env bash
# =============================================================================
#  Lumina 2.0 — Pre-flight Health Check
#  Run before starting the backend or deploying contracts.
#  Usage: bash scripts/health-check.sh
# =============================================================================

set -euo pipefail

# ── Colors & helpers ─────────────────────────────────────────────────────────
RED='\033[0;31m';  GREEN='\033[0;32m';  YELLOW='\033[1;33m'
BLUE='\033[0;34m'; CYAN='\033[0;36m';  BOLD='\033[1m';  NC='\033[0m'

ok()   { echo -e "${GREEN}  ✅  $*${NC}"; }
warn() { echo -e "${YELLOW}  ⚠   $*${NC}"; WARNINGS=$((WARNINGS+1)); }
err()  { echo -e "${RED}  ❌  $*${NC}"; ERRORS=$((ERRORS+1)); }
info() { echo -e "${CYAN}  ℹ   $*${NC}"; }
head() { echo -e "\n${BOLD}${BLUE}── $* ──${NC}"; }

WARNINGS=0; ERRORS=0

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="${ROOT_DIR}/.env"

# ── Load .env ────────────────────────────────────────────────────────────────
if [ -f "$ENV_FILE" ]; then
  set -a; source "$ENV_FILE"; set +a
  info "Loaded ${ENV_FILE}"
else
  warn ".env not found — copy .env.example to .env and fill in values"
fi

# ═════════════════════════════════════════════════════════════════════════════
head "1. Tool Checks"
# ═════════════════════════════════════════════════════════════════════════════

# Node.js ≥ 18
if command -v node &>/dev/null; then
  NODE_VER=$(node --version | sed 's/v//')
  NODE_MAJOR=$(echo "$NODE_VER" | cut -d. -f1)
  if [ "$NODE_MAJOR" -ge 18 ]; then
    ok "Node.js v${NODE_VER}"
  else
    err "Node.js v${NODE_VER} — requires ≥ 18"
  fi
else
  err "Node.js not found — install from https://nodejs.org"
fi

# npm
if command -v npm &>/dev/null; then
  ok "npm $(npm --version)"
else
  err "npm not found"
fi

# Rust / cargo
if command -v cargo &>/dev/null; then
  ok "$(cargo --version)"
else
  err "cargo not found — install from https://rustup.rs"
fi

# wasm32-unknown-unknown target
if rustup target list --installed 2>/dev/null | grep -q "wasm32-unknown-unknown"; then
  ok "wasm32-unknown-unknown target installed"
else
  warn "wasm32-unknown-unknown target missing — run: rustup target add wasm32-unknown-unknown"
fi

# stellar CLI
if command -v stellar &>/dev/null; then
  STELLAR_VER=$(stellar --version 2>/dev/null | head -1)
  ok "stellar CLI: ${STELLAR_VER}"
else
  warn "stellar CLI not found — install: cargo install --locked stellar-cli --features opt"
fi

# jq (needed for e2e tests)
if command -v jq &>/dev/null; then
  ok "jq $(jq --version)"
else
  warn "jq not installed — required for e2e tests: apt install jq / brew install jq"
fi

# openssl (for e2e test data generation)
if command -v openssl &>/dev/null; then
  ok "openssl $(openssl version | awk '{print $2}')"
else
  warn "openssl not found — needed for e2e test data generation"
fi

# ═════════════════════════════════════════════════════════════════════════════
head "2. Environment Variables"
# ═════════════════════════════════════════════════════════════════════════════

check_required() {
  local VAR="$1"; local VAL="${!VAR:-}"
  if [ -z "$VAL" ]; then
    err "${VAR} is not set"
  else
    # Mask secrets
    if [[ "$VAR" == *"SECRET"* ]] || [[ "$VAR" == *"KEY"* ]] || [[ "$VAR" == *"TOKEN"* ]]; then
      info "${VAR} = ${VAL:0:4}…${VAL: -4}"
    else
      info "${VAR} = ${VAL}"
    fi
    ok "${VAR}"
  fi
}

# ADMIN_SECRET_KEY — must start with S and be 56 chars
ADMIN_SK="${ADMIN_SECRET_KEY:-}"
if [ -z "$ADMIN_SK" ]; then
  err "ADMIN_SECRET_KEY is not set"
elif [[ ! "$ADMIN_SK" =~ ^S[A-Z2-7]{55}$ ]]; then
  err "ADMIN_SECRET_KEY format invalid (expected S + 55 base32 chars, got ${#ADMIN_SK} chars)"
else
  ok "ADMIN_SECRET_KEY format valid (S… ${ADMIN_SK: -4})"
fi

# Contract IDs — accept "C + 55 base32 chars" or "deploy pending"
check_contract_id() {
  local VAR="$1"; local VAL="${!VAR:-}"
  if [ -z "$VAL" ]; then
    warn "${VAR} not set (acceptable before deploy)"
  elif [[ "$VAL" =~ ^C[A-Z2-7]{55}$ ]]; then
    ok "${VAR} = ${VAL:0:8}…"
  elif [ "$VAL" = "deploy pending" ] || [ "$VAL" = "DEPLOY_PENDING" ]; then
    warn "${VAR} = deploy pending"
  else
    err "${VAR} format invalid (expected C + 55 base32 chars, got: ${VAL:0:20}…)"
  fi
}

check_contract_id "LUMINA_CORE_CONTRACT_ID"
check_contract_id "NULLIFIER_REGISTRY_CONTRACT_ID"
check_contract_id "RISK_ORACLE_CONTRACT_ID"
check_contract_id "LIQUIDITY_POOLS_CONTRACT_ID"

# Other env vars
STELLAR_RPC="${STELLAR_RPC_URL:-}"
if [ -z "$STELLAR_RPC" ]; then
  warn "STELLAR_RPC_URL not set — will use default testnet RPC"
else
  ok "STELLAR_RPC_URL = ${STELLAR_RPC}"
fi

NETWORK_PP="${STELLAR_NETWORK_PASSPHRASE:-}"
if [ -z "$NETWORK_PP" ]; then
  warn "STELLAR_NETWORK_PASSPHRASE not set — defaults to testnet"
else
  ok "STELLAR_NETWORK_PASSPHRASE set (${#NETWORK_PP} chars)"
fi

# ═════════════════════════════════════════════════════════════════════════════
head "3. Port Availability"
# ═════════════════════════════════════════════════════════════════════════════

check_port() {
  local PORT="$1"; local SERVICE="$2"
  # nc -z exits 0 if port is open (something is listening)
  if nc -z 127.0.0.1 "$PORT" 2>/dev/null; then
    warn "Port :${PORT} is already in use — ${SERVICE} may fail to start"
  else
    ok ":${PORT} is available for ${SERVICE}"
  fi
}

check_port 4000 "Backend"
check_port 3000 "Frontend"

# ═════════════════════════════════════════════════════════════════════════════
head "4. Network Connectivity"
# ═════════════════════════════════════════════════════════════════════════════

check_url() {
  local NAME="$1"; local URL="$2"; local TIMEOUT="${3:-5}"
  HTTP=$(curl -sf -o /dev/null -w "%{http_code}" --max-time "$TIMEOUT" "$URL" 2>/dev/null || echo "000")
  if [ "$HTTP" -ge 200 ] && [ "$HTTP" -lt 500 ] 2>/dev/null; then
    ok "${NAME} reachable (HTTP ${HTTP})"
  else
    warn "${NAME} not reachable (HTTP ${HTTP}) — may be offline or blocked"
  fi
}

check_url "Stellar testnet RPC"     "https://soroban-testnet.stellar.org/"
check_url "Horizon testnet"          "https://horizon-testnet.stellar.org/"
check_url "Friendbot"               "https://friendbot.stellar.org/" 3

# Optional: check Bonsai if API key is set
if [ -n "${BONSAI_API_KEY:-}" ]; then
  ok "BONSAI_API_KEY is set (Bonsai ZK proving enabled)"
else
  info "BONSAI_API_KEY not set — ZK proving will use local mode"
fi

# ═════════════════════════════════════════════════════════════════════════════
head "5. Project Structure"
# ═════════════════════════════════════════════════════════════════════════════

check_dir()  { [ -d "${ROOT_DIR}/$1" ] && ok "$1/" || warn "$1/ not found"; }
check_file() { [ -f "${ROOT_DIR}/$1" ] && ok "$1"  || warn "$1 not found"; }

check_dir  "contracts"
check_dir  "zk-prover"
check_dir  "backend/src"
check_dir  "frontend/app"
check_dir  "scripts"
check_file "backend/src/index.ts"
check_file "contracts/Cargo.toml"
check_file "zk-prover/Cargo.toml"

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}═══════════════════════════════════════${NC}"
echo -e "${BOLD}  Health Check Summary${NC}"
echo -e "${BOLD}═══════════════════════════════════════${NC}"
echo -e "  Errors   : ${RED}${ERRORS}${NC}"
echo -e "  Warnings : ${YELLOW}${WARNINGS}${NC}"
echo ""

if [ "$ERRORS" -gt 0 ]; then
  echo -e "${RED}${BOLD}  ❌ ${ERRORS} error(s) must be fixed before proceeding.${NC}"
  echo ""
  echo -e "${CYAN}  Recommended next steps:${NC}"
  echo "    1. Fix the errors listed above"
  echo "    2. Re-run: bash scripts/health-check.sh"
  echo "    3. Deploy:  bash scripts/deploy.sh testnet"
  exit 1
elif [ "$WARNINGS" -gt 0 ]; then
  echo -e "${YELLOW}${BOLD}  ⚠ ${WARNINGS} warning(s) — review before deploying to mainnet.${NC}"
  echo ""
  echo -e "${CYAN}  Suggested commands:${NC}"
  echo "    npm run dev:backend   # Start backend"
  echo "    npm run dev:frontend  # Start frontend"
  echo "    npm run e2e           # Run integration tests"
  exit 0
else
  echo -e "${GREEN}${BOLD}  ✅ All checks passed!${NC}"
  echo ""
  echo -e "${CYAN}  Ready to go:${NC}"
  echo "    npm run dev           # Start both servers"
  echo "    bash scripts/deploy.sh testnet   # Deploy contracts"
  echo "    npm run e2e           # Run E2E tests"
  exit 0
fi
