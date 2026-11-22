//! The backrun-sol-usdc informed bot, scoring v2's 1: the reference's
//! backrunner ([`backrunner`], the same file, so the two can't drift
//! apart), told the session's latent state as `MTX_REGIME`: each cycle
//! searcher's latency, size and bid rule, each pool searcher's, and the calm
//! flow's rate. Never the orders, their chunks or the reference path.
//!
//! What it does with it: it bids what pays best against the session's
//! searchers, trigger by trigger. A searcher reaches the trigger's slot
//! when its drawn latency is 0, which its range sets the odds of; one that
//! tips outranks a bundle that bids less per requested compute unit, and
//! one that pays a priority fee lands behind every bundle on a Jito
//! leader's block, so it sets no bid. So for each bid worth considering
//! (the least that outbids the background's bundles, or just over one
//! tipping rival's) it weighs the chance no rival above it reaches the slot
//! against the profit left after the tip, takes the best, and stays out
//! when none pays. It knows no more than that: the same costs, mandate and
//! latency, and nothing of when the next chunk comes.

#[path = "../../../../reference/src/backrunner.rs"]
pub mod backrunner;

use backrunner::{Backrunner, Bidding, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::{log, prelude::*, serde_json};

/// Compute units a bundle asks for beside its swaps: its tip's transfer,
/// which asks for the default 200,000 (the layer's reports).
const TIP_UNITS: f64 = 200_000.0;
/// What a cycle asks for, a searcher's or this bot's: its simulation's
/// units (150k to 230k on the snapshot) and a margin, so about 210,000.
const CYCLE_UNITS: f64 = 210_000.0;
/// What a pool searcher's one swap asks for, the same way.
const RIVAL_POOL_UNITS: f64 = 120_000.0;
/// What a pool searcher grosses where a cycle would make 1: it trades the
/// moved pool alone, against the reference (NOTES.md §2: 0.88 to 1.02 SOL
/// where the cycle's optimum was about 0.70).
const POOL_GROSS_RATIO: f64 = 1.35;
/// What it bids over a rival's tip it means to beat.
const OUTBID: f64 = 1.1;
/// The least it tips, in SOL: a bundle that writes a contended pool must
/// also outbid the background's bundles there, whose tips are log-normal
/// around Jito's tip floor (the calibration's: median 2,500 lamports,
/// p99 about 800,000), so 2,000,000 lamports lands nearly always and costs
/// a sliver of any cycle worth sending.
const LEAST_TIP_SOL: f64 = 0.002;
/// Searchers a session holds at most (the grader's `SEARCHERS_MAX`).
const RIVALS_MAX: usize = 16;

/// How a rival trades: a cycle of the best size or a fixed clip, or one
/// pool back to the reference.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Cycle { fixed_sol: Option<f64> },
    Pool,
}

/// One rival that tips: how it trades, the share of its expected profit it
/// tips, the least it sends for, and the chance it reaches the trigger's
/// slot.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rival {
    kind: Kind,
    share: f64,
    min_profit_sol: f64,
    reaches: f64,
}

/// The session's tipping rivals, from its latent state.
#[derive(Debug, Default)]
pub struct Rivals {
    rivals: Vec<Rival>,
}

impl Rivals {
    /// From `MTX_REGIME`: `searchers` and `pool_searchers`, each with its
    /// `latency_slots`, `bid_share`, `pays` and `min_profit_sol`, and a
    /// cycle searcher's `fixed_sol`.
    pub fn from_regime(regime: &str) -> Result<Self, String> {
        let latent: serde_json::Value = serde_json::from_str(regime).map_err(|e| e.to_string())?;
        let cycles = latent["searchers"]
            .as_array()
            .ok_or("MTX_REGIME has no `searchers`")?;
        let pools = latent["pool_searchers"]
            .as_array()
            .ok_or("MTX_REGIME has no `pool_searchers`")?;
        if cycles.len() + pools.len() > RIVALS_MAX {
            return Err("more searchers than a session holds".into());
        }
        let mut rivals = Vec::new();
        for (spec, is_cycle) in cycles
            .iter()
            .map(|s| (s, true))
            .chain(pools.iter().map(|s| (s, false)))
        {
            if spec["pays"].as_str() != Some("tip") {
                continue;
            }
            let share = spec["bid_share"]
                .as_f64()
                .ok_or("a searcher without a bid")?;
            if !(0.0..=1.0).contains(&share) {
                return Err(format!("a bid share out of range: {share}"));
            }
            let latency = spec["latency_slots"]
                .as_array()
                .and_then(|l| Some((l.first()?.as_u64()?, l.get(1)?.as_u64()?)))
                .ok_or("a searcher without its latency")?;
            if latency.0 > latency.1 {
                return Err(format!("a latency range out of order: {latency:?}"));
            }
            // A send's latency is drawn evenly in the range: 0, the
            // trigger's own slot, once in its width.
            let reaches = match latency.0 {
                0 => 1.0 / (latency.1 + 1) as f64,
                _ => 0.0,
            };
            rivals.push(Rival {
                kind: match is_cycle {
                    true => Kind::Cycle {
                        fixed_sol: spec["fixed_sol"].as_f64(),
                    },
                    false => Kind::Pool,
                },
                share,
                min_profit_sol: spec["min_profit_sol"].as_f64().unwrap_or(0.01),
                reaches,
            });
        }
        Ok(Self { rivals })
    }

    /// What `rival` tips for `found`, as this bot's tip would have to be to
    /// rank with it, in SOL; `None` when it doesn't send.
    fn bar_sol(rival: &Rival, found: &Opportunity) -> Option<f64> {
        let (profit_sol, units) = match rival.kind {
            Kind::Cycle {
                fixed_sol: Some(sol),
            } => (found.profit_of_sol(sol)?, CYCLE_UNITS),
            Kind::Cycle { fixed_sol: None } => (found.profit_sol(), CYCLE_UNITS),
            Kind::Pool => (found.profit_sol() * POOL_GROSS_RATIO, RIVAL_POOL_UNITS),
        };
        if profit_sol < rival.min_profit_sol {
            return None;
        }
        let tip_sol = (rival.share * profit_sol).max(MIN_TIP_LAMPORTS as f64 / 1e9);
        let own_units = CYCLE_UNITS + TIP_UNITS;
        Some(tip_sol * own_units / (units + TIP_UNITS))
    }
}

impl Bidding for Rivals {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        let profit_sol = found.profit_sol();
        assert!(profit_sol > 0.0, "a cycle that profits");
        // Each rival that may reach the slot, with the bar it sets.
        let mut bars: Vec<(f64, f64)> = self
            .rivals
            .iter()
            .filter(|r| r.reaches > 0.0)
            .filter_map(|r| Some((Self::bar_sol(r, found)?, r.reaches)))
            .collect();
        bars.sort_by(|a, b| a.0.total_cmp(&b.0));
        let least_sol = LEAST_TIP_SOL.max(MIN_TIP_LAMPORTS as f64 / 1e9);
        let mut best: Option<(f64, f64)> = None;
        for candidate in std::iter::once(least_sol).chain(bars.iter().map(|b| b.0 * OUTBID)) {
            let wins: f64 = bars
                .iter()
                .filter(|(bar, _)| *bar >= candidate)
                .map(|(_, reaches)| 1.0 - reaches)
                .product();
            let value = wins * (profit_sol - candidate);
            if best.is_none_or(|(_, v)| value > v) {
                best = Some((candidate, value));
            }
        }
        let (tip_sol, value) = best.expect("the least tip is a candidate");
        if value <= 0.0 {
            return None;
        }
        Some(((tip_sol * 1e9) as u64).max(MIN_TIP_LAMPORTS))
    }
}

#[derive(Default)]
pub struct Informed {
    backrunner: Backrunner,
    rivals: Rivals,
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        let regime = session
            .var("MTX_REGIME")
            .ok_or("the informed bot runs told the session's latent state (MTX_REGIME)")?;
        self.rivals = Rivals::from_regime(regime)?;
        log(&format!("tipping rivals: {:?}", self.rivals));
        self.backrunner.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.backrunner.step(market, orders, &self.rivals)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tipping_rivals_count_and_latency_sets_the_odds() {
        let rivals = Rivals::from_regime(
            r#"{"searchers": [
                {"latency_slots": [0, 1], "bid_share": 0.4, "pays": "tip", "min_profit_sol": 0.01},
                {"latency_slots": [0, 0], "bid_share": 0.9, "pays": "priority_fee", "min_profit_sol": 0.01},
                {"latency_slots": [1, 3], "bid_share": 0.9, "pays": "tip", "min_profit_sol": 0.01, "fixed_sol": 200.0}
            ], "pool_searchers": [], "flow_scale": 1.0}"#,
        )
        .unwrap();
        assert_eq!(rivals.rivals.len(), 2);
        assert_eq!(rivals.rivals[0].reaches, 0.5);
        assert_eq!(rivals.rivals[1].reaches, 0.0);
        assert!(Rivals::from_regime("{}").is_err());
        assert!(
            Rivals::from_regime(
                r#"{"searchers": [{"latency_slots": [2, 1], "bid_share": 0.4, "pays": "tip"}], "pool_searchers": []}"#
            )
            .is_err()
        );
    }
}
