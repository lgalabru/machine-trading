//! The liquidation-flash family: Kamino's main market, SOL collateral and
//! USDC debt, with Scope's SOL price pushed rather than written, competing
//! liquidators and Scope's keeper in the transaction layer's auction, and
//! Orca's and Raydium's SOL/USDC pools to sell seized SOL through
//! (RFD 0005, task 1; ADR 0004).
//!
//! Every range comes from the research (`docs/RESEARCH.md`, "Kamino Lend
//! liquidations", and `ops/research/atomic-arb/kamino-liquidations/NOTES.md`
//! §6), with borrowers five to ten times mainnet's median size so a
//! liquidation pays more than the fee noise. A session draws:
//!
//! - **SOL's upstream path.** Legs of drift, each up or down, of a size and
//!   a length drawn independently; the first leg goes either way at even
//!   odds (a set is stratified on it), and each later one falls with a
//!   chance drawn per session. Within a leg the price ticks at uneven gaps
//!   of 1 to 9 slots by the leg's drift plus noise of a few basis points
//!   (the research's ticks: p50 3.6 bp, p90 21 bp), with an occasional jump
//!   of 20 to 65 bp. So some sessions fall far, some recover, some rally
//!   and liquidate nothing, and the session ends while the path still
//!   moves.
//! - **The borrowers.** How many, their collateral (log-normal around a
//!   median drawn per session, clipped), and their health when the session
//!   starts, from just above the threshold to a range drawn per session.
//! - **The competition.** How many liquidators, how they bid (a share of
//!   the bonus they expect, as a tip or a priority fee), how fast they
//!   react and how many join a race, each drawn per session
//!   ([`crate::lending::auction::Race`]); and the keeper's cadence.
//!
//! The pools are the snapshot's own, moved back to the path by the pools'
//! arbitrageur every step; their depth isn't drawn.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    lending::auction::{Bid, Race},
    oracles::Level,
    scenario::{Scenario, SplitMix64},
};

/// What a session was drawn as, beneath its scenario: the ladder's told
/// rungs see all of it, the informed bot only [`LiquidationShape::latent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiquidationShape {
    /// Whether the path's first leg falls, and the chance each later one
    /// does.
    pub first_down: bool,
    pub down_odds: f64,
    /// The path's lowest and highest level and where it ends, in percent of
    /// the start.
    pub low_pct: f64,
    pub high_pct: f64,
    pub end_pct: f64,
    pub borrowers: u32,
    /// The borrowers' collateral: the median, its log spread, and the range
    /// it is clipped to, in USD.
    pub size_median_usd: f64,
    pub size_sigma: f64,
    /// The highest health a borrower starts at.
    pub health_high: f64,
    /// How many liquidators compete, how each one bids, and the race's
    /// rules.
    pub liquidators: u32,
    pub bids: Vec<LiquidatorBid>,
    pub race: Race,
    /// Slots between the keeper's pushes of Scope.
    pub keeper_every_slots: u64,
    /// The path itself: what the ladder's path oracle reads ahead in.
    pub path: Vec<Level>,
}

/// One liquidator's bid rule, as the auction draws it
/// ([`crate::lending::auction::Bid`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LiquidatorBid {
    pub share: f64,
    pub tips: bool,
}

impl LiquidationShape {
    /// What the informed bot is told: the competition and the keeper, the
    /// process the session's races are drawn from. Never the path, nor
    /// which liquidator joins which race, nor how late.
    pub fn latent(&self) -> Value {
        json!({
            "liquidators": self.liquidators,
            "bids": self.bids,
            "bid_share": self.race.bid_share,
            "tipping_share": self.race.tipping_share,
            "same_slot_share": self.race.same_slot_share,
            "competition": self.race.competition,
            "keeper_every_slots": self.keeper_every_slots,
        })
    }
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct LiquidationFamily {
    pub duration_slots: (u64, u64),
    /// The chance a leg after the first falls, drawn per session.
    pub down_odds: (f64, f64),
    /// A leg's drift, in basis points, log-uniform, and its length in slots.
    pub leg_bps: (f64, f64),
    pub leg_slots: (u64, u64),
    /// The deepest the path may go, in basis points below the start: a leg
    /// that would go further turns. SOL fell 11% on the research's worst day.
    pub floor_bps: f64,
    /// Slots between ticks, and each tick's noise: log-normal around a
    /// median, in basis points, either way.
    pub tick_gap_slots: (u64, u64),
    pub noise_median_bps: f64,
    pub noise_sigma: f64,
    pub noise_max_bps: f64,
    /// The chance a tick jumps, the jump's size in basis points, and the
    /// chance it goes the leg's way.
    pub jump_odds: f64,
    pub jump_bps: (f64, f64),
    pub jump_with_leg: f64,
    pub borrowers: (u64, u64),
    /// The borrowers' median collateral per session (log-uniform), its log
    /// spread, and the range it is clipped to, in USD.
    pub size_median_usd: (f64, f64),
    pub size_sigma: (f64, f64),
    pub size_clip_usd: (f64, f64),
    /// Borrowers start between `health_low` and a top drawn per session,
    /// log-uniform over its distance from 1.
    pub health_low: f64,
    pub health_high: (f64, f64),
    pub liquidators: (u64, u64),
    /// The race's rules per session: the low end of the bid shares and the
    /// range's width, the tipping share, the same-slot share and the
    /// competition's scale.
    pub bid_share_low: (f64, f64),
    pub bid_share_width: (f64, f64),
    pub tipping_share: (f64, f64),
    pub same_slot_share: (f64, f64),
    pub competition: (f64, f64),
    pub keeper_every_slots: (u64, u64),
    /// SOL's price on the snapshot, for the layer's fee accounting.
    pub sol_usd: f64,
    /// The USDC in the bot's wallet when the session starts.
    pub wallet_usdc: f64,
}

/// The Scope account, SOL's entry and its TWAP's on the main market
/// (NOTES.md §5).
const SCOPE_PRICES: &str = "3t4JZcueEzTbVP6kLxXrL3VpWx45jDer4eqysweBchNH";
const SCOPE_SOL_ENTRY: u16 = 3;
const SCOPE_SOL_TWAP_ENTRY: u16 = 455;
/// Orca's and Raydium's SOL/USDC pools, cloned on the snapshot.
const WHIRLPOOL: &str = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE";
const RAYDIUM_CLMM: &str = "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv";
const WRAPPED_SOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// Most points a path holds: a tick a slot over the longest session.
const PATH_POINTS_MAX: usize = 1_024;

impl LiquidationFamily {
    pub fn kamino_main() -> Self {
        Self {
            duration_slots: (260, 360),
            down_odds: (0.25, 0.8),
            leg_bps: (20.0, 450.0),
            leg_slots: (30, 150),
            floor_bps: 1_100.0,
            // Scope updated every 5 slots at the median while active.
            tick_gap_slots: (1, 9),
            noise_median_bps: 1.5,
            noise_sigma: 0.9,
            noise_max_bps: 20.0,
            jump_odds: 0.03,
            jump_bps: (20.0, 65.0),
            jump_with_leg: 0.75,
            borrowers: (4, 14),
            // Mainnet's USDC←SOL debt p50 is $76 and all debts' $165; five to
            // ten times that, at the reserve's 75% threshold.
            size_median_usd: (1_100.0, 2_600.0),
            size_sigma: (0.9, 1.6),
            size_clip_usd: (20.0, 150_000.0),
            // Health just before the trigger: p50 1.8e-4, p90 1.2e-3.
            health_low: 1.0001,
            health_high: (1.001, 1.04),
            // At least two: the research's large liquidations drew 4.4
            // rivals on average, and a session of one can't contest them.
            liquidators: (2, 5),
            // The research: 0.25 to 0.45 of the bonus overall, 0.28 to 0.44
            // by size, tips in 46% of wins; a session's own spread wider.
            bid_share_low: (0.15, 0.5),
            bid_share_width: (0.05, 0.3),
            tipping_share: (0.2, 0.8),
            // 50% in the trigger's slot overall.
            same_slot_share: (0.3, 0.7),
            competition: (0.4, 1.6),
            keeper_every_slots: (2, 12),
            sol_usd: 70.26,
            wallet_usdc: 1_000.0,
        }
    }

    /// `count` scenarios named `<prefix>-<n>`, half of them (rounded) with
    /// a first leg that falls, in an order of `seed`'s own.
    pub fn draw_set(
        &self,
        prefix: &str,
        count: usize,
        seed: u64,
    ) -> Vec<(Scenario, LiquidationShape)> {
        let downs = count / 2 + (count % 2) * usize::from(seed.is_multiple_of(2));
        let mut left = [count - downs, downs];
        let mut drawn = Vec::with_capacity(count);
        let mut draw = 0u64;
        while drawn.len() < count {
            draw += 1;
            assert!(draw < 64 * count as u64 + 64, "a set fills its two cells");
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let cell = usize::from(Self::first_down(draw_seed));
            if left[cell] == 0 {
                continue;
            }
            left[cell] -= 1;
            drawn.push(draw_seed);
        }
        crate::scenario::shuffled(drawn, seed)
            .into_iter()
            .enumerate()
            .map(|(i, draw_seed)| self.draw(&format!("{prefix}-{}", i + 1), draw_seed))
            .collect()
    }

    /// Whether a seed's first leg falls: the first draw of [`Self::draw`].
    fn first_down(seed: u64) -> bool {
        SplitMix64(seed).next_f64() < 0.5
    }

    /// One session.
    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, LiquidationShape) {
        let mut rng = SplitMix64(seed);
        let first_down = rng.next_f64() < 0.5;
        assert_eq!(first_down, Self::first_down(seed));
        let duration = rng.range_u64(self.duration_slots);
        let down_odds = rng.range_f64(self.down_odds);
        let path = self.path(&mut rng, duration, first_down, down_odds);

        let borrowers = rng.range_u64(self.borrowers) as u32;
        let size_median_usd = rng.log_range_f64(self.size_median_usd);
        let size_sigma = rng.range_f64(self.size_sigma);
        let health_high =
            1.0 + rng.log_range_f64((self.health_high.0 - 1.0, self.health_high.1 - 1.0));
        let liquidators = rng.range_u64(self.liquidators) as u32;
        let low = rng.range_f64(self.bid_share_low);
        let high = (low + rng.range_f64(self.bid_share_width)).min(1.0);
        let race = Race {
            bid_share: Some([low, high]),
            tipping_share: Some(rng.range_f64(self.tipping_share)),
            same_slot_share: Some(rng.range_f64(self.same_slot_share)),
            competition: Some(rng.range_f64(self.competition)),
        };
        assert!(race.validate().is_ok(), "{race:?}");
        let keeper_every_slots = rng.range_u64(self.keeper_every_slots);
        let lending_seed = rng.next_u64() % 1_000_000;
        let oracle_seed = rng.next_u64() % 1_000_000;
        let layer_seed = rng.next_u64() % 1_000_000;
        let bids = (0..liquidators)
            .map(|i| {
                let bid = Bid::draw(lending_seed, i, &race);
                LiquidatorBid {
                    share: bid.share,
                    tips: bid.tips,
                }
            })
            .collect();

        // The clip keeps the median inside it, so the range's geometric
        // middle is the median the lending session draws around.
        let spread =
            (self.size_clip_usd.1 / size_median_usd).min(size_median_usd / self.size_clip_usd.0);
        assert!(spread > 1.0, "the median inside the clip");
        let size_usd = [size_median_usd / spread, size_median_usd * spread];
        let levels =
            |f: fn(f64, f64) -> f64, start: f64| path.iter().map(|l| l.level_pct).fold(start, f);
        let shape = LiquidationShape {
            first_down,
            down_odds,
            low_pct: levels(f64::min, 0.0),
            high_pct: levels(f64::max, 0.0),
            end_pct: path.last().map_or(0.0, |l| l.level_pct),
            borrowers,
            size_median_usd,
            size_sigma,
            health_high,
            liquidators,
            bids,
            race,
            keeper_every_slots,
            path,
        };
        let scenario = json!({
            "schema": 2,
            "name": name,
            "description": format!(
                "SOL on Kamino's main market for {duration} slots, Scope's price pushed: {borrowers} borrowers, {liquidators} competing liquidators, a keeper every {keeper_every_slots} slots (liquidation-flash)"
            ),
            "duration_slots": duration,
            "assets": { "symbol": "SOL" },
            "venues": {
                "lending": {
                    "seed": lending_seed,
                    "reserves": ["SOL", "USDC"],
                    "wallet": { "USDC": self.wallet_usdc },
                    "borrowers": {
                        "count": borrowers,
                        "collateral": "SOL",
                        "debt": "USDC",
                        "health": [self.health_low, health_high],
                        "size_usd": size_usd,
                        "size_sigma": size_sigma,
                    },
                    "liquidators": { "count": liquidators, "race": race },
                    "keeper": { "every_slots": keeper_every_slots, "cu_price": 100 },
                },
                "amm": {
                    "pools": [format!("orca:amm:{WHIRLPOOL}"), format!("raydium:amm:{RAYDIUM_CLMM}")],
                },
            },
            "oracles": {
                "seed": oracle_seed,
                "feeds": [{
                    "name": "sol",
                    "source": {
                        "kind": "scope",
                        "account": SCOPE_PRICES,
                        "index": SCOPE_SOL_ENTRY,
                        "twap_index": SCOPE_SOL_TWAP_ENTRY,
                        "mode": "pushed",
                    },
                    "cadence_slots": 1,
                    "path": shape.path,
                }],
            },
            "tx": {
                "layer": {
                    "seed": layer_seed,
                    "hot_accounts": [
                        { "address": WHIRLPOOL, "class": "amm_pool" },
                        { "address": RAYDIUM_CLMM, "class": "amm_pool" },
                        { "address": SCOPE_PRICES, "class": "oracle" },
                    ],
                    // The research's lag from the update to the landing
                    // holds every delay a send meets, the route's
                    // included: the liquidators draw it, and a route adds
                    // none on top, the bot's or theirs. Sends made in one
                    // step reach one slot, where the bid ranks them.
                    "routes": {
                        "rpc": { "latency_slots": [0, 0], "loss": 0.01, "loss_congested": 0.3 },
                        "jito": { "latency_slots": [0, 0], "loss": 0.0, "loss_congested": 0.0 },
                    },
                    "fees_in_pnl": true,
                    "native_sol": 1.0,
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
        (scenario, shape)
    }

    /// SOL's upstream path, in percent of its start, a point per tick.
    fn path(
        &self,
        rng: &mut SplitMix64,
        duration: u64,
        first_down: bool,
        down_odds: f64,
    ) -> Vec<Level> {
        let mut points = vec![Level {
            slot: 0,
            level_pct: 0.0,
        }];
        let mut level_bps = 0.0;
        let mut slot = 0u64;
        let mut down = first_down;
        let mut legs = 0u32;
        while slot < duration {
            if legs > 0 {
                down = rng.next_f64() < down_odds;
            }
            legs += 1;
            let leg_slots = rng.range_u64(self.leg_slots);
            let mut leg_bps = rng.log_range_f64(self.leg_bps);
            if down && level_bps - leg_bps < -self.floor_bps {
                leg_bps = (level_bps + self.floor_bps).max(0.0);
            }
            let sign = if down { -1.0 } else { 1.0 };
            let leg_end = (slot + leg_slots).min(duration);
            while slot < leg_end {
                let gap = rng.range_u64(self.tick_gap_slots).min(leg_end - slot);
                slot += gap;
                let drift = sign * leg_bps * gap as f64 / leg_slots as f64;
                let noise_sign = if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
                let noise = noise_sign
                    * (self.noise_median_bps * (self.noise_sigma * rng.normal()).exp())
                        .min(self.noise_max_bps);
                let jump = if rng.next_f64() < self.jump_odds {
                    let with_leg = rng.next_f64() < self.jump_with_leg;
                    let size = rng.range_f64(self.jump_bps);
                    if with_leg { sign * size } else { -sign * size }
                } else {
                    0.0
                };
                level_bps = (level_bps + drift + noise + jump).max(-self.floor_bps);
                points.push(Level {
                    slot,
                    level_pct: level_bps / 100.0,
                });
                assert!(points.len() <= PATH_POINTS_MAX, "a tick per slot at most");
            }
        }
        assert!(points.windows(2).all(|w| w[0].slot < w[1].slot));
        assert_eq!(points.last().map(|l| l.slot), Some(duration));
        points
    }
}

/// The session's mandate, v0.3.0 (`specs/examples/liquidation-flash.json`):
/// Kamino's SOL and USDC reserves with flash borrows, any Orca or Raydium
/// pool between SOL and USDC, a loss limit, at most $100 of anything but
/// USDC after any transaction, and a budget for fees and tips.
fn mandate() -> Value {
    json!({
        "version": "v0.3.0",
        "universe": [
            {
                "instrument": "kamino:money-market:USDC",
                "max_position": 50_000.0,
                "min_position": 0.0,
                "max_flash_borrow": 250_000.0,
            },
            {
                "instrument": "kamino:money-market:SOL",
                "max_position": 1.0,
                "min_position": 0.0,
                "max_flash_borrow": 1_500.0,
            },
            { "instrument": "orca:amm:*", "assets": [WRAPPED_SOL, USDC] },
            { "instrument": "raydium:amm:*", "assets": [WRAPPED_SOL, USDC] },
        ],
        "limits": {
            "max_drawdown_usd": 500.0,
            "inventory": { "numeraire": USDC, "max_other_usd": 100.0 },
            "max_costs_usd": 300.0,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A set is half falling first legs, every scenario reads back, and the
    /// draw is a pure function of the seed.
    #[test]
    fn a_set_splits_its_first_legs_and_replays() {
        let family = LiquidationFamily::kamino_main();
        let set = family.draw_set("hidden", 12, 7);
        assert_eq!(set.len(), 12);
        assert_eq!(set.iter().filter(|(_, s)| s.first_down).count(), 6);
        let again = family.draw_set("hidden", 12, 7);
        for ((a, sa), (b, sb)) in set.iter().zip(&again) {
            assert_eq!(sa, sb);
            assert_eq!(a.name, b.name);
            assert!(a.lending.is_some() && a.amm.is_some() && a.tx_layer.is_some());
        }
    }

    /// The path's ticks look like Scope's on mainnet: a median of a few
    /// basis points, uneven gaps, and both directions first.
    #[test]
    fn ticks_are_small_and_uneven() {
        let family = LiquidationFamily::kamino_main();
        let mut moves: Vec<f64> = Vec::new();
        let mut gaps: Vec<u64> = Vec::new();
        let mut lows = 0;
        let mut quiet = 0;
        for (_, shape) in family.draw_set("t", 200, 3) {
            for w in shape.path.windows(2) {
                moves.push((w[1].level_pct - w[0].level_pct).abs() * 100.0);
                gaps.push(w[1].slot - w[0].slot);
            }
            if shape.low_pct < -2.0 {
                lows += 1;
            }
            if shape.low_pct > -0.1 {
                quiet += 1;
            }
            assert!(shape.low_pct >= -family.floor_bps / 100.0 - 1e-9);
        }
        moves.sort_by(f64::total_cmp);
        let p50 = moves[moves.len() / 2];
        let p90 = moves[moves.len() * 9 / 10];
        assert!((1.5..12.0).contains(&p50), "p50 {p50}");
        assert!((8.0..40.0).contains(&p90), "p90 {p90}");
        assert!(gaps.contains(&1) && gaps.iter().any(|g| *g >= 8));
        // Some sessions fall far, and some never dip far enough to matter.
        assert!(lows > 40, "{lows}");
        assert!(quiet > 10, "{quiet}");
    }

    /// The latent state is the competition and the keeper, and none of the
    /// path.
    #[test]
    fn the_latent_state_holds_no_price() {
        let (_, shape) = LiquidationFamily::kamino_main().draw("x", 11);
        let latent = shape.latent();
        let keys: Vec<&str> = latent
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for forbidden in ["path", "low_pct", "end_pct", "down_odds", "first_down"] {
            assert!(!keys.contains(&forbidden), "{forbidden}");
        }
        assert_eq!(
            latent["bids"].as_array().unwrap().len() as u32,
            shape.liquidators
        );
    }
}
