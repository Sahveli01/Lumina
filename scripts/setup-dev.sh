#!/usr/bin/env bash
# scripts/setup-dev.sh — One-time development environment setup

set -euo pipefail

echo "═══════════════════════════════════════════════════════"
echo "  Lumina Dev Setup"
echo "═══════════════════════════════════════════════════════"

# ── Rust & wasm32 target ──────────────────────────────────────────────────────
if ! command -v cargo &> /dev/null; then
  echo "► Installing Rust..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source "$HOME/.cargo/env"
fi

echo "► Adding wasm32 target..."
rustup target add wasm32-unknown-unknown

# ── Stellar CLI ───────────────────────────────────────────────────────────────
if ! command -v stellar &> /dev/null; then
  echo "► Installing Stellar CLI..."
  cargo install --locked stellar-cli --features opt
fi

echo "Stellar CLI: $(stellar --version)"

# ── RISC Zero (rzup) ─────────────────────────────────────────────────────────
if ! command -v rzup &> /dev/null; then
  echo "► Installing rzup (RISC Zero toolchain manager)..."
  curl -L https://risczero.com/install | bash
  source "$HOME/.bashrc" 2>/dev/null || source "$HOME/.profile" 2>/dev/null || true
fi

if command -v rzup &> /dev/null; then
  echo "► Installing RISC Zero toolchain..."
  rzup install
fi

# ── Node.js deps ──────────────────────────────────────────────────────────────
echo "► Installing Node.js dependencies..."
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$ROOT_DIR"
npm install

# ── .env setup ───────────────────────────────────────────────────────────────
if [[ ! -f "$ROOT_DIR/.env" ]]; then
  cp "$ROOT_DIR/.env.example" "$ROOT_DIR/.env"
  echo "► Created .env from .env.example — fill in your keys!"
fi

echo ""
echo "═══════════════════════════════════════════════════════"
echo "  Setup complete! Next steps:"
echo "  1. Fill in .env with your keys"
echo "  2. npm run dev:frontend  (terminal 1)"
echo "  3. npm run dev:backend   (terminal 2)"
echo "  4. bash scripts/deploy.sh testnet"
echo "═══════════════════════════════════════════════════════"
