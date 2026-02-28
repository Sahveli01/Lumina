#!/usr/bin/env bash
# Helper: build backend + ZK prover, start backend, run E2E tests
set -euo pipefail

NVM_DIR="/home/anan1234/.nvm"
CARGO_BIN="/home/anan1234/.cargo/bin"

source "$NVM_DIR/nvm.sh"
export PATH="$CARGO_BIN:$PATH"

echo "=== Environment check ==="
echo "node: $(which node) $(node --version)"
echo "cargo: $(which cargo) $(cargo --version)"
echo ""

cd /mnt/c/Lumina

# ── 1. Build backend ────────────────────────────────────────────────────────
echo "=== Building backend (tsc) ==="
cd backend && npx tsc 2>&1 | tail -3 && echo "tsc OK"
cd ..

# ── 2. Pre-build ZK prover (caches binary so runtime calls are fast) ────────
echo ""
echo "=== Pre-building ZK prover (RISC0_DEV_MODE=1) ==="
RISC0_DEV_MODE=1 RISC0_PROVER=local \
  cargo build --manifest-path zk-prover/host/Cargo.toml 2>&1 | tail -3
echo "ZK prover build OK"

# ── 3. Start backend ────────────────────────────────────────────────────────
echo ""
echo "=== Starting backend ==="
# Pass explicit PATH so cargo is findable from the node subprocess
CARGO_PATH="$CARGO_BIN" node backend/dist/index.js &
BACKEND_PID=$!
echo "Backend PID: $BACKEND_PID"

cleanup() { kill "$BACKEND_PID" 2>/dev/null || true; }
trap cleanup EXIT

echo "Waiting for backend health (up to 50s — includes one-time whitelist tx)..."
for i in $(seq 1 50); do
  HEALTH=$(curl -sf --max-time 2 http://localhost:4000/health 2>/dev/null) && break
  sleep 1
  echo -n "."
done
echo ""

if [ -z "${HEALTH:-}" ]; then
  echo "ERROR: Backend did not become healthy in 50s"
  exit 1
fi
echo "Backend healthy!"

# ── 4. Run E2E tests ────────────────────────────────────────────────────────
echo ""
echo "=== Running E2E tests ==="
bash scripts/e2e-test.sh http://localhost:4000
