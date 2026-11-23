//! What an `amm` scenario says of its actors (`venues.amm`): the whale and
//! its chunks, the calm flow, and the two kinds of searcher. Every range's
//! default is the research's (`docs/RESEARCH.md`, "SOL/USDC backruns on
//! concentrated pools", and `ops/research/atomic-arb/clmm-backrun/NOTES.md`).

use mtx_core::mandate::InstrumentRef;
use serde::{Deserialize, Serialize};

/// Most accounts a whale works its orders from: the research's bursts came
/// from one signer each, and the week's top 200 from 13.
pub const WHALE_ACCOUNTS_MAX: u32 = 16;
/// Most chunks a session's whale sends: one every 30 slots over the longest
/// session a scenario runs is far fewer.
pub const CHUNKS_MAX: usize = 1_000;
/// The largest chunk, in SOL: past the research's largest trigger (1,286
/// SOL), short of what the cloned tick arrays hold.
pub const CHUNK_SOL_MAX: f64 = 2_000.0;
/// Swaps a minute a pool's calm flow may have: one a slot at most.
pub const SWAPS_PER_MINUTE_MAX: f64 = 150.0;
/// The largest calm swap, in SOL: the research's largest was 280 SOL.
pub const FLOW_SOL_MAX: f64 = 1_000.0;
/// The longest a searcher's send takes to reach a leader, in slots: the
/// research's late backruns landed within a slot or two.
pub const SEARCHER_LATENCY_SLOTS_MAX: u64 = 8;
/// The largest fixed cycle, in SOL.
pub const FIXED_SOL_MAX: f64 = 5_000.0;
/// The largest move a chunk may aim for, in basis points: past the
/// research's p90 (5,877 bp), short of what twelve cloned tick arrays a
/// side hold on the thinnest pool.
pub const CHUNK_IMPACT_BPS_MAX: f64 = 10_000.0;

/// The side of a whale's parent order: it sells or buys the pool's base.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WhaleSide {
    Sell,
    Buy,
}

/// The trader working large orders into one pool, a chunk at a time: each
/// chunk is a trigger. The research's bursts sold 6 chunks of about 1,000
/// SOL, and 14 of 500, every 30 to 440 slots, into the thinner pool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Whale {
    /// The accounts the chunks go out from, shared by its orders: each
    /// chunk's is drawn from the session's seed.
    pub accounts: u32,
    pub orders: Vec<ParentOrder>,
}

/// A parent order: one pool, one side, its chunks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ParentOrder {
    /// The pool, as `venues.amm.pools` names it.
    pub pool: String,
    pub side: WhaleSide,
    pub chunks: Vec<Chunk>,
}

/// One chunk: the slot it lands in, from the session's start, and its size
/// in SOL, or in SOL's worth of the pool's base where the base isn't SOL
/// (at the session's reference prices).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    pub slot: u64,
    pub sol: f64,
    /// How far the chunk moves the pool's price, in basis points: a sale
    /// divides it by `1 + impact`, a buy multiplies it. `sol` is then the
    /// most it trades, and the program stops the swap at the price. A thin
    /// pool's trigger is its impact (the research's median 545 bp), not
    /// its size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact_bps: Option<f64>,
}

impl Whale {
    /// Every chunk, in the order it lands, with its order's index.
    pub fn chunks(&self) -> Vec<(usize, Chunk)> {
        let mut all: Vec<(usize, Chunk)> = self
            .orders
            .iter()
            .enumerate()
            .flat_map(|(i, order)| order.chunks.iter().map(move |chunk| (i, *chunk)))
            .collect();
        all.sort_by_key(|(_, chunk)| chunk.slot);
        all
    }

    /// One to [`WHALE_ACCOUNTS_MAX`] accounts; orders on the session's
    /// pools; chunks of a positive size up to [`CHUNK_SOL_MAX`], each in a
    /// slot of its own inside the session (which the steps check again).
    pub fn validate(&self, pools: &[InstrumentRef], duration_slots: u64) -> Result<(), String> {
        if self.accounts == 0 || self.accounts > WHALE_ACCOUNTS_MAX {
            return Err(format!("amm.whale: accounts is 1 to {WHALE_ACCOUNTS_MAX}"));
        }
        for order in &self.orders {
            let pool: InstrumentRef = order.pool.parse().map_err(|e| format!("amm.whale: {e}"))?;
            if !pools.contains(&pool) {
                return Err(format!(
                    "amm.whale: {} isn't one of the session's pools",
                    order.pool
                ));
            }
            for chunk in &order.chunks {
                if !(chunk.sol.is_finite() && chunk.sol > 0.0 && chunk.sol <= CHUNK_SOL_MAX) {
                    return Err(format!(
                        "amm.whale: a chunk is 0 to {CHUNK_SOL_MAX} SOL, not {}",
                        chunk.sol
                    ));
                }
                if chunk.impact_bps.is_some_and(|bps| {
                    !(bps.is_finite() && bps > 0.0 && bps <= CHUNK_IMPACT_BPS_MAX)
                }) {
                    return Err(format!(
                        "amm.whale: a chunk's impact is 0 to {CHUNK_IMPACT_BPS_MAX} bp, not {:?}",
                        chunk.impact_bps
                    ));
                }
            }
        }
        let chunks = self.chunks();
        if chunks.len() > CHUNKS_MAX {
            return Err(format!("amm.whale: at most {CHUNKS_MAX} chunks"));
        }
        let slots: Vec<u64> = chunks.iter().map(|(_, c)| c.slot).collect();
        crate::trigger::schedule(duration_slots, &slots)
            .map(|_| ())
            .map_err(|reason| format!("amm.whale: {reason}"))
    }
}

/// The calm flow on each pool, between chunks: small swaps of either side
/// at a pool's rate, ordinary transactions in the auction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Background {
    pub pools: Vec<PoolFlow>,
}

/// One pool's calm flow. A swap's size is log-normal through its median
/// and 90th percentile, capped at `max_sol`; the research's calm Saturday:
/// Whirlpool 15 a minute (median 3.4 SOL, p90 41, largest 280), Raydium 12
/// (median 1.1, p90 7.5, largest 71).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PoolFlow {
    pub pool: String,
    pub swaps_per_minute: f64,
    pub median_sol: f64,
    pub p90_sol: f64,
    pub max_sol: f64,
}

impl Background {
    pub fn validate(&self, pools: &[InstrumentRef]) -> Result<(), String> {
        for (i, flow) in self.pools.iter().enumerate() {
            let pool: InstrumentRef = flow
                .pool
                .parse()
                .map_err(|e| format!("amm.background: {e}"))?;
            if !pools.contains(&pool) || self.pools[..i].iter().any(|f| f.pool == flow.pool) {
                return Err(format!(
                    "amm.background: {} isn't one of the session's pools, or is listed twice",
                    flow.pool
                ));
            }
            let rate_ok = flow.swaps_per_minute.is_finite()
                && flow.swaps_per_minute >= 0.0
                && flow.swaps_per_minute <= SWAPS_PER_MINUTE_MAX;
            let sizes_ok = flow.median_sol > 0.0
                && flow.p90_sol >= flow.median_sol
                && flow.max_sol >= flow.median_sol
                && flow.max_sol <= FLOW_SOL_MAX;
            if !(rate_ok && sizes_ok) {
                return Err(format!(
                    "amm.background: {}'s flow needs 0 to {SWAPS_PER_MINUTE_MAX} swaps a minute, and 0 < median <= p90, median <= max <= {FLOW_SOL_MAX} SOL",
                    flow.pool
                ));
            }
        }
        Ok(())
    }
}

/// How a searcher pays the leader for its place.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Pays {
    /// A compute unit price, so the transaction ranks by it.
    PriorityFee,
    /// A Jito tip, as a bundle.
    Tip,
}

/// A searcher that closes a gap between the session's pools with one atomic
/// cycle from SOL back to SOL: on two pools of one pair, sell SOL where it
/// is dear and buy it back where it is cheap; on a graph, the cycle of at
/// most `max_legs` legs that profits most ([`super::cycle`]). The
/// research's winners took 0.86 to 1.10 of the optimum and paid 0 to 93% of
/// it to the leader; one always traded a fixed 200 SOL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CycleSearcher {
    /// Slots its send takes to reach a leader, drawn in this range for each
    /// send: 0 lands in the slot it saw the gap in.
    pub latency_slots: [u64; 2],
    /// The share of the profit it expects that it pays the leader.
    pub bid_share: f64,
    pub pays: Pays,
    /// A fixed size in SOL; without it, the size that profits most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_sol: Option<f64>,
    /// The least profit it sends for, in SOL: the research's detector fired
    /// above 0.01 SOL.
    #[serde(default = "default_min_profit_sol")]
    pub min_profit_sol: f64,
    /// The most legs a cycle of its has, 2 to [`super::cycle::LEGS_MAX`]:
    /// some searchers only look at a pair's two pools.
    #[serde(
        default = "default_max_legs",
        skip_serializing_if = "is_default_max_legs"
    )]
    pub max_legs: u32,
}

fn default_min_profit_sol() -> f64 {
    0.01
}

const fn default_max_legs() -> u32 {
    2
}

fn is_default_max_legs(legs: &u32) -> bool {
    *legs == default_max_legs()
}

/// A searcher that trades one pool back to the reference price, against a
/// venue outside the session (RESEARCH: the strongest competitors in the
/// whale windows grossed 0.88 to 1.02 SOL where the cycle's optimum was
/// about 0.70, and paid 76 to 99% of it in priority fees).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PoolSearcher {
    pub latency_slots: [u64; 2],
    pub bid_share: f64,
    pub pays: Pays,
    /// How far past its fee a pool must stray before it trades, in basis
    /// points.
    #[serde(default)]
    pub margin_bps: f64,
    #[serde(default = "default_min_profit_sol")]
    pub min_profit_sol: f64,
}

fn latency_ok(latency: [u64; 2]) -> bool {
    latency[0] <= latency[1] && latency[1] <= SEARCHER_LATENCY_SLOTS_MAX
}

fn bid_ok(share: f64, min_profit_sol: f64) -> bool {
    share.is_finite()
        && (0.0..=1.0).contains(&share)
        && min_profit_sol.is_finite()
        && min_profit_sol >= 0.0
}

impl CycleSearcher {
    pub fn validate(&self) -> Result<(), String> {
        let fixed_ok = self
            .fixed_sol
            .is_none_or(|sol| sol.is_finite() && sol > 0.0 && sol <= FIXED_SOL_MAX);
        let legs_ok = (2..=super::cycle::LEGS_MAX as u32).contains(&self.max_legs);
        if !(latency_ok(self.latency_slots)
            && bid_ok(self.bid_share, self.min_profit_sol)
            && fixed_ok
            && legs_ok)
        {
            return Err(format!(
                "amm.searchers: a latency of 0 to {SEARCHER_LATENCY_SLOTS_MAX} slots (low to high), a bid share of 0 to 1, a min profit of at least 0, a fixed size of 0 to {FIXED_SOL_MAX} SOL and 2 to {} legs, not {self:?}",
                super::cycle::LEGS_MAX
            ));
        }
        Ok(())
    }
}

impl PoolSearcher {
    pub fn validate(&self) -> Result<(), String> {
        let margin_ok = self.margin_bps.is_finite() && (0.0..=1_000.0).contains(&self.margin_bps);
        if !(latency_ok(self.latency_slots)
            && bid_ok(self.bid_share, self.min_profit_sol)
            && margin_ok)
        {
            return Err(format!(
                "amm.pool_searchers: a latency of 0 to {SEARCHER_LATENCY_SLOTS_MAX} slots (low to high), a bid share of 0 to 1, a min profit of at least 0 and a margin of 0 to 1,000 bp, not {self:?}"
            ));
        }
        Ok(())
    }
}
