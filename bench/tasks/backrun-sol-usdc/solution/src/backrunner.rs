//! What the backrun-sol-usdc reference does on a trigger step, and what its
//! informed bot and the ladder's rungs share with it: read both pools
//! exactly, size the SOL→USDC→SOL cycle that profits most through both
//! curves, and send it as one atomic group of two swaps whose last limit is
//! the route's least profit. How much it tips is the [`Bidding`] it is
//! handed: the one thing the reference and the informed bot do differently.
//!
//! The pools are Orca's Whirlpool and Raydium's CLMM pool, both SOL/USDC
//! (SOL has 9 decimals and USDC 6), read through the kit's decoders and
//! quoted with its exact quotes, which reproduce each program's integer
//! math.

use mtx_kit::{
    AmountUnit, Pubkey, log,
    prelude::*,
    protocols::{orca::whirlpool, raydium::clmm},
    rpc, rpc_accounts, serde_json,
};

/// The share of the profit it expects that the reference tips: what
/// mainnet's winners paid the leader at the median, of their gross (NOTES.md §1: 0% in 7 of
/// 20, about 34% in 6, 50 to 80% in 4, 93% in 3), and what the week's top
/// winner, MriyaN, bid on every win. A principle, not a fit: it knows
/// nothing of the session's searchers. It outbids every searcher in the
/// trigger's slot that pays less per unit, a tip or a priority fee, and
/// keeps two thirds of every race it wins (DESIGN.md, "Calibration").
///
/// Here, beside the search every bot of the task shares, so the ladder's
/// rungs that bid as the reference does (`fixed-clip`) can't drift from it.
pub const TIP_SHARE: f64 = 0.34;
/// Jito's least tip, in lamports: a bundle that tips less is refused.
pub const MIN_TIP_LAMPORTS: u64 = 1_000;
/// What the group's two transactions pay in base fees, in SOL: the swaps'
/// and the tip's, 5,000 lamports a signature.
pub const FEE_SOL: f64 = 0.000_01;
/// The least profit it sends for, in SOL, after the fees: below it a gap is
/// inside the fee band for any size worth the trouble.
pub const MIN_PROFIT_SOL: f64 = 0.005;
/// The share of its SOL a cycle may put in.
const WALLET_SHARE: f64 = 0.95;
/// Golden-section steps over the cycle's size: 0.618^60 of a 1,500 SOL
/// range is far under an atom.
const SEARCH_STEPS: u32 = 60;
/// The least a cycle puts in, in atoms: 0.001 SOL.
const SOL_IN_MIN_ATOMS: f64 = 1e6;
/// Tick arrays a Raydium quote may need either way: a 1,000 SOL chunk
/// crosses two or three of its 60-tick arrays.
const CLMM_ARRAYS: usize = 16;
const SOL_ATOMS: f64 = 1e9;
const USDC_ATOMS: f64 = 1e6;
const CLOCK: &str = "SysvarC1ock11111111111111111111111111111111";

/// One pool as read on a step: what a quote through it needs.
pub enum Pool {
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

    /// USDC a SOL, before fees.
    pub fn mid(&self) -> f64 {
        (self.sqrt_price_x64() as f64 / 2f64.powi(64)).powi(2) * SOL_ATOMS / USDC_ATOMS
    }

    /// Atoms out for `amount` atoms in, exactly as the program computes
    /// them: SOL in (`sells`) or USDC in. `None` when the quote can't take
    /// all of it.
    pub fn out(&self, sells: bool, amount: u64) -> Option<u64> {
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

/// A cycle: `sol_in` atoms sold into pool `high`, the `usdc` atoms it gives
/// spent on pool `low`, which gives `sol_out` atoms back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cycle {
    pub high: usize,
    pub low: usize,
    pub sol_in: u64,
    pub usdc: u64,
    pub sol_out: u64,
}

impl Cycle {
    /// What it makes before tips and fees, in SOL.
    pub fn profit_sol(&self) -> f64 {
        (self.sol_out as f64 - self.sol_in as f64) / SOL_ATOMS
    }
}

/// The cycle of `sol_in` atoms through `pools`, from the dearer pool to the
/// cheaper.
pub fn cycle_of(pools: &[Pool; 2], sol_in: u64) -> Option<Cycle> {
    let (high, low) = match pools[0].mid() >= pools[1].mid() {
        true => (0, 1),
        false => (1, 0),
    };
    let usdc = pools[high].out(true, sol_in)?;
    let sol_out = pools[low].out(false, usdc)?;
    Some(Cycle {
        high,
        low,
        sol_in,
        usdc,
        sol_out,
    })
}

/// The cycle of at most `max_in` atoms that profits most, by golden-section
/// search: profit is concave in the size, as each pool's price moves
/// against the cycle while it trades. `None` when no size profits.
pub fn best(pools: &[Pool; 2], max_in: u64) -> Option<Cycle> {
    let profit = |sol_in: u64| {
        cycle_of(pools, sol_in).map_or(i128::MIN, |c| i128::from(c.sol_out) - i128::from(c.sol_in))
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
    cycle_of(pools, ((lo + hi) / 2.0) as u64).filter(|c| c.sol_out > c.sol_in)
}

/// What a trigger left to take: the best cycle, on the pools as read.
pub struct Opportunity<'a> {
    pub cycle: Cycle,
    pub pools: &'a [Pool; 2],
    /// The gap between the pools, in basis points.
    pub gap_bps: f64,
}

impl Opportunity<'_> {
    /// What the best cycle makes before tips and fees, in SOL.
    pub fn profit_sol(&self) -> f64 {
        self.cycle.profit_sol()
    }

    /// What a cycle of `sol` SOL would make on the same pools, in SOL: a
    /// rival that trades a fixed clip.
    pub fn profit_of_sol(&self, sol: f64) -> Option<f64> {
        cycle_of(self.pools, (sol * SOL_ATOMS) as u64).map(|c| c.profit_sol())
    }
}

/// How much to pay the leader for a cycle: a tip, in lamports, or `None`
/// to stay out.
pub trait Bidding {
    fn tip(&self, found: &Opportunity) -> Option<u64>;
}

/// The USDC `owner`'s token accounts hold, in USDC; `None` if it can't be
/// read.
fn usdc_held(owner: &str, mint: &str) -> Option<f64> {
    let params = serde_json::json!([owner, { "mint": mint }, { "encoding": "jsonParsed" }]);
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

/// The backrunner: the session's two pools, and its groups' ids.
#[derive(Default)]
pub struct Backrunner {
    /// Each pool's id as the session names it, its address, and whether
    /// it is a Whirlpool.
    pools: Vec<(String, Pubkey, bool)>,
    /// The session's key, and the USDC mint: whose USDC it spends.
    authority: String,
    usdc_mint: String,
    group: u32,
    /// A fixed clip in SOL in place of the best size: the ladder's
    /// fixed-clip rung, as one mainnet bot traded 200 SOL every time.
    pub clip_sol: Option<f64>,
}

impl Backrunner {
    /// The session's pools, from its venue metadata.
    pub fn start(&mut self, session: &Session) -> BotResult {
        let venue: serde_json::Value =
            serde_json::from_str(&session.venue).map_err(|e| e.to_string())?;
        for p in venue["pools"].as_array().into_iter().flatten() {
            let text = |name: &str| p[name].as_str().unwrap_or_default().to_string();
            let address = key(&text("address")).ok_or("a pool's address")?;
            let whirl = text("program") == whirlpool::PROGRAM_ID.to_string();
            self.pools.push((text("id"), address, whirl));
        }
        if self.pools.len() != 2 {
            return Err(format!("two pools to cycle through, not {}", self.pools.len()).into());
        }
        self.authority = session.authority.clone();
        self.usdc_mint = venue["pools"][0]["quote_mint"]
            .as_str()
            .ok_or("the pools' quote mint")?
            .to_string();
        Ok(())
    }

    /// Both pools as they are now.
    pub fn read_pools(&self) -> Option<[Pool; 2]> {
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

    /// On a trigger step, the best cycle, tipped as `bidding` says, as one
    /// atomic group; nothing on any other step.
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
        let max_in = (market.position.size.max(0.0) * WALLET_SHARE * SOL_ATOMS) as u64;
        let (mids, cheap) = (
            [pools[0].mid(), pools[1].mid()],
            pools[0].mid().min(pools[1].mid()),
        );
        let gap_bps = (mids[0].max(mids[1]) / cheap - 1.0) * 10_000.0;
        let sized = match self.clip_sol {
            Some(clip) => cycle_of(&pools, ((clip * SOL_ATOMS) as u64).min(max_in))
                .filter(|c| c.sol_out > c.sol_in),
            None => best(&pools, max_in),
        };
        let Some(cycle) = sized else {
            log(&format!(
                "step {}: {:?} {} SOL on {}, gap {gap_bps:.1} bp: no cycle pays",
                market.k, trigger.side, trigger.base, trigger.market
            ));
            return Ok(());
        };
        let found = Opportunity {
            cycle,
            pools: &pools,
            gap_bps,
        };
        let profit = found.profit_sol() - FEE_SOL;
        if profit < MIN_PROFIT_SOL {
            log(&format!(
                "step {}: gap {gap_bps:.1} bp, best cycle {profit:.5} SOL: too small",
                market.k
            ));
            return Ok(());
        }
        let Some(tip_lamports) = bidding.tip(&found) else {
            log(&format!(
                "step {}: gap {gap_bps:.1} bp, best cycle {profit:.5} SOL: stays out",
                market.k
            ));
            return Ok(());
        };
        let tip_lamports = tip_lamports.max(MIN_TIP_LAMPORTS);
        let tip_sol = tip_lamports as f64 / SOL_ATOMS;
        if tip_sol >= profit {
            return Ok(());
        }
        self.send(cycle, tip_lamports, orders);
        log(&format!(
            "step {} (slot {}): {:?} {} SOL on {}, gap {gap_bps:.1} bp: {:.3} SOL into {} then {}, profit {profit:.5} SOL, tip {tip_sol:.5}",
            market.k,
            market.slot,
            trigger.side,
            trigger.base,
            trigger.market,
            cycle.sol_in as f64 / SOL_ATOMS,
            self.pools[cycle.high].0,
            self.pools[cycle.low].0,
        ));
        Ok(())
    }

    /// The cycle as one atomic group of two swaps, a bundle with its tip:
    /// the sale sells SOL for exactly the USDC it was quoted, at no more
    /// than the SOL quoted, and the buy spends exactly that USDC (and any
    /// left from before) for at least the SOL sold, the tip and the fees
    /// back. Mandate v0.3.0 allows $5 of USDC after any transaction: when
    /// the pool has moved its way, the sale sells less SOL rather than
    /// leaving USDC behind.
    /// The group asks for the compute units its simulation used and a
    /// margin (the host's default): a bundle ranks by its bid per requested
    /// unit.
    fn send(&mut self, cycle: Cycle, tip_lamports: u64, orders: &mut Orders) {
        assert!(cycle.sol_out > cycle.sol_in, "a cycle that profits");
        let (sol, usdc) = (
            cycle.sol_in as f64 / SOL_ATOMS,
            cycle.usdc as f64 / USDC_ATOMS,
        );
        let tip_sol = tip_lamports as f64 / SOL_ATOMS;
        let sell_limit = usdc * (1.0 - 1e-9) / sol;
        // The buy's worst average price is the cycle's: spending the
        // surplus too at that price still brings back the SOL sold, the
        // tip and the fees.
        let buy_limit = usdc / (sol + tip_sol + FEE_SOL);
        let held = usdc_held(&self.authority, &self.usdc_mint).unwrap_or(0.0);
        let spent = usdc + held;
        assert!(sell_limit > 0.0 && buy_limit > 0.0);
        self.group += 1;
        let options = TxOptions {
            tip: Some(tip_lamports),
            atomic: Some(self.group),
            ..TxOptions::default()
        };
        orders
            .swap(Side::Ask, usdc, AmountUnit::Quote, sell_limit)
            .at(&self.pools[cycle.high].0)
            .tx(options);
        orders
            .swap(Side::Bid, spent, AmountUnit::Quote, buy_limit)
            .at(&self.pools[cycle.low].0)
            .atomic(self.group);
    }
}
