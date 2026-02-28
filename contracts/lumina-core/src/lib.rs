#![no_std]

//! # Lumina Core
//!
//! Ana faktoring kontratı. Invoice'ları kayıt altına alır, ZK proof ile
//! finanse eder ve geri ödemeyi takip eder.
//!
//! ## Fonksiyon Akışı
//! ```text
//! submit_invoice → [Pending]
//!                        ↓ factor_invoice
//!                    [Funded]  ← collect_premium (liquidity-pools)
//!                              ← update_state(Funded) (nullifier-registry)
//!                   ↙       ↘
//!           repay()       mark_defaulted()
//!         [Repaid]         [Defaulted]
//! ```

use soroban_sdk::{
    contract, contractimpl, contracttype, contracterror,
    Address, BytesN, Env, IntoVal, Symbol, Val,
};

// ── TTL (Time-To-Live) sabitleri ─────────────────────────────────────────────

/// Persistent entry için TTL yenileme eşiği (15 gün)
const TTL_THRESHOLD: u32 = 259_200;
/// Persistent entry için TTL uzatma miktarı (180 gün)
const TTL_EXTEND: u32 = 3_110_400;

// ── Hata kodları ──────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ContractError {
    AlreadyInitialized = 1,
    NotInitialized     = 2,
    Unauthorized       = 3,
    InvoiceNotFound    = 4,
    InvalidState       = 5,
    InvalidAmount      = 6,
    InvalidRiskScore   = 7, // risk_score > 100
    InvalidDueDate     = 8, // due_date geçmişte
    DuplicateHash      = 9, // aynı invoice_hash ikinci kez
}

// ── Veri tipleri ──────────────────────────────────────────────────────────────

/// Invoice yaşam döngüsü
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvoiceState {
    Pending,   // submit_invoice sonrası
    Funded,    // factor_invoice sonrası, avans ödendi
    Repaid,    // debtor tam tutarı ödedi
    Defaulted, // vadesi geçti, geri ödeme yapılmadı
}

/// Tam invoice kaydı
#[contracttype]
#[derive(Clone, Debug)]
pub struct Invoice {
    /// Otomatik artan benzersiz kimlik
    pub id: u64,
    /// SHA-256(invoice belgesi) — ZK bağlama noktası
    pub invoice_hash: BytesN<32>,
    /// Fatura yüz değeri (stablecoin en küçük birimi)
    pub amount: i128,
    /// Avansı alacak şirket (receivable satıcısı)
    pub company: Address,
    /// Borçlu — tam tutarı ödeyecek taraf
    pub debtor: Address,
    /// Vade tarihi (Unix timestamp)
    pub due_date: u64,
    /// Mevcut durum
    pub state: InvoiceState,
    /// factor_invoice ile ödenen avans tutarı
    pub advance_amount: i128,
    /// Uygulanan yıllık faiz oranı (basis points)
    pub apr_bps: u32,
    /// submit_invoice zamanı (ledger timestamp)
    pub created_at: u64,
    /// factor_invoice zamanı (0 = henüz finanse edilmedi)
    pub funded_at: u64,
    /// APR hesabında kullanılan risk skoru (0-100)
    pub risk_score: u32,
    /// ZK nullifier = SHA-256(invoice_hash ‖ debtor_id)
    /// nullifier-registry'deki state güncellemeleri için kullanılır.
    pub nullifier: BytesN<32>,
}

/// factor_invoice'ın döndürdüğü özet bilgi
#[contracttype]
#[derive(Clone, Debug)]
pub struct FactorResult {
    /// Şirkete ödenen avans tutarı (iskonto uygulanmış)
    pub advance_amount: i128,
    /// Uygulanan yıllık faiz oranı (basis points)
    pub apr_bps: u32,
    /// ZK'dan gelen risk skoru (0-100)
    pub risk_score: u32,
}

/// Storage anahtarları
#[contracttype]
pub enum DataKey {
    /// Admin adresi
    Admin,
    /// nullifier-registry kontrat adresi
    NullifierRegistry,
    /// risk-oracle kontrat adresi
    RiskOracle,
    /// liquidity-pools kontrat adresi
    LiquidityPool,
    /// Sonraki invoice ID sayacı
    NextInvoiceId,
    /// Invoice(id) → Invoice struct
    Invoice(u64),
    /// Aynı hash'in iki kez gönderilmesini engeller
    InvoiceByHash(BytesN<32>),
    /// Toplam dağıtılan avans miktarı
    TotalDisbursed,
}

// ── Kontrat ───────────────────────────────────────────────────────────────────

#[contract]
pub struct LuminaCore;

#[contractimpl]
impl LuminaCore {
    // ── Yönetim ──────────────────────────────────────────────────────────────

    /// Kontratı başlatır. Yalnızca bir kez çağrılabilir.
    ///
    /// # Parametreler
    /// - `admin`               : admin adresi
    /// - `nullifier_registry`  : nullifier-registry kontrat adresi
    /// - `risk_oracle`         : risk-oracle kontrat adresi
    /// - `liquidity_pools`     : liquidity-pools kontrat adresi
    pub fn initialize(
        env: Env,
        admin: Address,
        nullifier_registry: Address,
        risk_oracle: Address,
        liquidity_pools: Address,
    ) -> Result<(), ContractError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(ContractError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::NullifierRegistry, &nullifier_registry);
        env.storage().instance().set(&DataKey::RiskOracle, &risk_oracle);
        env.storage().instance().set(&DataKey::LiquidityPool, &liquidity_pools);
        env.storage().instance().set(&DataKey::NextInvoiceId, &1_u64);
        env.storage().instance().set(&DataKey::TotalDisbursed, &0_i128);
        Ok(())
    }

    // ── Temel fonksiyonlar ────────────────────────────────────────────────────

    /// Yeni bir invoice kaydeder ve otomatik artan ID döndürür.
    ///
    /// # Parametreler
    /// - `company`      : avansı alacak şirket (bu çağrıyı imzalamalı)
    /// - `invoice_hash` : SHA-256(invoice belgesi); ZK proof'ta kullanılır
    /// - `amount`       : fatura yüz değeri, stablecoin en küçük birimi
    /// - `debtor`       : borçlu adresi (geri ödeme yapacak)
    /// - `due_date`     : son ödeme zamanı (Unix timestamp, gelecekte olmalı)
    /// - `nullifier`    : SHA-256(invoice_hash ‖ debtor_id); registry için saklanır
    pub fn submit_invoice(
        env: Env,
        company: Address,
        invoice_hash: BytesN<32>,
        amount: i128,
        debtor: Address,
        due_date: u64,
        nullifier: BytesN<32>,
    ) -> Result<u64, ContractError> {
        company.require_auth();
        Self::assert_initialized(&env)?;

        if amount <= 0 {
            return Err(ContractError::InvalidAmount);
        }
        if due_date <= env.ledger().timestamp() {
            return Err(ContractError::InvalidDueDate);
        }
        if env.storage().persistent().has(&DataKey::InvoiceByHash(invoice_hash.clone())) {
            return Err(ContractError::DuplicateHash);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextInvoiceId)
            .unwrap_or(1);

        let invoice = Invoice {
            id,
            invoice_hash: invoice_hash.clone(),
            amount,
            company: company.clone(),
            debtor,
            due_date,
            state: InvoiceState::Pending,
            advance_amount: 0,
            apr_bps: 0,
            created_at: env.ledger().timestamp(),
            funded_at: 0,
            risk_score: 0,
            nullifier,
        };

        env.storage().persistent().set(&DataKey::Invoice(id), &invoice);
        env.storage()
            .persistent()
            .set(&DataKey::InvoiceByHash(invoice_hash), &id);

        // TTL uzat — invoice kaydı kalıcı olmalı
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Invoice(id), TTL_THRESHOLD, TTL_EXTEND);

        env.storage().instance().set(&DataKey::NextInvoiceId, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "invoice_sub"), company),
            (id, amount),
        );

        Ok(id)
    }

    /// Bekleyen bir invoice'ı finanse eder; şirkete avans ödemesini başlatır.
    ///
    /// # APR Bantları (basis points)
    /// ```text
    /// risk_score  0– 20 →  500 bps ( 5%)
    /// risk_score 21– 40 →  800 bps ( 8%)
    /// risk_score 41– 60 → 1200 bps (12%)
    /// risk_score 61– 80 → 1800 bps (18%)
    /// risk_score 81–100 → 2500 bps (25%)
    /// ```
    ///
    /// # Cross-contract Çağrılar
    /// 1. `liquidity-pools.collect_premium(invoice_id, amount)` → sigorta prim tahsili
    /// 2. `nullifier-registry.update_state(nullifier, Funded)` → registry state güncelleme
    /// 3. `liquidity-pools.disburse(company, advance_amount)` → avans transferi
    ///
    /// # Yetkilendirme
    /// Yalnızca admin çağırabilir.
    pub fn factor_invoice(
        env: Env,
        invoice_id: u64,
        risk_score: u32,
    ) -> Result<FactorResult, ContractError> {
        Self::require_admin(&env)?;

        if risk_score > 100 {
            return Err(ContractError::InvalidRiskScore);
        }

        let mut invoice: Invoice = env
            .storage()
            .persistent()
            .get(&DataKey::Invoice(invoice_id))
            .ok_or(ContractError::InvoiceNotFound)?;

        if invoice.state != InvoiceState::Pending {
            return Err(ContractError::InvalidState);
        }

        let now = env.ledger().timestamp();
        if invoice.due_date <= now {
            return Err(ContractError::InvalidDueDate);
        }

        // ── Bantlı APR seçimi ────────────────────────────────────────────────
        let apr_bps: u32 = match risk_score {
            0..=20  =>  500,
            21..=40 =>  800,
            41..=60 => 1200,
            61..=80 => 1800,
            _       => 2500, // 81..=100
        };

        // ── Zaman bazlı avans hesabı ─────────────────────────────────────────
        let days_to_maturity: i128 = ((invoice.due_date - now) / 86_400) as i128;
        let discount: i128 =
            invoice.amount * apr_bps as i128 * days_to_maturity / (365 * 10_000);
        let advance_amount: i128 = invoice.amount - discount;

        // ── Durum güncellemesi ───────────────────────────────────────────────
        invoice.state = InvoiceState::Funded;
        invoice.advance_amount = advance_amount;
        invoice.apr_bps = apr_bps;
        invoice.funded_at = env.ledger().timestamp();
        invoice.risk_score = risk_score;

        env.storage()
            .persistent()
            .set(&DataKey::Invoice(invoice_id), &invoice);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Invoice(invoice_id), TTL_THRESHOLD, TTL_EXTEND);

        let pool: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityPool)
            .unwrap();

        let nullifier_registry: Address = env
            .storage()
            .instance()
            .get(&DataKey::NullifierRegistry)
            .unwrap();

        // ── Cross-contract 1: collect_premium ────────────────────────────────
        // liquidity-pools.collect_premium(invoice_id: u64, invoice_amount: i128) → i128
        let premium_args: soroban_sdk::Vec<Val> = soroban_sdk::vec![
            &env,
            invoice_id.into_val(&env),
            invoice.amount.into_val(&env),
        ];
        env.invoke_contract::<i128>(&pool, &Symbol::new(&env, "collect_premium"), premium_args);

        // ── Cross-contract 2: update_state(nullifier, Funded) ────────────────
        // RegistryState::Funded → #[contracttype] unit enum → SCV_SYMBOL("Funded")
        let state_args: soroban_sdk::Vec<Val> = soroban_sdk::vec![
            &env,
            invoice.nullifier.clone().into_val(&env),
            Symbol::new(&env, "Funded").into_val(&env),
        ];
        env.invoke_contract::<()>(
            &nullifier_registry,
            &Symbol::new(&env, "update_state"),
            state_args,
        );

        // ── Cross-contract 3: disburse avans ─────────────────────────────────
        // liquidity-pools.disburse(recipient: Address, amount: i128)
        let disburse_args: soroban_sdk::Vec<Val> = soroban_sdk::vec![
            &env,
            invoice.company.into_val(&env),
            advance_amount.into_val(&env),
        ];
        env.invoke_contract::<()>(&pool, &Symbol::new(&env, "disburse"), disburse_args);

        // ── Toplam sayacı güncelle ───────────────────────────────────────────
        let prev_total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalDisbursed)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalDisbursed, &(prev_total + advance_amount));

        env.events().publish(
            (Symbol::new(&env, "invoice_funded"), invoice_id),
            (risk_score, advance_amount, apr_bps),
        );

        Ok(FactorResult { advance_amount, apr_bps, risk_score })
    }

    /// Finanse edilmiş invoice'ın geri ödemesini işler.
    ///
    /// Çağıran (debtor) `record_repayment` üzerinden havuza bildirir.
    /// Token transferi debtor tarafından doğrudan havuza yapılır.
    ///
    /// # Döndürür
    /// - `true`  : ödeme başarıyla işlendi
    /// - `false` : invoice bulunamadı veya `Funded` durumunda değil
    ///
    /// # Yetkilendirme
    /// invoice.debtor imzalamalıdır.
    pub fn repay(env: Env, invoice_id: u64) -> Result<bool, ContractError> {
        let maybe: Option<Invoice> = env
            .storage()
            .persistent()
            .get(&DataKey::Invoice(invoice_id));

        let mut invoice = match maybe {
            Some(inv) => inv,
            None => return Ok(false),
        };

        if invoice.state != InvoiceState::Funded {
            return Ok(false);
        }

        invoice.debtor.require_auth();

        let pool: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityPool)
            .unwrap();

        // Havuza deployed capital'ı düşür
        let repay_args: soroban_sdk::Vec<Val> = soroban_sdk::vec![
            &env,
            invoice.amount.into_val(&env),
        ];
        env.invoke_contract::<()>(&pool, &Symbol::new(&env, "record_repayment"), repay_args);

        invoice.state = InvoiceState::Repaid;
        env.storage()
            .persistent()
            .set(&DataKey::Invoice(invoice_id), &invoice);

        env.events().publish(
            (Symbol::new(&env, "invoice_repaid"), invoice_id),
            invoice.amount,
        );

        Ok(true)
    }

    /// Admin: Vadesi geçmiş, ödenmemiş invoice'ı varsayılan olarak işaretler.
    pub fn mark_defaulted(env: Env, invoice_id: u64) -> Result<(), ContractError> {
        Self::require_admin(&env)?;

        let mut invoice: Invoice = env
            .storage()
            .persistent()
            .get(&DataKey::Invoice(invoice_id))
            .ok_or(ContractError::InvoiceNotFound)?;

        if invoice.state != InvoiceState::Funded {
            return Err(ContractError::InvalidState);
        }

        invoice.state = InvoiceState::Defaulted;
        env.storage()
            .persistent()
            .set(&DataKey::Invoice(invoice_id), &invoice);

        env.events()
            .publish((Symbol::new(&env, "invoice_default"),), invoice_id);

        Ok(())
    }

    // ── View fonksiyonları ────────────────────────────────────────────────────

    pub fn get_invoice(env: Env, invoice_id: u64) -> Option<Invoice> {
        env.storage().persistent().get(&DataKey::Invoice(invoice_id))
    }

    pub fn get_state(env: Env, invoice_id: u64) -> Option<InvoiceState> {
        env.storage()
            .persistent()
            .get::<DataKey, Invoice>(&DataKey::Invoice(invoice_id))
            .map(|inv| inv.state)
    }

    pub fn get_total_disbursed(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalDisbursed)
            .unwrap_or(0)
    }

    pub fn get_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    pub fn get_next_id(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NextInvoiceId)
            .unwrap_or(1)
    }

    // ── İç yardımcılar ───────────────────────────────────────────────────────

    fn assert_initialized(env: &Env) -> Result<(), ContractError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(ContractError::NotInitialized);
        }
        Ok(())
    }

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

// ── Test ortamı için mock kontratlar ──────────────────────────────────────────

#[cfg(test)]
pub mod mock_pool {
    use soroban_sdk::{contract, contractimpl, Env, Address};

    #[contract]
    pub struct MockPool;

    #[contractimpl]
    impl MockPool {
        /// lumina-core'dan gelen avans çağrısı
        pub fn disburse(_env: Env, _recipient: Address, _amount: i128) {}

        /// factor_invoice'dan gelen sigorta primi tahsili; prim tutarını döndürür
        pub fn collect_premium(_env: Env, _invoice_id: u64, invoice_amount: i128) -> i128 {
            // %1.5 prim (150/10_000)
            invoice_amount * 150 / 10_000
        }

        /// repay sonrası deployed capital düşürme
        pub fn record_repayment(_env: Env, _amount: i128) {}
    }
}

#[cfg(test)]
pub mod mock_nullifier_registry {
    use soroban_sdk::{contract, contractimpl, Env, BytesN, Symbol};

    #[contract]
    pub struct MockNullifierRegistry;

    #[contractimpl]
    impl MockNullifierRegistry {
        /// lumina-core'dan gelen state geçiş çağrısı
        /// new_state: SCV_SYMBOL("Funded") olarak gelir
        pub fn update_state(_env: Env, _nullifier: BytesN<32>, _new_state: Symbol) {}
    }
}

#[cfg(test)]
pub mod mock_risk_oracle {
    use soroban_sdk::{contract, contractimpl, Env};

    #[contract]
    pub struct MockRiskOracle;

    #[contractimpl]
    impl MockRiskOracle {
        // risk-oracle şu an lumina-core'dan doğrudan çağrılmıyor;
        // bu mock yalnızca initialize'a geçerli bir Address sağlamak için vardır.
        pub fn ping(_env: Env) {}
    }
}

// ── Unit testler ──────────────────────────────────────────────────────────────
//
// NOT: factor_invoice APR bantları (basis points):
//   score  0–20  →  500 bps
//   score 21–40  →  800 bps
//   score 41–60  → 1200 bps
//   score 61–80  → 1800 bps
//   score 81–100 → 2500 bps
//
// Avans hesabı (test env timestamp=0, due=+1 gün → days=1, amount=1_000_000):
//   500 bps  → discount = 1_000_000×500×1 / 3_650_000 = 136  → advance = 999_864
//   800 bps  → discount = 219  → advance = 999_781
//  1200 bps  → discount = 328  → advance = 999_672
//  1800 bps  → discount = 493  → advance = 999_507
//  2500 bps  → discount = 684  → advance = 999_316

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Env, Address};

    // ── Test kurulumu ─────────────────────────────────────────────────────────

    struct TestFixture {
        env:              Env,
        contract_id:      Address,
        admin:            Address,
        pool_addr:        Address,
        nullifier_reg_addr: Address,
        risk_oracle_addr: Address,
    }

    impl TestFixture {
        fn new() -> Self {
            let env = Env::default();
            env.mock_all_auths();

            let contract_id = env.register(LuminaCore {}, ());

            // Mock kontratları kaydet
            let pool_id          = env.register(mock_pool::MockPool {}, ());
            let nullifier_reg_id = env.register(mock_nullifier_registry::MockNullifierRegistry {}, ());
            let risk_oracle_id   = env.register(mock_risk_oracle::MockRiskOracle {}, ());

            let admin = Address::generate(&env);
            LuminaCoreClient::new(&env, &contract_id).initialize(
                &admin,
                &nullifier_reg_id,
                &risk_oracle_id,
                &pool_id,
            );

            Self {
                env,
                contract_id,
                admin,
                pool_addr:          pool_id,
                nullifier_reg_addr: nullifier_reg_id,
                risk_oracle_addr:   risk_oracle_id,
            }
        }

        fn client(&self) -> LuminaCoreClient<'_> {
            LuminaCoreClient::new(&self.env, &self.contract_id)
        }

        /// 1 günlük vadeyle geçerli bir invoice submit eder; (id, company, debtor) döndürür.
        fn submit_default(&self) -> (u64, Address, Address) {
            self.submit_with_days(1, 1)
        }

        /// `days` gün vadeyle, verilen `hash_byte` ile invoice submit eder.
        fn submit_with_days(&self, days: u64, hash_byte: u8) -> (u64, Address, Address) {
            let company  = Address::generate(&self.env);
            let debtor   = Address::generate(&self.env);
            let hash     = BytesN::from_array(&self.env, &[hash_byte; 32]);
            let due      = self.env.ledger().timestamp() + days * 86_400;
            let nullifier = BytesN::from_array(&self.env, &[hash_byte.wrapping_add(128); 32]);

            let id = self.client().submit_invoice(
                &company, &hash, &1_000_000_i128, &debtor, &due, &nullifier,
            );

            (id, company, debtor)
        }
    }

    // ── initialize ────────────────────────────────────────────────────────────

    #[test]
    fn test_initialize_sets_admin() {
        let f = TestFixture::new();
        assert_eq!(f.client().get_admin(), Some(f.admin));
    }

    #[test]
    fn test_initialize_twice_fails() {
        let f = TestFixture::new();
        let result = f.client().try_initialize(
            &f.admin,
            &f.nullifier_reg_addr,
            &f.risk_oracle_addr,
            &f.pool_addr,
        );
        assert_eq!(
            result.unwrap_err().unwrap(),
            ContractError::AlreadyInitialized
        );
    }

    #[test]
    fn test_initial_total_disbursed_is_zero() {
        let f = TestFixture::new();
        assert_eq!(f.client().get_total_disbursed(), 0);
    }

    // ── submit_invoice ────────────────────────────────────────────────────────

    #[test]
    fn test_submit_returns_incrementing_ids() {
        let f = TestFixture::new();
        let company  = Address::generate(&f.env);
        let debtor   = Address::generate(&f.env);
        let due      = f.env.ledger().timestamp() + 3_600;
        let nul1     = BytesN::from_array(&f.env, &[1u8; 32]);
        let nul2     = BytesN::from_array(&f.env, &[2u8; 32]);

        let id1 = f.client().submit_invoice(
            &company,
            &BytesN::from_array(&f.env, &[1u8; 32]),
            &100_i128, &debtor, &due, &nul1,
        );

        let id2 = f.client().submit_invoice(
            &company,
            &BytesN::from_array(&f.env, &[2u8; 32]),
            &200_i128, &debtor, &due, &nul2,
        );

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn test_submit_stores_pending_state() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        assert_eq!(f.client().get_state(&id), Some(InvoiceState::Pending));
    }

    #[test]
    fn test_submit_stores_invoice_fields() {
        let f = TestFixture::new();
        let company  = Address::generate(&f.env);
        let debtor   = Address::generate(&f.env);
        let hash     = BytesN::from_array(&f.env, &[77u8; 32]);
        let due      = f.env.ledger().timestamp() + 7_200;
        let nullifier = BytesN::from_array(&f.env, &[0u8; 32]);

        let id = f.client().submit_invoice(
            &company, &hash, &500_000_i128, &debtor, &due, &nullifier,
        );

        let inv = f.client().get_invoice(&id).unwrap();
        assert_eq!(inv.amount, 500_000);
        assert_eq!(inv.due_date, due);
        assert_eq!(inv.state, InvoiceState::Pending);
        assert_eq!(inv.advance_amount, 0);
        assert_eq!(inv.apr_bps, 0);
    }

    #[test]
    fn test_submit_zero_amount_fails() {
        let f = TestFixture::new();
        let due = f.env.ledger().timestamp() + 3_600;
        let nul = BytesN::from_array(&f.env, &[0u8; 32]);
        let result = f.client().try_submit_invoice(
            &Address::generate(&f.env),
            &BytesN::from_array(&f.env, &[1u8; 32]),
            &0_i128,
            &Address::generate(&f.env),
            &due,
            &nul,
        );
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidAmount);
    }

    #[test]
    fn test_submit_negative_amount_fails() {
        let f = TestFixture::new();
        let due = f.env.ledger().timestamp() + 3_600;
        let nul = BytesN::from_array(&f.env, &[0u8; 32]);
        let result = f.client().try_submit_invoice(
            &Address::generate(&f.env),
            &BytesN::from_array(&f.env, &[1u8; 32]),
            &(-1_i128),
            &Address::generate(&f.env),
            &due,
            &nul,
        );
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidAmount);
    }

    #[test]
    fn test_submit_past_due_date_fails() {
        let f = TestFixture::new();
        let nul = BytesN::from_array(&f.env, &[0u8; 32]);
        let result = f.client().try_submit_invoice(
            &Address::generate(&f.env),
            &BytesN::from_array(&f.env, &[1u8; 32]),
            &100_i128,
            &Address::generate(&f.env),
            &0_u64, // due_date ≤ timestamp (0 ≤ 0)
            &nul,
        );
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidDueDate);
    }

    #[test]
    fn test_submit_duplicate_hash_fails() {
        let f = TestFixture::new();
        let company  = Address::generate(&f.env);
        let debtor   = Address::generate(&f.env);
        let hash     = BytesN::from_array(&f.env, &[99u8; 32]);
        let due      = f.env.ledger().timestamp() + 3_600;
        let nul      = BytesN::from_array(&f.env, &[0u8; 32]);

        f.client().submit_invoice(&company, &hash, &100_i128, &debtor, &due, &nul);

        let result = f.client().try_submit_invoice(&company, &hash, &100_i128, &debtor, &due, &nul);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::DuplicateHash);
    }

    // ── factor_invoice — APR bant testleri ────────────────────────────────────

    #[test]
    fn test_factor_band_0_to_20_gives_500bps() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        let r = f.client().factor_invoice(&id, &10);
        assert_eq!(r.apr_bps, 500);
        assert_eq!(r.risk_score, 10);
        assert_eq!(r.advance_amount, 999_864);
    }

    #[test]
    fn test_factor_band_21_to_40_gives_800bps() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        let r = f.client().factor_invoice(&id, &30);
        assert_eq!(r.apr_bps, 800);
        assert_eq!(r.advance_amount, 999_781);
    }

    #[test]
    fn test_factor_band_41_to_60_gives_1200bps() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        let r = f.client().factor_invoice(&id, &50);
        assert_eq!(r.apr_bps, 1200);
        assert_eq!(r.advance_amount, 999_672);
    }

    #[test]
    fn test_factor_band_61_to_80_gives_1800bps() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        let r = f.client().factor_invoice(&id, &70);
        assert_eq!(r.apr_bps, 1800);
        assert_eq!(r.advance_amount, 999_507);
    }

    #[test]
    fn test_factor_band_81_to_100_gives_2500bps() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        let r = f.client().factor_invoice(&id, &90);
        assert_eq!(r.apr_bps, 2500);
        assert_eq!(r.advance_amount, 999_316);
    }

    #[test]
    fn test_factor_score_0_uses_500bps() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        let r = f.client().factor_invoice(&id, &0);
        assert_eq!(r.apr_bps, 500);
        assert_eq!(r.advance_amount, 999_864);
    }

    #[test]
    fn test_factor_score_100_uses_2500bps() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        let r = f.client().factor_invoice(&id, &100);
        assert_eq!(r.apr_bps, 2500);
        assert_eq!(r.advance_amount, 999_316);
    }

    // ── APR bant sınır testleri ───────────────────────────────────────────────

    #[test]
    fn test_factor_band_boundary_20_vs_21() {
        let f = TestFixture::new();
        let (id1, _, _) = f.submit_with_days(1, 20);
        let r1 = f.client().factor_invoice(&id1, &20);
        assert_eq!(r1.apr_bps, 500);

        let (id2, _, _) = f.submit_with_days(1, 21);
        let r2 = f.client().factor_invoice(&id2, &21);
        assert_eq!(r2.apr_bps, 800);
    }

    #[test]
    fn test_factor_band_boundary_40_vs_41() {
        let f = TestFixture::new();
        let (id1, _, _) = f.submit_with_days(1, 40);
        let r1 = f.client().factor_invoice(&id1, &40);
        assert_eq!(r1.apr_bps, 800);

        let (id2, _, _) = f.submit_with_days(1, 41);
        let r2 = f.client().factor_invoice(&id2, &41);
        assert_eq!(r2.apr_bps, 1200);
    }

    #[test]
    fn test_factor_band_boundary_60_vs_61() {
        let f = TestFixture::new();
        let (id1, _, _) = f.submit_with_days(1, 60);
        let r1 = f.client().factor_invoice(&id1, &60);
        assert_eq!(r1.apr_bps, 1200);

        let (id2, _, _) = f.submit_with_days(1, 61);
        let r2 = f.client().factor_invoice(&id2, &61);
        assert_eq!(r2.apr_bps, 1800);
    }

    #[test]
    fn test_factor_band_boundary_80_vs_81() {
        let f = TestFixture::new();
        let (id1, _, _) = f.submit_with_days(1, 80);
        let r1 = f.client().factor_invoice(&id1, &80);
        assert_eq!(r1.apr_bps, 1800);

        let (id2, _, _) = f.submit_with_days(1, 81);
        let r2 = f.client().factor_invoice(&id2, &81);
        assert_eq!(r2.apr_bps, 2500);
    }

    // ── Zaman bazlı iskonto testi ─────────────────────────────────────────────

    #[test]
    fn test_factor_time_based_discount_30_days() {
        let f = TestFixture::new();
        // 30 gün: score=50 → apr=1200bps
        // discount = 1_000_000 × 1200 × 30 / 3_650_000 = 9_863
        let (id, _, _) = f.submit_with_days(30, 99);
        let r = f.client().factor_invoice(&id, &50);
        assert_eq!(r.apr_bps, 1200);
        assert_eq!(r.advance_amount, 990_137);
    }

    #[test]
    fn test_factor_time_based_discount_90_days() {
        let f = TestFixture::new();
        // 90 gün: score=0 → apr=500bps
        // discount = 1_000_000 × 500 × 90 / 3_650_000 = 12_328
        let (id, _, _) = f.submit_with_days(90, 98);
        let r = f.client().factor_invoice(&id, &0);
        assert_eq!(r.apr_bps, 500);
        assert_eq!(r.advance_amount, 987_672);
    }

    // ── Invoice alanları ──────────────────────────────────────────────────────

    #[test]
    fn test_factor_stores_advance_amount_and_apr() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        f.client().factor_invoice(&id, &0);
        let inv = f.client().get_invoice(&id).unwrap();
        assert_eq!(inv.advance_amount, 999_864);
        assert_eq!(inv.apr_bps, 500);
        assert_eq!(inv.risk_score, 0);
    }

    #[test]
    fn test_factor_changes_state_to_funded() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        f.client().factor_invoice(&id, &50);
        assert_eq!(f.client().get_state(&id), Some(InvoiceState::Funded));
    }

    #[test]
    fn test_factor_invalid_risk_score_fails() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        let result = f.client().try_factor_invoice(&id, &101);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidRiskScore);
    }

    #[test]
    fn test_factor_nonexistent_invoice_fails() {
        let f = TestFixture::new();
        let result = f.client().try_factor_invoice(&999, &50);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvoiceNotFound);
    }

    #[test]
    fn test_factor_already_funded_fails() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        f.client().factor_invoice(&id, &50);
        let result = f.client().try_factor_invoice(&id, &50);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidState);
    }

    // ── TotalDisbursed sayacı ─────────────────────────────────────────────────

    #[test]
    fn test_total_disbursed_accumulates() {
        let f = TestFixture::new();
        let company  = Address::generate(&f.env);
        let debtor   = Address::generate(&f.env);
        let due      = f.env.ledger().timestamp() + 3_600;

        let id1 = f.client().submit_invoice(
            &company, &BytesN::from_array(&f.env, &[1u8; 32]),
            &1_000_000_i128, &debtor, &due,
            &BytesN::from_array(&f.env, &[11u8; 32]),
        );
        let id2 = f.client().submit_invoice(
            &company, &BytesN::from_array(&f.env, &[2u8; 32]),
            &2_000_000_i128, &debtor, &due,
            &BytesN::from_array(&f.env, &[22u8; 32]),
        );

        f.client().factor_invoice(&id1, &0);
        f.client().factor_invoice(&id2, &0);

        assert_eq!(f.client().get_total_disbursed(), 3_000_000);
    }

    // ── repay ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_repay_funded_invoice_returns_true() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        f.client().factor_invoice(&id, &50);
        let result = f.client().repay(&id);
        assert!(result);
    }

    #[test]
    fn test_repay_changes_state_to_repaid() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        f.client().factor_invoice(&id, &50);
        f.client().repay(&id);
        assert_eq!(f.client().get_state(&id), Some(InvoiceState::Repaid));
    }

    #[test]
    fn test_repay_pending_invoice_returns_false() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        let result = f.client().repay(&id);
        assert!(!result);
        assert_eq!(f.client().get_state(&id), Some(InvoiceState::Pending));
    }

    #[test]
    fn test_repay_nonexistent_returns_false() {
        let f = TestFixture::new();
        let result = f.client().repay(&9999);
        assert!(!result);
    }

    #[test]
    fn test_repay_twice_second_returns_false() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        f.client().factor_invoice(&id, &50);
        f.client().repay(&id);
        let result = f.client().repay(&id);
        assert!(!result);
    }

    // ── mark_defaulted ────────────────────────────────────────────────────────

    #[test]
    fn test_mark_defaulted_funded_invoice() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        f.client().factor_invoice(&id, &80);
        f.client().mark_defaulted(&id);
        assert_eq!(f.client().get_state(&id), Some(InvoiceState::Defaulted));
    }

    #[test]
    fn test_mark_defaulted_pending_fails() {
        let f = TestFixture::new();
        let (id, _, _) = f.submit_default();
        let result = f.client().try_mark_defaulted(&id);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidState);
    }

    #[test]
    fn test_mark_defaulted_nonexistent_fails() {
        let f = TestFixture::new();
        let result = f.client().try_mark_defaulted(&999);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvoiceNotFound);
    }
}
