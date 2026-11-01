//! The market-making family: a market that walks, trends and jumps, a
//! competing maker quoting a set width around it, and the taker flow a bot
//! can earn the spread from.
//!
//! An offline surfnet has nobody to trade with, so the grader sends the flow
//! itself (see [`crate::session`]): orders from a shared pool of taker
//! accounts, listed in the scenario. Most are noise, either side at random.
//! Before each jump, some trade its way: that informed flow is what a maker
//! loses to. Some bursts are fake-outs with no jump after them, and informed
//! and noise orders come from the same accounts in overlapping sizes, so
//! telling them apart takes reading the market, not the generator.
//!
//! The competition is two layers. The snapshot's own Phoenix makers, their
//! spline liquidity widened at setup to the scenario's half-spread, re-centre
//! on every move and otherwise stand still. In front of them the grader runs
//! a maker of its own (see [`crate::compete`]): it quotes at the same
//! half-spread and, when a bot quotes inside it, steps inside the bot after
//! a short delay, down to a floor. Undercutting it starts a price war, so a
//! bot keeps the flow only as long as its quotes stay competitive, and what
//! it can earn per order shrinks toward the floor.
//!
//! Takers care about price: each order carries its own limit, so a maker
//! quoting wider than a taker will pay doesn't fill it at all.

use serde::{Deserialize, Serialize};

use crate::scenario::{Scenario, SplitMix64, VENUE, Waypoint};

/// Taker accounts the grader trades from. Every order picks one at random,
/// so an account says nothing about whether its flow is informed.
pub const TAKER_ACCOUNTS: u8 = 8;

/// What a market-making scenario adds to the price path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Making {
    /// The competing makers' half-spread around the mark, in basis points.
    pub competitor_bps: f64,
    /// How the grader's own maker competes.
    pub competition: Competition,
    /// The taker orders the grader sends, in slot order.
    pub takers: Vec<TakerOrder>,
}

/// The grader's maker (see [`crate::compete`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Competition {
    /// The tightest half-spread it goes to when undercut, in basis points.
    pub floor_bps: f64,
    /// Slots between its looks at the book: how fast it reacts.
    pub react_slots: u64,
    /// Its quote on each side, in base units.
    pub size: f64,
}

/// One market order from a taker, filled against the best quotes in the
/// book: the bot's when they're best, the competitor's otherwise.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TakerOrder {
    /// Slot, relative to the session start.
    pub slot: u64,
    pub side: TakerSide,
    /// In base units.
    pub size: f64,
    /// The worst price it takes, in basis points from the market's level:
    /// it fills only against quotes at least this close.
    pub limit_bps: f64,
    /// Which of the [`TAKER_ACCOUNTS`] sends it.
    pub account: u8,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TakerSide {
    Buy,
    Sell,
}

impl Making {
    pub fn validate(&self, duration_slots: u64) -> std::result::Result<(), String> {
        if !(self.competitor_bps.is_finite() && self.competitor_bps > 0.0) {
            return Err(format!(
                "competitor_bps {} must be positive",
                self.competitor_bps
            ));
        }
        let c = &self.competition;
        if !(c.floor_bps.is_finite() && c.floor_bps > 0.0 && c.floor_bps < self.competitor_bps) {
            return Err(format!(
                "competition floor_bps {} must be positive and under competitor_bps {}",
                c.floor_bps, self.competitor_bps
            ));
        }
        if c.react_slots == 0 || !(c.size.is_finite() && c.size > 0.0) {
            return Err(format!(
                "competition needs react_slots >= 1 and a positive size, not {} and {}",
                c.react_slots, c.size
            ));
        }
        let mut last = 0;
        for (i, order) in self.takers.iter().enumerate() {
            if order.slot < last {
                return Err(format!("taker order {i} is out of slot order"));
            }
            if order.slot >= duration_slots {
                return Err(format!(
                    "taker order {i} at slot {} is past the session",
                    order.slot
                ));
            }
            if !(order.size.is_finite() && order.size > 0.0) {
                return Err(format!("taker order {i} has size {}", order.size));
            }
            if !(order.limit_bps.is_finite() && order.limit_bps > 0.0) {
                return Err(format!("taker order {i} has limit {} bp", order.limit_bps));
            }
            if order.account >= TAKER_ACCOUNTS {
                return Err(format!(
                    "taker order {i} uses account {}, of {TAKER_ACCOUNTS}",
                    order.account
                ));
            }
            last = order.slot;
        }
        Ok(())
    }

    /// The most a maker could earn on this scenario's flow: every taker
    /// order filled from the market's level at that slot at the widest price
    /// it can fill at, the taker's own limit or the competitor's half-spread,
    /// whichever is closer, with nothing lost to informed flow or to
    /// inventory. No bot reaches it (the grader's maker undercuts it, informed
    /// flow costs it, and holding inventory through moves costs it).
    pub fn spread_bound_usd(&self, path: &[Waypoint], start_mark_usd: f64) -> f64 {
        self.takers
            .iter()
            .map(|order| {
                let price = start_mark_usd * (1.0 + level_at(path, order.slot) / 100.0);
                order.size * price * order.limit_bps.min(self.competitor_bps) / 10_000.0
            })
            .sum()
    }
}

/// The path's level at `slot`: the last waypoint at or before it.
pub fn level_at(path: &[Waypoint], slot: u64) -> f64 {
    path.iter()
        .take_while(|w| w.slot <= slot)
        .last()
        .map_or(0.0, |w| w.level_pct)
}

/// A market's regime. The family stratifies its sets by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    /// A random walk, no jumps: tight quotes win.
    Calm,
    /// A walk with jumps, each preceded by informed flow.
    Jumps,
    /// A walk that drifts one way, with flow leaning that way: symmetric
    /// quotes build a losing inventory.
    Trend,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct MakingFamily {
    pub symbol: String,
    pub calm_odds: f64,
    pub jumps_odds: f64,
    /// Session length, in slots.
    pub duration: (u64, u64),
    /// The competitor's half-spread, in basis points.
    pub competitor_bps: (f64, f64),
    /// Standard deviation of each walk step, in percent of the starting mark.
    pub step_vol_pct: (f64, f64),
    /// Slots between walk steps.
    pub step_gap: (u64, u64),
    /// A trend's drift per walk step, in percent.
    pub trend_drift_pct: (f64, f64),
    /// How much a trend tilts the noise flow its way: the chance a noise
    /// order goes with it, above one half.
    pub trend_lean: (f64, f64),
    /// Jumps in a Jumps session, and each one's size in percent.
    pub jumps: (u64, u64),
    pub jump_pct: (f64, f64),
    /// No jump this close to the start or the end, or to another jump.
    pub jump_margin: u64,
    /// Noise orders per slot.
    pub noise_rate: (f64, f64),
    /// Noise order sizes: lognormal around a median, clamped.
    pub size_median: f64,
    pub size_sigma: f64,
    pub size_clamp: (f64, f64),
    /// Odds a jump comes with informed flow ahead of it; the rest land
    /// unannounced.
    pub informed_odds: f64,
    /// Informed orders before a jump, how far ahead they start, and how much
    /// larger than noise they run.
    pub informed_orders: (u64, u64),
    pub informed_lead: (u64, u64),
    pub informed_scale: (f64, f64),
    /// Fake-out bursts per session, in any regime: informed-looking flow
    /// with no jump after it.
    pub fakeouts: (u64, u64),
    /// The grader's maker: its floor, in basis points, how many slots it
    /// takes to react, and its quote size.
    pub floor_bps: (f64, f64),
    pub react_slots: (u64, u64),
    pub competitor_size: (f64, f64),
    /// Taker limits, as multiples of the competitor's half-spread. Noise
    /// takers' are lognormal around 1, so some won't pay the competitor's
    /// price and fill only against a tighter quote; informed takers are in a
    /// hurry and pay up.
    pub noise_limit_sigma: f64,
    pub noise_limit_clamp: (f64, f64),
    pub informed_limit: (f64, f64),
    pub max_position: f64,
    pub max_drawdown_usd: f64,
}

impl MakingFamily {
    pub fn spcx() -> Self {
        Self {
            symbol: "SPCX".into(),
            calm_odds: 0.25,
            jumps_odds: 0.5,
            duration: (480, 800),
            competitor_bps: (8.0, 30.0),
            step_vol_pct: (0.02, 0.08),
            step_gap: (3, 9),
            trend_drift_pct: (0.006, 0.018),
            trend_lean: (0.05, 0.12),
            jumps: (1, 3),
            jump_pct: (1.5, 4.0),
            jump_margin: 60,
            noise_rate: (0.2, 0.45),
            size_median: 0.6,
            size_sigma: 0.7,
            size_clamp: (0.1, 4.0),
            informed_odds: 0.85,
            informed_orders: (8, 16),
            informed_lead: (4, 24),
            informed_scale: (2.0, 4.0),
            fakeouts: (0, 3),
            floor_bps: (2.0, 6.0),
            react_slots: (2, 8),
            competitor_size: (2.0, 5.0),
            noise_limit_sigma: 0.5,
            noise_limit_clamp: (0.3, 4.0),
            informed_limit: (2.0, 4.0),
            // ~0.3x the 10,000 USDC of collateral at ~$143: a maker shouldn't
            // warehouse risk.
            max_position: 20.0,
            // 2.5% of collateral: a full inventory through the largest jump
            // costs ~$70, so only a maker that keeps adding into a move gets
            // near it.
            max_drawdown_usd: 250.0,
        }
    }

    pub fn regime(&self, seed: u64) -> Regime {
        let draw = SplitMix64(seed).next_f64();
        if draw < self.calm_odds {
            Regime::Calm
        } else if draw < self.calm_odds + self.jumps_odds {
            Regime::Jumps
        } else {
            Regime::Trend
        }
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        let mut rng = SplitMix64(seed);
        // The regime comes first, so `regime` can read it alone.
        let regime = match rng.next_f64() {
            d if d < self.calm_odds => Regime::Calm,
            d if d < self.calm_odds + self.jumps_odds => Regime::Jumps,
            _ => Regime::Trend,
        };
        let duration = rng.range_u64(self.duration);
        let competitor_bps = round2(rng.range_f64(self.competitor_bps));
        let vol = rng.range_f64(self.step_vol_pct);
        let trend = if regime == Regime::Trend {
            let sign = if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
            Some((
                sign,
                rng.range_f64(self.trend_drift_pct),
                rng.range_f64(self.trend_lean),
            ))
        } else {
            None
        };

        // Jumps: slots spaced apart and away from the ends.
        let mut jumps: Vec<(u64, f64)> = Vec::new();
        if regime == Regime::Jumps {
            let count = rng.range_u64(self.jumps);
            let room = (self.jump_margin, duration - self.jump_margin);
            let mut tries = 0;
            while (jumps.len() as u64) < count && tries < 200 {
                tries += 1;
                let slot = rng.range_u64(room);
                if jumps
                    .iter()
                    .all(|(s, _)| s.abs_diff(slot) >= self.jump_margin)
                {
                    let sign = if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
                    jumps.push((slot, sign * rng.range_f64(self.jump_pct)));
                }
            }
            jumps.sort_by_key(|(s, _)| *s);
        }

        // The walk, with the jumps landing on their slots.
        let mut path = Vec::new();
        let mut level = 0.0;
        let mut slot = rng.range_u64(self.step_gap);
        let mut pending = jumps.iter().peekable();
        while slot + 4 < duration {
            let jump = pending.next_if(|(at, _)| *at <= slot);
            level = match jump {
                Some((_, size)) => level + size,
                None => {
                    let drift = trend.map_or(0.0, |(sign, drift, _)| sign * drift);
                    level + drift + vol * rng.normal()
                }
            };
            level = round3(level);
            path.push(Waypoint {
                slot,
                level_pct: level,
            });
            slot += rng.range_u64(self.step_gap);
        }

        // Flow: noise every slot at the scenario's rate, informed bursts
        // ahead of most jumps, and fake-outs.
        let rate = rng.range_f64(self.noise_rate);
        let mut takers = Vec::new();
        for slot in 0..duration {
            if rng.next_f64() >= rate {
                continue;
            }
            let buy_odds = 0.5 + trend.map_or(0.0, |(sign, _, lean)| sign * lean);
            let side = if rng.next_f64() < buy_odds {
                TakerSide::Buy
            } else {
                TakerSide::Sell
            };
            let size = self.size(&mut rng, 1.0);
            let (lo, hi) = self.noise_limit_clamp;
            let limit =
                competitor_bps * (self.noise_limit_sigma * rng.normal()).exp().clamp(lo, hi);
            takers.push(self.order(&mut rng, slot, side, size, limit));
        }
        let side_of = |size: f64| {
            if size > 0.0 {
                TakerSide::Buy
            } else {
                TakerSide::Sell
            }
        };
        let mut bursts: Vec<(u64, TakerSide)> = Vec::new();
        for &(at, size) in &jumps {
            if rng.next_f64() < self.informed_odds {
                bursts.push((at, side_of(size)));
            }
        }
        for _ in 0..rng.range_u64(self.fakeouts) {
            let at = rng.range_u64((self.jump_margin, duration - self.jump_margin));
            let sign = if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
            bursts.push((at, side_of(sign)));
        }
        for (at, side) in bursts {
            let lead = rng.range_u64(self.informed_lead).min(at);
            for _ in 0..rng.range_u64(self.informed_orders) {
                let slot = at - rng.range_u64((1, lead.max(1)));
                let scale = rng.range_f64(self.informed_scale);
                let size = self.size(&mut rng, scale);
                let limit = competitor_bps * rng.range_f64(self.informed_limit);
                takers.push(self.order(&mut rng, slot, side, size, limit));
            }
        }
        takers.sort_by_key(|o| o.slot);
        let competition = Competition {
            floor_bps: round2(rng.range_f64(self.floor_bps).min(0.5 * competitor_bps)),
            react_slots: rng.range_u64(self.react_slots),
            size: round2(rng.range_f64(self.competitor_size)),
        };

        let description = match regime {
            Regime::Calm => format!("{} walks without jumps", self.symbol),
            Regime::Jumps => format!(
                "{} walks and jumps {}",
                self.symbol,
                jumps
                    .iter()
                    .map(|(_, size)| format!("{size:+.1}%"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Regime::Trend => format!(
                "{} trends {}",
                self.symbol,
                if trend.is_some_and(|(sign, ..)| sign > 0.0) {
                    "up"
                } else {
                    "down"
                }
            ),
        } + &format!(
            "; makers quote ±{competitor_bps} bp; {} taker orders",
            takers.len()
        );
        Scenario {
            name: name.to_string(),
            description,
            symbol: self.symbol.clone(),
            duration_slots: duration,
            mandate: mth_botkit::Mandate::perp(
                VENUE,
                &self.symbol,
                self.max_position,
                self.max_drawdown_usd,
            ),
            path,
            making: Some(Making {
                competitor_bps,
                competition,
                takers,
            }),
            reference_pnl_usd: None,
        }
    }

    /// `count` scenarios named `<prefix>-<n>`, each regime in proportion to
    /// its odds (largest remainder). Seeds derive from `seed`.
    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        let odds = [
            (Regime::Calm, self.calm_odds),
            (Regime::Jumps, self.jumps_odds),
            (Regime::Trend, 1.0 - self.calm_odds - self.jumps_odds),
        ];
        let mut quotas: Vec<(Regime, usize, f64)> = odds
            .iter()
            .map(|&(regime, p)| {
                let exact = p * count as f64;
                (regime, exact.floor() as usize, exact - exact.floor())
            })
            .collect();
        let short = count - quotas.iter().map(|q| q.1).sum::<usize>();
        let mut by_remainder: Vec<usize> = (0..quotas.len()).collect();
        by_remainder.sort_by(|&a, &b| quotas[b].2.total_cmp(&quotas[a].2));
        for &i in by_remainder.iter().take(short) {
            quotas[i].1 += 1;
        }
        let mut scenarios = Vec::with_capacity(count);
        let mut draw = 0u64;
        while scenarios.len() < count {
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let regime = self.regime(draw_seed);
            let quota = quotas
                .iter_mut()
                .find(|q| q.0 == regime)
                .expect("every regime");
            if quota.1 == 0 {
                continue;
            }
            quota.1 -= 1;
            let name = format!("{prefix}-{}", scenarios.len() + 1);
            scenarios.push(self.sample(&name, draw_seed));
        }
        scenarios
    }

    fn size(&self, rng: &mut SplitMix64, scale: f64) -> f64 {
        let size = self.size_median * scale * (self.size_sigma * rng.normal()).exp();
        round3(size.clamp(self.size_clamp.0, self.size_clamp.1))
    }

    fn order(
        &self,
        rng: &mut SplitMix64,
        slot: u64,
        side: TakerSide,
        size: f64,
        limit_bps: f64,
    ) -> TakerOrder {
        TakerOrder {
            slot,
            side,
            size,
            limit_bps: round2(limit_bps),
            account: rng.range_u64((0, TAKER_ACCOUNTS as u64 - 1)) as u8,
        }
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampled_scenarios_are_valid_and_deterministic() {
        let family = MakingFamily::spcx();
        for seed in 0..200 {
            let s = family.sample("m", seed);
            assert_eq!(s.validate(), Ok(()), "seed {seed}: {s:?}");
            assert_eq!(s, family.sample("m", seed));
        }
    }

    #[test]
    fn informed_flow_leads_most_jumps_not_all() {
        let family = MakingFamily::spcx();
        let (mut jumps, mut led) = (0, 0);
        for seed in (0..2000)
            .filter(|&s| family.regime(s) == Regime::Jumps)
            .take(150)
        {
            let s = family.sample("m", seed);
            let making = s.making.as_ref().unwrap();
            // Same-side volume in the 25 slots before `at`.
            let ahead = |at: u64, side: TakerSide| -> f64 {
                making
                    .takers
                    .iter()
                    .filter(|o| o.slot < at && o.slot + 25 >= at && o.side == side)
                    .map(|o| o.size)
                    .sum()
            };
            // A typical 25-slot window's one-sided volume, for scale.
            let mut windows: Vec<f64> = (25..s.duration_slots)
                .step_by(25)
                .map(|at| ahead(at, TakerSide::Buy))
                .collect();
            windows.sort_by(f64::total_cmp);
            let typical = windows[windows.len() / 2].max(0.5);
            for w in s.path.windows(2) {
                let size = w[1].level_pct - w[0].level_pct;
                if size.abs() < family.jump_pct.0 - 0.01 {
                    continue;
                }
                jumps += 1;
                let side = if size > 0.0 {
                    TakerSide::Buy
                } else {
                    TakerSide::Sell
                };
                if ahead(w[1].slot, side) > 2.5 * typical {
                    led += 1;
                }
            }
        }
        let share = led as f64 / jumps as f64;
        assert!(jumps > 100, "{jumps} jumps");
        // About 85% are announced; a burst measured against a busy window
        // can read as typical, so the measured share runs a little under.
        assert!(
            (0.65..0.95).contains(&share),
            "{share} of {jumps} jumps are led by a burst"
        );
    }

    #[test]
    fn flow_is_spread_over_every_account_and_side() {
        let family = MakingFamily::spcx();
        let mut accounts = [0usize; TAKER_ACCOUNTS as usize];
        let (mut buys, mut sells) = (0, 0);
        for seed in 0..40 {
            for o in family.sample("m", seed).making.unwrap().takers {
                accounts[o.account as usize] += 1;
                match o.side {
                    TakerSide::Buy => buys += 1,
                    TakerSide::Sell => sells += 1,
                }
            }
        }
        assert!(accounts.iter().all(|&n| n > 100), "{accounts:?}");
        let share = buys as f64 / (buys + sells) as f64;
        assert!((0.4..0.6).contains(&share), "{share}");
    }

    #[test]
    fn sets_are_stratified_by_regime() {
        let family = MakingFamily::spcx();
        let set = family.sample_set("h", 12, 7);
        let count = |r: Regime| {
            set.iter()
                .filter(|s| {
                    s.description.contains(match r {
                        Regime::Calm => "without jumps",
                        Regime::Jumps => "and jumps",
                        Regime::Trend => "trends",
                    })
                })
                .count()
        };
        assert_eq!(
            (
                count(Regime::Calm),
                count(Regime::Jumps),
                count(Regime::Trend)
            ),
            (3, 6, 3)
        );
    }

    #[test]
    fn the_spread_bound_prices_each_order_at_its_closer_limit() {
        let making = Making {
            competitor_bps: 10.0,
            competition: Competition {
                floor_bps: 3.0,
                react_slots: 4,
                size: 2.0,
            },
            takers: vec![
                TakerOrder {
                    slot: 0,
                    side: TakerSide::Buy,
                    size: 2.0,
                    limit_bps: 25.0,
                    account: 0,
                },
                TakerOrder {
                    slot: 10,
                    side: TakerSide::Sell,
                    size: 1.0,
                    limit_bps: 4.0,
                    account: 1,
                },
            ],
        };
        let path = [Waypoint {
            slot: 5,
            level_pct: 10.0,
        }];
        // 2 at $100 at the competitor's 10 bp (the taker would pay 25), and 1
        // at $110 at the taker's own 4 bp.
        let bound = making.spread_bound_usd(&path, 100.0);
        assert!((bound - 0.244).abs() < 1e-9, "{bound}");
    }

    /// Some noise takers won't pay the competitor's price; informed ones
    /// always do.
    #[test]
    fn takers_care_about_price() {
        let family = MakingFamily::spcx();
        let s = family.sample("m", 3);
        let making = s.making.as_ref().unwrap();
        let below = making
            .takers
            .iter()
            .filter(|o| o.limit_bps < making.competitor_bps)
            .count();
        let share = below as f64 / making.takers.len() as f64;
        assert!(
            (0.25..0.6).contains(&share),
            "{share} of takers won't pay the competitor"
        );
        assert!(making.competition.floor_bps < making.competitor_bps);
        assert_eq!(s.validate(), Ok(()));
    }
}
