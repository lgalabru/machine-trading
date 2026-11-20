//! Kamino Lending (klend) as the grader reads and calls it: the accounts'
//! layouts, the addresses the program derives, and the instructions a
//! session sends. Laid out from the program's on-chain Anchor IDL, v1.27.0
//! (`8qLKwp1fk8WyqmzarkuMeZEX3AzL4VDSmA2UZTKT2aCJ`); no crate depends on
//! the Solana 2.x crates `klend-interface` needs.
//!
//! Amounts the program keeps as scaled fractions (`*_sf`) are fixed point
//! with 60 fractional bits.

use sha2::{Digest, Sha256};
use solana_pubkey::{Pubkey, pubkey};
use solana_transaction::{AccountMeta, Instruction};

pub const PROGRAM_ID: Pubkey = pubkey!("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD");
pub const FARMS_PROGRAM_ID: Pubkey = pubkey!("FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr");
pub const SCOPE_PROGRAM_ID: Pubkey = pubkey!("HFn8GnPADiny6XqUoWE8uRPPxb29ikn4yTuPa9MF2fWJ");
const INSTRUCTIONS_SYSVAR: Pubkey = pubkey!("Sysvar1nstructions1111111111111111111111111");
const RENT_SYSVAR: Pubkey = pubkey!("SysvarRent111111111111111111111111111111111");
const SYSTEM_PROGRAM: Pubkey = pubkey!("11111111111111111111111111111111");
/// A Scope chain's unused slot.
pub const NO_ENTRY: u16 = u16::MAX;
/// Each reserve's last refresh reports which price checks passed; with all
/// six, borrows and withdrawals go through.
pub const PRICE_STATUS_OK: u8 = 0b11_1111;

const SF: f64 = (1u64 << 60) as f64;

/// The first 8 bytes of an Anchor instruction's data.
pub fn instruction_tag(name: &str) -> [u8; 8] {
    tag(&format!("global:{name}"))
}

/// The first 8 bytes of an Anchor account: what a scan filters on.
pub fn account_tag(name: &str) -> [u8; 8] {
    tag(&format!("account:{name}"))
}

fn tag(preimage: &str) -> [u8; 8] {
    Sha256::digest(preimage.as_bytes())[..8]
        .try_into()
        .expect("8 bytes")
}

fn key(data: &[u8], at: usize) -> Pubkey {
    Pubkey::new_from_array(data[at..at + 32].try_into().expect("32 bytes"))
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes"))
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(data[at..at + 2].try_into().expect("2 bytes"))
}

fn sf_at(data: &[u8], at: usize) -> f64 {
    u128::from_le_bytes(data[at..at + 16].try_into().expect("16 bytes")) as f64 / SF
}

/// A reserve: one asset's pool in a lending market.
#[derive(Debug, Clone, PartialEq)]
pub struct Reserve {
    pub last_update_slot: u64,
    /// Which of the price checks passed at the last refresh.
    pub price_status: u8,
    pub lending_market: Pubkey,
    /// The farms rewarding supply and debt here; the default key for none.
    pub farm_collateral: Pubkey,
    pub farm_debt: Pubkey,
    pub mint: Pubkey,
    pub supply_vault: Pubkey,
    pub fee_vault: Pubkey,
    /// Atoms in the supply vault, and lent out (with interest).
    pub available: u64,
    pub borrowed: f64,
    /// The asset's price in the market's quote currency, per whole token,
    /// as of the last refresh.
    pub price: f64,
    pub decimals: u32,
    pub token_program: Pubkey,
    /// Fees taken out of the pool, which lenders don't own.
    pub fees: f64,
    pub collateral_mint: Pubkey,
    pub collateral_supply: u64,
    pub collateral_vault: Pubkey,
    pub loan_to_value_pct: u8,
    pub liquidation_threshold_pct: u8,
    pub min_liquidation_bonus_bps: u16,
    pub max_liquidation_bonus_bps: u16,
    /// The borrow rate curve: `(utilization bps, borrow rate bps)`.
    pub curve: Vec<(u32, u32)>,
    pub protocol_take_rate_pct: u8,
    pub borrow_factor_pct: u64,
    pub deposit_limit: u64,
    pub borrow_limit: u64,
    pub max_twap_divergence_bps: u64,
    pub max_age_price_seconds: u64,
    pub max_age_twap_seconds: u64,
    pub scope_prices: Pubkey,
    pub price_chain: [u16; 4],
    pub twap_chain: [u16; 4],
}

pub const RESERVE_LEN: usize = 8624;

impl Reserve {
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() != RESERVE_LEN || data[..8] != account_tag("Reserve") {
            return None;
        }
        let chain = |at: usize| std::array::from_fn(|i| u16_at(data, at + 2 * i));
        Some(Self {
            last_update_slot: u64_at(data, 16),
            price_status: data[25],
            lending_market: key(data, 32),
            farm_collateral: key(data, 64),
            farm_debt: key(data, 96),
            mint: key(data, 128),
            supply_vault: key(data, 160),
            fee_vault: key(data, 192),
            available: u64_at(data, 224),
            borrowed: sf_at(data, 232),
            price: sf_at(data, 248),
            decimals: u64_at(data, 272) as u32,
            fees: sf_at(data, 344) + sf_at(data, 360) + sf_at(data, 376),
            token_program: key(data, 408),
            collateral_mint: key(data, 2560),
            collateral_supply: u64_at(data, 2592),
            collateral_vault: key(data, 2600),
            protocol_take_rate_pct: data[4870],
            loan_to_value_pct: data[4872],
            liquidation_threshold_pct: data[4873],
            min_liquidation_bonus_bps: u16_at(data, 4874),
            max_liquidation_bonus_bps: u16_at(data, 4876),
            curve: (0..11)
                .map(|i| {
                    let at = 4920 + 8 * i;
                    (
                        u32::from_le_bytes(data[at..at + 4].try_into().expect("4")),
                        u32::from_le_bytes(data[at + 4..at + 8].try_into().expect("4")),
                    )
                })
                .collect(),
            borrow_factor_pct: u64_at(data, 5008),
            deposit_limit: u64_at(data, 5016),
            borrow_limit: u64_at(data, 5024),
            max_twap_divergence_bps: u64_at(data, 5088),
            max_age_price_seconds: u64_at(data, 5096),
            max_age_twap_seconds: u64_at(data, 5104),
            scope_prices: key(data, 5112),
            price_chain: chain(5144),
            twap_chain: chain(5152),
        })
    }

    /// Atoms the pool holds for its lenders: what is in the vault and what
    /// is lent out, less the fees.
    pub fn total_supply(&self) -> f64 {
        self.available as f64 + self.borrowed - self.fees
    }

    /// Asset atoms one collateral token redeems for.
    pub fn exchange_rate(&self) -> f64 {
        if self.collateral_supply == 0 {
            1.0
        } else {
            self.total_supply() / self.collateral_supply as f64
        }
    }

    /// Lent out over the lenders' total.
    pub fn utilization(&self) -> f64 {
        let total = self.total_supply();
        if total <= 0.0 {
            0.0
        } else {
            (self.borrowed / total).clamp(0.0, 1.0)
        }
    }

    /// The borrow APR the curve gives at the current utilization.
    pub fn borrow_apr(&self) -> f64 {
        let u = self.utilization() * 10_000.0;
        let mut last = (0.0, 0.0);
        for &(x, y) in &self.curve {
            let (x, y) = (f64::from(x), f64::from(y));
            if u <= x {
                let rate = if x > last.0 {
                    last.1 + (y - last.1) * (u - last.0) / (x - last.0)
                } else {
                    y
                };
                return rate / 10_000.0;
            }
            last = (x, y);
        }
        last.1 / 10_000.0
    }

    /// What lenders earn, as an APR: borrowers' interest, less the
    /// protocol's take, spread over the pool.
    pub fn supply_apr(&self) -> f64 {
        self.borrow_apr()
            * self.utilization()
            * (1.0 - f64::from(self.protocol_take_rate_pct) / 100.0)
    }

    /// A whole token's value from atoms.
    pub fn atoms_to_units(&self, atoms: f64) -> f64 {
        atoms / 10f64.powi(self.decimals as i32)
    }

    pub fn has_farm(farm: &Pubkey) -> bool {
        *farm != Pubkey::default()
    }
}

/// One deposit of an obligation's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Deposit {
    pub reserve: Pubkey,
    /// Collateral tokens.
    pub amount: u64,
    pub market_value: f64,
}

/// One borrow of an obligation's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Borrow {
    pub reserve: Pubkey,
    /// Asset atoms owed, interest included.
    pub amount: f64,
    pub market_value: f64,
}

/// An obligation: one owner's deposits and borrows in one lending market.
#[derive(Debug, Clone, PartialEq)]
pub struct Obligation {
    pub last_update_slot: u64,
    pub lending_market: Pubkey,
    pub owner: Pubkey,
    pub deposits: Vec<Deposit>,
    pub borrows: Vec<Borrow>,
    pub deposited_value: f64,
    pub borrow_factor_adjusted_debt_value: f64,
    pub borrowed_assets_market_value: f64,
    pub allowed_borrow_value: f64,
    pub unhealthy_borrow_value: f64,
}

pub const OBLIGATION_LEN: usize = 3344;
const DEPOSITS_AT: usize = 96;
const DEPOSIT_LEN: usize = 136;
const BORROWS_AT: usize = 1208;
const BORROW_LEN: usize = 200;
/// Where an obligation's lending market sits: what a scan filters on.
pub const OBLIGATION_MARKET_AT: usize = 32;

impl Obligation {
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() != OBLIGATION_LEN || data[..8] != account_tag("Obligation") {
            return None;
        }
        let deposits = (0..8)
            .map(|i| DEPOSITS_AT + i * DEPOSIT_LEN)
            .map(|at| Deposit {
                reserve: key(data, at),
                amount: u64_at(data, at + 32),
                market_value: sf_at(data, at + 40),
            })
            .filter(|d| d.reserve != Pubkey::default())
            .collect();
        let borrows = (0..5)
            .map(|i| BORROWS_AT + i * BORROW_LEN)
            .map(|at| Borrow {
                reserve: key(data, at),
                amount: sf_at(data, at + 88),
                market_value: sf_at(data, at + 104),
            })
            .filter(|b| b.reserve != Pubkey::default())
            .collect();
        Some(Self {
            last_update_slot: u64_at(data, 16),
            lending_market: key(data, 32),
            owner: key(data, 64),
            deposits,
            borrows,
            deposited_value: sf_at(data, 1192),
            borrow_factor_adjusted_debt_value: sf_at(data, 2208),
            borrowed_assets_market_value: sf_at(data, 2224),
            allowed_borrow_value: sf_at(data, 2240),
            unhealthy_borrow_value: sf_at(data, 2256),
        })
    }

    /// The program's health: its liquidation threshold over its debt,
    /// liquidatable below 1. None without debt.
    pub fn health(&self) -> Option<f64> {
        (self.borrow_factor_adjusted_debt_value > 0.0)
            .then(|| self.unhealthy_borrow_value / self.borrow_factor_adjusted_debt_value)
    }

    /// What it is worth: its deposits less its debt, at the program's
    /// prices.
    pub fn net_value(&self) -> f64 {
        self.deposited_value - self.borrowed_assets_market_value
    }

    /// Every reserve it touches, in the order `refresh_obligation` takes
    /// them: deposits, then borrows.
    pub fn reserves(&self) -> Vec<Pubkey> {
        self.deposits
            .iter()
            .map(|d| d.reserve)
            .chain(self.borrows.iter().map(|b| b.reserve))
            .collect()
    }
}

/// A lending market's terms for liquidations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LendingMarket {
    pub close_factor_pct: u8,
    pub insolvency_ltv_pct: u8,
}

impl LendingMarket {
    pub fn decode(data: &[u8]) -> Option<Self> {
        (data.len() >= 4664 && data[..8] == account_tag("LendingMarket")).then(|| Self {
            close_factor_pct: data[126],
            insolvency_ltv_pct: data[127],
        })
    }
}

/// The market's authority over its vaults.
pub fn market_authority(market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"lma", market.as_ref()], &PROGRAM_ID).0
}

pub fn user_metadata(owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_meta", owner.as_ref()], &PROGRAM_ID).0
}

/// An owner's plain obligation (tag 0, id 0) in `market`.
pub fn obligation(owner: &Pubkey, market: &Pubkey) -> Pubkey {
    let none = Pubkey::default();
    Pubkey::find_program_address(
        &[
            &[0],
            &[0],
            owner.as_ref(),
            market.as_ref(),
            none.as_ref(),
            none.as_ref(),
        ],
        &PROGRAM_ID,
    )
    .0
}

/// An obligation's account in a reserve's farm.
pub fn obligation_farm(farm: &Pubkey, obligation: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"user", farm.as_ref(), obligation.as_ref()],
        &FARMS_PROGRAM_ID,
    )
    .0
}

fn w(key: Pubkey) -> AccountMeta {
    AccountMeta::new(key, false)
}

fn r(key: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(key, false)
}

/// An optional account left out: Anchor takes the program's own id.
fn none() -> AccountMeta {
    r(PROGRAM_ID)
}

fn ix(name: &str, args: &[u8], accounts: Vec<AccountMeta>) -> Instruction {
    let mut data = instruction_tag(name).to_vec();
    data.extend_from_slice(args);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// A reserve as the instructions name it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReserveKeys {
    pub address: Pubkey,
    pub market: Pubkey,
    pub mint: Pubkey,
    pub supply_vault: Pubkey,
    pub fee_vault: Pubkey,
    pub collateral_mint: Pubkey,
    pub collateral_vault: Pubkey,
    pub token_program: Pubkey,
    pub farm_collateral: Pubkey,
    pub farm_debt: Pubkey,
    pub scope_prices: Pubkey,
}

impl ReserveKeys {
    pub fn of(address: Pubkey, reserve: &Reserve) -> Self {
        Self {
            address,
            market: reserve.lending_market,
            mint: reserve.mint,
            supply_vault: reserve.supply_vault,
            fee_vault: reserve.fee_vault,
            collateral_mint: reserve.collateral_mint,
            collateral_vault: reserve.collateral_vault,
            token_program: reserve.token_program,
            farm_collateral: reserve.farm_collateral,
            farm_debt: reserve.farm_debt,
            scope_prices: reserve.scope_prices,
        }
    }

    /// The obligation's farm accounts for one of the reserve's farms, or
    /// the program's id twice for none.
    fn farm_accounts(&self, farm: &Pubkey, obligation: &Pubkey) -> [AccountMeta; 2] {
        if Reserve::has_farm(farm) {
            [w(obligation_farm(farm, obligation)), w(*farm)]
        } else {
            [none(), none()]
        }
    }
}

/// Collateral tokens are SPL tokens whatever the asset's program.
const COLLATERAL_TOKEN_PROGRAM: Pubkey = mtx_kit::token::TOKEN_PROGRAM_ID;

pub fn refresh_reserve(reserve: &ReserveKeys) -> Instruction {
    ix(
        "refresh_reserve",
        &[],
        vec![
            w(reserve.address),
            r(reserve.market),
            none(),
            none(),
            none(),
            r(reserve.scope_prices),
        ],
    )
}

/// Refresh `obligation`, whose reserves are `reserves` in its own order.
pub fn refresh_obligation(
    market: &Pubkey,
    obligation: &Pubkey,
    reserves: &[Pubkey],
) -> Instruction {
    let mut accounts = vec![r(*market), w(*obligation)];
    accounts.extend(reserves.iter().copied().map(w));
    ix("refresh_obligation", &[], accounts)
}

pub fn init_user_metadata(owner: &Pubkey) -> Instruction {
    ix(
        "init_user_metadata",
        Pubkey::default().as_ref(),
        vec![
            AccountMeta::new_readonly(*owner, true),
            AccountMeta::new(*owner, true),
            w(user_metadata(owner)),
            none(),
            r(RENT_SYSVAR),
            r(SYSTEM_PROGRAM),
        ],
    )
}

pub fn init_obligation(owner: &Pubkey, market: &Pubkey) -> Instruction {
    ix(
        "init_obligation",
        &[0, 0],
        vec![
            AccountMeta::new_readonly(*owner, true),
            AccountMeta::new(*owner, true),
            w(obligation(owner, market)),
            r(*market),
            r(Pubkey::default()),
            r(Pubkey::default()),
            r(user_metadata(owner)),
            r(RENT_SYSVAR),
            r(SYSTEM_PROGRAM),
        ],
    )
}

/// Which of a reserve's farms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FarmSide {
    Collateral = 0,
    Debt = 1,
}

pub fn init_obligation_farm(
    owner: &Pubkey,
    reserve: &ReserveKeys,
    side: FarmSide,
) -> Option<Instruction> {
    let farm = match side {
        FarmSide::Collateral => reserve.farm_collateral,
        FarmSide::Debt => reserve.farm_debt,
    };
    if !Reserve::has_farm(&farm) {
        return None;
    }
    let obligation = obligation(owner, &reserve.market);
    Some(ix(
        "init_obligation_farms_for_reserve",
        &[side as u8],
        vec![
            AccountMeta::new(*owner, true),
            r(*owner),
            w(obligation),
            r(market_authority(&reserve.market)),
            w(reserve.address),
            w(farm),
            w(obligation_farm(&farm, &obligation)),
            r(reserve.market),
            r(FARMS_PROGRAM_ID),
            r(RENT_SYSVAR),
            r(SYSTEM_PROGRAM),
        ],
    ))
}

fn user_ata(owner: &Pubkey, reserve: &ReserveKeys) -> Pubkey {
    mtx_kit::token::associated_token_address(owner, &reserve.mint, &reserve.token_program)
}

pub fn deposit(owner: &Pubkey, reserve: &ReserveKeys, atoms: u64) -> Instruction {
    let obligation = obligation(owner, &reserve.market);
    let mut accounts = vec![
        AccountMeta::new(*owner, true),
        w(obligation),
        r(reserve.market),
        r(market_authority(&reserve.market)),
        w(reserve.address),
        r(reserve.mint),
        w(reserve.supply_vault),
        w(reserve.collateral_mint),
        w(reserve.collateral_vault),
        w(user_ata(owner, reserve)),
        none(),
        r(COLLATERAL_TOKEN_PROGRAM),
        r(reserve.token_program),
        r(INSTRUCTIONS_SYSVAR),
    ];
    accounts.extend(reserve.farm_accounts(&reserve.farm_collateral, &obligation));
    accounts.push(r(FARMS_PROGRAM_ID));
    ix(
        "deposit_reserve_liquidity_and_obligation_collateral_v2",
        &atoms.to_le_bytes(),
        accounts,
    )
}

/// Withdraw `collateral` collateral tokens, redeemed for the asset.
pub fn withdraw(owner: &Pubkey, reserve: &ReserveKeys, collateral: u64) -> Instruction {
    let obligation = obligation(owner, &reserve.market);
    let mut accounts = vec![
        AccountMeta::new(*owner, true),
        w(obligation),
        r(reserve.market),
        r(market_authority(&reserve.market)),
        w(reserve.address),
        r(reserve.mint),
        w(reserve.collateral_vault),
        w(reserve.collateral_mint),
        w(reserve.supply_vault),
        w(user_ata(owner, reserve)),
        none(),
        r(COLLATERAL_TOKEN_PROGRAM),
        r(reserve.token_program),
        r(INSTRUCTIONS_SYSVAR),
    ];
    accounts.extend(reserve.farm_accounts(&reserve.farm_collateral, &obligation));
    accounts.push(r(FARMS_PROGRAM_ID));
    ix(
        "withdraw_obligation_collateral_and_redeem_reserve_collateral_v2",
        &collateral.to_le_bytes(),
        accounts,
    )
}

pub fn borrow(owner: &Pubkey, reserve: &ReserveKeys, atoms: u64) -> Instruction {
    let obligation = obligation(owner, &reserve.market);
    let mut accounts = vec![
        AccountMeta::new_readonly(*owner, true),
        w(obligation),
        r(reserve.market),
        r(market_authority(&reserve.market)),
        w(reserve.address),
        r(reserve.mint),
        w(reserve.supply_vault),
        w(reserve.fee_vault),
        w(user_ata(owner, reserve)),
        none(),
        r(reserve.token_program),
        r(INSTRUCTIONS_SYSVAR),
    ];
    accounts.extend(reserve.farm_accounts(&reserve.farm_debt, &obligation));
    accounts.push(r(FARMS_PROGRAM_ID));
    ix(
        "borrow_obligation_liquidity_v2",
        &atoms.to_le_bytes(),
        accounts,
    )
}

/// Repay `atoms` of the owner's debt; `u64::MAX` repays all of it.
pub fn repay(owner: &Pubkey, reserve: &ReserveKeys, atoms: u64) -> Instruction {
    let obligation = obligation(owner, &reserve.market);
    let mut accounts = vec![
        AccountMeta::new_readonly(*owner, true),
        w(obligation),
        r(reserve.market),
        w(reserve.address),
        r(reserve.mint),
        w(reserve.supply_vault),
        w(user_ata(owner, reserve)),
        r(reserve.token_program),
        r(INSTRUCTIONS_SYSVAR),
    ];
    accounts.extend(reserve.farm_accounts(&reserve.farm_debt, &obligation));
    accounts.push(r(market_authority(&reserve.market)));
    accounts.push(r(FARMS_PROGRAM_ID));
    ix(
        "repay_obligation_liquidity_v2",
        &atoms.to_le_bytes(),
        accounts,
    )
}

/// Repay `atoms` of `obligation`'s debt at `repay`, and take its collateral
/// at `withdraw`, redeemed for the asset: no less than `min_received` atoms.
pub fn liquidate(
    liquidator: &Pubkey,
    obligation: &Pubkey,
    repay: &ReserveKeys,
    withdraw: &ReserveKeys,
    atoms: u64,
    min_received: u64,
) -> Instruction {
    let mut args = atoms.to_le_bytes().to_vec();
    args.extend_from_slice(&min_received.to_le_bytes());
    args.extend_from_slice(&0u64.to_le_bytes());
    let mut accounts = vec![
        AccountMeta::new_readonly(*liquidator, true),
        w(*obligation),
        r(repay.market),
        r(market_authority(&repay.market)),
        w(repay.address),
        r(repay.mint),
        w(repay.supply_vault),
        w(withdraw.address),
        r(withdraw.mint),
        w(withdraw.collateral_mint),
        w(withdraw.collateral_vault),
        w(withdraw.supply_vault),
        w(withdraw.fee_vault),
        w(user_ata(liquidator, repay)),
        w(collateral_ata(liquidator, withdraw)),
        w(user_ata(liquidator, withdraw)),
        r(COLLATERAL_TOKEN_PROGRAM),
        r(repay.token_program),
        r(withdraw.token_program),
        r(INSTRUCTIONS_SYSVAR),
    ];
    accounts.extend(withdraw.farm_accounts(&withdraw.farm_collateral, obligation));
    accounts.extend(repay.farm_accounts(&repay.farm_debt, obligation));
    accounts.push(r(FARMS_PROGRAM_ID));
    ix(
        "liquidate_obligation_and_redeem_reserve_collateral_v2",
        &args,
        accounts,
    )
}

/// The liquidator's account for a reserve's collateral token, which a
/// liquidation passes through.
pub fn collateral_ata(owner: &Pubkey, reserve: &ReserveKeys) -> Pubkey {
    mtx_kit::token::associated_token_address(
        owner,
        &reserve.collateral_mint,
        &COLLATERAL_TOKEN_PROGRAM,
    )
}

/// The instructions a liquidator's collateral and asset accounts need, made
/// idempotently.
pub fn liquidator_accounts(
    liquidator: &Pubkey,
    repay: &ReserveKeys,
    withdraw: &ReserveKeys,
) -> Vec<Instruction> {
    use mtx_kit::chain::token::create_ata_idempotent_for as ata;
    vec![
        ata(liquidator, liquidator, &repay.mint, &repay.token_program),
        ata(
            liquidator,
            liquidator,
            &withdraw.collateral_mint,
            &COLLATERAL_TOKEN_PROGRAM,
        ),
        ata(
            liquidator,
            liquidator,
            &withdraw.mint,
            &withdraw.token_program,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_anchors() {
        // Anchor's `sha256("global:refresh_reserve")[..8]`, as the program
        // dispatches on it.
        assert_eq!(
            instruction_tag("refresh_reserve"),
            [2, 218, 138, 235, 79, 201, 25, 102]
        );
        assert_eq!(
            account_tag("Obligation"),
            [168, 206, 141, 106, 88, 76, 172, 167]
        );
    }

    #[test]
    fn an_obligation_reads_its_entries_and_health() {
        let mut data = vec![0u8; OBLIGATION_LEN];
        data[..8].copy_from_slice(&account_tag("Obligation"));
        let reserve = Pubkey::new_unique();
        let sf = |v: f64| ((v * SF) as u128).to_le_bytes();
        data[DEPOSITS_AT..DEPOSITS_AT + 32].copy_from_slice(reserve.as_ref());
        data[DEPOSITS_AT + 32..DEPOSITS_AT + 40].copy_from_slice(&7u64.to_le_bytes());
        data[1192..1208].copy_from_slice(&sf(1000.0));
        data[2208..2224].copy_from_slice(&sf(400.0));
        data[2224..2240].copy_from_slice(&sf(400.0));
        data[2256..2272].copy_from_slice(&sf(500.0));
        let o = Obligation::decode(&data).unwrap();
        assert_eq!(o.deposits.len(), 1);
        assert_eq!(o.deposits[0].amount, 7);
        assert!(o.borrows.is_empty());
        assert_eq!(o.health(), Some(1.25));
        assert_eq!(o.net_value(), 600.0);
        assert_eq!(o.reserves(), vec![reserve]);
        assert!(Obligation::decode(&data[1..]).is_none());
    }

    #[test]
    fn a_curve_interpolates() {
        let mut data = vec![0u8; RESERVE_LEN];
        data[..8].copy_from_slice(&account_tag("Reserve"));
        for (i, (x, y)) in [(0u32, 100u32), (8000, 1000), (10_000, 5000)]
            .iter()
            .chain(std::iter::repeat_n(&(10_000, 5000), 8))
            .enumerate()
        {
            let at = 4920 + 8 * i;
            data[at..at + 4].copy_from_slice(&x.to_le_bytes());
            data[at + 4..at + 8].copy_from_slice(&y.to_le_bytes());
        }
        // Half the pool lent out.
        data[224..232].copy_from_slice(&500u64.to_le_bytes());
        data[232..248].copy_from_slice(&((500.0 * SF) as u128).to_le_bytes());
        let reserve = Reserve::decode(&data).unwrap();
        assert!((reserve.utilization() - 0.5).abs() < 1e-12);
        assert!((reserve.borrow_apr() - (0.01 + 0.09 * 5000.0 / 8000.0)).abs() < 1e-12);
    }
}
