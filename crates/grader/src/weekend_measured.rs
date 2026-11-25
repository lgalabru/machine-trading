//! The weekend-gap family as mainnet measured it (RFD 0007): SPCX's perp
//! over a weekend on Phoenix, then the reopen.
//!
//! What the measurements say, and what this family draws from them:
//!
//! Every number is in `calibration/weekend/spcx-reopens.20261120.json`
//! (`calibration/fitter/weekend/reopens.py`) or the tape study.
//!
//! - **The weekend's move is the makers', not the takers'.** Over 21
//!   weekends (Phoenix SPCX 1 m candles, 2026-06-12 to 2026-11-20, holiday
//!   weekends left out) the book moved a median 0.86% between Friday's last
//!   external price and the reopen, with a median of 31 minutes holding
//!   $100 or more of prints in 48 hours; the tape's 23 recorded weekend
//!   hours (`phoenix-tape.20261125.fc443f1e`) held one order. Here the
//!   makers re-price once a compressed hour, a bridge from Friday's price
//!   to a weekend move resampled from the 21 measured ones, and the takers
//!   trade beside them without moving them (price impact is refused with a
//!   closed period).
//! - **The reopen keeps most of the weekend's move.** Across the same 21
//!   weekends the mark five minutes after the Sunday 20:00 New York reopen
//!   sat 0.78 times the weekend's move from Friday (a fit through the
//!   origin, 95% bootstrap interval 0.57 to 1.04), with a residual standard
//!   deviation of 0.50%. Here the open is that, drawn: a bot that knows it
//!   fades about a fifth of the weekend's move into the reopen.
//! - **Nothing else measured reads the reopen.** BTC's and SOL's weekend
//!   moves didn't move with SPCX's reopen gap (r −0.17 and −0.13 over the 21
//!   weekends), so no cross-market channel is carried.
//! - **Departures, stated.** Two informed traders position into the reopen
//!   in its last stretch, right 80% of the time each, and two decoys trade
//!   as they do until they lose the thread: competition for the reopen that
//!   the tape never saw on SPCX (it saw one order in 23 weekend hours) and
//!   that the task needs for an informed bot to read. Their pace and sizes
//!   are SPCX's weekday-night takers'.

use mtx_kit::Mandate;
use serde::{Deserialize, Serialize};

use crate::{
    actors::library::{Decoy, Informed, Library, Noise, Scale, Window},
    offhours::{MAX_INDEX_GAP_PCT, OffHours},
    scenario::{Scenario, SplitMix64, VENUE, Waypoint},
};

/// The 21 weekends' moves, in percent of Friday's last external price,
/// unsigned: Phoenix SPCX's book (the last trade's price) at Sunday 19:59
/// New York against its mark at Friday 19:59, 2026-06-12 to 2026-11-20,
/// holiday weekends (2026-06-19, 07-03 and 09-04) left out. Fifteen of
/// the 21 went up; here the sign is a coin flip.
pub const WEEKEND_MOVES_PCT: [f64; 21] = [
    0.76, 2.42, 0.33, 2.75, 0.87, 0.82, 1.07, 0.92, 0.86, 0.60, 1.15, 0.65, 0.54, 0.05, 0.91, 0.16,
    0.65, 1.29, 0.44, 2.61, 1.23,
];

/// SPCX's mark at the snapshot (`phoenix-spcx-2026-10-29`), which turns the
/// tape's dollar sizes into SPCX.
const SPCX_MARK_USD: f64 = 143.11;

/// Hours in a weekend's closed period: Friday 20:00 to Sunday 20:00 New
/// York. A session compresses them into its closed period.
pub const WEEKEND_HOURS: u64 = 48;

/// The most re-prices a weekend's bridge has: one an hour.
const REPRICES_MAX: usize = WEEKEND_HOURS as usize;

/// Who traded what in a session, beneath its path: the session's annotation,
/// which no graded bot sees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasuredWeekendShape {
    /// Where the weekend took the makers' price by the reopen, where SPCX
    /// opened, and the gap between them, in percent of the starting mark.
    pub weekend_pct: f64,
    pub open_pct: f64,
    pub gap_pct: f64,
    pub half_spread_bps: f64,
    pub closed_slots: u64,
    pub reopen_slot: u64,
    /// The perp's path, the reopen's included.
    pub path: Vec<Waypoint>,
}

/// Which way the weekend moved: the family's strata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeekendDirection {
    Up,
    Down,
}

impl WeekendDirection {
    fn sign(self) -> f64 {
        match self {
            Self::Up => 1.0,
            Self::Down => -1.0,
        }
    }
}

/// The measured family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct MeasuredWeekendFamily {
    pub symbol: String,
    /// Slots of the closed period, then of the regular session after the
    /// reopen. Drawn independently of everything else.
    pub closed_slots: (u64, u64),
    pub open_slots: (u64, u64),
    /// Phoenix's closed-period band: 1 / max leverage (15x for SPCX).
    pub band_pct: f64,
    /// The weekend's move: one of the measured ones, times a lognormal
    /// jitter of this log standard deviation, either way.
    pub weekend_moves_pct: &'static [f64],
    pub weekend_jitter_log_sd: f64,
    /// The makers' hourly re-price over a weekend: the standard deviation
    /// of the mark's hourly return (21 weekends: 0.193%).
    pub hourly_sd_pct: f64,
    /// The open: `reopen_keep` times the weekend's move, plus a normal draw
    /// of this standard deviation.
    pub reopen_keep: f64,
    pub open_residual_sd_pct: f64,
    /// Phoenix's internal index: an EMA of the perp with this time
    /// constant, updated every `index_every_slots` slots.
    pub index_tau_slots: f64,
    pub index_every_slots: u64,
    /// The makers' half-spread, in basis points.
    pub half_spread_bps: (f64, f64),
    /// After the reopen: a small wobble around the open.
    pub open_wobble_pct: f64,
    pub open_wobble_gap_slots: (u64, u64),
    /// The weekend's takers: orders in a closed period, their median size
    /// in USD and its log standard deviation.
    pub weekend_orders: f64,
    pub order_usd_median: f64,
    pub order_log_sd: f64,
    /// The reopen's first slot: one order of this size, either way.
    pub burst_usd: f64,
    /// The traders positioning into the reopen, and the decoys.
    pub positioners: Positioners,
    pub max_position: f64,
    pub max_drawdown_usd: f64,
}

/// The informed traders who position into the reopen, and the decoys who
/// trade as they do until they don't (a stated departure: see the module).
#[derive(Debug, Clone)]
pub struct Positioners {
    pub informed_wallets: u32,
    pub decoy_wallets: u32,
    /// How far ahead they see, in slots: from this far before the reopen,
    /// they see the open.
    pub horizon_slots: u64,
    /// How often an informed wallet's view of the open is right, for the
    /// whole stretch; a decoy's, until it fades.
    pub hit_rate: f64,
    pub edge_bps: f64,
    pub view_noise_bps: f64,
    /// Orders each wallet sends over its stretch, on average.
    pub orders_per_wallet: f64,
}

impl crate::weekend::WeekendFamily {
    /// The weekend-gap family as mainnet measured it: see
    /// [`MeasuredWeekendFamily`]. A family of its own, so the old one's
    /// draws and pins don't move.
    pub fn mainnet() -> MeasuredWeekendFamily {
        MeasuredWeekendFamily::mainnet()
    }
}

impl MeasuredWeekendFamily {
    pub fn mainnet() -> Self {
        Self {
            symbol: "SPCX".into(),
            closed_slots: (400, 640),
            open_slots: (50, 160),
            band_pct: 100.0 / 15.0,
            weekend_moves_pct: &WEEKEND_MOVES_PCT,
            // Enough to keep 21 values from being the only ones; small
            // against the spread between them.
            weekend_jitter_log_sd: 0.2,
            hourly_sd_pct: 0.193,
            reopen_keep: 0.78,
            open_residual_sd_pct: 0.50,
            index_tau_slots: 120.0,
            index_every_slots: 8,
            // One tick (0.4 bp, luthor's quotes on a weeknight, 2026-11-25)
            // to the calibration's default (5 bp: no weekend book has been
            // recorded).
            half_spread_bps: (0.4, 5.0),
            open_wobble_pct: 0.06,
            open_wobble_gap_slots: (6, 14),
            // 31 minutes with $100 or more of prints, a median weekend
            // (candles); the tape's weekday-night orders' median $918 and
            // log standard deviation 1.53 (`markets.SPCX.flow.off`).
            weekend_orders: 31.0,
            order_usd_median: 918.0,
            order_log_sd: 1.53,
            // The tape's reopen, 2026-11-23 01:00:05 UTC: five buy fills in
            // one slot, $2,998.
            burst_usd: 2_998.0,
            positioners: Positioners {
                informed_wallets: 2,
                decoy_wallets: 2,
                horizon_slots: 40,
                hit_rate: 0.8,
                edge_bps: 10.0,
                view_noise_bps: 10.0,
                orders_per_wallet: 3.0,
            },
            max_position: 70.0,
            max_drawdown_usd: 1_000.0,
        }
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    /// Which way a seed's weekend moves: the first draw of [`Self::draw`].
    pub fn direction(&self, seed: u64) -> WeekendDirection {
        let mut rng = SplitMix64(seed);
        if rng.next_f64() < 0.5 {
            WeekendDirection::Down
        } else {
            WeekendDirection::Up
        }
    }

    /// A scenario and what it was drawn as. The same seed always yields the
    /// same scenario.
    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, MeasuredWeekendShape) {
        assert!(!self.weekend_moves_pct.is_empty());
        assert!(self.reopen_keep > 0.0 && self.open_residual_sd_pct > 0.0);
        let mut rng = SplitMix64(seed);
        let direction = if rng.next_f64() < 0.5 {
            WeekendDirection::Down
        } else {
            WeekendDirection::Up
        };
        let closed = rng.range_u64(self.closed_slots);
        let open = rng.range_u64(self.open_slots);
        let picked =
            self.weekend_moves_pct[(rng.next_u64() % self.weekend_moves_pct.len() as u64) as usize];
        let edge = self.band_pct - 0.5;
        let weekend =
            (direction.sign() * picked * (self.weekend_jitter_log_sd * rng.normal()).exp())
                .clamp(-edge, edge);
        let half_spread = round2(rng.range_f64(self.half_spread_bps));

        let reprices = self.bridge(&mut rng, closed, weekend, edge);
        let index = self.index(&reprices, closed);
        let mut path = Vec::with_capacity(reprices.len() + 1 + open as usize / 6);
        for w in &reprices {
            let index_here = crate::making::level_at(&index, w.slot);
            path.push(Waypoint::at(
                w.slot,
                round(w.level_pct.clamp(
                    index_here - MAX_INDEX_GAP_PCT,
                    index_here + MAX_INDEX_GAP_PCT,
                )),
            ));
        }
        let last = path.last().map_or(0.0, |w| w.level_pct);
        let reopen_slot = closed;
        let open_level = round(self.reopen_keep * last + self.open_residual_sd_pct * rng.normal());
        path.push(Waypoint::at(reopen_slot, open_level));
        self.wobble(&mut rng, &mut path, reopen_slot, open_level, closed + open);
        let duration = (closed + open).max(reopen_slot + 40);

        let shape = MeasuredWeekendShape {
            weekend_pct: last,
            open_pct: open_level,
            gap_pct: round(open_level - last),
            half_spread_bps: half_spread,
            closed_slots: closed,
            reopen_slot,
            path: path.clone(),
        };
        let description = format!(
            "{} closed for {closed} slots; the weekend moved it {last:+.2}% and it opens at {open_level:+.2}%",
            self.symbol
        );
        let scenario = self.scenario(
            name,
            description,
            duration,
            path,
            OffHours {
                reopen_slot,
                band_pct: round(self.band_pct),
                half_spread_bps: half_spread,
                index,
                takers: Vec::new(),
            },
            self.library(closed, reopen_slot),
        );
        (scenario, shape)
    }

    /// The makers' weekend: a re-price an hour, a random walk at the
    /// measured hourly spread bridged to `weekend` at the last one, which
    /// lands an hour before the reopen. Each lands at least three slots
    /// after the last, as Phoenix moves a market.
    fn bridge(&self, rng: &mut SplitMix64, closed: u64, weekend: f64, edge: f64) -> Vec<Waypoint> {
        let steps = REPRICES_MAX;
        let mut walk = [0.0_f64; REPRICES_MAX];
        let mut level = 0.0;
        for step in walk.iter_mut() {
            level += self.hourly_sd_pct * rng.normal();
            *step = level;
        }
        let end = walk[steps - 1];
        let gap_slots = closed / (WEEKEND_HOURS + 1);
        assert!(gap_slots >= 3, "a re-price every {gap_slots} slots");
        let mut reprices = Vec::with_capacity(steps);
        for (k, step) in walk.iter().enumerate() {
            let share = (k + 1) as f64 / steps as f64;
            let bridged = (step - share * (end - weekend)).clamp(-edge, edge);
            reprices.push(Waypoint::at(gap_slots * (k as u64 + 1), round(bridged)));
        }
        assert_eq!(reprices.len(), steps);
        assert!(
            reprices
                .last()
                .is_some_and(|w| w.slot + gap_slots <= closed)
        );
        reprices
    }

    /// Phoenix's index through the closed period: an EMA of the makers'
    /// price, never more than [`MAX_INDEX_GAP_PCT`] behind it.
    fn index(&self, reprices: &[Waypoint], closed: u64) -> Vec<Waypoint> {
        let alpha = 1.0 - (-(self.index_every_slots as f64) / self.index_tau_slots).exp();
        let mut index_level = 0.0_f64;
        let mut index = Vec::new();
        let mut slot = self.index_every_slots;
        while slot + 4 < closed {
            let perp = crate::making::level_at(reprices, slot);
            let next = round(
                (index_level + alpha * (perp - index_level))
                    .clamp(perp - MAX_INDEX_GAP_PCT, perp + MAX_INDEX_GAP_PCT)
                    .clamp(-self.band_pct, self.band_pct),
            );
            if next != index_level {
                index_level = next;
                index.push(Waypoint::at(slot, index_level));
            }
            slot += self.index_every_slots;
        }
        index
    }

    /// After the reopen, the price wobbles around the open.
    fn wobble(
        &self,
        rng: &mut SplitMix64,
        path: &mut Vec<Waypoint>,
        reopen_slot: u64,
        open_level: f64,
        duration: u64,
    ) {
        let mut slot = reopen_slot + crate::phoenix::SLOTS_BETWEEN_STEPS + 1;
        loop {
            slot += rng.range_u64(self.open_wobble_gap_slots);
            if slot + 4 > duration {
                break;
            }
            path.push(Waypoint::at(
                slot,
                round(open_level + self.open_wobble_pct * (2.0 * rng.next_f64() - 1.0)),
            ));
        }
    }

    /// The session's takers, from the actor library, on a clock the closed
    /// period compresses: the weekend's orders spread over it, the burst at
    /// the reopen, and the traders positioning into it.
    pub fn library(&self, closed: u64, reopen_slot: u64) -> Library {
        assert!(closed > self.positioners.horizon_slots && reopen_slot >= closed);
        let per_slot = self.weekend_orders / closed as f64;
        let scale = Scale {
            trades_per_minute: per_slot * 60.0 / crate::actors::library::SLOT_SECONDS,
            trade_size: self.order_usd_median / SPCX_MARK_USD,
            trade_size_sigma: self.order_log_sd,
        };
        debug_assert!((scale.per_slot() - per_slot).abs() < 1e-9);
        let p = &self.positioners;
        let stretch = Window {
            from: reopen_slot - p.horizon_slots,
            until: Some(reopen_slot),
        };
        // Orders a slot over the stretch, all wallets of a kind together.
        let stretch_rate =
            |wallets: u32| p.orders_per_wallet * wallets as f64 / p.horizon_slots as f64;
        Library {
            scale,
            noise: vec![
                Noise {
                    wallets: 3,
                    flow_mult: 1.0,
                    size_mult: 1.0,
                    limit_bps: 50.0,
                    active: Some(Window {
                        from: 0,
                        until: Some(reopen_slot),
                    }),
                },
                // The reopen's burst: one order, on average, in its first step.
                Noise {
                    wallets: 1,
                    flow_mult: 1.0 / (per_slot * crate::phoenix::SLOTS_BETWEEN_STEPS as f64),
                    size_mult: self.burst_usd / self.order_usd_median,
                    limit_bps: 50.0,
                    active: Some(Window {
                        from: reopen_slot,
                        until: Some(reopen_slot + crate::phoenix::SLOTS_BETWEEN_STEPS),
                    }),
                },
            ],
            informed: vec![Informed {
                wallets: p.informed_wallets,
                horizon_slots: p.horizon_slots,
                edge_bps: p.edge_bps,
                hit_rate: p.hit_rate,
                view_noise_bps: p.view_noise_bps,
                intensity_mult: stretch_rate(p.informed_wallets) / per_slot,
                size_mult: 1.0,
                limit_bps: 50.0,
                active: Some(stretch),
            }],
            decoys: vec![Decoy {
                wallets: p.decoy_wallets,
                horizon_slots: p.horizon_slots,
                edge_bps: p.edge_bps,
                hit_rate: p.hit_rate,
                view_noise_bps: p.view_noise_bps,
                informed_for: Window {
                    from: stretch.from,
                    until: Some(stretch.from + p.horizon_slots / 2),
                },
                intensity_mult: stretch_rate(p.decoy_wallets) / per_slot,
                size_mult: 1.0,
                limit_bps: 50.0,
                active: Some(stretch),
            }],
            ..Library::default()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn scenario(
        &self,
        name: &str,
        description: String,
        duration: u64,
        path: Vec<Waypoint>,
        offhours: OffHours,
        library: Library,
    ) -> Scenario {
        Scenario {
            name: name.to_string(),
            description,
            symbol: self.symbol.clone(),
            duration_slots: duration,
            mandate: Mandate::perp(
                VENUE,
                &self.symbol,
                self.max_position,
                self.max_drawdown_usd,
            ),
            path,
            making: None,
            offhours: Some(offhours),
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            actors: Some(library),
            takers_pricing: None,
            turn_data: None,
            oracles: None,
            lending: None,
            warps: Vec::new(),
            amm: None,
            order: None,
            participation: None,
            net: None,
            reference_pnl_usd: None,
            informed_pnl_usd: None,
            reference_max_drawdown_usd: None,
            informed_max_drawdown_usd: None,
            fair_path: None,
            bound_on: Default::default(),
            market_model: None,
            raw_sends: None,
            markets: Vec::new(),
            joint: None,
            start_positions: Default::default(),
            close_out: Default::default(),
            tx_layer: None,
            stop_out: None,
            liquidator: None,
            impact: None,
        }
    }

    /// `count` scenarios named `<prefix>-<n>`, half with weekends that went
    /// up and half down (the odd one up). Seeds derive from `seed`; the
    /// scenarios are then shuffled.
    pub fn draw_set(
        &self,
        prefix: &str,
        count: usize,
        seed: u64,
    ) -> Vec<(Scenario, MeasuredWeekendShape)> {
        let mut wanted = [
            (WeekendDirection::Up, count.div_ceil(2)),
            (WeekendDirection::Down, count / 2),
        ];
        let mut drawn = Vec::with_capacity(count);
        let mut draw = 0u64;
        // Each draw lands in a stratum with odds of a half: a set of
        // `count` needs about twice as many, never a hundred times.
        let draws_max = 100 * count as u64 + 100;
        while drawn.len() < count {
            assert!(draw < draws_max, "a stratum never fills");
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let direction = self.direction(draw_seed);
            let cell = wanted
                .iter_mut()
                .find(|c| c.0 == direction)
                .expect("every stratum");
            if cell.1 == 0 {
                continue;
            }
            cell.1 -= 1;
            drawn.push(draw_seed);
        }
        crate::scenario::shuffled(drawn, seed)
            .into_iter()
            .enumerate()
            .map(|(i, draw_seed)| self.draw(&format!("{prefix}-{}", i + 1), draw_seed))
            .collect()
    }
}

fn round(level_pct: f64) -> f64 {
    (level_pct * 1000.0).round() / 1000.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quantile(mut xs: Vec<f64>, q: f64) -> f64 {
        xs.sort_by(f64::total_cmp);
        xs[((xs.len() - 1) as f64 * q).round() as usize]
    }

    #[test]
    fn sampled_scenarios_are_valid_and_deterministic() {
        let family = MeasuredWeekendFamily::mainnet();
        for seed in 0..300 {
            let scenario = family.sample("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}");
            assert_eq!(scenario, family.sample("s", seed));
        }
    }

    #[test]
    fn sets_are_stratified_by_the_weekends_direction() {
        let family = MeasuredWeekendFamily::mainnet();
        for (count, ups) in [(72, 36), (12, 6), (7, 4)] {
            let set = family.draw_set("h", count, 9);
            assert_eq!(set.len(), count);
            let up = set.iter().filter(|(_, s)| s.weekend_pct > 0.0).count();
            assert_eq!(up, ups, "{count}");
        }
    }

    /// The weekend's moves are the measured ones: a median near 0.86%, and
    /// a p90 between the 21 weekends' 1.3% (their 18th of 21) and 2.4% (the
    /// 19th): `calibration/weekend/spcx-reopens.20261120.json`.
    #[test]
    fn weekend_moves_are_the_measured_ones() {
        let family = MeasuredWeekendFamily::mainnet();
        let moves: Vec<f64> = (0..2_000)
            .map(|seed| family.draw("s", seed).1.weekend_pct.abs())
            .collect();
        let median = quantile(moves.clone(), 0.5);
        let p90 = quantile(moves, 0.9);
        assert!((0.7..=1.0).contains(&median), "median {median}");
        assert!((1.1..=2.6).contains(&p90), "p90 {p90}");
    }

    /// The reopen keeps 0.78 of the weekend's move, give or take 0.50%, as
    /// 21 weekends measured; the gap's size sits between those weekends'
    /// (median 0.37%, p90 0.77%) and the calibration's 107 regular opens
    /// (`spcx.20261112.d1f48be2`: median 0.44%, p90 1.12%).
    #[test]
    fn the_reopen_keeps_the_measured_share_of_the_weekend() {
        let family = MeasuredWeekendFamily::mainnet();
        let shapes: Vec<MeasuredWeekendShape> =
            (0..4_000).map(|seed| family.draw("s", seed).1).collect();
        let sxy: f64 = shapes.iter().map(|s| s.weekend_pct * s.open_pct).sum();
        let sxx: f64 = shapes.iter().map(|s| s.weekend_pct * s.weekend_pct).sum();
        let keep = sxy / sxx;
        assert!((keep - 0.78).abs() < 0.03, "keep {keep}");
        let residuals: Vec<f64> = shapes
            .iter()
            .map(|s| s.open_pct - keep * s.weekend_pct)
            .collect();
        let sd = (residuals.iter().map(|r| r * r).sum::<f64>() / residuals.len() as f64).sqrt();
        assert!((sd - 0.50).abs() < 0.03, "sd {sd}");
        let gaps: Vec<f64> = shapes.iter().map(|s| s.gap_pct.abs()).collect();
        let median = quantile(gaps.clone(), 0.5);
        let p90 = quantile(gaps, 0.9);
        assert!((0.3..=0.5).contains(&median), "median gap {median}");
        assert!((0.7..=1.15).contains(&p90), "p90 gap {p90}");
    }

    /// The makers re-price once a compressed hour, never closer than
    /// Phoenix moves a market, and stop an hour before the reopen: the
    /// reopen's slot is clear of the weekend's last move.
    #[test]
    fn the_makers_reprice_hourly_and_the_perp_stays_near_its_index() {
        let family = MeasuredWeekendFamily::mainnet();
        for seed in 0..200 {
            let (scenario, shape) = family.draw("s", seed);
            let off = scenario.offhours.as_ref().unwrap();
            let closed: Vec<&Waypoint> = scenario
                .path
                .iter()
                .filter(|w| w.slot < off.reopen_slot)
                .collect();
            assert_eq!(closed.len(), REPRICES_MAX);
            for pair in closed.windows(2) {
                assert!(pair[1].slot >= pair[0].slot + 3);
            }
            assert!(closed.last().unwrap().slot + 8 <= off.reopen_slot);
            for w in &closed {
                assert!(w.level_pct.abs() <= family.band_pct);
                assert!(
                    (w.level_pct - crate::making::level_at(&off.index, w.slot)).abs()
                        <= MAX_INDEX_GAP_PCT + 0.5
                );
            }
            assert_eq!(shape.reopen_slot, shape.closed_slots);
        }
    }

    /// The weekend's takers: about 31 orders in a closed period, at the
    /// tape's weekday-night sizes; one burst order at the reopen; the
    /// positioners only in the last stretch, the informed told apart from
    /// the decoys by nothing in the scenario but their kind.
    #[test]
    fn the_takers_are_the_measured_weekends_and_the_positioners_trade_late() {
        let family = MeasuredWeekendFamily::mainnet();
        let (scenario, shape) = family.draw("s", 11);
        let library = scenario.actors.as_ref().unwrap();
        assert_eq!(library.validate(), Ok(()));
        let orders = library.scale.per_slot() * shape.closed_slots as f64;
        assert!((orders - 31.0).abs() < 1e-6, "{orders}");
        assert!((library.scale.trade_size * SPCX_MARK_USD - 918.0).abs() < 1e-6);
        let burst = &library.noise[1];
        let burst_orders = library.scale.per_slot() * burst.flow_mult * 2.0;
        assert!((burst_orders - 1.0).abs() < 1e-9);
        let informed = &library.informed[0];
        let decoy = &library.decoys[0];
        assert!(informed.hit_rate < 1.0);
        assert_eq!(informed.active, decoy.active);
        assert_eq!(informed.intensity_mult, decoy.intensity_mult);
        let stretch = informed.active.unwrap();
        assert_eq!(stretch.until, Some(shape.reopen_slot));
        assert_eq!(stretch.from + informed.horizon_slots, shape.reopen_slot);
    }

    /// The old family is untouched: its draws don't move.
    #[test]
    fn the_old_family_still_draws_as_it_did() {
        let old = crate::weekend::WeekendFamily::spcx().sample("s", 5);
        assert!(old.actors.is_none());
        assert!(!old.offhours.as_ref().unwrap().takers.is_empty());
    }
}
