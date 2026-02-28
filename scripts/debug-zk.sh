#!/usr/bin/env bash
# Debug: start backend and curl the ZK endpoint to see full error
source /home/anan1234/.nvm/nvm.sh
export PATH="/home/anan1234/.cargo/bin:$PATH"
cd /mnt/c/Lumina

echo "=== cargo location in this shell ==="
which cargo && cargo --version

echo ""
echo "=== Starting backend ==="
node backend/dist/index.js &
BACKEND_PID=$!
trap "kill $BACKEND_PID 2>/dev/null" EXIT

echo "Waiting for backend..."
for i in $(seq 1 50); do
  HEALTH=$(curl -sf --max-time 2 http://localhost:4000/health 2>/dev/null) && break
  sleep 1; echo -n "."
done
echo ""
echo "Backend health: $HEALTH"

echo ""
echo "=== Curling /api/datasource/manual/prepare (no -f flag to see body) ==="
curl -s -X POST http://localhost:4000/api/datasource/manual/prepare \
  -H "Content-Type: application/json" \
  -d '{
    "invoice_hash": "aabbccddaabbccddaabbccddaabbccddaabbccddaabbccddaabbccddaabbccdd",
    "amount": 1000000,
    "debtor_id":  "11223344112233441122334411223344112233441122334411223344112233441",
    "due_date": 9999999999,
    "payment_history_score": 75,
    "country_cds_spread": 150,
    "sector_risk": 30
  }' 2>&1
echo ""
echo "=== Done ==="
