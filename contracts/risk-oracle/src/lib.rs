#![no_std]

//! # Risk Oracle (Admin-Only)
//!
//! Invoice hash'lerine risk skoru atar ve bu skorların geçerliliğini
//! (expiry) yönetir. Yalnızca admin skor yazabilir.
//!
//! ## Skor Formatı
//! `score: u32` — 0 (en az riskli) ile 100 (en riskli) arasında bir tam sayı.
//! `lumina-core::factor_invoice` bu skoru bantlı APR hesabında kullanır:
//!
//! ```
//! score  0–20  → APR  500 bps (5%)
//! score 21–40  → APR  800 bps (8%)
//! score 41–60  → APR 1200 bps (12%)
//! score 61–80  → APR 1800 bps (18%)
//! score 81–100 → APR 2500 bps (25%)
//! ```
//!
//! ## Expiry
//! Her skor bir `expiry` (Unix timestamp) ile birlikte saklanır.
//! `get_risk_score` çağrısında, skor süresi dolmuşsa `None` döner.

use soroban_sdk::{
    contract, contractimpl, contracttype, contracterror,
    Address, BytesN, Env, Symbol,
};

// ── TTL sabitleri ─────────────────────────────────────────────────────────────

const TTL_THRESHOLD: u32 = 259_200;   // ~15 gün
const TTL_EXTEND: u32    = 3_110_400; // ~180 gün

// ── Hata kodları ──────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ContractError {
    AlreadyInitialized = 1,
    NotInitialized     = 2,
    Unauthorized       = 3,
    InvalidScore       = 4,
    InvalidExpiry      = 5,
    ScoreNotFound      = 6,
    ScoreExpired       = 7,
}

// ── Veri tipleri ──────────────────────────────────────────────────────────────

/// Bir invoice için saklanan risk bilgisi
#[contracttype]
#[derive(Clone, Debug)]
pub struct RiskEntry {
    /// 0–100 arası risk skoru
    pub score: u32,
    /// Bu skorun geçerlilik sona erme zamanı (Unix timestamp)
    pub expiry: u64,
    /// Bu skoru ilişkilendiren invoice hash (audit için)
    pub invoice_hash: BytesN<32>,
}

/// Storage anahtarları
#[contracttype]
pub enum DataKey {
    /// Admin adresi
    Admin,
    /// Risk skoru: RiskScore(BytesN<32>) → RiskEntry
    RiskScore(BytesN<32>),
    /// Toplam kaydedilen skor sayısı
    TotalScores,
}

// ── Kontrat ───────────────────────────────────────────────────────────────────

#[contract]
pub struct RiskOracle;

#[contractimpl]
impl RiskOracle {
    // ── Yönetim ──────────────────────────────────────────────────────────────

    /// Kontratı başlatır. Yalnızca bir kez çağrılabilir.
    pub fn initialize(env: Env, admin: Address) -> Result<(), ContractError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(ContractError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::TotalScores, &0_u64);
        env.events().publish(
            (Symbol::new(&env, "initialized"),),
            admin,
        );
        Ok(())
    }

    // ── Temel fonksiyonlar ────────────────────────────────────────────────────

    /// Invoice hash için risk skoru atar.
    ///
    /// # Parametreler
    /// - `invoice_hash` : SHA-256(invoice belgesi)
    /// - `score`        : 0–100 arası risk skoru
    /// - `expiry`       : bu skorun geçerlilik bitiş zamanı (Unix timestamp)
    ///
    /// # Yetkilendirme
    /// Yalnızca admin yazabilir.
    pub fn set_risk_score(
        env: Env,
        invoice_hash: BytesN<32>,
        score: u32,
        expiry: u64,
    ) -> Result<(), ContractError> {
        Self::require_admin(&env)?;

        if score > 100 {
            return Err(ContractError::InvalidScore);
        }
        if expiry <= env.ledger().timestamp() {
            return Err(ContractError::InvalidExpiry);
        }

        let entry = RiskEntry {
            score,
            expiry,
            invoice_hash: invoice_hash.clone(),
        };

        let is_new = !env
            .storage()
            .persistent()
            .has(&DataKey::RiskScore(invoice_hash.clone()));

        env.storage()
            .persistent()
            .set(&DataKey::RiskScore(invoice_hash.clone()), &entry);

        env.storage().persistent().extend_ttl(
            &DataKey::RiskScore(invoice_hash.clone()),
            TTL_THRESHOLD,
            TTL_EXTEND,
        );

        if is_new {
            let count: u64 = env
                .storage()
                .instance()
                .get(&DataKey::TotalScores)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::TotalScores, &(count + 1));
        }

        env.events().publish(
            (Symbol::new(&env, "score_set"), invoice_hash),
            (score, expiry),
        );

        Ok(())
    }

    /// Invoice hash için geçerli risk skorunu döndürür.
    ///
    /// # Döndürür
    /// - `Some(score)` : skor mevcut ve süresi dolmamış
    /// - `None`        : skor hiç set edilmemiş veya süresi dolmuş
    pub fn get_risk_score(env: Env, invoice_hash: BytesN<32>) -> Option<u32> {
        let entry: RiskEntry = env
            .storage()
            .persistent()
            .get(&DataKey::RiskScore(invoice_hash))?;

        if entry.expiry <= env.ledger().timestamp() {
            return None;
        }

        Some(entry.score)
    }

    /// Invoice hash için tam `RiskEntry`'yi döndürür (expiry dahil).
    /// Skor yoksa `None` döner; expiry kontrolü yapılmaz.
    pub fn get_risk_entry(env: Env, invoice_hash: BytesN<32>) -> Option<RiskEntry> {
        env.storage()
            .persistent()
            .get(&DataKey::RiskScore(invoice_hash))
    }

    /// Toplam kaydedilen skor sayısını döndürür.
    pub fn total_scores(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::TotalScores)
            .unwrap_or(0)
    }

    pub fn get_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    // ── İç yardımcılar ───────────────────────────────────────────────────────

    fn require_admin(env: &Env) -> Result<(), ContractError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(ContractError::NotInitialized)?;
        admin.require_auth();
        Ok(())
    }
}

// ── Unit testler ──────────────────────────────────────────────────────────────
//
// NOT: Soroban client generator, `Result<T, E>` döndüren metodlar için
// test modunda `T`'yi direkt döndürür (`()`, vb.).
// Hata testi için `try_method()` kullanılmalıdır.
//
// Örnek:
//   client.initialize(...)          → ()   (panic on error)
//   client.set_risk_score(...)      → ()   (panic on error)
//   client.try_set_risk_score(...)  → Result<(), Result<ContractError, Error>>
//   client.get_risk_entry(...)      → Option<RiskEntry>

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger, LedgerInfo};
    use soroban_sdk::{Env, Address};

    // ── Yardımcı kurulum ──────────────────────────────────────────────────────

    struct Fixture {
        env:         Env,
        contract_id: Address,
        admin:       Address,
    }

    impl Fixture {
        fn new() -> Self {
            let env = Env::default();
            env.mock_all_auths();

            let contract_id = env.register(RiskOracle {}, ());
            let admin = Address::generate(&env);
            // initialize: Result<(), ContractError> → client döndürür ()
            RiskOracleClient::new(&env, &contract_id).initialize(&admin);

            Self { env, contract_id, admin }
        }

        fn client(&self) -> RiskOracleClient<'_> {
            RiskOracleClient::new(&self.env, &self.contract_id)
        }

        fn hash(n: u8, env: &Env) -> BytesN<32> {
            BytesN::from_array(env, &[n; 32])
        }

        fn future_ts(&self, delta_secs: u64) -> u64 {
            self.env.ledger().timestamp() + delta_secs
        }
    }

    // ── initialize ────────────────────────────────────────────────────────────

    #[test]
    fn test_initialize_sets_admin() {
        let f = Fixture::new();
        assert_eq!(f.client().get_admin(), Some(f.admin));
    }

    #[test]
    fn test_initialize_total_scores_zero() {
        let f = Fixture::new();
        assert_eq!(f.client().total_scores(), 0);
    }

    #[test]
    fn test_initialize_twice_fails() {
        let f = Fixture::new();
        let result = f.client().try_initialize(&f.admin);
        assert_eq!(
            result.unwrap_err().unwrap(),
            ContractError::AlreadyInitialized
        );
    }

    // ── set_risk_score ────────────────────────────────────────────────────────

    #[test]
    fn test_set_score_by_admin_succeeds() {
        let f = Fixture::new();
        let hash = Fixture::hash(1, &f.env);
        // set_risk_score: Result<(), ContractError> → ()
        f.client().set_risk_score(&hash, &42_u32, &f.future_ts(86_400));
        assert_eq!(f.client().get_risk_score(&hash), Some(42));
    }

    #[test]
    fn test_score_overwrite() {
        let f = Fixture::new();
        let hash = Fixture::hash(2, &f.env);

        f.client().set_risk_score(&hash, &20_u32, &f.future_ts(3600));
        f.client().set_risk_score(&hash, &80_u32, &f.future_ts(7200));

        assert_eq!(f.client().get_risk_score(&hash), Some(80));
        // Overwrite → toplam sayı artmaz
        assert_eq!(f.client().total_scores(), 1);
    }

    #[test]
    fn test_score_above_100_fails() {
        let f = Fixture::new();
        let hash = Fixture::hash(3, &f.env);

        let result = f.client().try_set_risk_score(
            &hash, &101_u32, &f.future_ts(3600),
        );
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidScore);
    }

    #[test]
    fn test_past_expiry_fails() {
        let f = Fixture::new();
        let hash = Fixture::hash(4, &f.env);

        // expiry = ledger.timestamp() (eşit) → geçmiş kabul edilir
        let result = f.client().try_set_risk_score(
            &hash,
            &50_u32,
            &f.env.ledger().timestamp(),
        );
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidExpiry);
    }

    #[test]
    fn test_boundary_score_zero() {
        let f = Fixture::new();
        let hash = Fixture::hash(5, &f.env);
        f.client().set_risk_score(&hash, &0_u32, &f.future_ts(3600));
        assert_eq!(f.client().get_risk_score(&hash), Some(0));
    }

    #[test]
    fn test_boundary_score_100() {
        let f = Fixture::new();
        let hash = Fixture::hash(6, &f.env);
        f.client().set_risk_score(&hash, &100_u32, &f.future_ts(3600));
        assert_eq!(f.client().get_risk_score(&hash), Some(100));
    }

    #[test]
    fn test_total_scores_increments_for_new_hash() {
        let f = Fixture::new();
        assert_eq!(f.client().total_scores(), 0);

        f.client().set_risk_score(&Fixture::hash(7, &f.env), &30_u32, &f.future_ts(3600));
        assert_eq!(f.client().total_scores(), 1);

        f.client().set_risk_score(&Fixture::hash(8, &f.env), &60_u32, &f.future_ts(3600));
        assert_eq!(f.client().total_scores(), 2);
    }

    // ── get_risk_score ────────────────────────────────────────────────────────

    #[test]
    fn test_get_score_returns_none_if_not_set() {
        let f = Fixture::new();
        assert_eq!(f.client().get_risk_score(&Fixture::hash(9, &f.env)), None);
    }

    #[test]
    fn test_get_score_returns_none_if_expired() {
        let f = Fixture::new();
        let hash = Fixture::hash(10, &f.env);

        let expiry = f.env.ledger().timestamp() + 1;
        f.client().set_risk_score(&hash, &75_u32, &expiry);

        f.env.ledger().set(LedgerInfo {
            timestamp: expiry + 1,
            protocol_version: 22,
            sequence_number: f.env.ledger().sequence(),
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });

        assert_eq!(f.client().get_risk_score(&hash), None);
    }

    #[test]
    fn test_get_score_returns_some_while_valid() {
        let f = Fixture::new();
        let hash = Fixture::hash(11, &f.env);
        let expiry = f.env.ledger().timestamp() + 1000;

        f.client().set_risk_score(&hash, &55_u32, &expiry);

        f.env.ledger().set(LedgerInfo {
            timestamp: expiry - 1,
            protocol_version: 22,
            sequence_number: f.env.ledger().sequence(),
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });

        assert_eq!(f.client().get_risk_score(&hash), Some(55));
    }

    // ── get_risk_entry ────────────────────────────────────────────────────────

    #[test]
    fn test_get_entry_returns_full_struct() {
        let f = Fixture::new();
        let hash   = Fixture::hash(12, &f.env);
        let expiry = f.future_ts(3600);

        f.client().set_risk_score(&hash, &33_u32, &expiry);

        // get_risk_entry returns Option<RiskEntry> — .unwrap() on Option is valid
        let entry = f.client().get_risk_entry(&hash).unwrap();
        assert_eq!(entry.score, 33);
        assert_eq!(entry.expiry, expiry);
        assert_eq!(entry.invoice_hash, hash);
    }

    #[test]
    fn test_get_entry_returns_none_if_not_set() {
        let f = Fixture::new();
        assert!(f.client().get_risk_entry(&Fixture::hash(13, &f.env)).is_none());
    }
}
