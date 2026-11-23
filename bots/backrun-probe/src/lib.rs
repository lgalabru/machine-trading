//! A probe of `backrun-sol-usdc` (RFD 0005, task 2): on a trigger step (a
//! whale's chunk has just landed, its slot still open), it reads both pools,
//! sizes the SOL→USDC→SOL cycle that profits most on the kit's exact
//! quotes, and sends it as one atomic group of two swaps:
//!
//! 1. sell SOL into the pool that prices it higher, for at least what the
//!    quote gives;
//! 2. buy SOL back from the other with exactly that USDC, for at least the
//!    SOL sold plus the tip and a little: the route's least profit.
//!
//! It tips half the profit it expects, as a Jito bundle, so a group that
//! loses the race fails unpaid. Off trigger steps it does nothing. Its log
//! says what became of every group.

use mtx_kit::{
    AmountUnit, Pubkey, log,
    prelude::*,
    protocols::{orca::whirlpool, raydium::clmm},
    rpc_accounts,
};
use serde_json::Value;

/// The share of the expected profit it tips.
const TIP_SHARE: f64 = 0.5;
/// What it keeps above the tip at least, in SOL: the group's fees.
const FEE_SOL: f64 = 0.0001;
/// The least profit it sends for, in SOL.
const MIN_PROFIT_SOL: f64 = 0.005;
/// The share of its SOL a cycle may sell.
const WALLET_SHARE: f64 = 0.95;
/// Golden-section steps over the cycle's size.
const SEARCH_STEPS: u32 = 60;
/// Tick arrays a Raydium quote may need either way.
const CLMM_ARRAYS: usize = 16;
const SOL_ATOMS: f64 = 1e9;
const USDC_ATOMS: f64 = 1e6;
const CLOCK: &str = "SysvarC1ock11111111111111111111111111111111";

/// One pool as read on a step: what a quote through it needs.
enum Pool {
    Whirlpool {
        address: Pubkey,
        pool: Box<whirlpool::Whirlpool>,
        arrays: Vec<whirlpool::TickArray>,
        timestamp_s: u64,
    },
    Raydium {
        address: Pubkey,
        pool: Box<clmm::Pool>,
        config: clmm::AmmConfig,
        // Boxed: its bitmaps are kilobytes, the rest of the state isn't.
        extension: Option<Box<clmm::BitmapExtension>>,
        arrays: Vec<clmm::TickArray>,
        timestamp_s: u64,
    },
}

impl Pool {
    fn sqrt_price_x64(&self) -> u128 {
        match self {
            Pool::Whirlpool { pool, .. } => pool.sqrt_price_x64,
            Pool::Raydium { pool, .. } => pool.sqrt_price_x64,
        }
    }

    /// USDC a SOL, before fees: SOL has 9 decimals and USDC 6.
    fn mid(&self) -> f64 {
        (self.sqrt_price_x64() as f64 / 2f64.powi(64)).powi(2) * 1e3
    }

    /// Atoms out for `amount` atoms in: SOL in (`sells`) or USDC in.
    fn out(&self, sells: bool, amount: u64) -> Option<u64> {
        match self {
            Pool::Whirlpool {
                address,
                pool,
                arrays,
                timestamp_s,
            } => {
                let request = whirlpool::SwapRequest {
                    direction: match sells {
                        true => whirlpool::SwapDirection::AToB,
                        false => whirlpool::SwapDirection::BToA,
                    },
                    mode: whirlpool::SwapMode::ExactIn,
                    amount,
                    sqrt_price_limit_x64: 0,
                    timestamp_s: *timestamp_s,
                };
                let quote = whirlpool::quote(address, pool, arrays, None, &request).ok()?;
                (quote.amount_in == amount).then_some(quote.amount_out)
            }
            Pool::Raydium {
                address,
                pool,
                config,
                extension,
                arrays,
                timestamp_s,
            } => {
                let request = clmm::SwapRequest {
                    direction: match sells {
                        true => clmm::SwapDirection::ZeroForOne,
                        false => clmm::SwapDirection::OneForZero,
                    },
                    mode: clmm::SwapMode::ExactIn,
                    amount,
                    sqrt_price_limit_x64: 0,
                    block_timestamp_s: *timestamp_s,
                };
                let quote = clmm::quote(
                    address,
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

fn read_whirlpool(address: Pubkey, timestamp_s: u64) -> Option<Pool> {
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
    Some(Pool::Whirlpool {
        address,
        pool: Box::new(pool),
        arrays,
        timestamp_s,
    })
}

fn read_raydium(address: Pubkey, timestamp_s: u64) -> Option<Pool> {
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
    Some(Pool::Raydium {
        address,
        pool: Box::new(pool),
        config,
        extension,
        arrays,
        timestamp_s,
    })
}

/// A cycle: SOL sold into `high`, USDC it gives, SOL bought back on `low`.
struct Cycle {
    high: usize,
    low: usize,
    sol_in: u64,
    usdc: u64,
    sol_out: u64,
}

/// The cycle of at most `max_in` atoms that profits most, by golden-section
/// search: profit is concave in the size, as each pool's price moves
/// against the cycle while it trades.
fn best(pools: &[Pool; 2], max_in: u64) -> Option<Cycle> {
    let (high, low) = match pools[0].mid() >= pools[1].mid() {
        true => (0, 1),
        false => (1, 0),
    };
    let cycle = |sol_in: u64| -> Option<Cycle> {
        let usdc = pools[high].out(true, sol_in)?;
        let sol_out = pools[low].out(false, usdc)?;
        Some(Cycle {
            high,
            low,
            sol_in,
            usdc,
            sol_out,
        })
    };
    let profit = |sol_in: u64| {
        cycle(sol_in).map_or(i128::MIN, |c| i128::from(c.sol_out) - i128::from(c.sol_in))
    };
    let ratio = (5f64.sqrt() - 1.0) / 2.0;
    let (mut lo, mut hi) = (SOL_ATOMS / 1_000.0, max_in as f64);
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
    cycle(((lo + hi) / 2.0) as u64).filter(|c| c.sol_out > c.sol_in)
}

#[derive(Default)]
struct Probe {
    /// Each pool's id as the session names it, its address, and whether
    /// it is a Whirlpool.
    pools: Vec<(String, Pubkey, bool)>,
    group: u32,
}

impl Probe {
    fn read_pools(&self) -> Option<[Pool; 2]> {
        let now = timestamp_s()?;
        let mut read: Vec<Pool> = Vec::with_capacity(2);
        for (_, address, whirl) in &self.pools {
            read.push(match whirl {
                true => read_whirlpool(*address, now)?,
                false => read_raydium(*address, now)?,
            });
        }
        read.try_into().ok()
    }
}

impl Strategy for Probe {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue: Value = serde_json::from_str(&session.venue).map_err(|e| e.to_string())?;
        for p in venue["pools"].as_array().into_iter().flatten() {
            let text = |name: &str| p[name].as_str().unwrap_or_default().to_string();
            let address = key(&text("address")).ok_or("a pool's address")?;
            let whirl = text("program") == whirlpool::PROGRAM_ID.to_string();
            self.pools.push((text("id"), address, whirl));
        }
        if self.pools.len() != 2 {
            return Err(format!("two pools to cycle through, not {}", self.pools.len()).into());
        }
        log(&format!(
            "pools {}, lookup table {}",
            self.pools
                .iter()
                .map(|(id, ..)| id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            venue["lookup_table"]
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        for send in &market.sends {
            log(&format!(
                "step {} sent: {:?} at {}: {:?} slot {:?} fee {:?} tip {:?}",
                send.step, send.order, send.venue, send.state, send.slot, send.fee, send.tip
            ));
        }
        let Some(trigger) = &market.trigger else {
            return Ok(());
        };
        let Some(pools) = self.read_pools() else {
            log(&format!("step {}: couldn't read the pools", market.k));
            return Ok(());
        };
        let held = market.position.size;
        let max_in = (held * WALLET_SHARE * SOL_ATOMS) as u64;
        let gap_bps = (pools[0].mid().max(pools[1].mid()) / pools[0].mid().min(pools[1].mid())
            - 1.0)
            * 10_000.0;
        let Some(cycle) = best(&pools, max_in) else {
            log(&format!(
                "step {}: trigger {} ({:?} {} SOL on {}), gap {gap_bps:.1} bp: no cycle pays",
                market.k, trigger.signature, trigger.side, trigger.base, trigger.market
            ));
            return Ok(());
        };
        let profit = (cycle.sol_out - cycle.sol_in) as f64 / SOL_ATOMS;
        if profit < MIN_PROFIT_SOL {
            log(&format!(
                "step {}: gap {gap_bps:.1} bp, best cycle {profit:.5} SOL: too small",
                market.k
            ));
            return Ok(());
        }
        let tip_sol = profit * TIP_SHARE;
        let (sol, usdc) = (
            cycle.sol_in as f64 / SOL_ATOMS,
            cycle.usdc as f64 / USDC_ATOMS,
        );
        // The sale gives exactly the USDC the buy spends, for at most the
        // SOL quoted (mandate v0.3.0 allows $5 of USDC after any
        // transaction: a better price sells less SOL rather than leaving
        // USDC behind); the buy must give back the SOL sold, the tip and the
        // fees.
        let sell_limit = usdc * (1.0 - 1e-9) / sol;
        let buy_limit = usdc / (sol + tip_sol + FEE_SOL);
        log(&format!(
            "step {} (slot {}): trigger {} ({:?} {} SOL on {} at slot {}), gap {gap_bps:.1} bp: cycle {sol:.4} SOL -> {usdc:.2} USDC -> {:.4} SOL on {} then {}, profit {profit:.5} SOL, tip {tip_sol:.5}",
            market.k,
            market.slot,
            trigger.signature,
            trigger.side,
            trigger.base,
            trigger.market,
            trigger.slot,
            cycle.sol_out as f64 / SOL_ATOMS,
            self.pools[cycle.high].0,
            self.pools[cycle.low].0,
        ));
        self.group += 1;
        let options = TxOptions {
            tip: Some((tip_sol * SOL_ATOMS) as u64),
            atomic: Some(self.group),
            ..TxOptions::default()
        };
        orders
            .swap(Side::Ask, usdc, AmountUnit::Quote, sell_limit)
            .at(&self.pools[cycle.high].0)
            .tx(options);
        // Built with `split` (`just backrun-smoke`'s check of mandate
        // v0.3.0's rule 11), the buy goes as a transaction of its own in
        // the same step: between the two the wallet holds the sale's USDC,
        // and the rule breaks.
        let buy_options = match cfg!(feature = "split") {
            true => TxOptions {
                tip: Some((tip_sol * SOL_ATOMS) as u64),
                ..TxOptions::default()
            },
            false => TxOptions {
                atomic: Some(self.group),
                ..TxOptions::default()
            },
        };
        orders
            .swap(Side::Bid, usdc, AmountUnit::Quote, buy_limit)
            .at(&self.pools[cycle.low].0)
            .tx(buy_options);
        Ok(())
    }
}

mtx_kit::export!(Probe);
