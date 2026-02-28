#!/usr/bin/env bash
# =============================================================================
#  Lumina 2.0 — End-to-End Integration Test Suite
#  Usage: bash scripts/e2e-test.sh [--base-url http://localhost:4000]
# =============================================================================

set -euo pipefail

# ── Colors ───────────────────────────────────────────────────────────────────
RED='\033[0;31m';  GREEN='\033[0;32m';  YELLOW='\033[1;33m'
BLUE='\033[0;34m'; CYAN='\033[0;36m';  BOLD='\033[1m';  NC='\033[0m'

pass() { echo -e "${GREEN}  ✅ PASS${NC}  $*"; PASSED=$((PASSED+1)); }
fail() { echo -e "${RED}  ❌ FAIL${NC}  $*"; FAILED=$((FAILED+1)); }
info() { echo -e "${CYAN}  ℹ${NC}  $*"; }
head() { echo -e "\n${BOLD}${BLUE}══ $* ══${NC}"; }

PASSED=0; FAILED=0
BASE_URL="${1:-http://localhost:4000}"
START_TS=$(date +%s)

# ── Load .env ────────────────────────────────────────────────────────────────
ENV_FILE="$(dirname "$0")/../.env"
if [ -f "$ENV_FILE" ]; then
  set -a; source "$ENV_FILE"; set +a
fi

# ── Prerequisites ────────────────────────────────────────────────────────────
head "Prerequisites"

# jq
if ! command -v jq &>/dev/null; then
  echo -e "${RED}ERROR: jq is required. Install: apt install jq / brew install jq${NC}"
  exit 1
fi
info "jq $(jq --version)"

# Backend reachable — retry up to 30s (Stellar SDK startup takes ~10-15s)
HEALTH=""
for i in $(seq 1 30); do
  HEALTH=$(curl -sf --max-time 2 "${BASE_URL}/health" 2>/dev/null) && break
  sleep 1
done
if [ -z "$HEALTH" ]; then
  echo -e "${RED}ERROR: Backend not reachable at ${BASE_URL} after 30s. Run: npm run dev:backend${NC}"
  exit 1
fi
HEALTH_STATUS=$(echo "$HEALTH" | jq -r '.status // "unknown"')
if [ "$HEALTH_STATUS" = "ok" ]; then
  info "Backend health: OK (${BASE_URL})"
else
  echo -e "${RED}ERROR: Backend health check failed: ${HEALTH}${NC}"
  exit 1
fi

# ADMIN_PUBLIC_KEY
ADMIN_PUBLIC="${ADMIN_PUBLIC_KEY:-}"
if [ -z "$ADMIN_PUBLIC" ]; then
  # Try to derive from stellar CLI
  ADMIN_PUBLIC=$(stellar keys address admin 2>/dev/null || true)
fi
if [ -z "$ADMIN_PUBLIC" ] || [[ ! "$ADMIN_PUBLIC" =~ ^G[A-Z2-7]{55}$ ]]; then
  echo -e "${YELLOW}WARNING: ADMIN_PUBLIC_KEY not set or invalid. Pool balance test will be skipped.${NC}"
  ADMIN_PUBLIC=""
fi
[ -n "$ADMIN_PUBLIC" ] && info "Admin public key: ${ADMIN_PUBLIC:0:10}…"

# ── Generate test data ────────────────────────────────────────────────────────
INVOICE_HASH=$(openssl rand -hex 32)
DEBTOR_ID=$(openssl rand -hex 32)

# due_date = now + 30 days (Linux & macOS compatible)
if date -d "+30 days" &>/dev/null 2>&1; then
  DUE_DATE=$(date -d "+30 days" +%s)
else
  DUE_DATE=$(date -v+30d +%s)
fi

info "invoice_hash : ${INVOICE_HASH:0:16}…"
info "debtor_id    : ${DEBTOR_ID:0:16}…"
info "due_date     : $(date -d @"$DUE_DATE" 2>/dev/null || date -r "$DUE_DATE") ($DUE_DATE)"

NULLIFIER=""       # filled by Test 1
NUMERIC_ID=1       # filled by Test 2

# ─────────────────────────────────────────────────────────────────────────────
head "Test 1 — Manual Invoice Prepare (ZK Proof)"
# ─────────────────────────────────────────────────────────────────────────────

RESP=$(curl -sf -X POST "${BASE_URL}/api/datasource/manual/prepare" \
  -H "Content-Type: application/json" \
  -d "{
    \"invoice_hash\":          \"${INVOICE_HASH}\",
    \"amount\":                1000000000,
    \"debtor_id\":             \"${DEBTOR_ID}\",
    \"due_date\":              ${DUE_DATE},
    \"payment_history_score\": 75,
    \"country_cds_spread\":    150,
    \"sector_risk\":           30
  }" 2>&1) || { fail "HTTP request failed"; RESP="{}"; }

T1_SUCCESS=$(echo "$RESP" | jq -r '.success // "false"')
T1_VALID=$(echo "$RESP" | jq -r '.data.is_valid | if . == null then "false" else tostring end')
T1_RISK=$(echo "$RESP" | jq -r '.data.risk_score // "null"')
T1_VERIFIED=$(echo "$RESP" | jq -r '.data.is_verified_source | if . == null then "null" else tostring end')
NULLIFIER=$(echo "$RESP" | jq -r '.data.nullifier // ""')

if [ "$T1_SUCCESS" = "true" ] && [ "$T1_VALID" = "true" ] && [ -n "$T1_RISK" ] && [ "$T1_RISK" != "null" ]; then
  pass "is_valid=true, risk_score=${T1_RISK}, is_verified_source=${T1_VERIFIED}"
else
  fail "Unexpected response: success=${T1_SUCCESS}, is_valid=${T1_VALID}, risk_score=${T1_RISK}"
  info "Full response: $RESP"
fi

if [ "$T1_VERIFIED" != "false" ]; then
  fail "is_verified_source should be false for manual source (got: ${T1_VERIFIED})"
else
  pass "is_verified_source correctly false for manual entry"
fi

# ─────────────────────────────────────────────────────────────────────────────
head "Test 2 — Submit Invoice to Blockchain"
# ─────────────────────────────────────────────────────────────────────────────

RESP=$(curl -sf -o /tmp/lumina_t2.json -w "%{http_code}" -X POST "${BASE_URL}/api/invoice/submit" \
  -H "Content-Type: application/json" \
  -d "{
    \"invoice_hash\":          \"${INVOICE_HASH}\",
    \"amount\":                1000000000,
    \"debtor_id\":             \"${DEBTOR_ID}\",
    \"due_date\":              ${DUE_DATE},
    \"payment_history_score\": 75,
    \"country_cds_spread\":    150,
    \"sector_risk\":           30
  }" 2>&1) || RESP="500"

HTTP_CODE="$RESP"
BODY=$(cat /tmp/lumina_t2.json 2>/dev/null || echo "{}")

T2_SUCCESS=$(echo "$BODY" | jq -r '.success // "false"')
T2_NULL_TX=$(echo "$BODY" | jq -r '.data.nullifier_tx // ""')
T2_RISK_TX=$(echo "$BODY" | jq -r '.data.risk_score_tx // ""')
T2_INV_TX=$(echo "$BODY" | jq -r '.data.invoice_tx // ""')

if [ "$HTTP_CODE" = "201" ] && [ "$T2_SUCCESS" = "true" ] \
    && [ -n "$T2_NULL_TX" ] && [ -n "$T2_RISK_TX" ] && [ -n "$T2_INV_TX" ]; then
  pass "HTTP 201, 3 tx hashes returned"
  info "nullifier_tx : ${T2_NULL_TX:0:20}…"
  info "risk_score_tx: ${T2_RISK_TX:0:20}…"
  info "invoice_tx   : ${T2_INV_TX:0:20}…"
else
  fail "HTTP ${HTTP_CODE}, success=${T2_SUCCESS}, missing tx hashes"
  info "Full response: $BODY"
fi

# Get numeric invoice ID for factor step
INV_RESP=$(curl -sf "${BASE_URL}/api/invoice/${INVOICE_HASH}" 2>/dev/null || echo "{}")
NUMERIC_ID=$(echo "$INV_RESP" | jq -r '.data.id // "1"')
info "On-chain invoice numeric ID: ${NUMERIC_ID}"

# Update nullifier from submit response if test 1 nullifier is empty
if [ -z "$NULLIFIER" ]; then
  NULLIFIER=$(echo "$BODY" | jq -r '.data.nullifier // ""')
fi

# ─────────────────────────────────────────────────────────────────────────────
head "Test 3 — Registry Query (expect state: Active)"
# ─────────────────────────────────────────────────────────────────────────────

if [ -z "$NULLIFIER" ]; then
  fail "Nullifier not available — skipping registry query"
else
  RESP=$(curl -sf "${BASE_URL}/api/registry/query/${NULLIFIER}" 2>/dev/null || echo "{}")
  T3_STATE=$(echo "$RESP" | jq -r '.data.state // "null"')

  if [ "$T3_STATE" = "Active" ]; then
    pass "Registry state = Active"
  else
    fail "Expected state=Active, got: ${T3_STATE}"
    info "Full response: $RESP"
  fi
fi

# ─────────────────────────────────────────────────────────────────────────────
head "Test 4 — Factor Invoice (advance_amount > 0 + disbursement)"
# ─────────────────────────────────────────────────────────────────────────────

if [ -z "$ADMIN_PUBLIC" ]; then
  echo -e "${YELLOW}  ⏭ SKIP  ADMIN_PUBLIC_KEY not configured — skipping factor test${NC}"
else
  RESP=$(curl -sf -X POST "${BASE_URL}/api/invoice/factor/${INVOICE_HASH}" \
    -H "Content-Type: application/json" \
    -d "{
      \"invoiceNumericId\": ${NUMERIC_ID},
      \"recipientAddress\": \"${ADMIN_PUBLIC}\",
      \"assetCode\": \"XLM\"
    }" 2>/dev/null || echo "{}")

  T4_ADVANCE=$(echo "$RESP" | jq -r '.data.factoring.advance_amount // "0"')
  T4_APR=$(echo "$RESP" | jq -r '.data.factoring.apr_bps // "null"')
  T4_TX=$(echo "$RESP" | jq -r '.data.factoring.tx_hash // ""')
  T4_DISB=$(echo "$RESP" | jq -r '.data.disbursement.success // "false"')

  if [ -n "$T4_TX" ] && [ "$T4_ADVANCE" != "0" ] && [ "$T4_ADVANCE" != "null" ]; then
    pass "advance_amount=${T4_ADVANCE}, apr_bps=${T4_APR}, disbursement=${T4_DISB}"
    info "factor_tx: ${T4_TX:0:20}…"
  else
    fail "advance_amount=${T4_ADVANCE}, apr_bps=${T4_APR}, tx=${T4_TX:-empty}"
    info "Full response: $RESP"
  fi
fi

# ─────────────────────────────────────────────────────────────────────────────
head "Test 5 — Registry State Updated to Funded"
# ─────────────────────────────────────────────────────────────────────────────

if [ -z "$NULLIFIER" ] || [ -z "$ADMIN_PUBLIC" ]; then
  echo -e "${YELLOW}  ⏭ SKIP  Missing nullifier or admin key${NC}"
else
  RESP=$(curl -sf "${BASE_URL}/api/registry/query/${NULLIFIER}" 2>/dev/null || echo "{}")
  T5_STATE=$(echo "$RESP" | jq -r '.data.state // "null"')

  if [ "$T5_STATE" = "Funded" ]; then
    pass "Registry state = Funded"
  else
    fail "Expected state=Funded, got: ${T5_STATE}"
    info "Full response: $RESP"
  fi
fi

# ─────────────────────────────────────────────────────────────────────────────
head "Test 6 — Pool Balance"
# ─────────────────────────────────────────────────────────────────────────────

if [ -z "$ADMIN_PUBLIC" ]; then
  echo -e "${YELLOW}  ⏭ SKIP  ADMIN_PUBLIC_KEY not configured${NC}"
else
  RESP=$(curl -sf "${BASE_URL}/api/pool/balance/senior/${ADMIN_PUBLIC}" 2>/dev/null || echo "{}")
  T6_SUCCESS=$(echo "$RESP" | jq -r '.success // "false"')
  T6_BAL=$(echo "$RESP" | jq -r '.data.balance // "null"')

  if [ "$T6_SUCCESS" = "true" ]; then
    pass "Pool balance endpoint responded: balance=${T6_BAL}"
  else
    fail "Pool balance query failed: $RESP"
  fi
fi

# ─────────────────────────────────────────────────────────────────────────────
head "Test 7 — Registry Stats"
# ─────────────────────────────────────────────────────────────────────────────

RESP=$(curl -sf "${BASE_URL}/api/registry/stats" 2>/dev/null || echo "{}")
T7_SUCCESS=$(echo "$RESP" | jq -r '.success // "false"')
T7_TOTAL=$(echo "$RESP" | jq -r '.data.total // "0"')
T7_FUNDED=$(echo "$RESP" | jq -r '.data.funded // "0"')

if [ "$T7_SUCCESS" = "true" ]; then
  pass "Stats: total=${T7_TOTAL}, funded=${T7_FUNDED}, active=$(echo "$RESP" | jq -r '.data.active // "0"'), repaid=$(echo "$RESP" | jq -r '.data.repaid // "0"')"
  if [ "$T7_TOTAL" -ge 1 ] 2>/dev/null; then
    pass "total >= 1"
  else
    fail "Expected total >= 1, got: ${T7_TOTAL}"
  fi
else
  fail "Stats endpoint failed: $RESP"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
END_TS=$(date +%s)
ELAPSED=$((END_TS - START_TS))
TOTAL=$((PASSED + FAILED))

echo ""
echo -e "${BOLD}═══════════════════════════════════════${NC}"
echo -e "${BOLD}  E2E Test Summary${NC}"
echo -e "${BOLD}═══════════════════════════════════════${NC}"
echo -e "  Elapsed : ${ELAPSED}s"
echo -e "  Passed  : ${GREEN}${PASSED}${NC}"
echo -e "  Failed  : ${RED}${FAILED}${NC}"
echo -e "  Total   : ${TOTAL}"
echo ""

if [ "$FAILED" -eq 0 ]; then
  echo -e "${GREEN}${BOLD}  All tests passed! ✅${NC}"
  exit 0
else
  echo -e "${RED}${BOLD}  ${FAILED}/${TOTAL} tests failed. ❌${NC}"
  exit 1
fi
