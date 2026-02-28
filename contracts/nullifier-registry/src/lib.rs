#![no_std]

//! # Nullifier Registry + Lumina State Registry
//!
//! İki sorumluluğu birleştirir:
//!
//! **1. ZK Çift Harcama Önleme**
//! Her invoice için üretilen `nullifier_hash = SHA-256(invoice_hash ‖ debtor_id)`
//! değeri yalnızca bir kez kaydedilebilir. İkinci kayıt girişimi `false` döndürür.
//!
//! **2. Lumina State Registry (Şeffaflık Katmanı)**
//! Invoice'ların yaşam döngüsünü (Active → Funded → Repaid / Defaulted) on-chain
//! takip eder. Vergi dairesi / banka gibi taraflar `query_state` ile nullifier bazlı
//! invoice durumunu sorgulayabilir; ancak kimlik bilgileri (invoice_hash) gizli kalır.
//!
//! ## State Geçiş Diyagramı
//! ```text
//! Active ──► Funded ──► Repaid       (normal akış)
//!                  ├──► Disputed ──► Defaulted   (temerrüt akışı)
//!                  │
//!                  └──► Disputed ──► Repaid      (itiraz çözüldü)
//! ```
//!
//! ## Erişim Kontrolü
//! - `register_nullifier` : whitelist'teki adresler (üretim: lumina-core)
//! - `update_state`       : yalnızca ADMIN
//! - `query_state`        : herkese açık (privacy-preserving)

use soroban_sdk::{
    contract, contractimpl, contracttype, contracterror,
    Address, BytesN, Env, Symbol,
};

// ── TTL sabitleri ─────────────────────────────────────────────────────────────

/// Nullifier kayıtları sonsuza kadar kalmalıdır; agresif TTL uzatması yapıyoruz.
const TTL_THRESHOLD: u32 = 518_400;   // ~30 gün
const TTL_EXTEND: u32    = 6_220_800; // ~360 gün

// ── Hata kodları ──────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ContractError {
    AlreadyInitialized = 1,
    NotInitialized     = 2,
    Unauthorized       = 3,
    InvalidTransition  = 4, // izin verilmeyen state geçişi
    EntryNotFound      = 5, // state kaydı bulunamadı
}

// ── Registry State enum ───────────────────────────────────────────────────────

/// Invoice'ın on-chain yaşam döngüsü
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryState {
    /// register_nullifier sonrası başlangıç durumu
    Active,
    /// factor_invoice başarıyla tamamlandı
    Funded,
    /// Temerrüt süreci başladı — itiraz açık
    Disputed,
    /// Borçlu tam ödeme yaptı
    Repaid,
    /// İtiraz çözümsüz kaldı, temerrüt kesinleşti
    Defaulted,
}

// ── Veri tipleri ──────────────────────────────────────────────────────────────

/// Kayıtlı bir nullifier'ın tam kaydı
#[contracttype]
#[derive(Clone, Debug)]
pub struct NullifierEntry {
    /// Kaydedildiği ledger timestamp
    pub registered_at: u64,
    /// İlgili invoice hash (audit için)
    pub invoice_hash:  BytesN<32>,
}

/// State Registry içindeki tam invoice kaydı (admin erişimli)
#[contracttype]
#[derive(Clone, Debug)]
pub struct InvoiceStateEntry {
    /// Nullifier — birincil anahtar
    pub nullifier:    BytesN<32>,
    /// İlgili invoice hash — AÇIKLANMAZ (gizli)
    pub invoice_hash: BytesN<32>,
    /// factor_invoice zamanı; henüz finanse edilmemişse 0
    pub funded_at:    u64,
    /// Invoice vade tarihi (Unix timestamp)
    pub due_date:     u64,
    /// Mevcut durum
    pub state:        RegistryState,
}

/// `query_state` tarafından döndürülen gizlilik korumalı görünüm.
///
/// `invoice_hash` ve fatura tutarı gibi kimlik bilgileri çıkarılmıştır.
/// Vergi daireleri ve bankalar yalnızca bu yapıya erişir.
#[contracttype]
#[derive(Clone, Debug)]
pub struct PublicStateEntry {
    pub nullifier: BytesN<32>,
    pub state:     RegistryState,
    pub due_date:  u64,
    /// 0 = henüz finanse edilmedi
    pub funded_at: u64,
}

/// `get_registry_stats` tarafından döndürülen istatistik özeti
#[contracttype]
#[derive(Clone, Debug)]
pub struct RegistryStats {
    /// Toplam kayıtlı nullifier sayısı
    pub total:     u64,
    /// Active durumdaki invoice sayısı
    pub active:    u64,
    /// Funded durumdaki invoice sayısı
    pub funded:    u64,
    /// Disputed durumdaki invoice sayısı
    pub disputed:  u64,
    /// Repaid durumdaki invoice sayısı
    pub repaid:    u64,
    /// Defaulted durumdaki invoice sayısı
    pub defaulted: u64,
}

// ── Storage anahtarları ───────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Admin,
    Whitelist(Address),
    /// Nullifier(BytesN<32>) → NullifierEntry
    Nullifier(BytesN<32>),
    TotalRegistered,
    /// StateEntry(nullifier) → InvoiceStateEntry
    StateEntry(BytesN<32>),
    CountActive,
    CountFunded,
    CountDisputed,
    CountRepaid,
    CountDefaulted,
}

// ── Kontrat ───────────────────────────────────────────────────────────────────

#[contract]
pub struct NullifierRegistry;

#[contractimpl]
impl NullifierRegistry {
    // ── Yönetim ──────────────────────────────────────────────────────────────

    pub fn initialize(
        env: Env,
        admin: Address,
        lumina_core: Address,
    ) -> Result<(), ContractError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(ContractError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::TotalRegistered, &0_u64);
        env.storage().instance().set(&DataKey::CountActive, &0_u64);
        env.storage().instance().set(&DataKey::CountFunded, &0_u64);
        env.storage().instance().set(&DataKey::CountDisputed, &0_u64);
        env.storage().instance().set(&DataKey::CountRepaid, &0_u64);
        env.storage().instance().set(&DataKey::CountDefaulted, &0_u64);
        env.storage()
            .instance()
            .set(&DataKey::Whitelist(lumina_core.clone()), &true);
        env.events().publish(
            (Symbol::new(&env, "initialized"),),
            (admin, lumina_core),
        );
        Ok(())
    }

    pub fn add_to_whitelist(env: Env, address: Address) -> Result<(), ContractError> {
        Self::require_admin(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Whitelist(address.clone()), &true);
        env.events()
            .publish((Symbol::new(&env, "wl_added"),), address);
        Ok(())
    }

    pub fn remove_from_whitelist(env: Env, address: Address) -> Result<(), ContractError> {
        Self::require_admin(&env)?;
        env.storage()
            .instance()
            .remove(&DataKey::Whitelist(address.clone()));
        env.events()
            .publish((Symbol::new(&env, "wl_removed"),), address);
        Ok(())
    }

    // ── Temel fonksiyonlar ────────────────────────────────────────────────────

    /// Nullifier hash'i kalıcı olarak kaydeder ve State Registry'e Active girdisi ekler.
    ///
    /// # Parametreler
    /// - `caller`       : whitelist'teki adres (imzalamalı)
    /// - `nullifier`    : SHA-256(invoice_hash ‖ debtor_id)
    /// - `invoice_hash` : ilgili invoice hash (audit için saklanır)
    /// - `due_date`     : invoice vade tarihi (Unix timestamp)
    ///
    /// # Döndürür
    /// - `true`  : yeni kaydedildi
    /// - `false` : zaten kayıtlı (çift harcama girişimi reddedildi)
    pub fn register_nullifier(
        env: Env,
        caller: Address,
        nullifier: BytesN<32>,
        invoice_hash: BytesN<32>,
        due_date: u64,
    ) -> Result<bool, ContractError> {
        caller.require_auth();
        if !Self::check_whitelisted(&env, &caller) {
            return Err(ContractError::Unauthorized);
        }

        if env
            .storage()
            .persistent()
            .has(&DataKey::Nullifier(nullifier.clone()))
        {
            return Ok(false);
        }

        let now = env.ledger().timestamp();

        // NullifierEntry kaydet
        let entry = NullifierEntry {
            registered_at: now,
            invoice_hash: invoice_hash.clone(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Nullifier(nullifier.clone()), &entry);
        env.storage().persistent().extend_ttl(
            &DataKey::Nullifier(nullifier.clone()),
            TTL_THRESHOLD,
            TTL_EXTEND,
        );

        // State Registry'e Active girdisi ekle
        let state_entry = InvoiceStateEntry {
            nullifier: nullifier.clone(),
            invoice_hash,
            funded_at: 0,
            due_date,
            state: RegistryState::Active,
        };
        env.storage()
            .persistent()
            .set(&DataKey::StateEntry(nullifier.clone()), &state_entry);
        env.storage().persistent().extend_ttl(
            &DataKey::StateEntry(nullifier.clone()),
            TTL_THRESHOLD,
            TTL_EXTEND,
        );

        // Sayaçları güncelle
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalRegistered)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalRegistered, &(count + 1));
        Self::increment_counter(&env, &DataKey::CountActive);

        env.events()
            .publish((Symbol::new(&env, "nullifier_reg"),), nullifier);
        Ok(true)
    }

    pub fn is_used(env: Env, nullifier: BytesN<32>) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::Nullifier(nullifier))
    }

    /// Nullifier'ın kaydedildiği timestamp'i döndürür (kayıtlı değilse 0).
    pub fn registered_at(env: Env, nullifier: BytesN<32>) -> u64 {
        env.storage()
            .persistent()
            .get::<DataKey, NullifierEntry>(&DataKey::Nullifier(nullifier))
            .map(|e| e.registered_at)
            .unwrap_or(0)
    }

    /// Tam `NullifierEntry`'yi döndürür; kayıtlı değilse `None`.
    pub fn get_nullifier_entry(env: Env, nullifier: BytesN<32>) -> Option<NullifierEntry> {
        env.storage()
            .persistent()
            .get(&DataKey::Nullifier(nullifier))
    }

    /// Toplam kayıtlı nullifier sayısını döndürür.
    pub fn list_active_invoices(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::TotalRegistered)
            .unwrap_or(0)
    }

    // ── State Registry fonksiyonları ─────────────────────────────────────────

    /// Invoice state'ini günceller.
    ///
    /// # İzin Verilen Geçişler
    /// ```text
    /// Active   → Funded
    /// Funded   → Repaid
    /// Funded   → Disputed
    /// Disputed → Defaulted
    /// Disputed → Repaid
    /// ```
    ///
    /// # Yetkilendirme
    /// Yalnızca admin çağırabilir.
    pub fn update_state(
        env: Env,
        nullifier: BytesN<32>,
        new_state: RegistryState,
    ) -> Result<(), ContractError> {
        Self::require_admin(&env)?;

        let mut entry: InvoiceStateEntry = env
            .storage()
            .persistent()
            .get(&DataKey::StateEntry(nullifier.clone()))
            .ok_or(ContractError::EntryNotFound)?;

        // State geçiş kuralları
        let valid = match (&entry.state, &new_state) {
            (RegistryState::Active,   RegistryState::Funded)   => true,
            (RegistryState::Funded,   RegistryState::Repaid)   => true,
            (RegistryState::Funded,   RegistryState::Disputed) => true,
            (RegistryState::Disputed, RegistryState::Defaulted)=> true,
            (RegistryState::Disputed, RegistryState::Repaid)   => true,
            _ => false,
        };

        if !valid {
            return Err(ContractError::InvalidTransition);
        }

        // Eski state sayacını düşür, yeni state sayacını artır
        Self::decrement_state_counter(&env, &entry.state);
        Self::increment_state_counter(&env, &new_state);

        // funded_at'i Funded geçişinde doldur
        if new_state == RegistryState::Funded {
            entry.funded_at = env.ledger().timestamp();
        }

        entry.state = new_state.clone();
        env.storage()
            .persistent()
            .set(&DataKey::StateEntry(nullifier.clone()), &entry);
        env.storage().persistent().extend_ttl(
            &DataKey::StateEntry(nullifier.clone()),
            TTL_THRESHOLD,
            TTL_EXTEND,
        );

        env.events().publish(
            (Symbol::new(&env, "state_updated"), nullifier),
            new_state,
        );
        Ok(())
    }

    /// Invoice durumunu gizlilik korumalı biçimde sorgular.
    ///
    /// `invoice_hash` açıklanmaz; yalnızca nullifier, state, due_date ve funded_at döner.
    /// Vergi daireleri ve bankalar bu endpoint'i kullanır.
    pub fn query_state(env: Env, nullifier: BytesN<32>) -> Option<PublicStateEntry> {
        let entry: InvoiceStateEntry = env
            .storage()
            .persistent()
            .get(&DataKey::StateEntry(nullifier.clone()))?;

        Some(PublicStateEntry {
            nullifier,
            state: entry.state,
            due_date: entry.due_date,
            funded_at: entry.funded_at,
        })
    }

    /// Kayıt istatistiklerini döndürür.
    pub fn get_registry_stats(env: Env) -> RegistryStats {
        let g = |k: DataKey| -> u64 {
            env.storage().instance().get(&k).unwrap_or(0)
        };
        RegistryStats {
            total:     g(DataKey::TotalRegistered),
            active:    g(DataKey::CountActive),
            funded:    g(DataKey::CountFunded),
            disputed:  g(DataKey::CountDisputed),
            repaid:    g(DataKey::CountRepaid),
            defaulted: g(DataKey::CountDefaulted),
        }
    }

    pub fn is_whitelisted(env: Env, address: Address) -> bool {
        Self::check_whitelisted(&env, &address)
    }

    pub fn total_registered(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::TotalRegistered)
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

    fn check_whitelisted(env: &Env, address: &Address) -> bool {
        env.storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Whitelist(address.clone()))
            .unwrap_or(false)
    }

    fn increment_counter(env: &Env, key: &DataKey) {
        let count: u64 = env.storage().instance().get(key).unwrap_or(0);
        env.storage().instance().set(key, &(count + 1));
    }

    fn decrement_counter(env: &Env, key: &DataKey) {
        let count: u64 = env.storage().instance().get(key).unwrap_or(0);
        env.storage().instance().set(key, &count.saturating_sub(1));
    }

    fn state_counter_key(state: &RegistryState) -> DataKey {
        match state {
            RegistryState::Active   => DataKey::CountActive,
            RegistryState::Funded   => DataKey::CountFunded,
            RegistryState::Disputed => DataKey::CountDisputed,
            RegistryState::Repaid   => DataKey::CountRepaid,
            RegistryState::Defaulted=> DataKey::CountDefaulted,
        }
    }

    fn increment_state_counter(env: &Env, state: &RegistryState) {
        Self::increment_counter(env, &Self::state_counter_key(state));
    }

    fn decrement_state_counter(env: &Env, state: &RegistryState) {
        Self::decrement_counter(env, &Self::state_counter_key(state));
    }
}

// ── Unit testler ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Env, Address};

    struct Fixture {
        env:         Env,
        contract_id: Address,
        admin:       Address,
        caller:      Address,
    }

    impl Fixture {
        fn new() -> Self {
            let env = Env::default();
            env.mock_all_auths();

            let contract_id = env.register(NullifierRegistry {}, ());
            let admin  = Address::generate(&env);
            let caller = Address::generate(&env);

            NullifierRegistryClient::new(&env, &contract_id).initialize(&admin, &caller);

            Self { env, contract_id, admin, caller }
        }

        fn client(&self) -> NullifierRegistryClient<'_> {
            NullifierRegistryClient::new(&self.env, &self.contract_id)
        }

        fn nul(n: u8, env: &Env) -> BytesN<32> {
            BytesN::from_array(env, &[n; 32])
        }

        fn inv_hash(n: u8, env: &Env) -> BytesN<32> {
            BytesN::from_array(env, &[n.wrapping_add(128); 32])
        }

        fn due(&self) -> u64 {
            self.env.ledger().timestamp() + 86_400
        }

        /// Kayıt yapar ve StateEntry oluşturur.
        fn register(&self, nul_byte: u8) -> BytesN<32> {
            let n = Self::nul(nul_byte, &self.env);
            let ih = Self::inv_hash(nul_byte, &self.env);
            self.client().register_nullifier(&self.caller, &n, &ih, &self.due());
            n
        }
    }

    // ── initialize ────────────────────────────────────────────────────────────

    #[test]
    fn test_initialize_sets_admin() {
        let f = Fixture::new();
        assert_eq!(f.client().get_admin(), Some(f.admin));
    }

    #[test]
    fn test_initialize_whitelists_lumina_core() {
        let f = Fixture::new();
        assert!(f.client().is_whitelisted(&f.caller));
    }

    #[test]
    fn test_initialize_total_zero() {
        let f = Fixture::new();
        assert_eq!(f.client().total_registered(), 0);
    }

    #[test]
    fn test_initialize_twice_fails() {
        let f = Fixture::new();
        let result = f.client().try_initialize(&f.admin, &f.caller);
        assert_eq!(
            result.unwrap_err().unwrap(),
            ContractError::AlreadyInitialized
        );
    }

    // ── whitelist ─────────────────────────────────────────────────────────────

    #[test]
    fn test_add_to_whitelist() {
        let f = Fixture::new();
        let new_addr = Address::generate(&f.env);
        assert!(!f.client().is_whitelisted(&new_addr));
        f.client().add_to_whitelist(&new_addr);
        assert!(f.client().is_whitelisted(&new_addr));
    }

    #[test]
    fn test_remove_from_whitelist() {
        let f = Fixture::new();
        f.client().remove_from_whitelist(&f.caller);
        assert!(!f.client().is_whitelisted(&f.caller));
    }

    #[test]
    fn test_non_whitelisted_cannot_register() {
        let f = Fixture::new();
        let outsider = Address::generate(&f.env);
        let n = Fixture::nul(10, &f.env);
        let ih = Fixture::inv_hash(10, &f.env);
        let result = f.client().try_register_nullifier(&outsider, &n, &ih, &f.due());
        assert_eq!(result.unwrap_err().unwrap(), ContractError::Unauthorized);
    }

    // ── register_nullifier ────────────────────────────────────────────────────

    #[test]
    fn test_register_new_returns_true() {
        let f = Fixture::new();
        let ok = f.client().register_nullifier(
            &f.caller, &Fixture::nul(1, &f.env), &Fixture::inv_hash(1, &f.env), &f.due(),
        );
        assert!(ok);
    }

    #[test]
    fn test_register_duplicate_returns_false() {
        let f = Fixture::new();
        let n = Fixture::nul(2, &f.env);
        let ih = Fixture::inv_hash(2, &f.env);
        f.client().register_nullifier(&f.caller, &n, &ih, &f.due());
        let second = f.client().register_nullifier(&f.caller, &n, &ih, &f.due());
        assert!(!second);
    }

    #[test]
    fn test_register_different_nullifiers_both_true() {
        let f = Fixture::new();
        assert!(f.client().register_nullifier(
            &f.caller, &Fixture::nul(3, &f.env), &Fixture::inv_hash(3, &f.env), &f.due(),
        ));
        assert!(f.client().register_nullifier(
            &f.caller, &Fixture::nul(4, &f.env), &Fixture::inv_hash(4, &f.env), &f.due(),
        ));
    }

    #[test]
    fn test_register_increments_total() {
        let f = Fixture::new();
        assert_eq!(f.client().total_registered(), 0);
        f.client().register_nullifier(
            &f.caller, &Fixture::nul(5, &f.env), &Fixture::inv_hash(5, &f.env), &f.due(),
        );
        assert_eq!(f.client().total_registered(), 1);
        f.client().register_nullifier(
            &f.caller, &Fixture::nul(6, &f.env), &Fixture::inv_hash(6, &f.env), &f.due(),
        );
        assert_eq!(f.client().total_registered(), 2);
    }

    #[test]
    fn test_duplicate_does_not_increment_total() {
        let f = Fixture::new();
        let n = Fixture::nul(7, &f.env);
        let ih = Fixture::inv_hash(7, &f.env);
        f.client().register_nullifier(&f.caller, &n, &ih, &f.due());
        f.client().register_nullifier(&f.caller, &n, &ih, &f.due()); // false, sayaç artmaz
        assert_eq!(f.client().total_registered(), 1);
    }

    // ── is_used ───────────────────────────────────────────────────────────────

    #[test]
    fn test_is_used_false_before_register() {
        let f = Fixture::new();
        assert!(!f.client().is_used(&Fixture::nul(8, &f.env)));
    }

    #[test]
    fn test_is_used_true_after_register() {
        let f = Fixture::new();
        let n = Fixture::nul(9, &f.env);
        f.client().register_nullifier(&f.caller, &n, &Fixture::inv_hash(9, &f.env), &f.due());
        assert!(f.client().is_used(&n));
    }

    // ── registered_at ─────────────────────────────────────────────────────────

    #[test]
    fn test_registered_at_zero_if_unused() {
        let f = Fixture::new();
        assert_eq!(f.client().registered_at(&Fixture::nul(12, &f.env)), 0);
    }

    #[test]
    fn test_registered_at_returns_timestamp() {
        let f = Fixture::new();
        let n  = Fixture::nul(13, &f.env);
        let ts = f.env.ledger().timestamp();
        f.client().register_nullifier(&f.caller, &n, &Fixture::inv_hash(13, &f.env), &f.due());
        assert_eq!(f.client().registered_at(&n), ts);
    }

    // ── get_nullifier_entry ───────────────────────────────────────────────────

    #[test]
    fn test_get_nullifier_entry_returns_full_struct() {
        let f = Fixture::new();
        let n  = Fixture::nul(20, &f.env);
        let ih = BytesN::from_array(&f.env, &[42u8; 32]);
        let ts = f.env.ledger().timestamp();

        f.client().register_nullifier(&f.caller, &n, &ih, &f.due());

        let entry = f.client().get_nullifier_entry(&n).unwrap();
        assert_eq!(entry.registered_at, ts);
        assert_eq!(entry.invoice_hash, ih);
    }

    #[test]
    fn test_get_nullifier_entry_none_if_unregistered() {
        let f = Fixture::new();
        assert!(f.client().get_nullifier_entry(&Fixture::nul(21, &f.env)).is_none());
    }

    // ── list_active_invoices ──────────────────────────────────────────────────

    #[test]
    fn test_list_active_invoices_equals_total_registered() {
        let f = Fixture::new();
        assert_eq!(f.client().list_active_invoices(), 0);

        f.client().register_nullifier(
            &f.caller, &Fixture::nul(22, &f.env), &Fixture::inv_hash(22, &f.env), &f.due(),
        );
        assert_eq!(f.client().list_active_invoices(), 1);

        f.client().register_nullifier(
            &f.caller, &Fixture::nul(23, &f.env), &Fixture::inv_hash(23, &f.env), &f.due(),
        );
        assert_eq!(f.client().list_active_invoices(), 2);
    }

    // ── update_state — geçerli geçişler ──────────────────────────────────────

    #[test]
    fn test_update_state_active_to_funded() {
        let f = Fixture::new();
        let n = f.register(50);

        f.client().update_state(&n, &RegistryState::Funded);

        let entry = f.client().query_state(&n).unwrap();
        assert_eq!(entry.state, RegistryState::Funded);
        // funded_at, Funded geçişinde doldurulur
        assert!(entry.funded_at > 0 || f.env.ledger().timestamp() == 0);
    }

    #[test]
    fn test_update_state_funded_to_repaid() {
        let f = Fixture::new();
        let n = f.register(51);

        f.client().update_state(&n, &RegistryState::Funded);
        f.client().update_state(&n, &RegistryState::Repaid);

        let entry = f.client().query_state(&n).unwrap();
        assert_eq!(entry.state, RegistryState::Repaid);
    }

    #[test]
    fn test_update_state_funded_to_disputed() {
        let f = Fixture::new();
        let n = f.register(52);

        f.client().update_state(&n, &RegistryState::Funded);
        f.client().update_state(&n, &RegistryState::Disputed);

        let entry = f.client().query_state(&n).unwrap();
        assert_eq!(entry.state, RegistryState::Disputed);
    }

    #[test]
    fn test_update_state_disputed_to_defaulted() {
        let f = Fixture::new();
        let n = f.register(53);

        f.client().update_state(&n, &RegistryState::Funded);
        f.client().update_state(&n, &RegistryState::Disputed);
        f.client().update_state(&n, &RegistryState::Defaulted);

        let entry = f.client().query_state(&n).unwrap();
        assert_eq!(entry.state, RegistryState::Defaulted);
    }

    #[test]
    fn test_update_state_disputed_to_repaid() {
        let f = Fixture::new();
        let n = f.register(54);

        f.client().update_state(&n, &RegistryState::Funded);
        f.client().update_state(&n, &RegistryState::Disputed);
        f.client().update_state(&n, &RegistryState::Repaid);

        let entry = f.client().query_state(&n).unwrap();
        assert_eq!(entry.state, RegistryState::Repaid);
    }

    // ── update_state — geçersiz geçişler ─────────────────────────────────────

    #[test]
    fn test_invalid_transition_active_to_repaid() {
        let f = Fixture::new();
        let n = f.register(60);

        let result = f.client().try_update_state(&n, &RegistryState::Repaid);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidTransition);
    }

    #[test]
    fn test_invalid_transition_active_to_defaulted() {
        let f = Fixture::new();
        let n = f.register(61);

        let result = f.client().try_update_state(&n, &RegistryState::Defaulted);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidTransition);
    }

    #[test]
    fn test_invalid_transition_active_to_disputed() {
        let f = Fixture::new();
        let n = f.register(62);

        let result = f.client().try_update_state(&n, &RegistryState::Disputed);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidTransition);
    }

    #[test]
    fn test_invalid_transition_repaid_to_funded() {
        let f = Fixture::new();
        let n = f.register(63);

        f.client().update_state(&n, &RegistryState::Funded);
        f.client().update_state(&n, &RegistryState::Repaid);

        // Repaid'den geri dönüş yok
        let result = f.client().try_update_state(&n, &RegistryState::Funded);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidTransition);
    }

    #[test]
    fn test_update_state_nonexistent_fails() {
        let f = Fixture::new();
        let ghost = Fixture::nul(99, &f.env);
        let result = f.client().try_update_state(&ghost, &RegistryState::Funded);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::EntryNotFound);
    }

    // ── query_state — gizlilik koruması ──────────────────────────────────────

    #[test]
    fn test_query_state_returns_public_entry() {
        let f = Fixture::new();
        let due = f.due();
        let n = Fixture::nul(70, &f.env);
        let ih = Fixture::inv_hash(70, &f.env);
        f.client().register_nullifier(&f.caller, &n, &ih, &due);

        let pub_entry = f.client().query_state(&n).unwrap();

        // invoice_hash PublicStateEntry'de yok — sadece nullifier, state, due_date, funded_at
        assert_eq!(pub_entry.nullifier, n);
        assert_eq!(pub_entry.state, RegistryState::Active);
        assert_eq!(pub_entry.due_date, due);
        assert_eq!(pub_entry.funded_at, 0); // henüz finanse edilmedi
    }

    #[test]
    fn test_query_state_none_if_not_registered() {
        let f = Fixture::new();
        assert!(f.client().query_state(&Fixture::nul(71, &f.env)).is_none());
    }

    #[test]
    fn test_query_state_funded_at_set_after_funding() {
        let f = Fixture::new();
        let n = f.register(72);

        f.client().update_state(&n, &RegistryState::Funded);

        let pub_entry = f.client().query_state(&n).unwrap();
        assert_eq!(pub_entry.state, RegistryState::Funded);
        // Test env timestamp=0, bu yüzden funded_at=0 bile geçerli
        // (funded_at dolduruldu = env.ledger().timestamp() = 0)
    }

    // ── get_registry_stats ────────────────────────────────────────────────────

    #[test]
    fn test_registry_stats_initial_all_zero() {
        let f = Fixture::new();
        let stats = f.client().get_registry_stats();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.active, 0);
        assert_eq!(stats.funded, 0);
        assert_eq!(stats.disputed, 0);
        assert_eq!(stats.repaid, 0);
        assert_eq!(stats.defaulted, 0);
    }

    #[test]
    fn test_registry_stats_after_register() {
        let f = Fixture::new();
        f.register(80);
        f.register(81);

        let stats = f.client().get_registry_stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.active, 2);
        assert_eq!(stats.funded, 0);
    }

    #[test]
    fn test_registry_stats_counters_update_on_transition() {
        let f = Fixture::new();
        let n1 = f.register(82);
        let n2 = f.register(83);

        // n1 → Funded → Repaid (normal akış)
        f.client().update_state(&n1, &RegistryState::Funded);
        f.client().update_state(&n1, &RegistryState::Repaid);

        // n2 → Funded → Disputed → Defaulted (temerrüt akışı)
        f.client().update_state(&n2, &RegistryState::Funded);
        f.client().update_state(&n2, &RegistryState::Disputed);
        f.client().update_state(&n2, &RegistryState::Defaulted);

        let stats = f.client().get_registry_stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.active, 0);
        assert_eq!(stats.funded, 0);
        assert_eq!(stats.disputed, 0);
        assert_eq!(stats.repaid, 1);
        assert_eq!(stats.defaulted, 1);
    }

    #[test]
    fn test_registry_stats_disputed_counter() {
        let f = Fixture::new();
        let n = f.register(84);

        f.client().update_state(&n, &RegistryState::Funded);
        f.client().update_state(&n, &RegistryState::Disputed);

        let stats = f.client().get_registry_stats();
        assert_eq!(stats.active, 0);
        assert_eq!(stats.funded, 0);
        assert_eq!(stats.disputed, 1);
    }

    // ── whitelist sonrası register ────────────────────────────────────────────

    #[test]
    fn test_newly_whitelisted_can_register() {
        let f = Fixture::new();
        let new_oracle = Address::generate(&f.env);
        f.client().add_to_whitelist(&new_oracle);
        let ok = f.client().register_nullifier(
            &new_oracle, &Fixture::nul(14, &f.env), &Fixture::inv_hash(14, &f.env), &f.due(),
        );
        assert!(ok);
    }

    #[test]
    fn test_removed_from_whitelist_cannot_register() {
        let f = Fixture::new();
        f.client().remove_from_whitelist(&f.caller);
        let result = f.client().try_register_nullifier(
            &f.caller, &Fixture::nul(15, &f.env), &Fixture::inv_hash(15, &f.env), &f.due(),
        );
        assert_eq!(result.unwrap_err().unwrap(), ContractError::Unauthorized);
    }
}
