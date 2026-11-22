//! The `clone-pool` recipe action: every account a swap through a mainnet
//! concentrated-liquidity pool reads, loaded into a recipe's fork so the
//! snapshot can swap through the pool offline.
//!
//! A session moves a cloned pool with real swaps, which keep its price, its
//! liquidity and its tick arrays consistent; an account override could not
//! (RFD 0005, "Moving the pools").
//!
//! A scenario's `venues.amm` ([`AmmVenue`]) names the cloned pools a
//! session trades, beside its lending markets; [`route_accounts`] lists what
//! a swap through one reads, for the session's lookup table.

use std::collections::BTreeMap;

use mtx_core::mandate::{InstrumentRef, Kind};
use mtx_kit::protocols::{
    USDC_MINT,
    orca::whirlpool::{self, Whirlpool},
    raydium::clmm,
};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use tracing::info;

use crate::{Error, Result, snapshot::Source};

/// Tick arrays a clone takes on each side of the current one by default.
/// Whirlpool SOL/USDC arrays span 352 ticks (about 3.5%) and Raydium's 60
/// (about 0.6%); the research's largest trigger, 1,286 SOL, crossed three
/// of Raydium's, so twelve leave room for a session's whole drift.
pub const TICK_ARRAYS_PER_SIDE_DEFAULT: u32 = 12;
/// At most this many a side: Raydium's navigator walks at most
/// [`clmm::TICK_ARRAYS_MAX`], and 64 Whirlpool arrays already span the
/// price range many times over.
pub const TICK_ARRAYS_PER_SIDE_MAX: u32 = 64;
/// `getMultipleAccounts` takes at most 100 keys.
const ACCOUNTS_PER_CALL_MAX: usize = 100;

const WRAPPED_SOL_MINT: Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");
const SOL_DECIMALS: i32 = 9;
const USDC_DECIMALS: i32 = 6;

/// Pools a session may list: the thin-pools snapshot's ten and room for
/// two more. Each adds 15 to 37 accounts to the session's lookup tables
/// (256 addresses each): ten need 279, in two.
pub const POOLS_MAX: usize = 12;

/// A scenario's pools (`venues.amm`): each a cloned pool of the snapshot's,
/// by the instrument the mandate names it with, `orca:amm:<address>` or
/// `raydium:amm:<address>`.
///
/// Beside `venues.lending`, `pools` alone: the pools a liquidation sells
/// its collateral through, which the lending session's arbitrageur keeps
/// on its prices. On its own, `venues.amm` is an `amm` session
/// ([`crate::amm`]): the bot's wallet, the reference price the account is
/// valued at, and the actors that move the pools and compete for what
/// their moves open. Those fields are the `amm` session's only.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AmmVenue {
    pub pools: Vec<String>,
    /// What every draw of an `amm` session's actors comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// The bot's wallet, by asset (`SOL`, held wrapped, and `USDC`), in
    /// whole units.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub wallet: BTreeMap<String, f64>,
    /// SOL's reference price when the session starts, in USD: what the
    /// scenario's path (`market_model.path`) moves, and what the account is
    /// valued at, never the pools' own prices. Without it, the first
    /// pool's price as the session is set up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_usd: Option<f64>,
    /// The reference price of every other token a pool trades, by mint, in
    /// SOL a unit: what the account values it at and the pool searchers
    /// trade its pools back to. SOL and USDC have none (USDC is worth a
    /// dollar). Fixed for the session: only SOL's dollar price moves.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reference_sol: BTreeMap<String, f64>,
    /// The seller (or buyer) whose chunks are the session's triggers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whale: Option<crate::amm::Whale>,
    /// The calm flow on each pool between chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<crate::amm::Background>,
    /// Searchers that close a gap between two pools with an atomic cycle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub searchers: Vec<crate::amm::CycleSearcher>,
    /// Searchers that trade one pool back to the reference price.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pool_searchers: Vec<crate::amm::PoolSearcher>,
    /// What the reward divides PnL by, in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_usd: Option<f64>,
}

impl AmmVenue {
    /// Each pool's instrument.
    pub fn instruments(&self) -> Vec<InstrumentRef> {
        self.pools.iter().filter_map(|p| p.parse().ok()).collect()
    }

    /// One to [`POOLS_MAX`] pools, each an `amm` instrument of a venue the
    /// runtime offers, named by an address, and listed once.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.pools.is_empty() || self.pools.len() > POOLS_MAX {
            return Err(format!("amm: pools lists 1 to {POOLS_MAX} pools"));
        }
        for (i, pool) in self.pools.iter().enumerate() {
            let instrument: InstrumentRef = pool.parse().map_err(|e| format!("amm: {e}"))?;
            if instrument.kind != Kind::Amm
                || !crate::venue::amm::VENUES.contains(&instrument.venue.as_str())
            {
                return Err(format!(
                    "amm: {pool} is not a pool of {}",
                    crate::venue::amm::VENUES.join(" or ")
                ));
            }
            if instrument.symbol.parse::<Pubkey>().is_err() {
                return Err(format!("amm: {pool} names its pool by an address"));
            }
            if self.pools[..i].contains(pool) {
                return Err(format!("amm: {pool} is listed twice"));
            }
        }
        Ok(())
    }

    /// Whether it sets anything but its pools: an `amm` session's fields.
    pub fn has_session_fields(&self) -> bool {
        *self
            != Self {
                pools: self.pools.clone(),
                ..Self::default()
            }
    }

    /// An `amm` session's fields, on their own: a seed, a wallet of SOL and
    /// USDC, a positive reference, and actors that check themselves
    /// against the session's pools and length.
    pub fn validate_session(&self, duration_slots: u64) -> std::result::Result<(), String> {
        if self.seed.is_none() {
            return Err("amm: an amm session needs a seed".into());
        }
        for (asset, units) in &self.wallet {
            if !crate::amm::WALLET_ASSETS.contains(&asset.as_str()) {
                return Err(format!(
                    "amm: the wallet holds {}, not {asset}",
                    crate::amm::WALLET_ASSETS.join(" and ")
                ));
            }
            if !(units.is_finite() && *units >= 0.0 && *units <= crate::amm::WALLET_UNITS_MAX) {
                return Err(format!(
                    "amm: the wallet's {asset} is 0 to {}, not {units}",
                    crate::amm::WALLET_UNITS_MAX
                ));
            }
        }
        if self
            .reference_usd
            .is_some_and(|price| !(price.is_finite() && price > 0.0))
        {
            return Err("amm: reference_usd is a positive price".into());
        }
        for (mint, price) in &self.reference_sol {
            if mint.parse::<Pubkey>().is_err()
                || [WRAPPED_SOL_MINT, USDC_MINT]
                    .map(|m| m.to_string())
                    .contains(mint)
            {
                return Err(format!(
                    "amm: reference_sol names a mint other than SOL's and USDC's, not {mint}"
                ));
            }
            if !(price.is_finite() && *price > 0.0) {
                return Err(format!("amm: reference_sol's {mint} is a positive price"));
            }
        }
        if self.bound_usd.is_some_and(|bound| !bound.is_finite()) {
            return Err("amm: bound_usd must be a number".into());
        }
        let pools = self.instruments();
        if let Some(whale) = &self.whale {
            whale.validate(&pools, duration_slots)?;
        }
        if let Some(background) = &self.background {
            background.validate(&pools)?;
        }
        if self.searchers.len() + self.pool_searchers.len() > crate::amm::SEARCHERS_MAX {
            return Err(format!(
                "amm: at most {} searchers in all",
                crate::amm::SEARCHERS_MAX
            ));
        }
        if !self.searchers.is_empty() && pools.len() < 2 {
            return Err("amm: cycle searchers cycle through two pools at least".into());
        }
        for searcher in &self.searchers {
            searcher.validate()?;
        }
        for searcher in &self.pool_searchers {
            searcher.validate()?;
        }
        Ok(())
    }
}

/// Every account a swap through the pool at `address` names but its
/// trader's own, as the snapshot holds them: the pool and its program, the
/// token programs and the memo program, its config, mints and vaults, its
/// oracle or observation and bitmap extension, and the tick arrays within
/// [`TICK_ARRAYS_PER_SIDE_DEFAULT`] of the current one that exist. What a
/// session's lookup table takes for a route through the pool.
pub fn route_accounts(rpc: &RpcClient, address: &Pubkey) -> Result<Vec<Pubkey>> {
    let pool = rpc.get_account(address)?;
    let program = PoolProgram::of_owner(&pool.owner).ok_or_else(|| {
        invalid(
            "route accounts",
            format!("{address} is not an Orca Whirlpool or a Raydium CLMM pool"),
        )
    })?;
    let spec = ClonePoolSpec {
        name: address.to_string(),
        address: address.to_string(),
        tick_arrays: TICK_ARRAYS_PER_SIDE_DEFAULT,
    };
    let cloned = match program {
        PoolProgram::OrcaWhirlpool => clone_whirlpool(rpc, address, &pool.data, &spec)?,
        PoolProgram::RaydiumClmm => clone_raydium(rpc, address, &pool.data, &spec)?,
    };
    let found = fetch(rpc, &cloned.accounts)?;
    let mut accounts = vec![
        *address,
        program.program_id(),
        mtx_kit::token::TOKEN_PROGRAM_ID,
        mtx_kit::token::TOKEN_2022_PROGRAM_ID,
        whirlpool::ix::MEMO_PROGRAM_ID,
    ];
    for (key, account) in cloned.accounts.iter().zip(&found) {
        // An absent oracle is still a Whirlpool swap's account.
        let named = account.is_some() || *key == whirlpool::ix::oracle_address(address);
        if named && !accounts.contains(key) {
            accounts.push(*key);
        }
    }
    assert!(accounts.len() <= cloned.accounts.len() + 5);
    Ok(accounts)
}

/// A `clone-pool` action.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClonePoolSpec {
    /// The name the manifest lists the pool under.
    pub name: String,
    /// The pool account: a Whirlpool or a Raydium CLMM pool.
    pub address: String,
    /// Tick arrays to take on each side of the current one.
    #[serde(default = "default_tick_arrays")]
    pub tick_arrays: u32,
}

fn default_tick_arrays() -> u32 {
    TICK_ARRAYS_PER_SIDE_DEFAULT
}

/// The pool programs `clone-pool` knows, by the pool account's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolProgram {
    OrcaWhirlpool,
    RaydiumClmm,
}

impl PoolProgram {
    pub fn of_owner(owner: &Pubkey) -> Option<Self> {
        if *owner == whirlpool::PROGRAM_ID {
            Some(Self::OrcaWhirlpool)
        } else if *owner == clmm::PROGRAM_ID {
            Some(Self::RaydiumClmm)
        } else {
            None
        }
    }

    pub fn program_id(self) -> Pubkey {
        match self {
            Self::OrcaWhirlpool => whirlpool::PROGRAM_ID,
            Self::RaydiumClmm => clmm::PROGRAM_ID,
        }
    }
}

/// What a clone loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct Cloned {
    pub program: PoolProgram,
    /// Every account the clone asked for besides the pool, the program and
    /// its code, in order; the chain may hold none at some of them.
    pub accounts: Vec<Pubkey>,
    /// How many of `accounts` exist.
    pub existing: usize,
    /// How many tick arrays exist.
    pub tick_arrays: usize,
    /// SOL's price in USDC, for a SOL/USDC pool: its square-root price
    /// squared, before fees.
    pub sol_usd: Option<f64>,
}

/// Loads `spec`'s pool into the fork behind `rpc`: the program and its
/// code, the pool, its config, vaults and mints, the Whirlpool oracle or
/// the Raydium observation and bitmap extension, and its tick arrays.
/// Returns the manifest's entry for the pool and, for a SOL/USDC pool,
/// SOL's price in USDC: a snapshot of pools alone has no other price.
pub fn clone_pool(rpc: &RpcClient, spec: &ClonePoolSpec) -> Result<(Source, Option<f64>)> {
    let address: Pubkey = spec
        .address
        .parse()
        .map_err(|_| invalid(&spec.name, format!("{} is not an address", spec.address)))?;
    if !(1..=TICK_ARRAYS_PER_SIDE_MAX).contains(&spec.tick_arrays) {
        return Err(invalid(
            &spec.name,
            format!(
                "tick_arrays is {}, not 1 to {TICK_ARRAYS_PER_SIDE_MAX}",
                spec.tick_arrays
            ),
        ));
    }
    let pool = rpc.get_account(&address)?;
    let program = PoolProgram::of_owner(&pool.owner).ok_or_else(|| {
        invalid(
            &spec.name,
            format!(
                "{address} is owned by {}, not Orca Whirlpool or Raydium CLMM",
                pool.owner
            ),
        )
    })?;
    load_program(rpc, &program.program_id())?;
    let cloned = match program {
        PoolProgram::OrcaWhirlpool => clone_whirlpool(rpc, &address, &pool.data, spec)?,
        PoolProgram::RaydiumClmm => clone_raydium(rpc, &address, &pool.data, spec)?,
    };
    info!(
        pool = %address,
        name = spec.name,
        accounts = cloned.accounts.len(),
        existing = cloned.existing,
        tick_arrays = cloned.tick_arrays,
        sol_usd = cloned.sol_usd,
        "pool cloned"
    );
    let source = Source {
        role: "pool".into(),
        name: spec.name.clone(),
        address: address.to_string(),
        programdata: None,
        deploy_slot: None,
        code_sha256: None,
        pinned_from: None,
    };
    Ok((source, cloned.sol_usd))
}

/// SOL's price in USDC on a pool of `base` and `quote` at
/// `sqrt_price_x64` (quote atoms per base atom, Q64.64), before fees; `None`
/// for a pool of other mints.
pub fn sol_usd(base: &Pubkey, quote: &Pubkey, sqrt_price_x64: u128) -> Option<f64> {
    let per_atom = (sqrt_price_x64 as f64 / 2f64.powi(64)).powi(2);
    let price = if (*base, *quote) == (WRAPPED_SOL_MINT, USDC_MINT) {
        per_atom * 10f64.powi(SOL_DECIMALS - USDC_DECIMALS)
    } else if (*base, *quote) == (USDC_MINT, WRAPPED_SOL_MINT) {
        1.0 / (per_atom * 10f64.powi(USDC_DECIMALS - SOL_DECIMALS))
    } else {
        return None;
    };
    assert!(
        price.is_finite() && price > 0.0,
        "a pool's price is positive"
    );
    Some(price)
}

fn clone_whirlpool(
    rpc: &RpcClient,
    address: &Pubkey,
    data: &[u8],
    spec: &ClonePoolSpec,
) -> Result<Cloned> {
    let pool = Whirlpool::decode(data).map_err(|e| invalid(&spec.name, e.to_string()))?;
    let starts = whirlpool_array_starts(&pool, spec.tick_arrays);
    let arrays: Vec<Pubkey> = starts
        .iter()
        .map(|&start| whirlpool::ix::tick_array_address(address, start))
        .collect();
    // The oracle is a swap's account whether or not it exists: a pool
    // without an adaptive fee has none.
    let mut accounts = vec![
        pool.whirlpools_config,
        pool.token_mint_a,
        pool.token_mint_b,
        pool.token_vault_a,
        pool.token_vault_b,
        whirlpool::ix::oracle_address(address),
    ];
    accounts.extend(&arrays);
    let found = fetch(rpc, &accounts)?;
    let required = &found[..5];
    if let Some(missing) = required.iter().position(Option::is_none) {
        return Err(invalid(
            &spec.name,
            format!("the pool's {} does not exist", accounts[missing]),
        ));
    }
    let mut tick_arrays = 0;
    for ((start, key), account) in starts.iter().zip(&arrays).zip(&found[6..]) {
        let Some(account) = account else { continue };
        let array = whirlpool::TickArray::decode(account)
            .map_err(|e| invalid(&spec.name, format!("{key}: {e}")))?;
        assert_eq!(array.whirlpool, *address, "a tick array of its own pool");
        assert_eq!(array.start_tick_index, *start, "the array at its address");
        tick_arrays += 1;
    }
    Ok(Cloned {
        program: PoolProgram::OrcaWhirlpool,
        existing: found.iter().filter(|a| a.is_some()).count(),
        accounts,
        tick_arrays,
        sol_usd: sol_usd(&pool.token_mint_a, &pool.token_mint_b, pool.sqrt_price_x64),
    })
}

/// The start ticks of the Whirlpool arrays within `per_side` arrays of the
/// current one, in order. A swap walks contiguous arrays (an absent one is
/// uninitialized), so a span is what one can reach.
pub fn whirlpool_array_starts(pool: &Whirlpool, per_side: u32) -> Vec<i32> {
    assert!(per_side <= TICK_ARRAYS_PER_SIDE_MAX);
    let ticks_in_array = pool.ticks_in_array();
    let current = pool.tick_current_index.div_euclid(ticks_in_array) * ticks_in_array;
    let per_side = i32::try_from(per_side).expect("at most 64");
    let starts: Vec<i32> = (-per_side..=per_side)
        .map(|offset| current + offset * ticks_in_array)
        .filter(|&start| {
            start + ticks_in_array > whirlpool::math::MIN_TICK_INDEX
                && start <= whirlpool::math::MAX_TICK_INDEX
        })
        .collect();
    assert!(starts.contains(&current), "the current array is in range");
    starts
}

fn clone_raydium(
    rpc: &RpcClient,
    address: &Pubkey,
    data: &[u8],
    spec: &ClonePoolSpec,
) -> Result<Cloned> {
    let pool = clmm::Pool::decode(data).map_err(|e| invalid(&spec.name, e.to_string()))?;
    let extension_address = clmm::ix::bitmap_extension_address(address);
    let mut accounts = vec![
        pool.amm_config,
        pool.token_mint_0,
        pool.token_mint_1,
        pool.token_vault_0,
        pool.token_vault_1,
        pool.observation,
        extension_address,
    ];
    let found = fetch(rpc, &accounts)?;
    if let Some(missing) = found[..6].iter().position(Option::is_none) {
        return Err(invalid(
            &spec.name,
            format!("the pool's {} does not exist", accounts[missing]),
        ));
    }
    let extension = match &found[6] {
        Some(data) => Some(
            clmm::BitmapExtension::decode(data)
                .map_err(|e| invalid(&spec.name, format!("{extension_address}: {e}")))?,
        ),
        None => None,
    };
    let starts = raydium_array_starts(&pool, extension.as_ref(), spec.tick_arrays)
        .map_err(|e| invalid(&spec.name, format!("{e:?}")))?;
    let arrays: Vec<Pubkey> = starts
        .iter()
        .map(|&start| clmm::ix::tick_array_address(address, start))
        .collect();
    let found_arrays = fetch(rpc, &arrays)?;
    let mut tick_arrays = 0;
    for ((start, key), account) in starts.iter().zip(&arrays).zip(&found_arrays) {
        // The bitmap marks each array as initialized: one it names exists.
        let account = account
            .as_ref()
            .ok_or_else(|| invalid(&spec.name, format!("tick array {key} ({start}) is absent")))?;
        let array = clmm::TickArray::decode(account)
            .map_err(|e| invalid(&spec.name, format!("{key}: {e}")))?;
        assert_eq!(array.pool, *address, "a tick array of its own pool");
        assert_eq!(array.start_tick_index, *start, "the array at its address");
        tick_arrays += 1;
    }
    let existing = found.iter().filter(|a| a.is_some()).count() + tick_arrays;
    accounts.extend(&arrays);
    Ok(Cloned {
        program: PoolProgram::RaydiumClmm,
        accounts,
        existing,
        tick_arrays,
        sol_usd: sol_usd(&pool.token_mint_0, &pool.token_mint_1, pool.sqrt_price_x64),
    })
}

/// The start ticks of the Raydium arrays a swap reaches within `per_side`
/// initialized arrays each way, the current one first. Raydium's swap
/// skips uninitialized arrays through the pool's bitmap, so the
/// initialized ones are what one can reach, however far apart.
pub fn raydium_array_starts(
    pool: &clmm::Pool,
    extension: Option<&clmm::BitmapExtension>,
    per_side: u32,
) -> std::result::Result<Vec<i32>, clmm::ClmmError> {
    assert!(per_side <= TICK_ARRAYS_PER_SIDE_MAX);
    let count_max = usize::try_from(per_side).expect("at most 64") + 1;
    let mut starts = Vec::with_capacity(2 * count_max);
    for direction in [
        clmm::SwapDirection::ZeroForOne,
        clmm::SwapDirection::OneForZero,
    ] {
        for start in clmm::tick_arrays_in_direction(pool, extension, direction, count_max)? {
            if !starts.contains(&start) {
                starts.push(start);
            }
        }
    }
    assert!(starts.len() <= 2 * count_max);
    Ok(starts)
}

/// Loads a program and, for an upgradeable one, its code.
fn load_program(rpc: &RpcClient, program: &Pubkey) -> Result<()> {
    const UPGRADEABLE_LOADER: Pubkey =
        solana_pubkey::pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");
    let account = rpc.get_account(program)?;
    if !account.executable {
        return Err(invalid(
            "clone-pool",
            format!("{program} is not executable"),
        ));
    }
    if account.owner == UPGRADEABLE_LOADER && account.data.len() == 36 {
        let programdata = Pubkey::new_from_array(account.data[4..36].try_into().expect("32"));
        rpc.get_account(&programdata)?;
    }
    Ok(())
}

/// Each account's data, or `None` where the chain has none, fetched through
/// the fork (which keeps them).
fn fetch(rpc: &RpcClient, keys: &[Pubkey]) -> Result<Vec<Option<Vec<u8>>>> {
    let mut found = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(ACCOUNTS_PER_CALL_MAX) {
        found.extend(
            rpc.get_multiple_accounts(chunk)?
                .into_iter()
                .map(|a| a.map(|a| a.data)),
        );
    }
    assert_eq!(found.len(), keys.len());
    Ok(found)
}

fn invalid(name: &str, reason: String) -> Error {
    Error::Transaction {
        label: format!("clone-pool {name}"),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whirlpool(tick_current_index: i32, tick_spacing: u16) -> Whirlpool {
        Whirlpool {
            whirlpools_config: Pubkey::default(),
            tick_spacing,
            fee_tier_index: tick_spacing,
            fee_rate: 400,
            protocol_fee_rate: 1_300,
            liquidity: 1,
            sqrt_price_x64: whirlpool::math::sqrt_price_from_tick_index(tick_current_index),
            tick_current_index,
            token_mint_a: Pubkey::default(),
            token_vault_a: Pubkey::default(),
            fee_growth_global_a: 0,
            token_mint_b: Pubkey::default(),
            token_vault_b: Pubkey::default(),
            fee_growth_global_b: 0,
            reward_last_updated_timestamp_s: 0,
        }
    }

    #[test]
    fn a_whirlpool_span_is_contiguous_around_the_current_array() {
        // SOL/USDC: tick spacing 4, 352 ticks an array.
        let starts = whirlpool_array_starts(&whirlpool(-17_000, 4), 2);
        assert_eq!(starts, [-17_952, -17_600, -17_248, -16_896, -16_544]);
        // A negative tick on an array's edge belongs to the array below.
        let edge = whirlpool_array_starts(&whirlpool(-352, 4), 0);
        assert_eq!(edge, [-352]);
        assert_eq!(whirlpool_array_starts(&whirlpool(-1, 4), 0), [-352]);
    }

    #[test]
    fn a_whirlpool_span_stops_at_the_price_range() {
        let top = whirlpool::math::MAX_TICK_INDEX - 1;
        let starts = whirlpool_array_starts(&whirlpool(top, 64), 3);
        assert_eq!(starts.len(), 4, "nothing past the last array: {starts:?}");
        assert!(starts.iter().all(|s| *s <= whirlpool::math::MAX_TICK_INDEX));
    }

    #[test]
    fn a_clone_pool_action_parses_with_its_default_span() {
        let text = "name=\"t\"\nsymbol=\"x\"\n[[warmup]]\naction=\"clone-pool\"\nname=\"orca\"\naddress=\"Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE\"";
        let recipe = crate::recipe::Recipe::parse(text).unwrap();
        let crate::recipe::Action::ClonePool(spec) = &recipe.warmup[0] else {
            panic!("not a clone-pool action");
        };
        assert_eq!(spec.tick_arrays, TICK_ARRAYS_PER_SIDE_DEFAULT);
        let bad = format!("{text}\nbogus=1");
        assert!(crate::recipe::Recipe::parse(&bad).is_err());
    }

    /// The kit's mainnet fixture of Raydium SOL/USDC: the pool, its bitmap
    /// extension and its 13 initialized tick arrays nearest the price.
    fn raydium_fixture() -> (Pubkey, clmm::Pool, clmm::BitmapExtension, Vec<i32>) {
        use std::io::Read as _;

        use base64::Engine as _;
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../kit/tests/fixtures/raydium-clmm-sol-usdc/accounts.json.gz"
        );
        let mut text = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        let fixture: serde_json::Value = serde_json::from_str(&text).unwrap();
        let address: Pubkey = fixture["pool"].as_str().unwrap().parse().unwrap();
        let data = |key: &Pubkey| {
            base64::engine::general_purpose::STANDARD
                .decode(
                    fixture["accounts"][key.to_string()]["data"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap()
        };
        let pool = clmm::Pool::decode(&data(&address)).unwrap();
        let extension =
            clmm::BitmapExtension::decode(&data(&clmm::ix::bitmap_extension_address(&address)))
                .unwrap();
        let mut starts = Vec::new();
        for (_, account) in fixture["accounts"].as_object().unwrap() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(account["data"].as_str().unwrap())
                .unwrap();
            if let Ok(array) = clmm::TickArray::decode(&bytes) {
                starts.push(array.start_tick_index);
            }
        }
        (address, pool, extension, starts)
    }

    #[test]
    fn a_raydium_span_takes_the_initialized_arrays_nearest_the_price() {
        let (address, pool, extension, initialized) = raydium_fixture();
        assert_eq!(address, clmm::SOL_USDC_POOL);
        assert_eq!(initialized.len(), 13);
        let starts = raydium_array_starts(&pool, Some(&extension), 2).unwrap();
        // Two each way and the current one, each initialized, none twice.
        assert!((3..=6).contains(&starts.len()), "{starts:?}");
        assert!(starts.iter().all(|s| initialized.contains(s)), "{starts:?}");
        let current = clmm::state::array_start_index(pool.tick_current, pool.tick_spacing);
        assert!(starts.iter().any(|s| *s <= current));
        assert!(starts.iter().any(|s| *s > current));
        let mut unique = starts.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), starts.len());
        // A wider span never takes fewer.
        let wider = raydium_array_starts(&pool, Some(&extension), 4).unwrap();
        assert!(starts.iter().all(|s| wider.contains(s)));
    }

    #[test]
    fn only_a_sol_usdc_pool_prices_sol() {
        // 70 USDC a SOL is 0.07 USDC atoms a SOL atom.
        let sqrt = |per_atom: f64| (per_atom.sqrt() * 2f64.powi(64)) as u128;
        let direct = sol_usd(&WRAPPED_SOL_MINT, &USDC_MINT, sqrt(0.07)).unwrap();
        assert!((direct - 70.0).abs() < 1e-9, "{direct}");
        let inverse = sol_usd(&USDC_MINT, &WRAPPED_SOL_MINT, sqrt(1.0 / 0.07)).unwrap();
        assert!((inverse - 70.0).abs() < 1e-9, "{inverse}");
        let other = Pubkey::new_unique();
        assert_eq!(sol_usd(&WRAPPED_SOL_MINT, &other, sqrt(0.07)), None);
        assert_eq!(sol_usd(&other, &USDC_MINT, sqrt(0.07)), None);
    }

    #[test]
    fn only_the_two_pool_programs_are_known() {
        assert_eq!(
            PoolProgram::of_owner(&whirlpool::PROGRAM_ID),
            Some(PoolProgram::OrcaWhirlpool)
        );
        assert_eq!(
            PoolProgram::of_owner(&clmm::PROGRAM_ID),
            Some(PoolProgram::RaydiumClmm)
        );
        assert_eq!(PoolProgram::of_owner(&Pubkey::default()), None);
    }
}
