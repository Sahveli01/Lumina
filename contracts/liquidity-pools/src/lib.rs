#![no_std]

//! # Liquidity Pools + On-Chain Insurance Primitive
//!
//! Senior/Junior tranche modeli ile invoice faktoring likidite yönetimi,
//! otomatik temerrüt koruması ve Risk NFT temsili.
//!
//! ## Tranche Modeli
//! - **`"senior"`** : Düşük risk. Geri ödemelerde önce senior havuzu doldurulur.
//!   Kayıplarda sigorta rezervi devreye girer.
//! - **`"junior"`** : Yüksek risk, yüksek getiri. İlk zarar tampon havuzu.
//!
//! ## Insurance Reserve
//! Her faktoring işleminden %1.5 prim tahsil edilir.
//! Temerrüt gerçekleştiğinde rezerv senior depositor'ları korur.
//!
//! ## Risk NFT (RiskPosition)
//! Her depositor'ın pozisyonu on-chain tutulur:
//! tranche, tutar, giriş zamanı ve o andaki havuz risk skoru.
//!
//! ## Fon Akışı
//! ```text
//! Depositor ──deposit()──► Pool
//!                               │
//!                         factor_invoice
//!                               │
//!                    collect_premium() ──► InsuranceReserve
//!                               │
//!                          disburse() ──► Company (avans)
//!                               │
//!                      Debtor repay()
//!                               │
//!                    record_repayment() ◄── lumina-core
//!
//! Temerrüt:
//!                    trigger_default_protection()
//!                    InsuranceReserve ──► TotalDeposits(senior)
//! ```

use soroban_sdk::{
    contract, contractimpl, contracttype, contracterror,
    Address, Env, Symbol, token,
};

// ── Sabitler ──────────────────────────────────────────────────────────────────

const TTL_THRESHOLD: u32 = 259_200;   // ~15 gün
const TTL_EXTEND: u32    = 3_110_400; // ~180 gün

/// Prim oranı: %1.5 = 150 / 10_000
const PREMIUM_NUMERATOR: i128   = 150;
const PREMIUM_DENOMINATOR: i128 = 10_000;

/// Başlangıç pool risk skoru (0-100)
const DEFAULT_POOL_RISK_SCORE: u32 = 50;

// ── Hata kodları ──────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ContractError {
    AlreadyInitialized   = 1,
    NotInitialized       = 2,
    Unauthorized         = 3,
    InvalidTranche       = 4, // "senior" veya "junior" dışı
    ZeroAmount           = 5,
    InsufficientBalance  = 6, // kullanıcı bakiyesi yetersiz
    InsufficientPool     = 7, // havuzda yeterli likidite yok
    InsufficientInsurance= 8, // sigorta rezervi yetersiz
    InvalidScore         = 9, // risk skoru 0-100 aralığı dışı
}

// ── Veri tipleri ──────────────────────────────────────────────────────────────

/// Bir depositor'ın havuzdaki risk pozisyonu (on-chain Risk NFT)
#[contracttype]
#[derive(Clone, Debug)]
pub struct RiskPosition {
    /// Pozisyon sahibi
    pub depositor:        Address,
    /// Tranche ("senior" / "junior")
    pub tranche:          Symbol,
    /// Toplam yatırılan miktar
    pub deposited_amount: i128,
    /// İlk deposit zamanı (Unix timestamp)
    pub entry_timestamp:  u64,
    /// Deposit anındaki ağırlıklı ortalama pool risk skoru
    pub risk_tier:        u32,
}

// ── Storage anahtarları ───────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    /// Admin adresi
    Admin,
    /// lumina-core kontrat adresi (disburse/record_repayment/collect_premium çağırabilir)
    LuminaCore,
    /// USDC stablecoin token adresi
    Stablecoin,
    /// TotalDeposits(tranche_sym) → i128
    TotalDeposits(Symbol),
    /// TotalDeployed — tüm tranche'larda toplam deploy edilen miktar
    TotalDeployed,
    /// Balance(tranche_sym, depositor_addr) → i128
    Balance(Symbol, Address),
    /// Birikmiş sigorta rezervi (primlerden)
    InsuranceReserve,
    /// Havuzun mevcut ortalama risk skoru (0-100)
    PoolRiskScore,
    /// Depositor risk pozisyonu: RiskPosition(depositor, tranche) → RiskPosition
    RiskPosition(Address, Symbol),
}

// ── Kontrat ───────────────────────────────────────────────────────────────────

#[contract]
pub struct LiquidityPools;

#[contractimpl]
impl LiquidityPools {
    // ── Yönetim ──────────────────────────────────────────────────────────────

    /// Kontratı başlatır. Yalnızca bir kez çağrılabilir.
    pub fn initialize(
        env: Env,
        admin: Address,
        lumina_core: Address,
        stablecoin: Address,
    ) -> Result<(), ContractError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(ContractError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::LuminaCore, &lumina_core);
        env.storage().instance().set(&DataKey::Stablecoin, &stablecoin);
        env.storage().instance().set(&DataKey::TotalDeployed, &0_i128);
        env.storage().instance().set(&DataKey::InsuranceReserve, &0_i128);
        env.storage().instance().set(&DataKey::PoolRiskScore, &DEFAULT_POOL_RISK_SCORE);

        let senior = sym_senior(&env);
        let junior = sym_junior(&env);
        env.storage().instance().set(&DataKey::TotalDeposits(senior), &0_i128);
        env.storage().instance().set(&DataKey::TotalDeposits(junior), &0_i128);

        env.events()
            .publish((Symbol::new(&env, "initialized"),), admin);
        Ok(())
    }

    // ── Temel LP fonksiyonları ────────────────────────────────────────────────

    /// Stablecoin yatırır, tranche bakiyesini ve RiskPosition'ı günceller.
    ///
    /// # RiskPosition
    /// İlk depozitoda: risk_tier = mevcut pool_risk_score, entry_timestamp = now.
    /// Sonraki depozitolarda: risk_tier ağırlıklı ortalama güncellenir.
    pub fn deposit(
        env: Env,
        tranche: Symbol,
        amount: i128,
        depositor: Address,
    ) -> Result<(), ContractError> {
        depositor.require_auth();
        Self::validate_tranche(&env, &tranche)?;

        if amount <= 0 {
            return Err(ContractError::ZeroAmount);
        }

        // Tokeni depositor'dan kontrata transfer et
        let stablecoin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Stablecoin)
            .unwrap();
        token::Client::new(&env, &stablecoin).transfer(
            &depositor,
            &env.current_contract_address(),
            &amount,
        );

        // Kullanıcı bakiyesini güncelle
        let bal_key = DataKey::Balance(tranche.clone(), depositor.clone());
        let prev: i128 = env.storage().persistent().get(&bal_key).unwrap_or(0);
        env.storage().persistent().set(&bal_key, &(prev + amount));
        env.storage().persistent().extend_ttl(&bal_key, TTL_THRESHOLD, TTL_EXTEND);

        // Tranche toplamını güncelle
        let total_key = DataKey::TotalDeposits(tranche.clone());
        let total: i128 = env.storage().instance().get(&total_key).unwrap_or(0);
        env.storage().instance().set(&total_key, &(total + amount));

        // RiskPosition oluştur / güncelle
        let pool_risk: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PoolRiskScore)
            .unwrap_or(DEFAULT_POOL_RISK_SCORE);

        let pos_key = DataKey::RiskPosition(depositor.clone(), tranche.clone());
        let new_pos: RiskPosition = match env
            .storage()
            .persistent()
            .get::<DataKey, RiskPosition>(&pos_key)
        {
            None => RiskPosition {
                depositor: depositor.clone(),
                tranche: tranche.clone(),
                deposited_amount: amount,
                entry_timestamp: env.ledger().timestamp(),
                risk_tier: pool_risk,
            },
            Some(pos) => {
                // Ağırlıklı ortalama risk_tier hesabı
                let total_amount = pos.deposited_amount + amount;
                let weighted_tier = if total_amount > 0 {
                    ((pos.deposited_amount * pos.risk_tier as i128
                        + amount * pool_risk as i128)
                        / total_amount) as u32
                } else {
                    pool_risk
                };
                RiskPosition {
                    depositor: depositor.clone(),
                    tranche: tranche.clone(),
                    deposited_amount: total_amount,
                    entry_timestamp: pos.entry_timestamp, // ilk giriş zamanı korunur
                    risk_tier: weighted_tier,
                }
            }
        };
        env.storage().persistent().set(&pos_key, &new_pos);
        env.storage()
            .persistent()
            .extend_ttl(&pos_key, TTL_THRESHOLD, TTL_EXTEND);

        env.events().publish(
            (Symbol::new(&env, "deposited"), depositor, tranche),
            amount,
        );
        Ok(())
    }

    /// Tranche'dan stablecoin çeker.
    pub fn withdraw(
        env: Env,
        tranche: Symbol,
        amount: i128,
        depositor: Address,
    ) -> Result<(), ContractError> {
        depositor.require_auth();
        Self::validate_tranche(&env, &tranche)?;

        if amount <= 0 {
            return Err(ContractError::ZeroAmount);
        }

        let bal_key = DataKey::Balance(tranche.clone(), depositor.clone());
        let balance: i128 = env.storage().persistent().get(&bal_key).unwrap_or(0);
        if balance < amount {
            return Err(ContractError::InsufficientBalance);
        }

        let available = Self::available_liquidity(&env);
        if available < amount {
            return Err(ContractError::InsufficientPool);
        }

        let stablecoin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Stablecoin)
            .unwrap();
        token::Client::new(&env, &stablecoin).transfer(
            &env.current_contract_address(),
            &depositor,
            &amount,
        );

        env.storage().persistent().set(&bal_key, &(balance - amount));
        env.storage().persistent().extend_ttl(&bal_key, TTL_THRESHOLD, TTL_EXTEND);

        let total_key = DataKey::TotalDeposits(tranche.clone());
        let total: i128 = env.storage().instance().get(&total_key).unwrap_or(0);
        env.storage().instance().set(&total_key, &(total - amount));

        env.events().publish(
            (Symbol::new(&env, "withdrawn"), depositor, tranche),
            amount,
        );
        Ok(())
    }

    // ── View fonksiyonları ────────────────────────────────────────────────────

    pub fn get_balance(env: Env, tranche: Symbol, depositor: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Balance(tranche, depositor))
            .unwrap_or(0)
    }

    pub fn get_total(env: Env, tranche: Symbol) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalDeposits(tranche))
            .unwrap_or(0)
    }

    pub fn get_available(env: Env) -> i128 {
        Self::available_liquidity(&env)
    }

    pub fn get_total_deployed(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalDeployed)
            .unwrap_or(0)
    }

    pub fn get_risk_position(env: Env, depositor: Address, tranche: Symbol) -> Option<RiskPosition> {
        env.storage()
            .persistent()
            .get(&DataKey::RiskPosition(depositor, tranche))
    }

    // ── Insurance Primitive ───────────────────────────────────────────────────

    /// Her faktoring işleminden %1.5 sigorta primi tahsil eder.
    ///
    /// `premium = invoice_amount × 150 / 10_000`
    ///
    /// Prim, sigorta rezervine eklenir (muhasebe kaydı — token muhasebe dahilinde).
    ///
    /// # Yetkilendirme
    /// Yalnızca lumina-core çağırabilir.
    ///
    /// # Döndürür
    /// Hesaplanan prim miktarı.
    pub fn collect_premium(
        env: Env,
        _invoice_id: u64,
        invoice_amount: i128,
    ) -> Result<i128, ContractError> {
        Self::require_lumina_core(&env)?;

        if invoice_amount <= 0 {
            return Err(ContractError::ZeroAmount);
        }

        let premium = invoice_amount * PREMIUM_NUMERATOR / PREMIUM_DENOMINATOR;

        let reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::InsuranceReserve)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::InsuranceReserve, &(reserve + premium));

        env.events().publish(
            (Symbol::new(&env, "premium_collected"),),
            (_invoice_id, premium),
        );

        Ok(premium)
    }

    /// Mevcut sigorta rezervini döndürür.
    pub fn get_insurance_reserve(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::InsuranceReserve)
            .unwrap_or(0)
    }

    /// Temerrüt koruma mekanizmasını tetikler.
    ///
    /// Sigorta rezervinden `senior_loss` miktarı çekilerek senior havuzu
    /// kredilendirilir (depositor'lara orantılı etki).
    ///
    /// # Kontroller
    /// - `insurance_reserve >= senior_loss`
    ///
    /// # Yetkilendirme
    /// Yalnızca admin çağırabilir.
    pub fn trigger_default_protection(
        env: Env,
        _invoice_id: u64,
        senior_loss: i128,
    ) -> Result<(), ContractError> {
        Self::require_admin(&env)?;

        if senior_loss <= 0 {
            return Err(ContractError::ZeroAmount);
        }

        let reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::InsuranceReserve)
            .unwrap_or(0);

        if reserve < senior_loss {
            return Err(ContractError::InsufficientInsurance);
        }

        // Rezervden düş
        env.storage()
            .instance()
            .set(&DataKey::InsuranceReserve, &(reserve - senior_loss));

        // Senior havuzunu kredilendirr (depositor'lar orantılı kazanır)
        let senior = sym_senior(&env);
        let senior_total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalDeposits(senior.clone()))
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalDeposits(senior), &(senior_total + senior_loss));

        env.events().publish(
            (Symbol::new(&env, "default_protected"),),
            (_invoice_id, senior_loss),
        );

        Ok(())
    }

    // ── Pool Risk Score ───────────────────────────────────────────────────────

    /// Havuzun ortalama risk skorunu günceller.
    ///
    /// Backend, her faktoring işleminden sonra bu endpoint'i çağırır.
    ///
    /// # Yetkilendirme
    /// Yalnızca admin çağırabilir.
    pub fn update_pool_risk_score(env: Env, new_score: u32) -> Result<(), ContractError> {
        Self::require_admin(&env)?;

        if new_score > 100 {
            return Err(ContractError::InvalidScore);
        }

        env.storage()
            .instance()
            .set(&DataKey::PoolRiskScore, &new_score);

        env.events().publish(
            (Symbol::new(&env, "risk_score_upd"),),
            new_score,
        );

        Ok(())
    }

    pub fn get_pool_risk_score(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::PoolRiskScore)
            .unwrap_or(DEFAULT_POOL_RISK_SCORE)
    }

    // ── lumina-core'a özel sistem fonksiyonları ───────────────────────────────

    /// `lumina-core::factor_invoice` tarafından çağrılır.
    pub fn disburse(
        env: Env,
        recipient: Address,
        amount: i128,
    ) -> Result<(), ContractError> {
        Self::require_lumina_core(&env)?;

        if amount <= 0 {
            return Err(ContractError::ZeroAmount);
        }

        let available = Self::available_liquidity(&env);
        if available < amount {
            return Err(ContractError::InsufficientPool);
        }

        let deployed: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalDeployed)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalDeployed, &(deployed + amount));

        let stablecoin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Stablecoin)
            .unwrap();
        token::Client::new(&env, &stablecoin).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events().publish(
            (Symbol::new(&env, "disbursed"), recipient),
            amount,
        );
        Ok(())
    }

    /// `lumina-core::repay` sonrası çağrılır.
    pub fn record_repayment(env: Env, amount: i128) -> Result<(), ContractError> {
        Self::require_lumina_core(&env)?;

        if amount <= 0 {
            return Err(ContractError::ZeroAmount);
        }

        let deployed: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalDeployed)
            .unwrap_or(0);

        let new_deployed = if amount > deployed { 0 } else { deployed - amount };
        env.storage()
            .instance()
            .set(&DataKey::TotalDeployed, &new_deployed);

        env.events()
            .publish((Symbol::new(&env, "repayment_rec"),), amount);
        Ok(())
    }

    // ── İç yardımcılar ───────────────────────────────────────────────────────

    fn validate_tranche(env: &Env, tranche: &Symbol) -> Result<(), ContractError> {
        if tranche == &sym_senior(env) || tranche == &sym_junior(env) {
            Ok(())
        } else {
            Err(ContractError::InvalidTranche)
        }
    }

    /// Tüm tranche'lardaki toplam mevduat − deployed capital
    fn available_liquidity(env: &Env) -> i128 {
        let senior_total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalDeposits(sym_senior(env)))
            .unwrap_or(0);
        let junior_total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalDeposits(sym_junior(env)))
            .unwrap_or(0);
        let deployed: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalDeployed)
            .unwrap_or(0);
        senior_total + junior_total - deployed
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

    fn require_lumina_core(env: &Env) -> Result<(), ContractError> {
        let core: Address = env
            .storage()
            .instance()
            .get(&DataKey::LuminaCore)
            .ok_or(ContractError::NotInitialized)?;
        core.require_auth();
        Ok(())
    }
}

// ── Symbol üretici yardımcılar ────────────────────────────────────────────────

fn sym_senior(env: &Env) -> Symbol {
    Symbol::new(env, "senior")
}

fn sym_junior(env: &Env) -> Symbol {
    Symbol::new(env, "junior")
}

// ── Test ortamı için mock kontratlar ──────────────────────────────────────────

#[cfg(test)]
pub mod mock_token {
    use soroban_sdk::{contract, contractimpl, Env, Address};

    #[contract]
    pub struct MockToken;

    #[contractimpl]
    impl MockToken {
        pub fn transfer(_env: Env, _from: Address, _to: Address, _amount: i128) {}
    }
}

// ── Unit testler ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Env, Address, Symbol};

    // ── Yardımcı kurulum ──────────────────────────────────────────────────────

    struct Fixture {
        env:         Env,
        contract_id: Address,
        admin:       Address,
        core:        Address,
        token_addr:  Address,
    }

    impl Fixture {
        fn new() -> Self {
            let env = Env::default();
            env.mock_all_auths();

            let contract_id = env.register(LiquidityPools {}, ());
            let admin = Address::generate(&env);
            let core  = Address::generate(&env);
            let tok   = env.register(mock_token::MockToken {}, ());

            LiquidityPoolsClient::new(&env, &contract_id).initialize(&admin, &core, &tok);

            Self { env, contract_id, admin, core, token_addr: tok }
        }

        fn client(&self) -> LiquidityPoolsClient<'_> {
            LiquidityPoolsClient::new(&self.env, &self.contract_id)
        }

        fn senior(&self) -> Symbol { Symbol::new(&self.env, "senior") }
        fn junior(&self) -> Symbol { Symbol::new(&self.env, "junior") }
    }

    // ── initialize ────────────────────────────────────────────────────────────

    #[test]
    fn test_initialize_sets_zero_totals() {
        let f = Fixture::new();
        assert_eq!(f.client().get_total(&f.senior()), 0);
        assert_eq!(f.client().get_total(&f.junior()), 0);
        assert_eq!(f.client().get_total_deployed(), 0);
        assert_eq!(f.client().get_available(), 0);
    }

    #[test]
    fn test_initialize_sets_zero_insurance_reserve() {
        let f = Fixture::new();
        assert_eq!(f.client().get_insurance_reserve(), 0);
    }

    #[test]
    fn test_initialize_sets_default_pool_risk_score() {
        let f = Fixture::new();
        assert_eq!(f.client().get_pool_risk_score(), 50);
    }

    #[test]
    fn test_initialize_twice_fails() {
        let f = Fixture::new();
        let result = f.client().try_initialize(&f.admin, &f.core, &f.token_addr);
        assert_eq!(
            result.unwrap_err().unwrap(),
            ContractError::AlreadyInitialized
        );
    }

    // ── deposit ───────────────────────────────────────────────────────────────

    #[test]
    fn test_deposit_senior_updates_balance() {
        let f = Fixture::new();
        let depositor = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &1_000_i128, &depositor);

        assert_eq!(f.client().get_balance(&f.senior(), &depositor), 1_000);
        assert_eq!(f.client().get_total(&f.senior()), 1_000);
    }

    #[test]
    fn test_deposit_junior_updates_balance() {
        let f = Fixture::new();
        let depositor = Address::generate(&f.env);

        f.client().deposit(&f.junior(), &500_i128, &depositor);

        assert_eq!(f.client().get_balance(&f.junior(), &depositor), 500);
        assert_eq!(f.client().get_total(&f.junior()), 500);
    }

    #[test]
    fn test_deposit_senior_and_junior_independent() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &800_i128, &dep);
        f.client().deposit(&f.junior(), &200_i128, &dep);

        assert_eq!(f.client().get_balance(&f.senior(), &dep), 800);
        assert_eq!(f.client().get_balance(&f.junior(), &dep), 200);
        assert_eq!(f.client().get_available(), 1_000);
    }

    #[test]
    fn test_deposit_accumulates_for_same_depositor() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &300_i128, &dep);
        f.client().deposit(&f.senior(), &700_i128, &dep);

        assert_eq!(f.client().get_balance(&f.senior(), &dep), 1_000);
        assert_eq!(f.client().get_total(&f.senior()), 1_000);
    }

    #[test]
    fn test_deposit_zero_fails() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);
        let result = f.client().try_deposit(&f.senior(), &0_i128, &dep);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::ZeroAmount);
    }

    #[test]
    fn test_deposit_invalid_tranche_fails() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);
        let bad = Symbol::new(&f.env, "mezzanine");
        let result = f.client().try_deposit(&bad, &100_i128, &dep);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidTranche);
    }

    // ── withdraw ──────────────────────────────────────────────────────────────

    #[test]
    fn test_withdraw_full_balance() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &1_000_i128, &dep);
        f.client().withdraw(&f.senior(), &1_000_i128, &dep);

        assert_eq!(f.client().get_balance(&f.senior(), &dep), 0);
        assert_eq!(f.client().get_total(&f.senior()), 0);
    }

    #[test]
    fn test_withdraw_partial_balance() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &1_000_i128, &dep);
        f.client().withdraw(&f.senior(), &400_i128, &dep);

        assert_eq!(f.client().get_balance(&f.senior(), &dep), 600);
        assert_eq!(f.client().get_total(&f.senior()), 600);
    }

    #[test]
    fn test_withdraw_more_than_balance_fails() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &500_i128, &dep);
        let result = f.client().try_withdraw(&f.senior(), &501_i128, &dep);
        assert_eq!(
            result.unwrap_err().unwrap(),
            ContractError::InsufficientBalance
        );
    }

    #[test]
    fn test_withdraw_zero_fails() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);
        f.client().deposit(&f.senior(), &100_i128, &dep);
        let result = f.client().try_withdraw(&f.senior(), &0_i128, &dep);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::ZeroAmount);
    }

    #[test]
    fn test_withdraw_fails_when_funds_deployed() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);
        let recipient = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &1_000_i128, &dep);
        f.client().disburse(&recipient, &1_000_i128);
        assert_eq!(f.client().get_available(), 0);

        let result = f.client().try_withdraw(&f.senior(), &1_000_i128, &dep);
        assert_eq!(
            result.unwrap_err().unwrap(),
            ContractError::InsufficientPool
        );
    }

    #[test]
    fn test_withdraw_invalid_tranche_fails() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);
        let bad = Symbol::new(&f.env, "mezz");
        let result = f.client().try_withdraw(&bad, &100_i128, &dep);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidTranche);
    }

    // ── collect_premium ───────────────────────────────────────────────────────

    #[test]
    fn test_collect_premium_correct_amount() {
        let f = Fixture::new();
        // 10_000 × 150 / 10_000 = 150
        let premium = f.client().collect_premium(&1_u64, &10_000_i128);
        assert_eq!(premium, 150);
        assert_eq!(f.client().get_insurance_reserve(), 150);
    }

    #[test]
    fn test_collect_premium_large_invoice() {
        let f = Fixture::new();
        // 100_000 × 150 / 10_000 = 1_500
        let premium = f.client().collect_premium(&2_u64, &100_000_i128);
        assert_eq!(premium, 1_500);
    }

    #[test]
    fn test_insurance_reserve_accumulates() {
        let f = Fixture::new();

        f.client().collect_premium(&1_u64, &10_000_i128); // 150
        f.client().collect_premium(&2_u64, &20_000_i128); // 300

        // 150 + 300 = 450
        assert_eq!(f.client().get_insurance_reserve(), 450);
    }

    #[test]
    fn test_collect_premium_zero_invoice_fails() {
        let f = Fixture::new();
        let result = f.client().try_collect_premium(&1_u64, &0_i128);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::ZeroAmount);
    }

    // ── trigger_default_protection ────────────────────────────────────────────

    #[test]
    fn test_trigger_default_protection_reduces_reserve() {
        let f = Fixture::new();

        // Rezerv biriktir
        f.client().collect_premium(&1_u64, &100_000_i128); // 1_500 prim

        let before = f.client().get_insurance_reserve();
        assert_eq!(before, 1_500);

        // Temerrüt korumasını tetikle: 500 senior kaybı
        f.client().trigger_default_protection(&1_u64, &500_i128);

        assert_eq!(f.client().get_insurance_reserve(), 1_000);
    }

    #[test]
    fn test_trigger_default_protection_credits_senior_pool() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        // Senior'a 5_000 yatır
        f.client().deposit(&f.senior(), &5_000_i128, &dep);
        assert_eq!(f.client().get_total(&f.senior()), 5_000);

        // Rezerv biriktir
        f.client().collect_premium(&1_u64, &100_000_i128); // 1_500

        // 300 senior kaybı → senior pool +300
        f.client().trigger_default_protection(&1_u64, &300_i128);

        assert_eq!(f.client().get_total(&f.senior()), 5_300);
        assert_eq!(f.client().get_insurance_reserve(), 1_200);
    }

    #[test]
    fn test_trigger_default_protection_insufficient_reserve_fails() {
        let f = Fixture::new();

        // Çok az rezerv: 10_000 × 150 / 10_000 = 150
        f.client().collect_premium(&1_u64, &10_000_i128);
        assert_eq!(f.client().get_insurance_reserve(), 150);

        // 200 talep et ama rezerv 150 — hata
        let result = f.client().try_trigger_default_protection(&1_u64, &200_i128);
        assert_eq!(
            result.unwrap_err().unwrap(),
            ContractError::InsufficientInsurance
        );
    }

    #[test]
    fn test_trigger_default_protection_exact_reserve_succeeds() {
        let f = Fixture::new();

        f.client().collect_premium(&1_u64, &10_000_i128); // 150 prim

        // Tam rezerv kadar talep
        f.client().trigger_default_protection(&1_u64, &150_i128);

        assert_eq!(f.client().get_insurance_reserve(), 0);
    }

    #[test]
    fn test_trigger_default_protection_zero_amount_fails() {
        let f = Fixture::new();
        f.client().collect_premium(&1_u64, &10_000_i128);
        let result = f.client().try_trigger_default_protection(&1_u64, &0_i128);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::ZeroAmount);
    }

    // ── RiskPosition ──────────────────────────────────────────────────────────

    #[test]
    fn test_risk_position_created_on_first_deposit() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        // Pool risk skoru varsayılan: 50
        f.client().deposit(&f.senior(), &1_000_i128, &dep);

        let pos = f.client().get_risk_position(&dep, &f.senior()).unwrap();
        assert_eq!(pos.deposited_amount, 1_000);
        assert_eq!(pos.risk_tier, 50); // varsayılan pool risk skoru
        assert_eq!(pos.entry_timestamp, f.env.ledger().timestamp());
    }

    #[test]
    fn test_risk_position_none_before_deposit() {
        let f = Fixture::new();
        let stranger = Address::generate(&f.env);
        assert!(f.client().get_risk_position(&stranger, &f.senior()).is_none());
    }

    #[test]
    fn test_risk_position_updated_on_second_deposit_weighted_average() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        // İlk deposit: 1_000, risk=50 (varsayılan)
        f.client().deposit(&f.senior(), &1_000_i128, &dep);

        // Pool risk skorunu güncelle: 70
        f.client().update_pool_risk_score(&70_u32);

        // İkinci deposit: 1_000, risk=70
        // weighted = (1_000×50 + 1_000×70) / 2_000 = 120_000 / 2_000 = 60
        f.client().deposit(&f.senior(), &1_000_i128, &dep);

        let pos = f.client().get_risk_position(&dep, &f.senior()).unwrap();
        assert_eq!(pos.deposited_amount, 2_000);
        assert_eq!(pos.risk_tier, 60); // ağırlıklı ortalama
    }

    #[test]
    fn test_risk_position_senior_and_junior_independent() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &500_i128, &dep);
        f.client().update_pool_risk_score(&80_u32);
        f.client().deposit(&f.junior(), &500_i128, &dep);

        let senior_pos = f.client().get_risk_position(&dep, &f.senior()).unwrap();
        let junior_pos = f.client().get_risk_position(&dep, &f.junior()).unwrap();

        assert_eq!(senior_pos.risk_tier, 50); // 50'de yatırıldı
        assert_eq!(junior_pos.risk_tier, 80); // 80'de yatırıldı
    }

    #[test]
    fn test_risk_position_entry_timestamp_preserved_on_update() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        let ts_first = f.env.ledger().timestamp();
        f.client().deposit(&f.senior(), &1_000_i128, &dep);

        // İkinci deposit (aynı timestamp'te, test env değişmedi)
        f.client().deposit(&f.senior(), &500_i128, &dep);

        let pos = f.client().get_risk_position(&dep, &f.senior()).unwrap();
        assert_eq!(pos.entry_timestamp, ts_first); // orijinal timestamp korunur
    }

    // ── update_pool_risk_score ────────────────────────────────────────────────

    #[test]
    fn test_update_pool_risk_score_success() {
        let f = Fixture::new();

        f.client().update_pool_risk_score(&75_u32);
        assert_eq!(f.client().get_pool_risk_score(), 75);
    }

    #[test]
    fn test_update_pool_risk_score_boundary_zero() {
        let f = Fixture::new();
        f.client().update_pool_risk_score(&0_u32);
        assert_eq!(f.client().get_pool_risk_score(), 0);
    }

    #[test]
    fn test_update_pool_risk_score_boundary_100() {
        let f = Fixture::new();
        f.client().update_pool_risk_score(&100_u32);
        assert_eq!(f.client().get_pool_risk_score(), 100);
    }

    #[test]
    fn test_update_pool_risk_score_above_100_fails() {
        let f = Fixture::new();
        let result = f.client().try_update_pool_risk_score(&101_u32);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::InvalidScore);
    }

    #[test]
    fn test_risk_position_uses_updated_pool_score_on_new_deposit() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);

        // Pool risk skorunu 30'a güncelle
        f.client().update_pool_risk_score(&30_u32);

        // Yeni depositor → risk_tier=30 almalı
        f.client().deposit(&f.senior(), &2_000_i128, &dep);

        let pos = f.client().get_risk_position(&dep, &f.senior()).unwrap();
        assert_eq!(pos.risk_tier, 30);
    }

    // ── get_balance / disburse / record_repayment ─────────────────────────────

    #[test]
    fn test_get_balance_zero_for_new_address() {
        let f = Fixture::new();
        let stranger = Address::generate(&f.env);
        assert_eq!(f.client().get_balance(&f.senior(), &stranger), 0);
        assert_eq!(f.client().get_balance(&f.junior(), &stranger), 0);
    }

    #[test]
    fn test_disburse_reduces_available() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);
        let recipient = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &2_000_i128, &dep);
        f.client().disburse(&recipient, &800_i128);

        assert_eq!(f.client().get_total_deployed(), 800);
        assert_eq!(f.client().get_available(), 1_200);
    }

    #[test]
    fn test_disburse_beyond_available_fails() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);
        let recipient = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &500_i128, &dep);
        let result = f.client().try_disburse(&recipient, &501_i128);
        assert_eq!(
            result.unwrap_err().unwrap(),
            ContractError::InsufficientPool
        );
    }

    #[test]
    fn test_disburse_zero_fails() {
        let f = Fixture::new();
        let recipient = Address::generate(&f.env);
        let result = f.client().try_disburse(&recipient, &0_i128);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::ZeroAmount);
    }

    #[test]
    fn test_record_repayment_reduces_deployed() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);
        let r   = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &1_000_i128, &dep);
        f.client().disburse(&r, &1_000_i128);
        assert_eq!(f.client().get_available(), 0);

        f.client().record_repayment(&1_000_i128);
        assert_eq!(f.client().get_total_deployed(), 0);
        assert_eq!(f.client().get_available(), 1_000);
    }

    #[test]
    fn test_record_repayment_zero_fails() {
        let f = Fixture::new();
        let result = f.client().try_record_repayment(&0_i128);
        assert_eq!(result.unwrap_err().unwrap(), ContractError::ZeroAmount);
    }

    #[test]
    fn test_record_repayment_over_deployed_clamps_to_zero() {
        let f = Fixture::new();
        let dep = Address::generate(&f.env);
        let r   = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &300_i128, &dep);
        f.client().disburse(&r, &300_i128);

        f.client().record_repayment(&400_i128);
        assert_eq!(f.client().get_total_deployed(), 0);
    }

    // ── Tam senaryo: deposit + premium + default ──────────────────────────────

    #[test]
    fn test_full_insurance_lifecycle() {
        let f = Fixture::new();
        let lp = Address::generate(&f.env);
        let company = Address::generate(&f.env);

        // 1. LP likidite sağlar
        f.client().deposit(&f.senior(), &10_000_i128, &lp);
        assert_eq!(f.client().get_available(), 10_000);

        // 2. Faktoring: avans dağıt + prim topla
        f.client().disburse(&company, &5_000_i128);
        // invoice_amount=5_000 → premium=75
        f.client().collect_premium(&1_u64, &5_000_i128);
        assert_eq!(f.client().get_insurance_reserve(), 75);
        assert_eq!(f.client().get_available(), 5_000);

        // 3. Temerrüt: 50 senior kaybı sigorta rezervinden karşılanır
        f.client().trigger_default_protection(&1_u64, &50_i128);
        assert_eq!(f.client().get_insurance_reserve(), 25);
        // Senior havuzu +50 kredilendi
        assert_eq!(f.client().get_total(&f.senior()), 10_050);

        // 4. Risk pozisyonu doğrula
        let pos = f.client().get_risk_position(&lp, &f.senior()).unwrap();
        assert_eq!(pos.deposited_amount, 10_000);
        assert_eq!(pos.risk_tier, 50); // varsayılan pool risk skoru
    }

    #[test]
    fn test_full_lifecycle() {
        let f = Fixture::new();
        let lp1 = Address::generate(&f.env);
        let lp2 = Address::generate(&f.env);
        let company = Address::generate(&f.env);

        f.client().deposit(&f.senior(), &6_000_i128, &lp1);
        f.client().deposit(&f.junior(), &4_000_i128, &lp2);
        assert_eq!(f.client().get_available(), 10_000);

        f.client().disburse(&company, &3_000_i128);
        f.client().disburse(&company, &2_000_i128);
        assert_eq!(f.client().get_total_deployed(), 5_000);
        assert_eq!(f.client().get_available(), 5_000);

        f.client().record_repayment(&3_000_i128);
        assert_eq!(f.client().get_total_deployed(), 2_000);
        assert_eq!(f.client().get_available(), 8_000);

        f.client().withdraw(&f.senior(), &2_000_i128, &lp1);
        assert_eq!(f.client().get_balance(&f.senior(), &lp1), 4_000);
        assert_eq!(f.client().get_total(&f.senior()), 4_000);
        assert_eq!(f.client().get_available(), 6_000);

        f.client().record_repayment(&2_000_i128);
        assert_eq!(f.client().get_total_deployed(), 0);
        assert_eq!(f.client().get_available(), 8_000);
    }
}
