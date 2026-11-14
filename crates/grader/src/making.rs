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
//! a maker of its own (see [`crate::actors::competitor`]): it quotes at the same
//! half-spread and, when a bot quotes inside it, steps inside the bot after
//! a short delay, down to a floor. Undercutting it starts a price war, so a
//! bot keeps the flow only as long as its quotes stay competitive, and what
//! it can earn per order shrinks toward the floor.
//!
//! Takers care about price: each order carries its own limit, so a maker
//! quoting wider than a taker will pay doesn't fill it at all.

use serde::{Deserialize, Serialize};

use crate::scenario::{Scenario, SplitMix64, VENUE, Waypoint, move_slots};

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

/// The grader's maker (see [`crate::actors::competitor`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Competition {
    /// The tightest half-spread it goes to when undercut, in basis points.
    pub floor_bps: f64,
    /// Slots between its looks at the book: how fast it reacts.
    pub react_slots: u64,
    /// Its quote on each side, in base units.
    pub size: f64,
    /// Opt-in: the reactive competitor of the redesigned family, which
    /// counts in whole ticks, ignores odd lots, jitters its timing and
    /// widens after one-sided fills. Absent, it competes as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactive: Option<Reactive>,
}

/// The redesigned competitor's rules (see [`crate::actors::competitor`]).
/// Every field has a default, so `{}` turns the whole behaviour on.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Reactive {
    /// The smallest resting size it reacts to, in base units: quotes under
    /// it are odd lots and set no touch.
    pub q_react: f64,
    /// How far each look's delay varies around `react_slots`, as a fraction
    /// of it: the delay is drawn from `react_slots * (1 ± jitter)`.
    pub jitter: f64,
    /// The ticks it steps inside a rival by, drawn from this range.
    pub step_ticks: (u32, u32),
    /// It reads toxic flow when its own fills on one side within
    /// `toxic_window` slots reach `toxic_size_mult` times its quote size.
    pub toxic_size_mult: f64,
    pub toxic_window: u64,
    /// Then it quotes the hit side at its full half-spread for a number of
    /// slots drawn from this range.
    pub widen_slots: (u64, u64),
}

impl Default for Reactive {
    fn default() -> Self {
        Self {
            q_react: 1.0,
            jitter: 0.5,
            step_ticks: (1, 2),
            toxic_size_mult: 2.0,
            toxic_window: 8,
            widen_slots: (20, 40),
        }
    }
}

impl Reactive {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(self.q_react.is_finite() && self.q_react >= 0.0) {
            return Err(format!(
                "reactive q_react {} must be 0 or more",
                self.q_react
            ));
        }
        if !(self.jitter.is_finite() && (0.0..1.0).contains(&self.jitter)) {
            return Err(format!("reactive jitter {} must be in [0, 1)", self.jitter));
        }
        if self.step_ticks.0 == 0 || self.step_ticks.0 > self.step_ticks.1 {
            return Err(format!(
                "reactive step_ticks {:?} must be an ascending range of at least 1",
                self.step_ticks
            ));
        }
        if !(self.toxic_size_mult.is_finite() && self.toxic_size_mult > 0.0)
            || self.toxic_window == 0
        {
            return Err("reactive toxicity needs a positive size multiple and window".into());
        }
        if self.widen_slots.0 == 0 || self.widen_slots.0 > self.widen_slots.1 {
            return Err(format!(
                "reactive widen_slots {:?} must be an ascending range of at least 1",
                self.widen_slots
            ));
        }
        Ok(())
    }
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
        if let Some(reactive) = &c.reactive {
            reactive.validate()?;
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

/// The book's level at `slot`, with its premium: the last waypoint at or
/// before it.
pub fn book_at(path: &[Waypoint], slot: u64) -> f64 {
    path.iter()
        .take_while(|w| w.slot <= slot)
        .last()
        .map_or(0.0, |w| w.book_pct())
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
    /// Informed orders before a jump, and how far ahead they start. Each one
    /// is drawn like a noise order (size, limit, account): only its side and
    /// its timing say it's informed. In 0.10.0 informed orders ran 2 to 4
    /// times noise sizes with limits of 2 to 4 times the competitor's
    /// half-spread, ranges noise rarely reached, so each order gave itself
    /// away and the best bots took the cap with every burst.
    pub informed_orders: (u64, u64),
    pub informed_lead: (u64, u64),
    /// The share of an announced jump that leaks into the price over its
    /// burst, in steps that follow the informed orders (drawn per jump); the
    /// rest lands at the jump slot. `None` lands the whole jump at its slot.
    pub leak: Option<(f64, f64)>,
    /// Sets round the calm and trend counts to the nearest and give the jump
    /// sessions the rest (72 sessions: 22, 28, 22), instead of the largest
    /// remainder (22, 29, 21).
    pub jumps_take_rest: bool,
    /// Fake-out bursts per session, in any regime: drawn exactly like an
    /// informed burst, with no jump after it. As common as real bursts, so a
    /// burst alone is about a coin flip.
    pub fakeouts: (u64, u64),
    /// The grader's maker: its floor, in basis points, how many slots it
    /// takes to react, and its quote size.
    pub floor_bps: (f64, f64),
    pub react_slots: (u64, u64),
    pub competitor_size: (f64, f64),
    /// Taker limits, as multiples of the competitor's half-spread: lognormal
    /// around 1, so some takers won't pay the competitor's price and fill
    /// only against a tighter quote. Informed takers draw theirs the same way.
    pub noise_limit_sigma: f64,
    pub noise_limit_clamp: (f64, f64),
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
            informed_orders: (6, 14),
            informed_lead: (6, 30),
            leak: None,
            jumps_take_rest: false,
            fakeouts: (0, 2),
            floor_bps: (2.0, 6.0),
            react_slots: (2, 8),
            competitor_size: (2.0, 5.0),
            noise_limit_sigma: 0.5,
            noise_limit_clamp: (0.3, 4.0),
            // ~0.3x the 10,000 USDC of collateral at ~$143: a maker shouldn't
            // warehouse risk.
            max_position: 20.0,
            // 2.5% of collateral: a full inventory through the largest jump
            // costs ~$70, so only a maker that keeps adding into a move gets
            // near it.
            max_drawdown_usd: 250.0,
        }
    }

    /// The redesigned family: longer sessions, fewer jump sessions, and
    /// announced jumps that leak into the price over their burst. Opt-in
    /// (the `market-making-v2` generator); [`MakingFamily::spcx`] is unchanged.
    pub fn v2() -> Self {
        Self {
            calm_odds: 0.3,
            jumps_odds: 0.4,
            duration: (960, 1600),
            leak: Some((0.3, 0.6)),
            jumps_take_rest: true,
            ..Self::spcx()
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
        self.sample_labeled(name, seed).0
    }

    /// [`MakingFamily::sample`], with which of its taker orders are informed
    /// (ahead of a jump; noise and fake-outs aren't), in the scenario's
    /// order. The labels are for the ladder's flow oracle only: they never go
    /// in a scenario file.
    pub fn sample_labeled(&self, name: &str, seed: u64) -> (Scenario, Vec<bool>) {
        let (scenario, informed, _) = self.sample_detailed(name, seed);
        (scenario, informed)
    }

    /// [`MakingFamily::sample_labeled`], with each announced jump's leak
    /// (empty unless the family leaks). For checks: never in a scenario file.
    pub fn sample_detailed(&self, name: &str, seed: u64) -> (Scenario, Vec<bool>, Vec<Leak>) {
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
            let (prev_slot, prev_level) = (slot, level);
            level = match jump {
                Some((_, size)) => level + size,
                None => {
                    let drift = trend.map_or(0.0, |(sign, drift, _)| sign * drift);
                    level + drift + vol * rng.normal()
                }
            };
            level = round3(level);
            path.push(Waypoint::at(slot, level));
            slot += rng.range_u64(self.step_gap);
            if self.leak.is_some() {
                // A jump of 4% can take two of Phoenix's steps: wait them out.
                slot = slot.max(prev_slot + move_slots(prev_level, level) + 1);
            }
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
            let size = self.size(&mut rng);
            let limit = self.limit(&mut rng, competitor_bps);
            takers.push((self.order(&mut rng, slot, side, size, limit), false));
        }
        let side_of = |size: f64| {
            if size > 0.0 {
                TakerSide::Buy
            } else {
                TakerSide::Sell
            }
        };
        let mut bursts: Vec<(u64, TakerSide)> = Vec::new();
        let mut announced: Vec<(u64, f64)> = Vec::new();
        for &(at, size) in &jumps {
            if rng.next_f64() < self.informed_odds {
                bursts.push((at, side_of(size)));
                announced.push((at, size));
            }
        }
        let informed_bursts = bursts.len();
        for _ in 0..rng.range_u64(self.fakeouts) {
            let at = rng.range_u64((self.jump_margin, duration - self.jump_margin));
            let sign = if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
            bursts.push((at, side_of(sign)));
        }
        let mut burst_slots: Vec<Vec<u64>> = Vec::new();
        for (burst, (at, side)) in bursts.into_iter().enumerate() {
            let lead = rng.range_u64(self.informed_lead).min(at);
            burst_slots.push(Vec::new());
            for _ in 0..rng.range_u64(self.informed_orders) {
                let slot = at - rng.range_u64((1, lead.max(1)));
                burst_slots[burst].push(slot);
                let size = self.size(&mut rng);
                let limit = self.limit(&mut rng, competitor_bps);
                takers.push((
                    self.order(&mut rng, slot, side, size, limit),
                    burst < informed_bursts,
                ));
            }
        }
        // Stable: orders in the same slot keep the order they were drawn in.
        takers.sort_by_key(|(o, _)| o.slot);
        let (takers, informed): (Vec<TakerOrder>, Vec<bool>) = takers.into_iter().unzip();
        let competition = Competition {
            floor_bps: round2(rng.range_f64(self.floor_bps).min(0.5 * competitor_bps)),
            react_slots: rng.range_u64(self.react_slots),
            size: round2(rng.range_f64(self.competitor_size)),
            reactive: None,
        };

        let mut leaks = Vec::new();
        if let Some(range) = self.leak {
            for (&(at, size), slots) in announced.iter().zip(&burst_slots) {
                let share = round3(rng.range_f64(range));
                leak_into(&mut path, at, size * share, slots);
                leaks.push(Leak {
                    slot: at,
                    size_pct: size,
                    share,
                });
            }
        }

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
        let scenario = Scenario {
            name: name.to_string(),
            description,
            symbol: self.symbol.clone(),
            duration_slots: duration,
            mandate: mtx_core::Mandate::perp(
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
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            actors: None,
            takers_pricing: None,
            turn_data: None,
            oracles: None,
            lending: None,
            warps: Vec::new(),
            order: None,
            participation: None,
            net: None,
            reference_pnl_usd: None,
            informed_pnl_usd: None,
            fair_path: None,
            market_model: None,
            raw_sends: None,
            start_positions: Default::default(),
            close_out: Default::default(),
        };
        (scenario, informed, leaks)
    }

    /// `count` scenarios named `<prefix>-<n>`, each regime in proportion to
    /// its odds (largest remainder). Seeds derive from `seed`.
    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.sample_set_labeled(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// [`MakingFamily::sample_set`], each scenario with its informed labels
    /// (see [`MakingFamily::sample_labeled`]).
    pub fn sample_set_labeled(
        &self,
        prefix: &str,
        count: usize,
        seed: u64,
    ) -> Vec<(Scenario, Vec<bool>)> {
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
        if self.jumps_take_rest {
            for q in quotas.iter_mut().filter(|q| q.0 != Regime::Jumps) {
                q.1 += (q.2 >= 0.5) as usize;
            }
            let others: usize = quotas
                .iter()
                .filter(|q| q.0 != Regime::Jumps)
                .map(|q| q.1)
                .sum();
            quotas[1].1 = count - others;
            quotas[1].2 = 0.0;
        }
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
            scenarios.push(self.sample_labeled(&name, draw_seed));
        }
        scenarios
    }

    /// A taker order's size: lognormal around the median, clamped. Informed
    /// and noise orders draw it the same way.
    fn size(&self, rng: &mut SplitMix64) -> f64 {
        let size = self.size_median * (self.size_sigma * rng.normal()).exp();
        round3(size.clamp(self.size_clamp.0, self.size_clamp.1))
    }

    /// A taker order's limit, in basis points: lognormal around the
    /// competitor's half-spread, clamped. Informed and noise orders alike.
    fn limit(&self, rng: &mut SplitMix64, competitor_bps: f64) -> f64 {
        let (lo, hi) = self.noise_limit_clamp;
        competitor_bps * (self.noise_limit_sigma * rng.normal()).exp().clamp(lo, hi)
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

/// How much of one announced jump leaked into the price ahead of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Leak {
    /// The jump's slot.
    pub slot: u64,
    /// The whole jump, in percent.
    pub size_pct: f64,
    /// The share of it that leaked over the burst.
    pub share: f64,
}

/// Moves `leaked` percent of the jump at slot `at` into the path ahead of
/// it: each informed
/// order, at `slots`, lifts the level by an equal part of it from its slot
/// on. The waypoint that carries the jump keeps the remainder, since the
/// levels after it already include the whole jump.
fn leak_into(path: &mut Vec<Waypoint>, at: u64, leaked: f64, slots: &[u64]) {
    let Some(&first) = slots.iter().min() else {
        return;
    };
    let part = leaked / slots.len() as f64;
    let cum = |slot: u64| part * slots.iter().filter(|&&s| s <= slot).count() as f64;
    // The jump's waypoint: the first one at or after its slot.
    let carrier = path.iter().position(|w| w.slot >= at).unwrap_or(path.len());
    let mut moved: Vec<Waypoint> = Vec::new();
    for (i, w) in path.iter().enumerate() {
        if i < carrier && w.slot >= first {
            moved.push(Waypoint::at(w.slot, round3(w.level_pct + cum(w.slot))));
        } else {
            moved.push(*w);
        }
    }
    let mut slots = slots.to_vec();
    slots.sort_unstable();
    slots.dedup();
    // A step goes in at its order's slot, or a few slots later where each
    // move wouldn't finish before the next starts; it stays before the jump.
    for slot in slots {
        let level = cum(slot);
        for at_slot in slot..(slot + 6).min(at) {
            if moved.iter().any(|w| w.slot == at_slot) {
                continue;
            }
            let mut trial = moved.clone();
            trial.push(Waypoint::at(
                at_slot,
                round3(level_at(path, at_slot) + level),
            ));
            trial.sort_by_key(|w| w.slot);
            if moves_fit(&trial) {
                moved = trial;
                break;
            }
        }
    }
    moved.sort_by_key(|w| w.slot);
    // Orders too close to the jump for a step of their own still leak: the
    // last step before it carries whatever is left.
    if let Some(last) = moved.iter().rposition(|w| w.slot < at) {
        let slot = moved[last].slot;
        let level = round3(level_at(path, slot) + cum(u64::MAX));
        moved[last] = Waypoint::at(slot, level);
        if !moves_fit(&moved) {
            // Leave the path as built rather than break a move.
            moved[last] = Waypoint::at(slot, round3(level_at(path, slot) + cum(slot)));
        }
    }
    *path = moved;
}

/// Whether each move finishes before the next waypoint starts.
fn moves_fit(path: &[Waypoint]) -> bool {
    let (mut level, mut free_from) = (0.0, 0);
    for w in path {
        if w.slot < free_from {
            return false;
        }
        free_from = w.slot + move_slots(level, w.level_pct) + 1;
        level = w.level_pct;
    }
    true
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
    fn labels_mark_informed_orders_only_where_there_are_jumps() {
        let family = MakingFamily::spcx();
        for seed in 0..200 {
            let (s, informed) = family.sample_labeled("m", seed);
            let takers = &s.making.as_ref().unwrap().takers;
            assert_eq!(informed.len(), takers.len(), "seed {seed}");
            if family.regime(seed) != Regime::Jumps {
                assert!(
                    !informed.contains(&true),
                    "seed {seed}: no jumps, nothing informed"
                );
            }
        }
    }

    /// About 85% of jumps have informed orders on their side just ahead of
    /// them; the rest land unannounced. Read from the labels: informed
    /// orders look like noise one at a time, so volume alone can't count them.
    #[test]
    fn informed_flow_leads_most_jumps_not_all() {
        let family = MakingFamily::spcx();
        let (mut jumps, mut led) = (0, 0);
        for seed in (0..4000)
            .filter(|&s| family.regime(s) == Regime::Jumps)
            .take(300)
        {
            let (s, informed) = family.sample_labeled("m", seed);
            let making = s.making.as_ref().unwrap();
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
                let lead = family.informed_lead.1;
                if making.takers.iter().zip(&informed).any(|(o, i)| {
                    *i && o.side == side && o.slot < w[1].slot && o.slot + lead >= w[1].slot
                }) {
                    led += 1;
                }
            }
        }
        let share = led as f64 / jumps as f64;
        assert!(jumps > 300, "{jumps} jumps");
        assert!(
            (0.78..0.92).contains(&share),
            "{share} of {jumps} jumps are led by informed flow"
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
                reactive: None,
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
        let path = [Waypoint::at(5, 10.0)];
        // 2 at $100 at the competitor's 10 bp (the taker would pay 25), and 1
        // at $110 at the taker's own 4 bp.
        let bound = making.spread_bound_usd(&path, 100.0);
        assert!((bound - 0.244).abs() < 1e-9, "{bound}");
    }

    /// No single order says it's informed: informed and noise orders share
    /// sizes and limits (in 0.10.0, informed ones ran 2-4x larger with 2-4x
    /// limits, and gave themselves away).
    #[test]
    fn informed_orders_look_like_noise() {
        let family = MakingFamily::spcx();
        let mut inf: Vec<(f64, f64)> = vec![];
        let mut noise: Vec<(f64, f64)> = vec![];
        for seed in 0..120 {
            let (s, informed) = family.sample_labeled("m", seed);
            let m = s.making.as_ref().unwrap();
            for (o, i) in m.takers.iter().zip(&informed) {
                let point = (o.size, o.limit_bps / m.competitor_bps);
                if *i {
                    inf.push(point)
                } else {
                    noise.push(point)
                }
            }
        }
        let median = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let (si, sn) = (
            median(inf.iter().map(|p| p.0).collect()),
            median(noise.iter().map(|p| p.0).collect()),
        );
        let (li, ln) = (
            median(inf.iter().map(|p| p.1).collect()),
            median(noise.iter().map(|p| p.1).collect()),
        );
        assert!(inf.len() > 500, "{} informed orders", inf.len());
        assert!(
            (si / sn - 1.0).abs() < 0.15,
            "sizes: informed {si}, noise {sn}"
        );
        assert!(
            (li / ln - 1.0).abs() < 0.15,
            "limits: informed {li}x, noise {ln}x"
        );
    }

    /// Fake-outs are as common as informed bursts, so a burst alone is a
    /// coin flip.
    #[test]
    fn fakeouts_are_as_common_as_announced_jumps() {
        let family = MakingFamily::spcx();
        let mut jumps_announced = 0.0;
        for seed in 0..400 {
            let (_, informed) = family.sample_labeled("m", seed);
            jumps_announced += informed.iter().any(|i| *i) as u8 as f64;
        }
        let fakeouts = (family.fakeouts.0 + family.fakeouts.1) as f64 / 2.0;
        // Announced jumps per session: jumps odds x mean jumps x informed odds.
        let announced = family.jumps_odds * 2.0 * family.informed_odds;
        assert!(
            (announced / fakeouts - 1.0).abs() < 0.5,
            "{announced} announced vs {fakeouts} fake-outs a session"
        );
        assert!(jumps_announced > 100.0);
    }

    /// Some takers won't pay the competitor's price, informed or not.
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

    #[test]
    fn v2_sessions_are_longer_and_the_mix_is_calmer() {
        let family = MakingFamily::v2();
        let mut counts = [0usize; 3];
        for seed in 0..600 {
            let s = family.sample("m", seed);
            assert_eq!(s.validate(), Ok(()), "seed {seed}");
            assert!((960..=1600).contains(&s.duration_slots), "seed {seed}");
            counts[family.regime(seed) as usize] += 1;
        }
        let share = |n: usize| n as f64 / 600.0;
        assert!((share(counts[0]) - 0.3).abs() < 0.06, "{counts:?}");
        assert!((share(counts[1]) - 0.4).abs() < 0.06, "{counts:?}");
        assert!((share(counts[2]) - 0.3).abs() < 0.06, "{counts:?}");
    }

    /// Announced jumps leak 30-60% of their size into the price over the
    /// burst, in steps that follow the informed orders, and the whole jump
    /// has landed after it.
    #[test]
    fn v2_announced_jumps_leak_into_the_burst() {
        let family = MakingFamily::v2();
        let mut seen = 0;
        for seed in (0..2000).filter(|&s| family.regime(s) == Regime::Jumps) {
            let (s, informed, leaks) = family.sample_detailed("m", seed);
            let takers = &s.making.as_ref().unwrap().takers;
            for leak in &leaks {
                seen += 1;
                assert!((0.3..=0.6).contains(&leak.share), "seed {seed}: {leak:?}");
                let slots: Vec<u64> = takers
                    .iter()
                    .zip(&informed)
                    .filter(|(o, i)| **i && o.slot < leak.slot && o.slot + 30 >= leak.slot)
                    .map(|(o, _)| o.slot)
                    .collect();
                assert!(!slots.is_empty(), "seed {seed}");
                // Against the same session with no leak, the walk is the same.
                let flat = MakingFamily {
                    leak: None,
                    ..family.clone()
                }
                .sample("m", seed);
                let leaked = level_at(&s.path, leak.slot - 1) - level_at(&flat.path, leak.slot - 1);
                assert!(
                    (leaked - leak.share * leak.size_pct).abs() < 0.2,
                    "seed {seed}: leaked {leaked} of {leak:?}"
                );
                let landed =
                    level_at(&s.path, leak.slot + 12) - level_at(&flat.path, leak.slot + 12);
                assert!(landed.abs() < 0.2, "seed {seed}: {landed} after the jump");
            }
        }
        assert!(seen > 100);
    }

    /// The v1 family draws exactly what it drew before the v2 knobs existed.
    #[test]
    fn v1_has_no_leaks() {
        let family = MakingFamily::spcx();
        assert!(family.leak.is_none());
        assert!(family.sample_detailed("m", 5).2.is_empty());
    }

    #[test]
    fn a_leak_moves_its_share_ahead_of_the_jump_and_keeps_the_total() {
        let mut path = vec![
            Waypoint::at(10, 0.1),
            Waypoint::at(40, 0.1),
            Waypoint::at(70, 3.1),
            Waypoint::at(80, 3.2),
        ];
        leak_into(&mut path, 70, 1.0, &[45, 52, 52, 60]);
        assert_eq!(level_at(&path, 44), 0.1);
        assert!((level_at(&path, 45) - 0.35).abs() < 1e-9);
        assert!((level_at(&path, 52) - 0.85).abs() < 1e-9);
        assert!((level_at(&path, 60) - 1.1).abs() < 1e-9);
        assert!((level_at(&path, 70) - 3.1).abs() < 1e-9);
        assert_eq!(level_at(&path, 80), 3.2);
        assert!(moves_fit(&path));
    }

    #[test]
    fn v2_hidden_sets_of_72_hold_22_28_22() {
        let set = MakingFamily::v2().sample_set("h", 72, 11);
        let count = |text: &str| set.iter().filter(|s| s.description.contains(text)).count();
        assert_eq!(
            (count("without jumps"), count("and jumps"), count("trends")),
            (22, 28, 22)
        );
    }
}
