//! The liquidation-flash informed bot, scoring v2's 1: the reference's
//! liquidator ([`liquidator`], the same file, so the two can't drift
//! apart), told the session's latent state as `MTX_REGIME`: how many
//! liquidators race, how each bids (a share of the bonus it expects to
//! keep, as a tip or a priority fee), how fast they react and how many
//! join, and the keeper's cadence. Never the path, nor which liquidator
//! joins which race.
//!
//! What it does with it: it bids what pays best against the session's
//! rivals, race by race. Each liquidator joins a race with a chance the
//! session's competition sets for its size, and acts in the slot it saw the
//! obligation turn with a chance its same-slot share sets; a tipping one
//! that does outranks a bundle that tips less per compute unit. So for each
//! bid worth considering (Jito's least, or just over one tipping rival's)
//! it weighs the chance no rival above it races it against the profit left
//! after the tip, takes the best, and stays out when none pays. A priority
//! fee buys nothing against a bundle on a Jito leader's block, so rivals
//! that pay one set no bid. It knows no more than that: same costs, same
//! mandate, same latency, and nothing of the path or of which rival joins
//! which race.

#[path = "../../../../reference/src/liquidator.rs"]
pub mod liquidator;

use liquidator::{Bidding, Liquidator, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::{log, prelude::*, serde_json};

/// A liquidator's transaction (a push and the liquidation, paid from its
/// own inventory) asks for about 245k to 271k compute units, and this
/// bot's group about 344k (the medians of the session reports): its tip
/// per unit must be this much larger to rank above the same tip.
const UNITS_RATIO: f64 = 1.45;
/// What it bids over a rival's tip it means to beat.
const OUTBID: f64 = 1.05;
/// The bonus a liquidator expects to keep: Kamino's main market keeps
/// half (the auction's `BONUS_KEPT_SHARE`).
const LIQUIDATOR_KEPT_SHARE: f64 = 0.5;
/// Where the race's size changes how many join, and their research means
/// (the auction's `competitors_mean`, NOTES.md §3).
const SMALL_USD: f64 = 10.0;
const LARGE_USD: f64 = 1_000.0;
const MEAN_SMALL: f64 = 0.56;
const MEAN_MEDIUM: f64 = 2.6;
const MEAN_LARGE: f64 = 4.4;
/// The research's lag shape (the auction's `Lag`): 2 slots or fewer at
/// `s + 0.42 (1 - s)` for a same-slot share `s`; above $1,000 a same-slot
/// share of `1 - 0.07 (1 - s) / 0.5`. A rival that acts within a slot
/// still lands in the step it saw the obligation turn.
const TWO_SLOTS_SHARE: f64 = 0.42;
const LARGE_TAIL: f64 = 0.07;
/// Rivals a session holds at most.
const RIVALS_MAX: usize = 5;

/// One rival's bid rule.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rival {
    share: f64,
    tips: bool,
}

/// The session's rivals and how they race, from its latent state.
#[derive(Debug, Default)]
pub struct Rivals {
    rivals: Vec<Rival>,
    competition: f64,
    same_slot_share: f64,
}

impl Rivals {
    /// From `MTX_REGIME`: `bids` (each liquidator's `share` and `tips`),
    /// `competition` and `same_slot_share`.
    pub fn from_regime(regime: &str) -> Result<Self, String> {
        let latent: serde_json::Value = serde_json::from_str(regime).map_err(|e| e.to_string())?;
        let bids = latent["bids"]
            .as_array()
            .ok_or("MTX_REGIME has no `bids`")?;
        if bids.len() > RIVALS_MAX {
            return Err(format!("{} rivals, more than a session holds", bids.len()));
        }
        let mut rivals = Vec::with_capacity(bids.len());
        for bid in bids {
            let share = bid["share"].as_f64().ok_or("a bid without a share")?;
            if !(0.0..=1.0).contains(&share) {
                return Err(format!("a share of the bonus out of range: {share}"));
            }
            rivals.push(Rival {
                share,
                tips: bid["tips"].as_bool() == Some(true),
            });
        }
        let competition = latent["competition"].as_f64().unwrap_or(1.0);
        let same_slot_share = latent["same_slot_share"].as_f64().unwrap_or(0.5);
        Ok(Self {
            rivals,
            competition,
            same_slot_share,
        })
    }

    /// The chance one rival races an obligation repaying `repay_usd` in the
    /// step it turns: it joins, and acts within a slot.
    fn races(&self, repay_usd: f64) -> f64 {
        let count = self.rivals.len();
        if count == 0 {
            return 0.0;
        }
        let mean = if repay_usd < SMALL_USD {
            MEAN_SMALL
        } else if repay_usd <= LARGE_USD {
            MEAN_MEDIUM
        } else {
            MEAN_LARGE
        };
        let joins = (mean * self.competition / count as f64).min(1.0);
        let s = self.same_slot_share;
        let prompt = if repay_usd > LARGE_USD {
            1.0 - LARGE_TAIL * (1.0 - s) / 0.5
        } else {
            // Same slot, and half the one-to-two-slot band.
            s + TWO_SLOTS_SHARE * (1.0 - s) / 2.0
        };
        let chance = joins * prompt;
        assert!((0.0..=1.0).contains(&chance));
        chance
    }
}

impl Bidding for Rivals {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        assert!(found.sol_usd > 0.0 && found.bonus_usd >= 0.0);
        let races = self.races(found.repay);
        // Each tipping rival's tip, as this bot's tip per unit would have
        // to match it, in USD.
        let mut bars: Vec<f64> = self
            .rivals
            .iter()
            .filter(|r| r.tips)
            .map(|r| r.share * found.bonus_usd * LIQUIDATOR_KEPT_SHARE * UNITS_RATIO)
            .collect();
        bars.sort_by(f64::total_cmp);
        let least_usd = MIN_TIP_LAMPORTS as f64 / 1e9 * found.sol_usd;
        let mut best: Option<(f64, f64)> = None;
        for candidate in std::iter::once(least_usd).chain(bars.iter().map(|b| b * OUTBID)) {
            let above = bars.iter().filter(|b| **b >= candidate).count();
            let wins = (1.0 - races).powi(above as i32);
            let value = wins * (found.profit_usd - candidate);
            if best.is_none_or(|(_, v)| value > v) {
                best = Some((candidate, value));
            }
        }
        let (tip_usd, value) = best.expect("the least tip is a candidate");
        if value <= 0.0 {
            return None;
        }
        Some(((tip_usd / found.sol_usd * 1e9) as u64).max(MIN_TIP_LAMPORTS))
    }
}

#[derive(Default)]
pub struct Informed {
    liquidator: Liquidator,
    rivals: Rivals,
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        let regime = session
            .var("MTX_REGIME")
            .ok_or("the informed bot runs told the session's latent state (MTX_REGIME)")?;
        self.rivals = Rivals::from_regime(regime)?;
        log(&format!("rivals: {:?}", self.rivals));
        self.liquidator.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.liquidator.step(market, orders, &self.rivals)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);

#[cfg(test)]
mod tests {
    use super::*;

    fn found(repay: f64) -> Opportunity {
        Opportunity {
            obligation: "o".into(),
            stale: true,
            repay,
            sell_sol: repay / 70.0,
            pool: "p".into(),
            limit: 69.0,
            bonus_usd: repay * 0.01,
            kept_usd: repay * 0.005,
            profit_usd: repay * 0.0045,
            sol_usd: 70.0,
        }
    }

    #[test]
    fn with_no_tipping_rival_it_tips_the_least() {
        let rivals =
            Rivals::from_regime(r#"{"bids": [{"share": 0.4, "tips": false}], "competition": 1.0, "same_slot_share": 0.5}"#)
                .unwrap();
        assert_eq!(rivals.tip(&found(500.0)), Some(MIN_TIP_LAMPORTS));
    }

    #[test]
    fn it_outbids_a_rival_that_races_often_and_skips_one_it_cannot_beat() {
        let eager = Rivals::from_regime(
            r#"{"bids": [{"share": 0.2, "tips": true}], "competition": 1.6, "same_slot_share": 0.7}"#,
        )
        .unwrap();
        let tip = eager.tip(&found(5_000.0)).unwrap();
        assert!(tip > MIN_TIP_LAMPORTS, "{tip}");
        // A rival bidding past the profit, racing nearly every time, and a
        // profit under Jito's least tip: nothing pays.
        let certain = Rivals {
            rivals: vec![Rival {
                share: 1.0,
                tips: true,
            }],
            competition: 4.0,
            same_slot_share: 0.95,
        };
        assert!(certain.races(5_000.0) > 0.99);
        let mut thin = found(5_000.0);
        thin.profit_usd = 1e-6;
        assert_eq!(certain.tip(&thin), None);
        assert!(Rivals::from_regime("{}").is_err());
        assert!(Rivals::from_regime(r#"{"bids": [{"share": 2.0, "tips": true}]}"#).is_err());
    }
}
