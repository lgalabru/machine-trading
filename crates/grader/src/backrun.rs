//! The backrun-sol-usdc family: an `amm` session on Orca's and Raydium's
//! SOL/USDC concentrated pools, where large parent orders are worked a
//! chunk at a time and searchers race to close the gap each chunk opens
//! (RFD 0005, task 2; ADR 0004).
//!
//! Every range comes from the research (`docs/RESEARCH.md`, "SOL/USDC
//! backruns on concentrated pools", and
//! `ops/research/atomic-arb/clmm-backrun/NOTES.md`). A session draws:
//!
//! - **The parent orders.** None, one or several, each of 2,000 to 7,000
//!   SOL, a side of its own at even odds, into the thinner pool mostly
//!   (19 of 21 triggers hit Raydium) and sometimes the Whirlpool; some
//!   orders route a share of their chunks to the other pool. An order is
//!   worked in chunks of 200 to 1,000 SOL, round for some traders (one
//!   wallet sold 14 chunks of 500) and uneven for others, a gap of 30 to
//!   200 slots apart, each drawn on its own. An order starts anywhere from
//!   well before the session to near its end, so the session may open in
//!   the middle of one, end before it does, or hold calm stretches with no
//!   order at all.
//! - **Lone swaps.** A few large swaps that belong to no order, either side
//!   and either pool: 3 of the research's 20 distinct triggers came from a
//!   liquidation flow or a bot, not a chunked seller. So the next trigger's
//!   side and pool are likely, never certain.
//! - **The calm flow.** The research's rates and sizes on each pool, scaled
//!   per session.
//! - **The reference price.** A small random walk, which the pool searchers
//!   trade the pools back to; the account is valued in SOL, so it moves no
//!   bot's PnL by itself.
//! - **The competition.** Cycle searchers, each with a latency, a size
//!   (the best, or a fixed clip as one mainnet bot's 200 SOL) and a bid
//!   rule (a share of the profit it expects, as a tip or a priority fee),
//!   and pool searchers, which trade one pool back to the reference and bid
//!   most of what they expect, as the research's strongest competitors did.
//!   Most sessions are contested: one more cycle searcher reaches every
//!   trigger's slot, as the research's first responders landed in the
//!   trigger's slot 19 times in 20. In a quiet session nobody can.
//!
//! The pools are the snapshot's own; their depth isn't drawn (see the task's
//! `DESIGN.md`, "Realism limits").

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    amm::{
        Background, Chunk, CycleSearcher, ParentOrder, Pays, PoolFlow, PoolSearcher, Whale,
        WhaleSide,
    },
    scenario::{Scenario, SplitMix64, Waypoint},
    thin_pools::Contest,
};

/// Orca's and Raydium's SOL/USDC pools, cloned on the snapshot: the
/// research's only backrun pair, 4 bp each.
pub const WHIRLPOOL: &str = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE";
pub const RAYDIUM_CLMM: &str = "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv";
const WRAPPED_SOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// Most parent orders a session draws, and most chunks one order holds:
/// 7,000 SOL in 200 SOL chunks is 35.
const ORDERS_MAX: usize = 3;
const ORDER_CHUNKS_MAX: usize = 64;
/// Most lone swaps a session draws.
const LONE_MAX: usize = 2;
/// The round chunk sizes a trader working an order may pick, in SOL: the
/// research's chunks were 200, 430 to 438, 500, 960 and 1,000 to 1,044.
const ROUND_CHUNKS_SOL: [f64; 10] = [
    200.0, 250.0, 300.0, 400.0, 450.0, 500.0, 600.0, 750.0, 800.0, 1_000.0,
];
/// Most points the reference path holds: a tick every other slot over the
/// longest session.
const PATH_POINTS_MAX: usize = 1_024;

/// The pool an order works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderPool {
    Raydium,
    Whirlpool,
}

impl OrderPool {
    fn other(self) -> Self {
        match self {
            OrderPool::Raydium => OrderPool::Whirlpool,
            OrderPool::Whirlpool => OrderPool::Raydium,
        }
    }

    fn instrument(self) -> String {
        match self {
            OrderPool::Raydium => format!("raydium:amm:{RAYDIUM_CLMM}"),
            OrderPool::Whirlpool => format!("orca:amm:{WHIRLPOOL}"),
        }
    }
}

/// One parent order as drawn: where, which way, how much, and how its
/// chunks were sized. A lone swap is an order of one chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderShape {
    pub pool: OrderPool,
    pub side: WhaleSide,
    pub total_sol: f64,
    /// Its round chunk, when the trader sends round chunks.
    pub round_sol: Option<f64>,
    /// The chance each chunk goes to the other pool: 0 for an order that
    /// keeps to one.
    pub other_pool_odds: f64,
    /// The slot it starts at, from the session's start: negative before it.
    pub start_slot: i64,
    /// Its chunks inside the session, each with the pool it went to.
    pub chunks: Vec<Chunk>,
    pub chunk_pools: Vec<OrderPool>,
    /// A swap of its own, from no parent order.
    pub lone: bool,
}

/// What a session was drawn as, beneath its scenario: the ladder's told
/// rungs see all of it, the informed bot only [`BackrunShape::latent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackrunShape {
    pub duration_slots: u64,
    pub orders: Vec<OrderShape>,
    pub contest: Contest,
    /// The calm flow's rate, as a multiple of the research's.
    pub flow_scale: f64,
    /// The reference path's tick size, in basis points.
    pub path_sigma_bps: f64,
    pub searchers: Vec<CycleSearcher>,
    pub pool_searchers: Vec<PoolSearcher>,
    /// The reference path itself.
    pub path: Vec<Waypoint>,
}

impl BackrunShape {
    /// What the informed bot is told: who it races and how each bids and
    /// reacts, and the calm flow's rate: the process the session's races
    /// are drawn from. Never the orders, their chunks or the path.
    pub fn latent(&self) -> Value {
        json!({
            "searchers": self.searchers,
            "pool_searchers": self.pool_searchers,
            "flow_scale": self.flow_scale,
        })
    }

    /// Every chunk inside the session.
    pub fn chunks(&self) -> usize {
        self.orders.iter().map(|o| o.chunks.len()).sum()
    }
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct BackrunFamily {
    pub duration_slots: (u64, u64),
    /// The chance of each count of parent orders, from none up.
    pub orders_odds: [f64; ORDERS_MAX + 1],
    /// A parent order's size in SOL, log-uniform.
    pub order_sol: (f64, f64),
    /// The chance an order goes into Raydium's pool, the thinner one.
    pub raydium_odds: f64,
    /// The chance an order routes some of its chunks to the other pool,
    /// and the share of its chunks that go there when it does.
    pub split_odds: f64,
    pub split_share: (f64, f64),
    /// The chance of each count of lone swaps, from none up, and their
    /// size in SOL, log-uniform.
    pub lone_odds: [f64; LONE_MAX + 1],
    pub lone_sol: (f64, f64),
    /// The chance a trader sends round chunks; else uneven ones, between a
    /// low drawn in `uneven_low_sol` and that plus `uneven_width_sol`.
    pub round_odds: f64,
    pub chunk_sol: (f64, f64),
    pub uneven_low_sol: (f64, f64),
    pub uneven_width_sol: (f64, f64),
    /// The chance a round chunk is a little over (the research's 1,044 of
    /// a 1,000 SOL trader), and by how much at most.
    pub round_over_odds: f64,
    pub round_over_share: f64,
    /// Slots between chunks, log-uniform.
    pub chunk_gap_slots: (f64, f64),
    /// The calm flow's rate per session, as a multiple of the research's.
    pub flow_scale: (f64, f64),
    /// The reference path: slots between ticks, and a tick's size, in
    /// basis points, drawn per session.
    pub path_gap_slots: (u64, u64),
    pub path_sigma_bps: (f64, f64),
    pub cycle_searchers: (u64, u64),
    /// The chance a session is [`Contest::Quiet`].
    pub quiet_odds: f64,
    /// The chance a cycle searcher trades a fixed clip, and the clips.
    pub fixed_odds: f64,
    pub fixed_sol: [f64; 5],
    /// The chance a cycle searcher tips rather than pays a priority fee.
    pub cycle_tip_odds: f64,
    pub min_profit_sol: (f64, f64),
    pub pool_searchers: (u64, u64),
    pub pool_share: (f64, f64),
    pub pool_tip_odds: f64,
    pub pool_margin_bps: (f64, f64),
    /// The bot's wallet, in SOL.
    pub wallet_sol: f64,
    /// SOL's price on the snapshot, for the layer's fee accounting.
    pub sol_usd: f64,
}

impl BackrunFamily {
    pub fn sol_usdc() -> Self {
        Self {
            duration_slots: (600, 1_100),
            orders_odds: [0.12, 0.43, 0.3, 0.15],
            // 6 chunks of about 1,000; 14 of 500 (NOTES.md §2).
            order_sol: (2_000.0, 7_000.0),
            raydium_odds: 0.85,
            // Not measured: the research's triggers each hit one pool, 19
            // of 21 Raydium's. A trader that routes some chunks elsewhere
            // keeps the pools' overall shares near the research's.
            split_odds: 0.3,
            split_share: (0.1, 0.35),
            // 3 of 20 distinct triggers weren't a chunked seller's, sized
            // like the rest (165 to 1,286 SOL).
            lone_odds: [0.45, 0.35, 0.2],
            lone_sol: (165.0, 1_286.0),
            round_odds: 0.6,
            chunk_sol: (200.0, 1_000.0),
            uneven_low_sol: (200.0, 600.0),
            uneven_width_sol: (100.0, 400.0),
            round_over_odds: 0.15,
            round_over_share: 0.05,
            // Every 30 to 200 slots (the research saw up to 440).
            chunk_gap_slots: (30.0, 200.0),
            flow_scale: (0.6, 1.4),
            // A path step takes two slots to land; ticks at least three apart.
            path_gap_slots: (3, 9),
            path_sigma_bps: (0.5, 2.0),
            // 11 different winners in one burst; up to 44 failing signers.
            cycle_searchers: (0, 5),
            // The first response landed in the trigger's slot 19 times in
            // 20 (NOTES.md §2).
            quiet_odds: 0.05,
            fixed_odds: 0.2,
            fixed_sol: [100.0, 150.0, 200.0, 250.0, 300.0],
            cycle_tip_odds: 0.5,
            min_profit_sol: (0.005, 0.03),
            // At least one: the research's pools stayed on the market between
            // chunks because traders arbitrage them against other venues.
            pool_searchers: (1, 2),
            // 76 to 99% of gross, as a priority fee mostly.
            pool_share: (0.76, 0.99),
            pool_tip_odds: 0.15,
            pool_margin_bps: (0.0, 6.0),
            // The research's largest cycle put in 996 SOL.
            wallet_sol: 1_500.0,
            sol_usd: 70.24,
        }
    }

    /// `count` scenarios named `<prefix>-<n>`, in an order of `seed`'s own.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, BackrunShape)> {
        let seeds: Vec<u64> = (1..=count as u64)
            .map(|draw| seed.wrapping_mul(1_000_003).wrapping_add(draw))
            .collect();
        crate::scenario::shuffled(seeds, seed)
            .into_iter()
            .enumerate()
            .map(|(i, draw_seed)| self.draw(&format!("{prefix}-{}", i + 1), draw_seed))
            .collect()
    }

    /// One session.
    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, BackrunShape) {
        let mut rng = SplitMix64(seed);
        let duration = rng.range_u64(self.duration_slots);
        let orders = self.orders(&mut rng, duration);
        let flow_scale = rng.range_f64(self.flow_scale);
        let path_sigma_bps = rng.range_f64(self.path_sigma_bps);
        let path = self.path(&mut rng, duration, path_sigma_bps);
        let contest = match rng.next_f64() < self.quiet_odds {
            true => Contest::Quiet,
            false => Contest::Contested,
        };
        let mut searchers: Vec<CycleSearcher> = (0..rng.range_u64(self.cycle_searchers))
            .map(|_| self.cycle_searcher(&mut rng, contest))
            .collect();
        if contest == Contest::Contested {
            // The fast one, at a place of its own among the rest: its
            // index says nothing.
            let fast = self.fast_searcher(&mut rng);
            let at = rng.range_u64((0, searchers.len() as u64)) as usize;
            searchers.insert(at, fast);
        }
        let pool_searchers: Vec<PoolSearcher> = (0..rng.range_u64(self.pool_searchers))
            .map(|_| self.pool_searcher(&mut rng, contest))
            .collect();
        assert!(match contest {
            Contest::Contested => searchers.iter().any(|s| s.latency_slots == [0, 0]),
            Contest::Quiet => {
                searchers.iter().all(|s| s.latency_slots[0] >= 1)
                    && pool_searchers.iter().all(|s| s.latency_slots[0] >= 1)
            }
        });
        let amm_seed = rng.next_u64() % 1_000_000;
        let layer_seed = rng.next_u64() % 1_000_000;
        let shape = BackrunShape {
            duration_slots: duration,
            orders,
            contest,
            flow_scale,
            path_sigma_bps,
            searchers,
            pool_searchers,
            path,
        };
        let scenario = self.scenario(name, &shape, amm_seed, layer_seed);
        (scenario, shape)
    }

    /// The scenario of `shape`.
    fn scenario(
        &self,
        name: &str,
        shape: &BackrunShape,
        amm_seed: u64,
        layer_seed: u64,
    ) -> Scenario {
        let pools = [
            format!("orca:amm:{WHIRLPOOL}"),
            format!("raydium:amm:{RAYDIUM_CLMM}"),
        ];
        // A parent order is one pool's: an order that routes chunks to both
        // is two.
        let mut orders: Vec<ParentOrder> = Vec::new();
        for order in &shape.orders {
            assert_eq!(order.chunks.len(), order.chunk_pools.len());
            for pool in [OrderPool::Raydium, OrderPool::Whirlpool] {
                let chunks: Vec<Chunk> = order
                    .chunks
                    .iter()
                    .zip(&order.chunk_pools)
                    .filter(|(_, p)| **p == pool)
                    .map(|(c, _)| *c)
                    .collect();
                if !chunks.is_empty() {
                    orders.push(ParentOrder {
                        pool: pool.instrument(),
                        side: order.side,
                        chunks,
                    });
                }
            }
        }
        // A trader's chunks come from a handful of wallets: the week's top
        // 200 had 13 trigger signers.
        let accounts = u32::try_from(orders.len())
            .expect("a few orders")
            .clamp(1, crate::amm::WHALE_ACCOUNTS_MAX - 1)
            + 1;
        let whale = (!orders.is_empty()).then_some(Whale { accounts, orders });
        // The research's calm Saturday (NOTES.md §3).
        let background = Background {
            pools: vec![
                PoolFlow {
                    pool: pools[0].clone(),
                    swaps_per_minute: 15.0 * shape.flow_scale,
                    median_sol: 3.4,
                    p90_sol: 41.0,
                    max_sol: 280.0,
                },
                PoolFlow {
                    pool: pools[1].clone(),
                    swaps_per_minute: 12.0 * shape.flow_scale,
                    median_sol: 1.1,
                    p90_sol: 7.5,
                    max_sol: 71.0,
                },
            ],
        };
        let scenario = json!({
            "schema": 2,
            "name": name,
            "description": format!(
                "SOL/USDC on Orca's and Raydium's pools for {} slots: {} chunks of large orders, {} cycle searchers and {} pool searchers (backrun-sol-usdc)",
                shape.duration_slots,
                shape.chunks(),
                shape.searchers.len(),
                shape.pool_searchers.len(),
            ),
            "duration_slots": shape.duration_slots,
            "assets": { "symbol": "SOL" },
            "venues": {
                "amm": {
                    "pools": pools,
                    "seed": amm_seed,
                    "wallet": { "SOL": self.wallet_sol },
                    "whale": whale,
                    "background": background,
                    "searchers": shape.searchers,
                    "pool_searchers": shape.pool_searchers,
                },
            },
            "market_model": { "path": shape.path },
            "tx": {
                "layer": {
                    "seed": layer_seed,
                    "hot_accounts": [
                        { "address": WHIRLPOOL, "class": "amm_pool" },
                        { "address": RAYDIUM_CLMM, "class": "amm_pool" },
                    ],
                    "fees_in_pnl": true,
                    "native_sol": 10.0,
                    "sol_usd": self.sol_usd,
                },
            },
            "mandate": mandate(),
        });
        let scenario = crate::scenario_v2::parse(&scenario.to_string())
            .unwrap_or_else(|e| panic!("the family draws a scenario the grader reads: {e}"));
        assert!(
            scenario.validate().is_ok(),
            "{name}: {:?}",
            scenario.validate()
        );
        scenario
    }

    /// The session's parent orders, each with its chunks inside the
    /// session, every chunk in a slot of its own.
    fn orders(&self, rng: &mut SplitMix64, duration: u64) -> Vec<OrderShape> {
        let count = pick(rng, &self.orders_odds);
        let lone = pick(rng, &self.lone_odds);
        let mut taken: Vec<u64> = Vec::new();
        let mut orders = Vec::with_capacity(count + lone);
        for i in 0..count + lone {
            let mut order = match i < count {
                true => self.order(rng, duration),
                false => self.lone(rng, duration),
            };
            for chunk in &mut order.chunks {
                // Two traders never land in one slot: a later one waits.
                while taken.contains(&chunk.slot) {
                    chunk.slot += 1;
                }
                taken.push(chunk.slot);
            }
            let inside: Vec<bool> = order.chunks.iter().map(|c| c.slot < duration).collect();
            let mut keep = inside.iter();
            order.chunks.retain(|_| *keep.next().expect("one each"));
            let mut keep = inside.iter();
            order
                .chunk_pools
                .retain(|_| *keep.next().expect("one each"));
            orders.push(order);
        }
        assert!(orders.len() <= ORDERS_MAX + LONE_MAX);
        orders
    }

    /// A lone swap: one chunk, either side, mostly into the thinner pool,
    /// anywhere in the session.
    fn lone(&self, rng: &mut SplitMix64, duration: u64) -> OrderShape {
        let pool = match rng.next_f64() < self.raydium_odds {
            true => OrderPool::Raydium,
            false => OrderPool::Whirlpool,
        };
        let side = match rng.next_f64() < 0.5 {
            true => WhaleSide::Sell,
            false => WhaleSide::Buy,
        };
        let sol = (rng.log_range_f64(self.lone_sol) * 10.0).round() / 10.0;
        let slot = rng.range_u64((1, duration - 1));
        OrderShape {
            pool,
            side,
            total_sol: sol,
            round_sol: None,
            other_pool_odds: 0.0,
            start_slot: slot as i64,
            chunks: vec![Chunk {
                slot,
                sol,
                impact_bps: None,
            }],
            chunk_pools: vec![pool],
            lone: true,
        }
    }

    /// One parent order: its chunks from its start, those inside the
    /// session kept.
    fn order(&self, rng: &mut SplitMix64, duration: u64) -> OrderShape {
        let pool = match rng.next_f64() < self.raydium_odds {
            true => OrderPool::Raydium,
            false => OrderPool::Whirlpool,
        };
        let side = match rng.next_f64() < 0.5 {
            true => WhaleSide::Sell,
            false => WhaleSide::Buy,
        };
        let other_pool_odds = match rng.next_f64() < self.split_odds {
            true => rng.range_f64(self.split_share),
            false => 0.0,
        };
        let total_sol = rng.log_range_f64(self.order_sol);
        let round_sol = (rng.next_f64() < self.round_odds)
            .then(|| ROUND_CHUNKS_SOL[(rng.next_u64() % ROUND_CHUNKS_SOL.len() as u64) as usize]);
        let uneven_low = rng.range_f64(self.uneven_low_sol);
        let uneven_high = (uneven_low + rng.range_f64(self.uneven_width_sol)).min(self.chunk_sol.1);
        // The order's expected span: its chunks at their middle size, each
        // a mean gap apart. It starts up to half that span before the
        // session, so a session may open in the middle of one, and late
        // enough that it may end after the session does.
        let (gap_low, gap_high) = self.chunk_gap_slots;
        let mean_gap_slots = (gap_high - gap_low) / (gap_high / gap_low).ln();
        let middle_chunk_sol = (self.chunk_sol.0 + self.chunk_sol.1) / 2.0;
        let span_slots = total_sol / middle_chunk_sol * mean_gap_slots;
        let start_slot = rng.range_f64((-span_slots / 2.0, duration as f64 * 0.8)) as i64;
        let mut chunks = Vec::new();
        let mut chunk_pools = Vec::new();
        let mut sent = 0.0;
        let mut slot = start_slot;
        while sent < total_sol && chunks.len() < ORDER_CHUNKS_MAX {
            let size = match round_sol {
                Some(round) if rng.next_f64() < self.round_over_odds => {
                    round * (1.0 + rng.range_f64((0.0, self.round_over_share)))
                }
                Some(round) => round,
                None => rng.range_f64((uneven_low, uneven_high)),
            };
            let size = ((size.min(total_sol - sent).max(self.chunk_sol.0)) * 10.0).round() / 10.0;
            assert!(
                (self.chunk_sol.0..=self.chunk_sol.1 * 1.1).contains(&size),
                "{size}"
            );
            // Drawn for every chunk, in the session or not, so a chunk's
            // pool doesn't hang on where the session starts.
            let chunk_pool = match rng.next_f64() < other_pool_odds {
                true => pool.other(),
                false => pool,
            };
            if slot >= 1 && (slot as u64) < duration {
                chunks.push(Chunk {
                    slot: slot as u64,
                    sol: size,
                    impact_bps: None,
                });
                chunk_pools.push(chunk_pool);
            }
            sent += size;
            slot += rng.log_range_f64(self.chunk_gap_slots).round() as i64;
        }
        OrderShape {
            pool,
            side,
            total_sol,
            round_sol,
            other_pool_odds,
            start_slot,
            chunks,
            chunk_pools,
            lone: false,
        }
    }

    /// The reference path, in percent of its start: a tick at uneven gaps.
    fn path(&self, rng: &mut SplitMix64, duration: u64, sigma_bps: f64) -> Vec<Waypoint> {
        let mut points = Vec::new();
        let mut level_bps = 0.0;
        let mut slot = 0;
        loop {
            slot += rng.range_u64(self.path_gap_slots);
            // A move lands over two slots, inside the session.
            if slot + 3 > duration {
                break;
            }
            level_bps += sigma_bps * rng.normal();
            points.push(Waypoint {
                slot,
                level_pct: level_bps / 100.0,
                premium_pct: 0.0,
            });
            assert!(
                points.len() <= PATH_POINTS_MAX,
                "a tick every other slot at most"
            );
        }
        assert!(points.windows(2).all(|w| w[0].slot < w[1].slot));
        points
    }

    fn cycle_searcher(&self, rng: &mut SplitMix64, contest: Contest) -> CycleSearcher {
        let bid_share = bid_share(rng);
        let pays = match rng.next_f64() < self.cycle_tip_odds {
            true => Pays::Tip,
            false => Pays::PriorityFee,
        };
        let fixed_sol = (rng.next_f64() < self.fixed_odds)
            .then(|| self.fixed_sol[(rng.next_u64() % self.fixed_sol.len() as u64) as usize]);
        CycleSearcher {
            latency_slots: latency(rng, contest),
            bid_share,
            pays,
            fixed_sol,
            min_profit_sol: rng.log_range_f64(self.min_profit_sol),
            max_legs: 2,
        }
    }

    /// A contested session's fast searcher: it reaches every trigger's
    /// slot and trades the best cycle; its share, its payment and its least
    /// profit are drawn as any cycle searcher's.
    fn fast_searcher(&self, rng: &mut SplitMix64) -> CycleSearcher {
        let bid_share = bid_share(rng);
        let pays = match rng.next_f64() < self.cycle_tip_odds {
            true => Pays::Tip,
            false => Pays::PriorityFee,
        };
        CycleSearcher {
            latency_slots: [0, 0],
            bid_share,
            pays,
            fixed_sol: None,
            min_profit_sol: rng.log_range_f64(self.min_profit_sol),
            max_legs: 2,
        }
    }

    fn pool_searcher(&self, rng: &mut SplitMix64, contest: Contest) -> PoolSearcher {
        PoolSearcher {
            latency_slots: latency(rng, contest),
            bid_share: rng.range_f64(self.pool_share),
            pays: match rng.next_f64() < self.pool_tip_odds {
                true => Pays::Tip,
                false => Pays::PriorityFee,
            },
            margin_bps: rng.range_f64(self.pool_margin_bps),
            min_profit_sol: rng.log_range_f64(self.min_profit_sol),
        }
    }
}

/// A count, drawn with `odds` of each from none up.
fn pick(rng: &mut SplitMix64, odds: &[f64]) -> usize {
    assert!(
        (odds.iter().sum::<f64>() - 1.0).abs() < 1e-9,
        "odds sum to 1"
    );
    let u = rng.next_f64();
    let mut cumulative = 0.0;
    for (n, chance) in odds.iter().enumerate() {
        cumulative += chance;
        if u < cumulative {
            return n;
        }
    }
    odds.len() - 1
}

/// A searcher's share of the profit it expects, paid to the leader: the
/// research's winners paid 0 (7 of 20), about 34% (6), 50 to 80% (4) and
/// 93% (3) of their gross, a mix of overlapping ranges.
fn bid_share(rng: &mut SplitMix64) -> f64 {
    let u = rng.next_f64();
    if u < 0.35 {
        rng.range_f64((0.0, 0.1))
    } else if u < 0.65 {
        rng.range_f64((0.25, 0.45))
    } else if u < 0.85 {
        rng.range_f64((0.45, 0.8))
    } else {
        rng.range_f64((0.8, 0.95))
    }
}

/// A searcher's latency range, in slots: the research's first responders
/// landed in the trigger's slot 19 times in 20, and late ones within a slot
/// or two. In a quiet session none starts at 0.
fn latency(rng: &mut SplitMix64, contest: Contest) -> [u64; 2] {
    let u = rng.next_f64();
    let low = if u < 0.5 {
        0
    } else if u < 0.85 {
        1
    } else {
        2
    };
    let low = match contest {
        Contest::Contested => low,
        Contest::Quiet => low.max(1),
    };
    [low, low + rng.range_u64((0, 2))]
}

/// The session's mandate, v0.3.0 (`specs/examples/backrun-sol-usdc.json`):
/// any Orca or Raydium pool between SOL and USDC, SOL as the numeraire with
/// at most $5 of anything else after any transaction, a loss limit and a
/// budget for fees and tips: $1,000, where the reference's costliest
/// session of a 72-session calibration spent $433 (the example's $500
/// left it too little room).
fn mandate() -> Value {
    json!({
        "version": "v0.3.0",
        "universe": [
            { "instrument": "orca:amm:*", "assets": [WRAPPED_SOL, USDC] },
            { "instrument": "raydium:amm:*", "assets": [WRAPPED_SOL, USDC] },
        ],
        "limits": {
            "max_drawdown_usd": 200.0,
            "inventory": { "numeraire": WRAPPED_SOL, "max_other_usd": 5.0 },
            "max_costs_usd": 1_000.0,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every scenario reads back, and the draw is a pure function of the
    /// seed.
    #[test]
    fn a_set_replays() {
        let family = BackrunFamily::sol_usdc();
        let set = family.draw_set("hidden", 12, 7);
        assert_eq!(set.len(), 12);
        let again = family.draw_set("hidden", 12, 7);
        for ((a, sa), (b, sb)) in set.iter().zip(&again) {
            assert_eq!(sa, sb);
            assert_eq!(a.name, b.name);
            assert!(a.amm.is_some() && a.tx_layer.is_some() && a.lending.is_none());
        }
    }

    /// Over many sessions: some hold no chunk, both sides and both pools
    /// trigger, chunk sizes and gaps are uneven and inside their ranges,
    /// some orders route chunks to both pools, and lone swaps go either
    /// way.
    #[test]
    fn chunks_come_from_the_research_s_ranges() {
        let family = BackrunFamily::sol_usdc();
        let set = family.draw_set("t", 300, 3);
        let mut sizes: Vec<f64> = Vec::new();
        let mut gaps: Vec<u64> = Vec::new();
        let (mut calm, mut sells, mut buys, mut whirlpool, mut split) = (0, 0, 0, 0, 0);
        let (mut lone, mut lone_sells) = (0, 0);
        for (scenario, shape) in &set {
            if shape.chunks() == 0 {
                calm += 1;
                assert!(scenario.amm.as_ref().unwrap().whale.is_none());
            }
            for order in &shape.orders {
                assert_eq!(order.chunks.len(), order.chunk_pools.len());
                if order.lone {
                    assert!(order.chunks.len() <= 1);
                    lone += order.chunks.len();
                    if order.side == WhaleSide::Sell {
                        lone_sells += order.chunks.len();
                    }
                    assert!(
                        order
                            .chunks
                            .iter()
                            .all(|c| (165.0..=1_286.0).contains(&c.sol))
                    );
                    continue;
                }
                if order.chunk_pools.iter().any(|p| *p != order.pool) {
                    split += 1;
                }
                for (chunk, pool) in order.chunks.iter().zip(&order.chunk_pools) {
                    sizes.push(chunk.sol);
                    match order.side {
                        WhaleSide::Sell => sells += 1,
                        WhaleSide::Buy => buys += 1,
                    }
                    if *pool == OrderPool::Whirlpool {
                        whirlpool += 1;
                    }
                }
                gaps.extend(order.chunks.windows(2).map(|w| w[1].slot - w[0].slot));
            }
        }
        // Fully calm: no order and no lone swap, about one in twenty.
        assert!(calm > 5 && calm < 40, "{calm}");
        let total = sizes.len();
        assert!(sells * 3 > total && buys * 3 > total, "{sells} {buys}");
        assert!(
            whirlpool > 0 && whirlpool * 4 < total,
            "{whirlpool} of {total}"
        );
        assert!(split > 20, "{split} orders split across pools");
        assert!(
            lone > 100 && lone_sells * 4 > lone && lone_sells * 4 < 3 * lone,
            "{lone_sells} of {lone}"
        );
        assert!(sizes.iter().all(|s| (200.0..=1_050.0).contains(s)));
        sizes.sort_by(f64::total_cmp);
        sizes.dedup();
        assert!(sizes.len() > 100, "uneven sizes too");
        // A chunk that meets another trader's slot waits a slot or two.
        assert!(gaps.iter().all(|g| *g >= 28) && gaps.iter().any(|g| *g >= 150));
    }

    /// Most sessions hold a searcher that reaches every trigger's slot;
    /// in a quiet one nobody's latency starts at 0.
    #[test]
    fn most_sessions_are_contested_and_quiet_ones_are_out_of_reach() {
        let family = BackrunFamily::sol_usdc();
        let set = family.draw_set("t", 300, 5);
        let mut quiet = 0;
        for (_, shape) in &set {
            match shape.contest {
                Contest::Contested => {
                    assert!(shape.searchers.iter().any(|s| s.latency_slots == [0, 0]))
                }
                Contest::Quiet => {
                    quiet += 1;
                    assert!(shape.searchers.iter().all(|s| s.latency_slots[0] >= 1));
                    assert!(shape.pool_searchers.iter().all(|s| s.latency_slots[0] >= 1));
                }
            }
        }
        assert!((3..=35).contains(&quiet), "{quiet} quiet of 300");
    }

    /// The latent state is the competition and the flow's rate, and none of
    /// the orders or the path.
    #[test]
    fn the_latent_state_holds_no_order() {
        let (_, shape) = BackrunFamily::sol_usdc().draw("x", 11);
        let latent = shape.latent();
        let keys: Vec<&str> = latent
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 3);
        for forbidden in ["orders", "path", "chunks", "duration_slots"] {
            assert!(!keys.contains(&forbidden), "{forbidden}");
        }
    }
}
