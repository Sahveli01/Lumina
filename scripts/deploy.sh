#!/usr/bin/env bash
# scripts/deploy.sh — Lumina Soroban kontratlarını Stellar testnet/mainnet'e deploy eder.
#
# Kullanım: bash scripts/deploy.sh [testnet|mainnet]
# Gereksinimler: stellar-cli, rust wasm32 target, .env (ADMIN_SECRET_KEY dolu)

set -euo pipefail

NETWORK="${1:-testnet}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

# ── Renk kodları ──────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
# All diagnostic output to stderr so $(build_and_deploy) captures only contract IDs
ok()   { echo -e "${GREEN}  ✓ $*${NC}" >&2; }
info() { echo -e "  ► $*" >&2; }
fail() { echo -e "${RED}  ✗ $*${NC}" >&2; exit 1; }
warn() { echo -e "${YELLOW}  ! $*${NC}" >&2; }

echo ""
echo "═══════════════════════════════════════════════════════"
echo "  Lumina Deploy — Network: $NETWORK"
echo "═══════════════════════════════════════════════════════"
echo ""

# ═══════════════════════════════════════════════════════════
# BÖLÜM 1: Ön kontroller
# ═══════════════════════════════════════════════════════════

info "Ön kontroller yapılıyor..."

# 1a. stellar-cli kurulu mu?
if ! command -v stellar &>/dev/null; then
  fail "stellar-cli bulunamadı. Kurulum için:\n  cargo install --locked stellar-cli --features opt"
fi
STELLAR_VER=$(stellar --version 2>&1 | head -1)
ok "stellar-cli: $STELLAR_VER"

# 1b. wasm32 target kurulu mu? (stellar-cli v25: wasm32v1-none kullanır)
if ! rustup target list --installed 2>/dev/null | grep -q "wasm32v1-none"; then
  if rustup target list --installed 2>/dev/null | grep -q "wasm32-unknown-unknown"; then
    warn "wasm32v1-none eksik (stellar-cli v25 için gerekli). Kurulumda deneniyor..."
    rustup target add wasm32v1-none 2>/dev/null || fail "wasm32v1-none kurulamadı"
  else
    fail "wasm32v1-none target eksik. Kurmak için:\n  rustup target add wasm32v1-none"
  fi
fi
ok "wasm32v1-none target mevcut"

# 1c. .env dosyası var mı?
ENV_FILE="$ROOT_DIR/.env"
if [[ ! -f "$ENV_FILE" ]]; then
  fail ".env dosyası bulunamadı: $ENV_FILE\n  .env.example dosyasını kopyalayıp düzenleyin."
fi
ok ".env dosyası mevcut"

# .env dosyasını yükle
set -a
# shellcheck disable=SC1090
source "$ENV_FILE"
set +a

# 1d. ADMIN_SECRET_KEY dolu mu?
if [[ -z "${ADMIN_SECRET_KEY:-}" ]]; then
  fail "ADMIN_SECRET_KEY .env dosyasında boş.\n  Yeni key oluşturmak için:\n  stellar keys generate --global admin && stellar keys address admin"
fi
ok "ADMIN_SECRET_KEY mevcut"

echo ""

# ═══════════════════════════════════════════════════════════
# BÖLÜM 2: Network & hesap kurulumu
# ═══════════════════════════════════════════════════════════

info "Network kurulumu yapılıyor: $NETWORK"

if [[ "$NETWORK" == "testnet" ]]; then
  RPC_URL="${STELLAR_RPC_URL:-https://soroban-testnet.stellar.org}"
  PASSPHRASE="${STELLAR_NETWORK_PASSPHRASE:-Test SDF Network ; September 2015}"
  FRIENDBOT_URL="https://friendbot.stellar.org"
else
  RPC_URL="${STELLAR_RPC_URL:?STELLAR_RPC_URL mainnet için zorunludur}"
  PASSPHRASE="${STELLAR_NETWORK_PASSPHRASE:?STELLAR_NETWORK_PASSPHRASE mainnet için zorunludur}"
  FRIENDBOT_URL=""
fi

NET_FLAGS=(
  "--network-passphrase" "$PASSPHRASE"
  "--rpc-url" "$RPC_URL"
)

# stellar network add (zaten varsa skip — v25 testnet/mainnet builtin)
if ! stellar network ls 2>/dev/null | grep -q "^${NETWORK}$"; then
  stellar network add "$NETWORK" \
    --rpc-url "$RPC_URL" \
    --network-passphrase "$PASSPHRASE" 2>/dev/null \
    || warn "'$NETWORK' network kaydı eklenemedi, devam ediliyor."
fi
ok "Network '$NETWORK' yapılandırıldı"

# "admin" kimliği yoksa oluştur (v25: toml dosyası oluştur veya seed phrase ile)
STELLAR_CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/stellar"
if ! stellar keys address admin &>/dev/null; then
  # lumina-admin varsa kopyala, yoksa ADMIN_SECRET_KEY'den yeni key oluştur
  if [[ -f "$STELLAR_CONFIG_DIR/identity/lumina-admin.toml" ]]; then
    cp "$STELLAR_CONFIG_DIR/identity/lumina-admin.toml" \
       "$STELLAR_CONFIG_DIR/identity/admin.toml"
    ok "admin key (lumina-admin'den kopyalandı)"
  else
    # Secret key → toml dosyası olarak kaydet
    mkdir -p "$STELLAR_CONFIG_DIR/identity"
    echo "secret_key = \"$ADMIN_SECRET_KEY\"" \
      > "$STELLAR_CONFIG_DIR/identity/admin.toml"
    ok "admin key (ADMIN_SECRET_KEY'den oluşturuldu)"
  fi
else
  ok "admin key zaten mevcut"
fi

# Public key'i türet
ADMIN_ADDR=$(stellar keys address admin)
ok "Admin public key: $ADMIN_ADDR"

# Testnet friendbot fonlama — Horizon API ile hesap kontrolü (v25 compat)
if [[ "$NETWORK" == "testnet" ]]; then
  info "Testnet hesabı kontrol ediliyor..."
  HORIZON_URL="${STELLAR_HORIZON_URL:-https://horizon-testnet.stellar.org}"
  HTTP_CODE=$(curl -so /dev/null -w "%{http_code}" \
    "${HORIZON_URL}/accounts/${ADMIN_ADDR}" 2>/dev/null || echo "000")
  if [[ "$HTTP_CODE" != "200" ]]; then
    info "Hesap bulunamadı (HTTP ${HTTP_CODE}), friendbot ile fonlanıyor..."
    FBOT_RESP=$(curl -sf "${FRIENDBOT_URL}?addr=${ADMIN_ADDR}" 2>&1) \
      && ok "Friendbot başarılı: $ADMIN_ADDR" \
      || fail "Friendbot hatası: $FBOT_RESP"
    sleep 3
  else
    ok "Hesap zaten fonlu (HTTP 200)"
  fi
fi

echo ""

# ═══════════════════════════════════════════════════════════
# BÖLÜM 3: Build + Deploy döngüsü
# ═══════════════════════════════════════════════════════════

WASM_DIR="$ROOT_DIR/contracts/target/wasm32v1-none/release"
CONTRACTS_DIR="$ROOT_DIR/contracts"

# Her kontrat için build + deploy
build_and_deploy() {
  local name="$1"          # örn: lumina-core
  local wasm_name="${name//-/_}"  # örn: lumina_core
  local wasm_file="$WASM_DIR/${wasm_name}.wasm"

  echo "" >&2
  echo "  ┌─ $name ──────────────────────────────────" >&2

  # Build
  info "  Build: $name"
  stellar contract build \
    --manifest-path "$CONTRACTS_DIR/$name/Cargo.toml" \
    2>&1 | grep -E "(error|Compiling|Finished|✅|Build Complete)" >&2 || true

  if [[ ! -f "$wasm_file" ]]; then
    fail "  WASM dosyası bulunamadı: $wasm_file"
  fi
  ok "  Build başarılı → $(du -sh "$wasm_file" | cut -f1)"

  # Deploy — contract ID'yi grep ile çıkart (stellar v25 çok satırlı çıktı verir)
  info "  Deploy: $name"
  local deploy_out contract_id
  deploy_out=$(stellar contract deploy \
    --wasm "$wasm_file" \
    --source admin \
    --network "$NETWORK" \
    "${NET_FLAGS[@]}" \
    2>&1)
  contract_id=$(echo "$deploy_out" | grep -E "^C[A-Z2-7]{55}$" | tail -1)

  # Hata kontrolü: contract ID "C" ile başlar (56 karakter)
  if [[ ! "$contract_id" =~ ^C[A-Z2-7]{55}$ ]]; then
    fail "  Deploy hatası — geçersiz contract ID:\n$(echo "$deploy_out" | tail -5)"
  fi

  ok "  Contract ID: $contract_id"
  echo "  └──────────────────────────────────────────" >&2
  # Only contract ID to stdout — captured by caller
  echo "$contract_id"
}

# Sırayla deploy et (lumina-core en sonda — diğerlerine bağımlı)
info "Kontratlar build & deploy ediliyor..."

NULLIFIER_ID=$(build_and_deploy "nullifier-registry")
ORACLE_ID=$(build_and_deploy "risk-oracle")
POOL_ID=$(build_and_deploy "liquidity-pools")
CORE_ID=$(build_and_deploy "lumina-core")

echo ""

# ═══════════════════════════════════════════════════════════
# BÖLÜM 4: .env güncelleme
# ═══════════════════════════════════════════════════════════

info "contract ID'leri .env dosyasına yazılıyor..."

update_env() {
  local key="$1" val="$2"
  if grep -q "^${key}=" "$ENV_FILE"; then
    # Mevcut satırı güncelle (macOS sed uyumluluğu için geçici dosya)
    sed -i.bak "s|^${key}=.*|${key}=${val}|" "$ENV_FILE" && rm -f "${ENV_FILE}.bak"
  else
    echo "${key}=${val}" >> "$ENV_FILE"
  fi
}

update_env "LUMINA_CORE_CONTRACT_ID"        "$CORE_ID"
update_env "NULLIFIER_REGISTRY_CONTRACT_ID" "$NULLIFIER_ID"
update_env "RISK_ORACLE_CONTRACT_ID"        "$ORACLE_ID"
update_env "LIQUIDITY_POOLS_CONTRACT_ID"    "$POOL_ID"
ok ".env güncellendi"

echo ""

# ═══════════════════════════════════════════════════════════
# BÖLÜM 5: Initialize çağrıları
# ═══════════════════════════════════════════════════════════

info "Kontratlar initialize ediliyor..."

invoke() {
  stellar contract invoke \
    --id "$1" \
    --source admin \
    --network "$NETWORK" \
    "${NET_FLAGS[@]}" \
    -- "${@:2}" 2>&1
}

# risk-oracle → initialize --admin
info "  risk-oracle: initialize"
invoke "$ORACLE_ID" initialize \
  --admin "$ADMIN_ADDR"
ok "  risk-oracle initialize OK"

# nullifier-registry → initialize --admin --lumina_core
info "  nullifier-registry: initialize"
invoke "$NULLIFIER_ID" initialize \
  --admin "$ADMIN_ADDR" \
  --lumina_core "$CORE_ID"
ok "  nullifier-registry initialize OK"

# liquidity-pools → initialize --admin --lumina_core --stablecoin
# USDC testnet kontrat adresi (Stellar testnet canonical)
USDC_TESTNET="${USDC_CONTRACT_ID:-CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA}"
info "  liquidity-pools: initialize (USDC: $USDC_TESTNET)"
invoke "$POOL_ID" initialize \
  --admin "$ADMIN_ADDR" \
  --lumina_core "$CORE_ID" \
  --stablecoin "$USDC_TESTNET"
ok "  liquidity-pools initialize OK"

# lumina-core → initialize --admin --nullifier_registry --risk_oracle --liquidity_pools
info "  lumina-core: initialize"
invoke "$CORE_ID" initialize \
  --admin "$ADMIN_ADDR" \
  --nullifier_registry "$NULLIFIER_ID" \
  --risk_oracle "$ORACLE_ID" \
  --liquidity_pools "$POOL_ID"
ok "  lumina-core initialize OK"

echo ""

# ═══════════════════════════════════════════════════════════
# BÖLÜM 6: Doğrulama çağrıları
# ═══════════════════════════════════════════════════════════

info "Kontratlar doğrulanıyor..."

# nullifier-registry → get_registry_stats
echo -n "  nullifier-registry get_registry_stats: "
STATS=$(invoke "$NULLIFIER_ID" get_registry_stats 2>&1)
if echo "$STATS" | grep -q "total"; then
  ok "✅  $STATS"
else
  warn "Yanıt alındı (kontrol edin): $STATS"
fi

# liquidity-pools → get_insurance_reserve
echo -n "  liquidity-pools get_insurance_reserve: "
RESERVE=$(invoke "$POOL_ID" get_insurance_reserve 2>&1)
if echo "$RESERVE" | grep -qE "^[0-9]"; then
  ok "✅  reserve=$RESERVE"
else
  warn "Yanıt alındı (kontrol edin): $RESERVE"
fi

# risk-oracle → get_admin
echo -n "  risk-oracle get_admin: "
ORACLE_ADMIN=$(invoke "$ORACLE_ID" get_admin 2>&1)
if echo "$ORACLE_ADMIN" | grep -q "$ADMIN_ADDR"; then
  ok "✅  admin=$ORACLE_ADMIN"
else
  warn "Yanıt alındı (kontrol edin): $ORACLE_ADMIN"
fi

# lumina-core → get_admin
echo -n "  lumina-core get_admin: "
CORE_ADMIN=$(invoke "$CORE_ID" get_admin 2>&1)
if echo "$CORE_ADMIN" | grep -q "$ADMIN_ADDR"; then
  ok "✅  admin=$CORE_ADMIN"
else
  warn "Yanıt alındı (kontrol edin): $CORE_ADMIN"
fi

echo ""

# ═══════════════════════════════════════════════════════════
# BÖLÜM 7: Özet
# ═══════════════════════════════════════════════════════════

echo "═══════════════════════════════════════════════════════"
echo -e "${GREEN}  Deploy Tamamlandı!${NC}"
echo "═══════════════════════════════════════════════════════"
echo "  Network:             $NETWORK"
echo "  Admin:               $ADMIN_ADDR"
echo ""
echo "  lumina-core:         $CORE_ID"
echo "  nullifier-registry:  $NULLIFIER_ID"
echo "  risk-oracle:         $ORACLE_ID"
echo "  liquidity-pools:     $POOL_ID"
echo "═══════════════════════════════════════════════════════"
echo ""
echo "  Sonraki adım — backend'i başlat:"
echo "    npm run dev:backend"
echo ""
