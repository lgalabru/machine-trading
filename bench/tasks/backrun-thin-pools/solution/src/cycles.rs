//! What the backrun-thin-pools reference does on a trigger step, and what
//! its informed bot and the ladder's rungs share with it: read every pool
//! of the session exactly, list the cycles from SOL back to SOL through the
//! graph of pools, skip those whose fees eat the gap, size the rest on the
//! chained exact quotes, and send the best as one atomic group of swaps
//! whose last limit is the route's least profit. How many legs it looks at
//! and how much it tips are what it is handed: the reference looks at two
//! and three, the informed bot at up to four.
//!
//! The pools are Orca Whirlpools and Raydium CLMM pools, read through the
//! kit's decoders and quoted with its exact quotes, which reproduce each
//! program's integer math. Each token's decimals come from its mint.

use mtx_kit::{
    AmountUnit, Pubkey, log,
    prelude::*,
    protocols::{orca::whirlpool, raydium::clmm},
    rpc, rpc_accounts, serde_json,
};

/// Jito's least tip, in lamports: a bundle that tips less is refused.
pub const MIN_TIP_LAMPORTS: u64 = 1_000;
/// What the group's two transactions pay in base fees, in SOL: the swaps'
/// and the tip's, 5,000 lamports a signature.
pub const FEE_SOL: f64 = 0.000_01;
/// The least profit it sends for, in SOL, after the fees: about 14 cents,
/// a sliver of a thin pool's backrun (the research's detector fired above
/// 0.01 SOL; the searchers' least is 0.002 to 0.02).
pub const MIN_PROFIT_SOL: f64 = 0.002;
/// The most legs a cycle has: the research's detector enumerated up to 4.
pub const LEGS_MAX: usize = 4;
/// The most pools a session lists (the grader's `POOLS_MAX`), and the most
/// cycles their graph holds.
const POOLS_MAX: usize = 12;
const CYCLES_MAX: usize = 4_096;
/// The share of its SOL a cycle may put in.
const WALLET_SHARE: f64 = 0.95;
/// Golden-section steps over the cycle's size: 0.618^60 of a 500 SOL range
/// is far under an atom.
const SEARCH_STEPS: u32 = 60;
/// The least a cycle puts in, in atoms: 0.001 SOL.
const SOL_IN_MIN_ATOMS: f64 = 1e6;
/// Tick arrays a Raydium quote may need either way: a 5,000 bp move on a
/// thin pool crosses a few of them.
const CLMM_ARRAYS: usize = 16;
/// How much less than its quote each leg but the last may give: none, to
/// the last part in a billion. A leg spends exactly what the one before it
/// was quoted to give.
const LEG_SLACK: f64 = 1e-9;
/// Significant digits a limit price keeps.
const LIMIT_DIGITS: i32 = 12;
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const CLOCK: &str = "SysvarC1ock11111111111111111111111111111111";

/// Which way a leg goes through its pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    /// Pays the base, receives the quote: a sale.
    BaseIn,
    /// Pays the quote, receives the base: a buy.
    QuoteIn,
}

/// One pool's state as read on a step: what a quote through it needs.
pub enum State {
    Whirlpool {
        pool: Box<whirlpool::Whirlpool>,
        arrays: Vec<whirlpool::TickArray>,
    },
    Raydium {
        pool: Box<clmm::Pool>,
        config: clmm::AmmConfig,
        // Boxed: its bitmaps are kilobytes, the rest of the state isn't.
        extension: Option<Box<clmm::BitmapExtension>>,
        arrays: Vec<clmm::TickArray>,
    },
}

/// One pool of the session: its id, address, tokens and fee, and its
/// state on this step.
pub struct Pool {
    pub id: String,
    pub address: Pubkey,
    pub base: Pubkey,
    pub quote: Pubkey,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub state: State,
    pub timestamp_s: u64,
}

impl Pool {
    fn sqrt_price_x64(&self) -> u128 {
        match &self.state {
            State::Whirlpool { pool, .. } => pool.sqrt_price_x64,
            State::Raydium { pool, .. } => pool.sqrt_price_x64,
        }
    }

    /// Quote units a base unit, before fees.
    pub fn mid(&self) -> f64 {
        let decimals = i32::from(self.base_decimals) - i32::from(self.quote_decimals);
        (self.sqrt_price_x64() as f64 / 2f64.powi(64)).powi(2) * 10f64.powi(decimals)
    }

    /// The fee, a fraction of what goes in.
    pub fn fee(&self) -> f64 {
        let millionths = match &self.state {
            State::Whirlpool { pool, .. } => u32::from(pool.fee_rate),
            State::Raydium { config, .. } => config.trade_fee_rate,
        };
        f64::from(millionths) / 1e6
    }

    /// The token a leg pays, and the one it receives.
    pub fn tokens(&self, way: Way) -> (Pubkey, Pubkey) {
        match way {
            Way::BaseIn => (self.base, self.quote),
            Way::QuoteIn => (self.quote, self.base),
        }
    }

    fn decimals(&self, token: &Pubkey) -> u8 {
        match *token == self.base {
            true => self.base_decimals,
            false => self.quote_decimals,
        }
    }

    /// Atoms out for `amount` atoms in, exactly as the program computes
    /// them. `None` when the pool can't take all of it.
    pub fn out(&self, way: Way, amount: u64) -> Option<u64> {
        match &self.state {
            State::Whirlpool { pool, arrays } => {
                let request = whirlpool::SwapRequest {
                    direction: match way {
                        Way::BaseIn => whirlpool::SwapDirection::AToB,
                        Way::QuoteIn => whirlpool::SwapDirection::BToA,
                    },
                    mode: whirlpool::SwapMode::ExactIn,
                    amount,
                    sqrt_price_limit_x64: 0,
                    timestamp_s: self.timestamp_s,
                };
                let quote = whirlpool::quote(&self.address, pool, arrays, None, &request).ok()?;
                (quote.amount_in == amount).then_some(quote.amount_out)
            }
            State::Raydium {
                pool,
                config,
                extension,
                arrays,
            } => {
                let request = clmm::SwapRequest {
                    direction: match way {
                        Way::BaseIn => clmm::SwapDirection::ZeroForOne,
                        Way::QuoteIn => clmm::SwapDirection::OneForZero,
                    },
                    mode: clmm::SwapMode::ExactIn,
                    amount,
                    sqrt_price_limit_x64: 0,
                    block_timestamp_s: self.timestamp_s,
                };
                let quote = clmm::quote(
                    &self.address,
                    pool,
                    config,
                    arrays,
                    extension.as_deref(),
                    &request,
                )
                .ok()?;
                (quote.amount_in == amount).then_some(quote.amount_out)
            }
        }
    }
}

/// One leg: a pool and the way through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg {
    pub pool: usize,
    pub way: Way,
}

/// A cycle quoted on the pools: `amounts[i]` atoms go into leg `i`, and
/// `amounts[legs.len()]` atoms of SOL come out of the last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    pub legs: Vec<Leg>,
    pub amounts: Vec<u64>,
}

impl Cycle {
    pub fn sol_in(&self) -> u64 {
        self.amounts[0]
    }

    pub fn sol_out(&self) -> u64 {
        *self.amounts.last().expect("a quoted cycle")
    }

    /// What it makes before tips and fees, in SOL.
    pub fn profit_sol(&self) -> f64 {
        (self.sol_out() as f64 - self.sol_in() as f64) / 1e9
    }
}

/// Every simple cycle through `sol` of two to [`LEGS_MAX`] legs: no pool
/// twice, no token but SOL twice.
pub fn cycles(pools: &[Pool], sol: &Pubkey) -> Vec<Vec<Leg>> {
    let mut out = Vec::new();
    let mut path = Vec::with_capacity(LEGS_MAX);
    extend(pools, sol, sol, &mut path, &mut out);
    assert!(out.len() <= CYCLES_MAX);
    out
}

fn extend(pools: &[Pool], sol: &Pubkey, at: &Pubkey, path: &mut Vec<Leg>, out: &mut Vec<Vec<Leg>>) {
    if path.len() == LEGS_MAX {
        return;
    }
    for (i, pool) in pools.iter().enumerate() {
        if path.iter().any(|leg| leg.pool == i) {
            continue;
        }
        for way in [Way::BaseIn, Way::QuoteIn] {
            let (pays, receives) = pool.tokens(way);
            if pays != *at {
                continue;
            }
            let leg = Leg { pool: i, way };
            if receives == *sol {
                if !path.is_empty() {
                    let mut cycle = path.clone();
                    cycle.push(leg);
                    out.push(cycle);
                }
                continue;
            }
            let visited = path
                .iter()
                .any(|l| pools[l.pool].tokens(l.way).1 == receives);
            if !visited {
                path.push(leg);
                extend(pools, sol, &receives, path, out);
                path.pop();
            }
        }
    }
}

/// The product of each leg's price after its fee: what a SOL becomes
/// through the cycle at its first atom. At most 1, no size profits.
pub fn marginal_rate(pools: &[Pool], legs: &[Leg]) -> f64 {
    legs.iter().fold(1.0, |rate, leg| {
        let pool = &pools[leg.pool];
        let price = match leg.way {
            Way::BaseIn => pool.mid(),
            Way::QuoteIn => 1.0 / pool.mid(),
        };
        rate * price * (1.0 - pool.fee())
    })
}

/// `legs` quoted for `sol_in` atoms, each leg exact in for what the one
/// before gives.
pub fn quote(pools: &[Pool], legs: &[Leg], sol_in: u64) -> Option<Cycle> {
    let mut amounts = Vec::with_capacity(legs.len() + 1);
    amounts.push(sol_in);
    let mut amount = sol_in;
    for leg in legs {
        amount = pools[leg.pool].out(leg.way, amount)?;
        amounts.push(amount);
    }
    Some(Cycle {
        legs: legs.to_vec(),
        amounts,
    })
}

/// The size of `legs` that profits most of at most `max_in` atoms, by
/// golden-section search: profit is concave in the size, as each pool's
/// price moves against the cycle while it trades. `None` when none profits.
pub fn sized(pools: &[Pool], legs: &[Leg], max_in: u64) -> Option<Cycle> {
    let profit = |sol_in: u64| {
        quote(pools, legs, sol_in).map_or(i128::MIN, |c| {
            i128::from(c.sol_out()) - i128::from(c.sol_in())
        })
    };
    let ratio = (5f64.sqrt() - 1.0) / 2.0;
    let (mut lo, mut hi) = (SOL_IN_MIN_ATOMS, max_in as f64);
    if hi <= lo {
        return None;
    }
    for _ in 0..SEARCH_STEPS {
        let a = hi - ratio * (hi - lo);
        let b = lo + ratio * (hi - lo);
        if profit(a as u64) >= profit(b as u64) {
            hi = b;
        } else {
            lo = a;
        }
    }
    quote(pools, legs, ((lo + hi) / 2.0) as u64).filter(|c| c.sol_out() > c.sol_in())
}

/// The cycle of at most `max_legs` legs that profits most, of at most
/// `max_in` atoms.
pub fn best(pools: &[Pool], all: &[Vec<Leg>], max_legs: usize, max_in: u64) -> Option<Cycle> {
    assert!((2..=LEGS_MAX).contains(&max_legs));
    all.iter()
        .filter(|legs| legs.len() <= max_legs && marginal_rate(pools, legs) > 1.0)
        .filter_map(|legs| sized(pools, legs, max_in))
        .max_by_key(|c| i128::from(c.sol_out()) - i128::from(c.sol_in()))
}

/// `price` to [`LIMIT_DIGITS`] significant digits, up or down: the seat
/// reads a limit as the decimal it prints as, and a price of twenty digits
/// times an amount of ten overflows its exact comparison. The rounding is
/// a part in a hundred billion, far inside [`LEG_SLACK`]'s room on a
/// middle leg and a lamport's on the last.
fn significant(price: f64, up: bool) -> f64 {
    assert!(price.is_finite() && price > 0.0);
    let scale = 10f64.powi(LIMIT_DIGITS - 1 - price.log10().floor() as i32);
    let scaled = price * scale;
    let rounded = match up {
        true => scaled.ceil(),
        false => scaled.floor(),
    } / scale;
    assert!(rounded > 0.0);
    rounded
}

/// What a trigger left to take: the best cycle, on the pools as read.
pub struct Opportunity<'a> {
    pub cycle: Cycle,
    pub pools: &'a [Pool],
    pub cycles: &'a [Vec<Leg>],
    pub max_in: u64,
}

impl Opportunity<'_> {
    /// What the best cycle makes before tips and fees, in SOL.
    pub fn profit_sol(&self) -> f64 {
        self.cycle.profit_sol()
    }

    /// What the best cycle of at most `legs` legs makes, in SOL: what a
    /// rival that looks no deeper expects.
    pub fn profit_within(&self, legs: usize) -> Option<f64> {
        match self.cycle.legs.len() <= legs {
            true => Some(self.profit_sol()),
            false => best(self.pools, self.cycles, legs, self.max_in).map(|c| c.profit_sol()),
        }
    }
}

/// How much to pay the leader for a cycle: a tip, in lamports, or `None`
/// to stay out.
pub trait Bidding {
    fn tip(&self, found: &Opportunity) -> Option<u64>;
}

/// What `owner`'s token accounts hold of `mint`, in its units; `None` if
/// it can't be read.
fn held(owner: &str, mint: &Pubkey) -> Option<f64> {
    let params =
        serde_json::json!([owner, { "mint": mint.to_string() }, { "encoding": "jsonParsed" }]);
    let text = rpc("getTokenAccountsByOwner", &params.to_string()).ok()?;
    let found: serde_json::Value = serde_json::from_str(&text).ok()?;
    let held: f64 = found["value"]
        .as_array()?
        .iter()
        .filter_map(|a| a["account"]["data"]["parsed"]["info"]["tokenAmount"]["uiAmount"].as_f64())
        .sum();
    (held.is_finite() && held >= 0.0).then_some(held)
}

fn key(text: &str) -> Option<Pubkey> {
    text.parse().ok()
}

fn read(keys: &[Pubkey]) -> Option<Vec<Option<Vec<u8>>>> {
    let texts: Vec<String> = keys.iter().map(ToString::to_string).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    rpc_accounts(&refs).ok()
}

fn timestamp_s() -> Option<u64> {
    let data = read(&[key(CLOCK)?])?.pop()??;
    let seconds = i64::from_le_bytes(data.get(32..40)?.try_into().ok()?);
    u64::try_from(seconds).ok()
}

fn read_whirlpool(address: Pubkey) -> Option<State> {
    let data = read(&[address])?.pop()??;
    let pool = whirlpool::Whirlpool::decode(&data).ok()?;
    let mut found: Vec<(i32, Pubkey)> = Vec::new();
    for direction in [
        whirlpool::SwapDirection::AToB,
        whirlpool::SwapDirection::BToA,
    ] {
        for array in whirlpool::tick_array_addresses(&address, &pool, direction) {
            if !found.contains(&array) {
                found.push(array);
            }
        }
    }
    let keys: Vec<Pubkey> = found.iter().map(|(_, k)| *k).collect();
    let accounts = read(&keys)?;
    let mut arrays = Vec::with_capacity(found.len());
    for ((start, _), account) in found.iter().zip(accounts) {
        arrays.push(match account {
            Some(data) => whirlpool::TickArray::decode(&data).ok()?,
            None => whirlpool::TickArray::uninitialized(address, *start),
        });
    }
    Some(State::Whirlpool {
        pool: Box::new(pool),
        arrays,
    })
}

fn read_raydium(address: Pubkey) -> Option<State> {
    let data = read(&[address])?.pop()??;
    let pool = clmm::Pool::decode(&data).ok()?;
    let extension_key = clmm::ix::bitmap_extension_address(&address);
    let mut found = read(&[pool.amm_config, extension_key])?;
    let extension = match found.pop()? {
        Some(data) => Some(Box::new(clmm::BitmapExtension::decode(&data).ok()?)),
        None => None,
    };
    let config = clmm::AmmConfig::decode(&found.pop()??).ok()?;
    let mut starts: Vec<i32> = Vec::new();
    for direction in [
        clmm::SwapDirection::ZeroForOne,
        clmm::SwapDirection::OneForZero,
    ] {
        let arrays =
            clmm::tick_arrays_in_direction(&pool, extension.as_deref(), direction, CLMM_ARRAYS)
                .ok()?;
        for start in arrays {
            if !starts.contains(&start) {
                starts.push(start);
            }
        }
    }
    let keys: Vec<Pubkey> = starts
        .iter()
        .map(|s| clmm::ix::tick_array_address(&address, *s))
        .collect();
    let arrays = read(&keys)?
        .into_iter()
        .flatten()
        .map(|data| clmm::TickArray::decode(&data).ok())
        .collect::<Option<Vec<_>>>()?;
    Some(State::Raydium {
        pool: Box::new(pool),
        config,
        extension,
        arrays,
    })
}

/// A mint's decimals: byte 44 of an SPL or Token-2022 mint.
fn decimals(mint: &Pubkey) -> Option<u8> {
    read(&[*mint])?.pop()??.get(44).copied()
}

/// What a pool is, read once from the session's venue metadata and the
/// mints.
struct PoolKeys {
    id: String,
    address: Pubkey,
    whirlpool: bool,
    base: Pubkey,
    quote: Pubkey,
    base_decimals: u8,
    quote_decimals: u8,
}

/// The backrunner: the session's pools, their cycles' shape, and its
/// groups' ids.
pub struct Backrunner {
    pools: Vec<PoolKeys>,
    sol: Pubkey,
    /// The session's key: whose tokens a cycle spends.
    authority: String,
    group: u32,
    /// The most legs a cycle of its has.
    pub max_legs: usize,
}

impl Backrunner {
    pub fn new(max_legs: usize) -> Self {
        assert!((2..=LEGS_MAX).contains(&max_legs));
        Self {
            pools: Vec::new(),
            sol: key(SOL_MINT).expect("SOL's mint"),
            authority: String::new(),
            group: 0,
            max_legs,
        }
    }

    /// The session's pools, from its venue metadata, and their tokens'
    /// decimals, from the mints.
    pub fn start(&mut self, session: &Session) -> BotResult {
        let venue: serde_json::Value =
            serde_json::from_str(&session.venue).map_err(|e| e.to_string())?;
        for p in venue["pools"].as_array().into_iter().flatten() {
            let text = |name: &str| p[name].as_str().unwrap_or_default().to_string();
            let address = key(&text("address")).ok_or("a pool's address")?;
            let base = key(&text("base_mint")).ok_or("a pool's base mint")?;
            let quote = key(&text("quote_mint")).ok_or("a pool's quote mint")?;
            self.pools.push(PoolKeys {
                id: text("id"),
                address,
                whirlpool: text("program") == whirlpool::PROGRAM_ID.to_string(),
                base,
                quote,
                base_decimals: decimals(&base).ok_or("a mint's decimals")?,
                quote_decimals: decimals(&quote).ok_or("a mint's decimals")?,
            });
        }
        if !(2..=POOLS_MAX).contains(&self.pools.len()) {
            return Err(format!("2 to {POOLS_MAX} pools, not {}", self.pools.len()).into());
        }
        self.authority = session.authority.clone();
        Ok(())
    }

    /// Every pool as it is now.
    pub fn read_pools(&self) -> Option<Vec<Pool>> {
        let now = timestamp_s()?;
        let mut read = Vec::with_capacity(self.pools.len());
        for keys in &self.pools {
            let state = match keys.whirlpool {
                true => read_whirlpool(keys.address)?,
                false => read_raydium(keys.address)?,
            };
            read.push(Pool {
                id: keys.id.clone(),
                address: keys.address,
                base: keys.base,
                quote: keys.quote,
                base_decimals: keys.base_decimals,
                quote_decimals: keys.quote_decimals,
                state,
                timestamp_s: now,
            });
        }
        Some(read)
    }

    /// On a trigger step, the best cycle of at most [`Self::max_legs`]
    /// legs, tipped as `bidding` says, as one atomic group; nothing on any
    /// other step.
    pub fn step(
        &mut self,
        market: &Market,
        orders: &mut Orders,
        bidding: &dyn Bidding,
    ) -> BotResult {
        for send in market.sends.iter().filter(|s| s.order.is_some()) {
            log(&format!(
                "step {} sent at {}: {:?} slot {:?} tip {:?}",
                send.step, send.venue, send.state, send.slot, send.tip
            ));
        }
        let Some(trigger) = &market.trigger else {
            return Ok(());
        };
        let Some(pools) = self.read_pools() else {
            log(&format!("step {}: couldn't read the pools", market.k));
            return Ok(());
        };
        let all = cycles(&pools, &self.sol);
        let max_in = (market.position.size.max(0.0) * WALLET_SHARE * 1e9) as u64;
        let Some(cycle) = best(&pools, &all, self.max_legs, max_in) else {
            log(&format!(
                "step {}: {:?} {} on {}: no cycle pays",
                market.k, trigger.side, trigger.base, trigger.market
            ));
            return Ok(());
        };
        let found = Opportunity {
            cycle,
            pools: &pools,
            cycles: &all,
            max_in,
        };
        let profit = found.profit_sol() - FEE_SOL;
        if profit < MIN_PROFIT_SOL {
            log(&format!(
                "step {}: best cycle {profit:.5} SOL: too small",
                market.k
            ));
            return Ok(());
        }
        let Some(tip_lamports) = bidding.tip(&found) else {
            log(&format!(
                "step {}: best cycle {profit:.5} SOL: stays out",
                market.k
            ));
            return Ok(());
        };
        let tip_lamports = tip_lamports.max(MIN_TIP_LAMPORTS);
        let tip_sol = tip_lamports as f64 / 1e9;
        if tip_sol >= profit {
            return Ok(());
        }
        let route: Vec<&str> = found
            .cycle
            .legs
            .iter()
            .map(|l| pools[l.pool].id.as_str())
            .collect();
        log(&format!(
            "step {} (slot {}): {:?} on {}: {:.4} SOL through {route:?}, profit {profit:.5} SOL, tip {tip_sol:.5}",
            market.k,
            market.slot,
            trigger.side,
            trigger.market,
            found.cycle.sol_in() as f64 / 1e9,
        ));
        self.send(&pools, &found.cycle, tip_lamports, orders);
        Ok(())
    }

    /// The cycle as one atomic group of swaps, a bundle with its tip. Each
    /// leg spends exactly what the one before it was quoted to give, at no
    /// less than that quote; the last gives back at least the SOL put in,
    /// the tip and the fees. The group asks for the compute units its
    /// simulation used and a margin (the host's default): a bundle ranks by
    /// its bid per requested unit.
    fn send(&mut self, pools: &[Pool], cycle: &Cycle, tip_lamports: u64, orders: &mut Orders) {
        assert!(cycle.sol_out() > cycle.sol_in(), "a cycle that profits");
        assert_eq!(cycle.amounts.len(), cycle.legs.len() + 1);
        let tip_sol = tip_lamports as f64 / 1e9;
        let last = cycle.legs.len() - 1;
        self.group += 1;
        for (n, leg) in cycle.legs.iter().enumerate() {
            let pool = &pools[leg.pool];
            let (pays, receives) = pool.tokens(leg.way);
            let units =
                |atoms: u64, token: &Pubkey| atoms as f64 / 10f64.powi(pool.decimals(token).into());
            let paid = units(cycle.amounts[n], &pays);
            let received = match n == last {
                true => (cycle.sol_in() as f64 / 1e9) + tip_sol + FEE_SOL,
                false => units(cycle.amounts[n + 1], &receives) * (1.0 - LEG_SLACK),
            };
            assert!(paid > 0.0 && received > 0.0);
            // A sale's limit is the least quote a base, rounded down; a
            // buy's the most, rounded up. Mandate v0.3.0 allows $5 of
            // anything but SOL after any transaction, so no leg may leave a
            // token behind: the first leg takes exactly the quoted amount of
            // the token it buys, paying at most the SOL quoted (and its
            // slack), so a better price sells less SOL; a later leg spends
            // exactly what the leg before quoted, and whatever an earlier
            // cycle left of the token, at the same worst price.
            let (sale, buy) = (
                significant(received / paid, false),
                significant(paid / received, true),
            );
            let line = match (n, leg.way) {
                (0, Way::BaseIn) => orders.swap(
                    Side::Ask,
                    units(cycle.amounts[1], &receives),
                    AmountUnit::Quote,
                    sale,
                ),
                (0, Way::QuoteIn) => orders.swap(
                    Side::Bid,
                    units(cycle.amounts[1], &receives),
                    AmountUnit::Base,
                    buy,
                ),
                (_, way) => {
                    let spent = paid + held(&self.authority, &pays).unwrap_or(0.0);
                    match way {
                        Way::BaseIn => orders.swap(Side::Ask, spent, AmountUnit::Base, sale),
                        Way::QuoteIn => orders.swap(Side::Bid, spent, AmountUnit::Quote, buy),
                    }
                }
            };
            let line = line.at(&pool.id);
            match n {
                0 => {
                    line.tx(TxOptions {
                        tip: Some(tip_lamports),
                        atomic: Some(self.group),
                        ..TxOptions::default()
                    });
                }
                _ => {
                    line.atomic(self.group);
                }
            }
        }
        assert_eq!(
            pools[cycle.legs[last].pool].tokens(cycle.legs[last].way).1,
            self.sol
        );
        assert_eq!(
            pools[cycle.legs[0].pool].tokens(cycle.legs[0].way).0,
            self.sol
        );
    }
}
