//! The backrun-thin-pools family: an `amm` session on ten Orca Whirlpool
//! and Raydium CLMM pools over SOL, USDC, JitoSOL, JTO and GEOD
//! (`snapshots/thin-pools-2026-11-21`), where a large swap on one pool at a
//! time opens a cycle through the graph and searchers race to close it
//! (RFD 0005, task 3; ADR 0004).
//!
//! Every range comes from the research (`docs/RESEARCH.md`, "LST cycles and
//! epoch steps", and `ops/research/atomic-arb/lst-cycles/NOTES.md`). A
//! session draws:
//!
//! - **The triggers.** None to several large swaps, each on one pool of the
//!   graph, a side of its own at even odds, sized by the move it makes:
//!   log-normal through the research's median impact (545 bp) and 90th
//!   percentile (5,877), truncated. Most land on the thin pools (JTO, GEOD
//!   and the JTO/JitoSOL pair), as the research's did; some on the deep
//!   SOL/USDC and SOL/JitoSOL pools, where the swap's size cap leaves a
//!   move inside the fee band. Their slots are drawn apart, uneven.
//! - **The calm flow.** Small swaps on every pool, at rates and sizes
//!   scaled to each pool's depth, scaled again per session.
//! - **The reference prices.** SOL's dollar price walks a little. Every
//!   other token's price in SOL starts at a level of the session's own near
//!   the snapshot's and drifts slowly (`reference_sol` and
//!   `reference_paths`), JitoSOL's inside the stake pool's band, so the
//!   fair value the pool searchers pull to is read from the pools, never
//!   memorised. The account, measured in SOL, still moves only with what
//!   the bot trades: it holds nothing else past a few dollars.
//! - **The competition.** Cycle searchers, each with a latency, a bid rule
//!   (a share of the profit it expects, as a tip or a priority fee), and a
//!   search depth (some only look at a pair's two pools: 2 legs; the rest
//!   at 3), and pool searchers, which trade one pool back to its reference
//!   and bid most of what they expect. Most sessions are contested: one
//!   cycle searcher tips from the trigger's own slot on every send and
//!   searches three legs, as the research's re-arbs within a slot or two
//!   (57 of 82) say someone usually does. In a quiet session nobody can
//!   reach the trigger's slot: knowing which session is which is what lets
//!   a bot bid low.
//!
//! The pools are the snapshot's own; their depth isn't drawn, and the
//! stake pool's epoch step is left out (see the task's `DESIGN.md`).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    amm::{
        Background, Chunk, CycleSearcher, ParentOrder, Pays, PoolFlow, PoolSearcher, Whale,
        WhaleSide,
    },
    scenario::{Scenario, SplitMix64, Waypoint},
};

const WRAPPED_SOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const JITOSOL: &str = "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn";
const JTO: &str = "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL";
const GEOD: &str = "7JA5eZdCzztSfQbJvS8aVVxMFfd81Rs9VvwnocV1mKHu";

/// The graph's tokens, as the mandate lists them.
pub const ASSETS: [&str; 5] = [WRAPPED_SOL, USDC, JITOSOL, JTO, GEOD];

/// JitoSOL's value in SOL at its stake pool on the snapshot
/// (`snapshots/thin-pools-2026-11-21/README.md`: the SOL/JitoSOL pool
/// prices it 10 bp under, at 1.311486).
const JITOSOL_STAKE_SOL: f64 = 1.312_796;
/// Where JitoSOL's reference sits from its stake value, in basis points:
/// the research's LST pools traded 4 to 13 bp under the stake pools' rate,
/// and an arbitrageur who can deposit keeps it from rising more than a
/// basis point past it.
const JITOSOL_BAND_BPS: (f64, f64) = (-12.0, 1.0);
/// The centres of JTO's and GEOD's reference prices in SOL: the snapshot's
/// deepest pool of each with SOL, JTO at the 30 bp SOL/JTO Whirlpool's
/// 320.3 a SOL, GEOD at the 5 bp Raydium pool's 302.1.
const CENTRES_SOL: [(&str, f64); 2] = [(JTO, 1.0 / 320.3), (GEOD, 1.0 / 302.1)];

/// One of the snapshot's pools: its instrument, the odds a trigger lands
/// on it, and its calm flow (swaps a minute, median, p90 and largest size
/// in SOL's worth).
struct PoolSpec {
    instrument: &'static str,
    trigger_odds: f64,
    flow: (f64, f64, f64, f64),
}

/// The ten pools, SOL/USDC first (the account's reference). The thin
/// pools' flow is the research's sparse swaps (a median gap of 50 slots on
/// the JitoSOL pools) sized to their depth: under a SOL moves five of them
/// by 10 bp. Not measured pool by pool (`DESIGN.md`, "Realism limits").
const POOLS: [PoolSpec; 10] = [
    PoolSpec {
        instrument: "orca:amm:Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE",
        trigger_odds: 0.03,
        flow: (15.0, 3.4, 41.0, 280.0),
    },
    PoolSpec {
        instrument: "raydium:amm:3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv",
        trigger_odds: 0.03,
        flow: (12.0, 1.1, 7.5, 71.0),
    },
    PoolSpec {
        instrument: "orca:amm:Hp53XEtt4S8SvPCXarsLSdGfZBuUr5mMmZmX2DRNXQKp",
        trigger_odds: 0.04,
        flow: (3.0, 5.0, 60.0, 300.0),
    },
    PoolSpec {
        instrument: "orca:amm:G2FiE1yn9N9ZJx5e1E2LxxMnHvb1H3hCuHLPfKJ98smA",
        trigger_odds: 0.15,
        flow: (1.0, 0.3, 2.0, 10.0),
    },
    PoolSpec {
        instrument: "orca:amm:4UVvSVdeth9mxpPe4nwwZseXPd2bqqDm4xVaJSve3DXw",
        trigger_odds: 0.10,
        flow: (0.3, 0.02, 0.1, 0.5),
    },
    PoolSpec {
        instrument: "raydium:amm:JVoPtWWDsRcLvQosu5fWc2CaNF6jEtJzbxdPtcEuvZo",
        trigger_odds: 0.15,
        flow: (0.5, 0.03, 0.2, 1.0),
    },
    PoolSpec {
        instrument: "orca:amm:2UhFnySoJi6c89aydGAGS7ZRemo2dbkFRhvSJqDX4gHJ",
        trigger_odds: 0.15,
        flow: (1.0, 0.2, 1.5, 8.0),
    },
    PoolSpec {
        instrument: "orca:amm:FsPFkRRTyCJrwK3S8x8JvU8Vmruk5EGKKQsLqywfvF5E",
        trigger_odds: 0.12,
        flow: (0.8, 0.2, 1.5, 6.0),
    },
    PoolSpec {
        instrument: "raydium:amm:3RA4PDZAZ49nm6hdDCnCTrnECqQD93ny8oHP4T1CSTcR",
        trigger_odds: 0.12,
        flow: (1.0, 0.1, 0.8, 4.0),
    },
    PoolSpec {
        instrument: "raydium:amm:BkX9RSHoDJGkU7pbHiksv3DprdLM3KWGoDSfGN9cGJ5d",
        trigger_odds: 0.11,
        flow: (1.0, 0.4, 3.0, 20.0),
    },
];

/// Most triggers a session draws.
const TRIGGERS_MAX: usize = 8;
/// Most points the reference path holds: a tick every other slot over the
/// longest session.
const PATH_POINTS_MAX: usize = 1_024;
/// The standard normal's 90th percentile.
const Z_90: f64 = 1.281_551_565_545;

/// One trigger as drawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerShape {
    pub pool: String,
    pub side: WhaleSide,
    pub slot: u64,
    /// The move it aims for, in basis points, and the most it trades, in
    /// SOL's worth.
    pub impact_bps: f64,
    pub cap_sol: f64,
}

/// Whether a session's competition can reach a trigger's slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Contest {
    /// One cycle searcher reaches every trigger's slot and tips; the rest
    /// are drawn as they come.
    Contested,
    /// Every searcher's latency is at least a slot: whoever reaches the
    /// trigger's slot pays only the background's floor.
    Quiet,
}

/// One token's reference price in SOL: where it starts and the path that
/// moves it, in percent of the start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenReference {
    pub mint: String,
    pub start_sol: f64,
    pub path: Vec<Waypoint>,
}

/// How a token's reference walks: `level_bps` from `centre_sol` at the
/// start, then a normal tick of `sigma_bps` at uneven gaps, its offset from
/// the centre kept inside `band_bps`.
#[derive(Debug, Clone, Copy)]
struct TokenWalk {
    centre_sol: f64,
    level_bps: f64,
    sigma_bps: f64,
    band_bps: (f64, f64),
}

/// What a session was drawn as, beneath its scenario: the ladder's told
/// rungs see all of it, the informed bot only [`ThinPoolsShape::latent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinPoolsShape {
    pub duration_slots: u64,
    pub triggers: Vec<TriggerShape>,
    pub contest: Contest,
    /// The calm flow's rate, as a multiple of each pool's.
    pub flow_scale: f64,
    /// The reference path's tick size, in basis points.
    pub path_sigma_bps: f64,
    /// JitoSOL's, JTO's and GEOD's reference prices.
    pub references: Vec<TokenReference>,
    pub searchers: Vec<CycleSearcher>,
    pub pool_searchers: Vec<PoolSearcher>,
    pub path: Vec<Waypoint>,
}

impl ThinPoolsShape {
    /// What the informed bot is told: who it races, how each searches, bids
    /// and reacts, and the calm flow's rate. Never the triggers or the path.
    pub fn latent(&self) -> Value {
        json!({
            "searchers": self.searchers,
            "pool_searchers": self.pool_searchers,
            "flow_scale": self.flow_scale,
        })
    }
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct ThinPoolsFamily {
    pub duration_slots: (u64, u64),
    /// The chance of each count of triggers, from none up.
    pub trigger_odds: [f64; TRIGGERS_MAX + 1],
    /// A trigger's move: log-normal through these median and p90, in basis
    /// points, truncated to the range. The research's p90 (5,877 bp) on
    /// this snapshot's pools would open cycles of tens of SOL, ten times
    /// the research's largest backrun profits: the tail is cut at 3,000.
    pub impact_median_bps: f64,
    pub impact_p90_bps: f64,
    pub impact_bps: (f64, f64),
    /// The most a trigger trades, in SOL's worth: a retail swap (the
    /// research's thin-pool triggers were a few thousand dollars; its
    /// arbs put in a median 9.3 SOL). On the deep pools it leaves a move
    /// inside the fee band.
    pub trigger_cap_sol: f64,
    /// The first slot a trigger may land in, and how close to the end.
    pub trigger_from_slot: u64,
    pub trigger_tail_slots: u64,
    pub flow_scale: (f64, f64),
    pub path_gap_slots: (u64, u64),
    pub path_sigma_bps: (f64, f64),
    /// How far JTO's and GEOD's session level sits from its centre, in
    /// basis points either way.
    pub token_level_bps: f64,
    /// The token paths' gaps between ticks, in slots, and their tick
    /// size, in basis points (JitoSOL's is the low end, as its stake rate
    /// barely moves).
    pub token_gap_slots: (u64, u64),
    pub token_sigma_bps: (f64, f64),
    pub cycle_searchers: (u64, u64),
    /// The chance a session is [`Contest::Quiet`].
    pub quiet_odds: f64,
    /// The chance a cycle searcher only looks at two-leg cycles.
    pub two_leg_odds: f64,
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

impl ThinPoolsFamily {
    pub fn snapshot() -> Self {
        Self {
            duration_slots: (400, 800),
            trigger_odds: [0.1, 0.15, 0.2, 0.18, 0.14, 0.1, 0.06, 0.04, 0.03],
            // The research's 46 same-block triggers with a decoded price.
            impact_median_bps: 545.0,
            impact_p90_bps: 5_877.0,
            impact_bps: (30.0, 3_000.0),
            trigger_cap_sol: 100.0,
            trigger_from_slot: 10,
            trigger_tail_slots: 8,
            flow_scale: (0.6, 1.4),
            path_gap_slots: (3, 9),
            path_sigma_bps: (0.5, 2.0),
            // Inside the 30 bp JTO pools' fee band, past the 5 bp GEOD
            // pool's: the pool searchers pull some pools to it as a session
            // opens, so nothing about a pool's price on the snapshot is the
            // fair value of every session.
            token_level_bps: 30.0,
            token_gap_slots: (9, 30),
            token_sigma_bps: (0.3, 1.5),
            cycle_searchers: (0, 5),
            // The research's re-arbs: the previous transaction on the pool
            // was an arb within 2 slots in 57 of 82 cases, and 77 of 106
            // backruns landed in the trigger's block, so a trigger nobody
            // races for is the exception (at most the other 25 of 82).
            // Rare, but present in most hidden sets of 72.
            quiet_odds: 0.12,
            two_leg_odds: 0.4,
            cycle_tip_odds: 0.5,
            min_profit_sol: (0.002, 0.02),
            pool_searchers: (1, 2),
            pool_share: (0.76, 0.99),
            pool_tip_odds: 0.15,
            pool_margin_bps: (0.0, 6.0),
            // The research's arbs put in a median 9.3 SOL, p90 249.
            wallet_sol: 500.0,
            sol_usd: 69.41,
        }
    }

    /// `count` scenarios named `<prefix>-<n>`, in an order of `seed`'s own.
    pub fn draw_set(
        &self,
        prefix: &str,
        count: usize,
        seed: u64,
    ) -> Vec<(Scenario, ThinPoolsShape)> {
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
    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, ThinPoolsShape) {
        let mut rng = SplitMix64(seed);
        let duration = rng.range_u64(self.duration_slots);
        let triggers = self.triggers(&mut rng, duration);
        let flow_scale = rng.range_f64(self.flow_scale);
        let path_sigma_bps = rng.range_f64(self.path_sigma_bps);
        let path = self.path(&mut rng, duration, path_sigma_bps);
        let references = self.references(&mut rng, duration);
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
            Contest::Contested => searchers.iter().any(reaches_every_trigger),
            Contest::Quiet => {
                searchers.iter().all(|s| s.latency_slots[0] >= 1)
                    && pool_searchers.iter().all(|s| s.latency_slots[0] >= 1)
            }
        });
        let amm_seed = rng.next_u64() % 1_000_000;
        let layer_seed = rng.next_u64() % 1_000_000;
        let shape = ThinPoolsShape {
            duration_slots: duration,
            triggers,
            contest,
            flow_scale,
            path_sigma_bps,
            references,
            searchers,
            pool_searchers,
            path,
        };
        let scenario = self.scenario(name, &shape, amm_seed, layer_seed);
        (scenario, shape)
    }

    fn scenario(
        &self,
        name: &str,
        shape: &ThinPoolsShape,
        amm_seed: u64,
        layer_seed: u64,
    ) -> Scenario {
        let pools: Vec<&str> = POOLS.iter().map(|p| p.instrument).collect();
        // Each trigger is a parent order of one chunk, from an account of
        // its own: the research's triggers were retail swaps.
        let orders: Vec<ParentOrder> = shape
            .triggers
            .iter()
            .map(|t| ParentOrder {
                pool: t.pool.clone(),
                side: t.side,
                chunks: vec![Chunk {
                    slot: t.slot,
                    sol: t.cap_sol,
                    impact_bps: Some(t.impact_bps),
                }],
            })
            .collect();
        let accounts = u32::try_from(orders.len().clamp(1, 16)).expect("a few orders");
        let whale = (!orders.is_empty()).then_some(Whale { accounts, orders });
        let background = Background {
            pools: POOLS
                .iter()
                .map(|p| {
                    let (rate, median, p90, max) = p.flow;
                    PoolFlow {
                        pool: p.instrument.to_string(),
                        swaps_per_minute: rate * shape.flow_scale,
                        median_sol: median,
                        p90_sol: p90,
                        max_sol: max,
                    }
                })
                .collect(),
        };
        let reference_sol: serde_json::Map<String, Value> = shape
            .references
            .iter()
            .map(|r| (r.mint.clone(), json!(r.start_sol)))
            .collect();
        let reference_paths: serde_json::Map<String, Value> = shape
            .references
            .iter()
            .map(|r| (r.mint.clone(), json!(r.path)))
            .collect();
        let hot_accounts: Vec<Value> = POOLS
            .iter()
            .map(|p| {
                let address = p.instrument.rsplit(':').next().expect("an address");
                json!({ "address": address, "class": "amm_pool" })
            })
            .collect();
        let scenario = json!({
            "schema": 2,
            "name": name,
            "description": format!(
                "Ten pools over SOL, USDC, JitoSOL, JTO and GEOD for {} slots: {} large swaps, {} cycle searchers and {} pool searchers (backrun-thin-pools)",
                shape.duration_slots,
                shape.triggers.len(),
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
                    "reference_sol": reference_sol,
                    "reference_paths": reference_paths,
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
                    "hot_accounts": hot_accounts,
                    "fees_in_pnl": true,
                    "native_sol": 100.0,
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

    /// The session's triggers, in slot order, each in a slot of its own.
    fn triggers(&self, rng: &mut SplitMix64, duration: u64) -> Vec<TriggerShape> {
        assert!((self.trigger_odds.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!((POOLS.iter().map(|p| p.trigger_odds).sum::<f64>() - 1.0).abs() < 1e-9);
        let count = pick(rng.next_f64(), &self.trigger_odds);
        let sigma = (self.impact_p90_bps / self.impact_median_bps).ln() / Z_90;
        let (first, last) = (self.trigger_from_slot, duration - self.trigger_tail_slots);
        assert!(first < last);
        let mut triggers: Vec<TriggerShape> = Vec::with_capacity(count);
        for _ in 0..count {
            let odds: Vec<f64> = POOLS.iter().map(|p| p.trigger_odds).collect();
            let pool = POOLS[pick(rng.next_f64(), &odds)].instrument.to_string();
            let side = match rng.next_f64() < 0.5 {
                true => WhaleSide::Sell,
                false => WhaleSide::Buy,
            };
            let impact = (self.impact_median_bps * (sigma * rng.normal()).exp())
                .clamp(self.impact_bps.0, self.impact_bps.1);
            let mut slot = rng.range_u64((first, last));
            // Two triggers never land in one slot: a later one waits.
            while triggers.iter().any(|t| t.slot == slot) {
                slot += 1;
            }
            triggers.push(TriggerShape {
                pool,
                side,
                slot: slot.min(duration - 1),
                impact_bps: (impact * 10.0).round() / 10.0,
                cap_sol: self.trigger_cap_sol,
            });
        }
        triggers.sort_by_key(|t| t.slot);
        triggers.dedup_by_key(|t| t.slot);
        assert!(triggers.windows(2).all(|w| w[0].slot < w[1].slot));
        triggers
    }

    /// JitoSOL's, JTO's and GEOD's reference prices: a level of the
    /// session's own, then a slow walk. JitoSOL's stays inside its band of
    /// the stake value.
    fn references(&self, rng: &mut SplitMix64, duration: u64) -> Vec<TokenReference> {
        let (low, high) = JITOSOL_BAND_BPS;
        let jitosol_bps = rng.range_f64((low, high));
        let jitosol_sigma = self.token_sigma_bps.0;
        let jitosol = TokenWalk {
            centre_sol: JITOSOL_STAKE_SOL,
            level_bps: jitosol_bps,
            sigma_bps: jitosol_sigma,
            band_bps: (low, high),
        };
        let mut references = vec![self.token_reference(rng, JITOSOL, &jitosol, duration)];
        for (mint, centre) in CENTRES_SOL {
            let level_bps = rng.range_f64((-self.token_level_bps, self.token_level_bps));
            let sigma = rng.range_f64(self.token_sigma_bps);
            // Unbounded but for the path's own check: a session's walk of
            // a few hundred slots moves it tens of basis points at most.
            let walk = TokenWalk {
                centre_sol: centre,
                level_bps,
                sigma_bps: sigma,
                band_bps: (-5_000.0, 5_000.0),
            };
            references.push(self.token_reference(rng, mint, &walk, duration));
        }
        references
    }

    /// `mint`'s reference as `walk` draws it.
    fn token_reference(
        &self,
        rng: &mut SplitMix64,
        mint: &str,
        walk: &TokenWalk,
        duration: u64,
    ) -> TokenReference {
        let TokenWalk {
            centre_sol: centre,
            level_bps,
            sigma_bps,
            band_bps,
        } = *walk;
        assert!(band_bps.0 <= level_bps && level_bps <= band_bps.1);
        assert!(centre > 0.0 && sigma_bps >= 0.0);
        let start_sol = centre * (1.0 + level_bps / 10_000.0);
        let mut offset_bps = level_bps;
        let mut path = Vec::new();
        let mut slot = 0;
        loop {
            slot += rng.range_u64(self.token_gap_slots);
            if slot + 3 > duration {
                break;
            }
            offset_bps = (offset_bps + sigma_bps * rng.normal()).clamp(band_bps.0, band_bps.1);
            let price = centre * (1.0 + offset_bps / 10_000.0);
            path.push(Waypoint {
                slot,
                level_pct: (price / start_sol - 1.0) * 100.0,
                premium_pct: 0.0,
            });
            assert!(path.len() <= PATH_POINTS_MAX);
        }
        TokenReference {
            mint: mint.to_string(),
            start_sol,
            path,
        }
    }

    /// The reference path, in percent of its start: a tick at uneven gaps.
    fn path(&self, rng: &mut SplitMix64, duration: u64, sigma_bps: f64) -> Vec<Waypoint> {
        let mut points = Vec::new();
        let mut level_bps = 0.0;
        let mut slot = 0;
        loop {
            slot += rng.range_u64(self.path_gap_slots);
            if slot + 3 > duration {
                break;
            }
            level_bps += sigma_bps * rng.normal();
            points.push(Waypoint {
                slot,
                level_pct: level_bps / 100.0,
                premium_pct: 0.0,
            });
            assert!(points.len() <= PATH_POINTS_MAX);
        }
        points
    }

    fn cycle_searcher(&self, rng: &mut SplitMix64, contest: Contest) -> CycleSearcher {
        let bid_share = bid_share(rng);
        let pays = match rng.next_f64() < self.cycle_tip_odds {
            true => Pays::Tip,
            false => Pays::PriorityFee,
        };
        let max_legs = match rng.next_f64() < self.two_leg_odds {
            true => 2,
            false => 3,
        };
        CycleSearcher {
            latency_slots: latency(rng, contest),
            bid_share,
            pays,
            fixed_sol: None,
            min_profit_sol: rng.log_range_f64(self.min_profit_sol),
            max_legs,
        }
    }

    /// A contested session's fast searcher: it reaches every trigger's
    /// slot, tips, and searches three legs; its share and least profit are
    /// drawn as any cycle searcher's. (How it pays doesn't move its rank:
    /// the builder ranks a tip and a priority fee on one scale.)
    fn fast_searcher(&self, rng: &mut SplitMix64) -> CycleSearcher {
        let searcher = CycleSearcher {
            latency_slots: [0, 0],
            bid_share: bid_share(rng),
            pays: Pays::Tip,
            fixed_sol: None,
            min_profit_sol: rng.log_range_f64(self.min_profit_sol),
            max_legs: 3,
        };
        assert!(reaches_every_trigger(&searcher));
        searcher
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

/// The index `u` falls on in cumulative `odds`.
fn pick(u: f64, odds: &[f64]) -> usize {
    let mut cumulative = 0.0;
    for (i, chance) in odds.iter().enumerate() {
        cumulative += chance;
        if u < cumulative {
            return i;
        }
    }
    odds.len() - 1
}

/// A searcher's share of the profit it expects, paid to the leader: the
/// research's backrun winners paid 0 to 93% of their gross, a mix of
/// overlapping ranges, as `backrun-sol-usdc`'s.
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

/// Whether `searcher` lands a tipped bundle in every trigger's slot.
fn reaches_every_trigger(searcher: &CycleSearcher) -> bool {
    searcher.latency_slots == [0, 0] && searcher.pays == Pays::Tip && searcher.max_legs >= 3
}

/// A searcher's latency range, in slots: the research's backruns landed in
/// the trigger's block 77 times in 106, and competitors re-arbed what was
/// left within a slot or two (57 of 82 latest cases). In a quiet session
/// none starts at 0.
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

/// The session's mandate, v0.3.0: any Orca or Raydium pool among the
/// graph's five tokens, SOL as the numeraire with at most $5 of anything
/// else after any transaction (a cycle ends in SOL), a loss limit and a
/// budget for fees and tips.
fn mandate() -> Value {
    json!({
        "version": "v0.3.0",
        "universe": [
            { "instrument": "orca:amm:*", "assets": ASSETS },
            { "instrument": "raydium:amm:*", "assets": ASSETS },
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

    #[test]
    fn a_set_replays() {
        let family = ThinPoolsFamily::snapshot();
        let set = family.draw_set("hidden", 12, 7);
        let again = family.draw_set("hidden", 12, 7);
        for ((a, sa), (b, sb)) in set.iter().zip(&again) {
            assert_eq!(sa, sb);
            assert_eq!(a.name, b.name);
            assert!(a.amm.is_some() && a.tx_layer.is_some());
        }
    }

    /// Over many sessions: some hold no trigger, both sides trigger, most
    /// on thin pools, impacts spread around the research's median, and both
    /// search depths compete.
    #[test]
    fn triggers_and_searchers_come_from_the_research_s_ranges() {
        let family = ThinPoolsFamily::snapshot();
        let set = family.draw_set("t", 300, 3);
        let mut impacts: Vec<f64> = Vec::new();
        let (mut calm, mut sells, mut deep, mut two, mut three) = (0, 0, 0, 0, 0);
        for (_, shape) in &set {
            if shape.triggers.is_empty() {
                calm += 1;
            }
            for t in &shape.triggers {
                impacts.push(t.impact_bps);
                if t.side == WhaleSide::Sell {
                    sells += 1;
                }
                if POOLS[..3].iter().any(|p| p.instrument == t.pool) {
                    deep += 1;
                }
                assert!(t.slot >= family.trigger_from_slot && t.slot < shape.duration_slots);
            }
            for s in &shape.searchers {
                match s.max_legs {
                    2 => two += 1,
                    _ => three += 1,
                }
            }
        }
        let total = impacts.len();
        assert!(calm > 10 && calm < 60, "{calm}");
        assert!(
            sells * 3 > total && sells * 3 < total * 2,
            "{sells} of {total}"
        );
        assert!(deep > 0 && deep * 5 < total, "{deep} of {total}");
        impacts.sort_by(f64::total_cmp);
        let median = impacts[total / 2];
        assert!((350.0..800.0).contains(&median), "{median}");
        assert!(two > 50 && three > 50, "{two} {three}");
    }

    /// Most sessions hold a searcher that reaches every trigger's slot and
    /// tips; the quiet rest hold none that can reach it at all.
    #[test]
    fn most_sessions_are_contested_and_quiet_ones_are_out_of_reach() {
        let family = ThinPoolsFamily::snapshot();
        let set = family.draw_set("t", 300, 5);
        let mut quiet = 0;
        for (_, shape) in &set {
            let fast = shape.searchers.iter().filter(|s| reaches_every_trigger(s));
            match shape.contest {
                Contest::Contested => assert!(fast.count() >= 1),
                Contest::Quiet => {
                    quiet += 1;
                    assert_eq!(fast.count(), 0);
                    let reach = shape.searchers.iter().map(|s| s.latency_slots[0]);
                    let pools = shape.pool_searchers.iter().map(|s| s.latency_slots[0]);
                    assert!(reach.chain(pools).all(|low| low >= 1));
                }
            }
        }
        assert!(quiet > 15 && quiet < 60, "{quiet} of 300");
    }

    /// Each session draws its own token prices; JitoSOL's stays inside the
    /// stake pool's band at every waypoint.
    #[test]
    fn token_references_are_the_session_s_own_and_jitosol_stays_in_its_band() {
        let family = ThinPoolsFamily::snapshot();
        let set = family.draw_set("t", 50, 9);
        let mut starts: Vec<f64> = Vec::new();
        for (scenario, shape) in &set {
            assert_eq!(shape.references.len(), 3);
            let jitosol = &shape.references[0];
            assert_eq!(jitosol.mint, JITOSOL);
            let band = |price: f64| {
                let bps = (price / JITOSOL_STAKE_SOL - 1.0) * 10_000.0;
                (JITOSOL_BAND_BPS.0 - 1e-6..=JITOSOL_BAND_BPS.1 + 1e-6).contains(&bps)
            };
            assert!(band(jitosol.start_sol));
            for w in &jitosol.path {
                assert!(band(jitosol.start_sol * (1.0 + w.level_pct / 100.0)));
            }
            for r in &shape.references[1..] {
                assert!(!r.path.is_empty());
            }
            starts.push(shape.references[1].start_sol);
            let amm = scenario.amm.as_ref().unwrap();
            assert_eq!(amm.reference_paths.len(), 3);
        }
        starts.sort_by(f64::total_cmp);
        starts.dedup();
        assert_eq!(starts.len(), 50, "a level per session");
    }

    #[test]
    fn the_latent_state_holds_no_trigger() {
        let (_, shape) = ThinPoolsFamily::snapshot().draw("x", 11);
        let latent = shape.latent();
        let keys: Vec<&str> = latent
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["flow_scale", "pool_searchers", "searchers"]);
    }
}
