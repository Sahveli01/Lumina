#!/usr/bin/env bash
# scripts/test-contracts.sh — Run all Soroban contract unit tests

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

echo "► Running Soroban contract tests..."
cd "$ROOT_DIR/contracts"
cargo test -- --nocapture 2>&1
echo ""
echo "✓ All contract tests passed"
