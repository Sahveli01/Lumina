#!/usr/bin/env bash
source /home/anan1234/.nvm/nvm.sh
export PATH="/home/anan1234/.cargo/bin:$PATH"

echo "=== Cargo ==="
which cargo && cargo --version || echo "NO CARGO"

echo ""
echo "=== RISC0 binary ==="
ls /mnt/c/Lumina/zk-prover/host/target/release/lumina-host 2>/dev/null && echo "EXISTS" || echo "NOT BUILT"

echo ""
echo "=== Node ==="
which node && node --version

echo ""
echo "=== RISC0_DEV_MODE check (from .env) ==="
grep RISC0 /mnt/c/Lumina/.env || echo "NOT SET"

echo ""
echo "=== Quick ZK prover test ==="
cd /mnt/c/Lumina
RISC0_DEV_MODE=1 RISC0_PROVER=local cargo run --manifest-path zk-prover/host/Cargo.toml -- '{"invoice_hash":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],"amount":1000000,"debtor_id":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],"due_date":9999999999,"payment_history_score":75,"country_cds_spread":150,"sector_risk":30,"current_timestamp":1700000000,"data_source":0,"tls_proof_hash":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"source_timestamp":0}' 2>/dev/null | head -5 || echo "ZK PROVER FAILED"
