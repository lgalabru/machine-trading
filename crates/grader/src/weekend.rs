//! The weekend-gap family: SPCX's perp trading while SPCX's own market is
//! closed, then the reopen.
//!
//! SPCX tracks a US-listed share. Phoenix prices its index from external
//! markets 24 hours a day on weekdays; over the weekend there's no external
//! price, and Phoenix switches to its own pricing: the index starts at the
//! last external price and follows Phoenix's own book, smoothed, inside a
//! band of 1 / max leverage (6.67% for SPCX) around that last price. When the
//! external window reopens, the index snaps to wherever SPCX opens. So over
//! the weekend the perp is the only price discovery there is, and the index
//! is the perp's own price, lagged: it says nothing the perp doesn't.
//!
//! A session compresses a closed period and the reopen into a few minutes:
//!
//! - **The weekend's move.** Where SPCX will open is drawn first, either way,
//!   log-uniform in size: most weekends move it a little, some a lot.
//! - **Informed flow.** In most sessions one trader knows where SPCX will
//!   open. It trades the perp toward that level, in market orders, until the
//!   perp gets there or the band stops it, and its orders move the perp for
//!   good: the makers re-price on them and don't come back.
//! - **One-sided uninformed flow.** In half the sessions another trader works
//!   a large order one way for its own reasons, in the same sizes and at the
//!   same pace as the informed one. Its orders move the perp as much, but
//!   the makers lean back: its impact decays.
//! - **Noise.** Every trader also sends small orders either way; their impact
//!   decays the same way.
//! - **Thin-book drift.** With no external anchor, the makers' price wanders
//!   with no trade behind it, and the reopen takes it back.
//! - **The reopen.** The index and the makers move to SPCX's open: where the
//!   weekend's move took it, plus what nobody knew.
//!
//! Every order goes through Phoenix from one of eight taker accounts; which
//! one is informed, and which one works the large order, is drawn per
//! session, so the account says nothing until the session shows what its
//! orders did. The impact per share and how fast uninformed impact decays
//! are drawn per session as well: a bot learns them from the session, not
//! from the family.

use mtx_kit::Mandate;
use serde::{Deserialize, Serialize};

use crate::{
    making::{TAKER_ACCOUNTS, TakerOrder, TakerSide},
    offhours::{MAX_INDEX_GAP_PCT, OffHours},
    scenario::{Scenario, SplitMix64, VENUE, Waypoint},
};

/// Who traded what in a session, beneath its path: what the ladder's told
/// rungs are told, and what no graded bot sees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeekendShape {
    /// Whether an informed trader traded, and from which account.
    pub informed: bool,
    pub informed_account: Option<u8>,
    /// Whether a one-sided uninformed trader worked an order, which way
    /// (+1 buys) and from which account.
    pub worker: bool,
    pub worker_account: Option<u8>,
    pub worker_side: f64,
    /// Impact per share traded, in percent of the starting mark, and the
    /// half-life of uninformed impact, in slots.
    pub impact_pct: f64,
    pub half_life_slots: f64,
    /// Where the weekend took SPCX (what the informed trader knows) and where
    /// it opened, in percent of the starting mark.
    pub weekend_pct: f64,
    pub open_pct: f64,
    /// The perp's permanent level at the reopen: what the informed flow had
    /// moved it by.
    pub permanent_pct: f64,
    /// The slots of the informed trader's orders, and of the worker's.
    pub informed_slots: Vec<u64>,
    pub worker_slots: Vec<u64>,
    /// The perp's path, the reopen's included: what the ladder's
    /// path oracle looks ahead in.
    pub path: Vec<Waypoint>,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct WeekendFamily {
    pub symbol: String,
    /// Slots of the closed period, then of the regular session after the
    /// reopen. Drawn before anything else.
    pub closed_slots: (u64, u64),
    pub open_slots: (u64, u64),
    pub band_pct: f64,
    /// The weekend's move, in percent: either way, log-uniform in size. A
    /// weekend with news someone trades on, and a quiet one.
    pub weekend_pct: (f64, f64),
    pub quiet_weekend_pct: (f64, f64),
    /// What nobody knew: a normal draw on top of the weekend's move at the
    /// open, with its standard deviation drawn per session.
    pub open_sd_pct: (f64, f64),
    pub informed_odds: f64,
    pub worker_odds: f64,
    /// Impact per share, in percent of the starting mark, per session; each
    /// order's own varies around it by `impact_jitter` (a standard deviation
    /// as a fraction of it).
    pub impact_pct: (f64, f64),
    pub impact_jitter: f64,
    /// Half-life of uninformed impact, in slots.
    pub half_life: (f64, f64),
    /// When the informed trader and the worker start, as fractions of the
    /// closed period, and the slots between their orders (each gap drawn
    /// from half to one and a half times a per-trader pace in this range).
    pub informed_start: (f64, f64),
    pub worker_start: (f64, f64),
    pub pace_slots: (f64, f64),
    /// Their order sizes: lognormal around a median, clamped.
    pub big_size_median: f64,
    pub big_size_sigma: f64,
    pub big_size_clamp: (f64, f64),
    /// The worker's whole order, in shares.
    pub worker_qty: (f64, f64),
    /// Noise orders per slot, per session, and their sizes.
    pub noise_rate: (f64, f64),
    pub noise_size_median: f64,
    pub noise_size_sigma: f64,
    pub noise_size_clamp: (f64, f64),
    /// Thin-book drift: a step every so many slots, its standard deviation
    /// in percent per session, pulled back with this half-life.
    pub drift_gap: (u64, u64),
    pub drift_sd_pct: (f64, f64),
    pub drift_half_life: f64,
    /// Phoenix's internal index: an EMA of the perp with this time
    /// constant, updated every `index_every` slots.
    pub index_tau_slots: f64,
    pub index_every: u64,
    /// The makers' half-spread, in basis points.
    pub half_spread_bps: (f64, f64),
    /// Taker limits, as multiples of the half-spread: noise, then the
    /// informed trader and the worker, who are in a hurry.
    pub noise_limit: (f64, f64),
    pub big_limit: (f64, f64),
    /// After the reopen: a small wobble around the open.
    pub open_wobble_pct: f64,
    pub open_wobble_gap: (u64, u64),
    pub max_position: f64,
    pub max_drawdown_usd: f64,
}

impl WeekendFamily {
    pub fn spcx() -> Self {
        Self {
            symbol: "SPCX".into(),
            closed_slots: (400, 640),
            open_slots: (50, 160),
            // SPCX allows 15x: Phoenix's band is 1/15.
            band_pct: 100.0 / 15.0,
            weekend_pct: (0.5, 12.0),
            quiet_weekend_pct: (0.2, 3.0),
            open_sd_pct: (0.2, 0.6),
            informed_odds: 0.6,
            worker_odds: 0.6,
            impact_pct: (0.08, 0.2),
            impact_jitter: 0.25,
            half_life: (60.0, 240.0),
            informed_start: (0.02, 0.5),
            worker_start: (0.0, 0.6),
            pace_slots: (6.0, 20.0),
            big_size_median: 1.0,
            big_size_sigma: 0.5,
            big_size_clamp: (0.2, 4.0),
            worker_qty: (12.0, 50.0),
            noise_rate: (0.03, 0.1),
            noise_size_median: 0.6,
            noise_size_sigma: 0.7,
            noise_size_clamp: (0.1, 4.0),
            drift_gap: (6, 20),
            drift_sd_pct: (0.02, 0.08),
            drift_half_life: 300.0,
            index_tau_slots: 120.0,
            index_every: 8,
            half_spread_bps: (8.0, 16.0),
            noise_limit: (1.5, 4.0),
            big_limit: (3.0, 6.0),
            open_wobble_pct: 0.06,
            open_wobble_gap: (6, 14),
            // ~1x the 10,000 USDC of collateral at ~$143.
            max_position: 70.0,
            // 10% of collateral: the cap held into a 10% gap the wrong way
            // breaks it.
            max_drawdown_usd: 1_000.0,
        }
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    /// Whether a seed draws an informed trader and a worker: the first two
    /// draws of [`Self::draw`].
    pub fn kind(&self, seed: u64) -> (bool, bool) {
        let mut rng = SplitMix64(seed);
        let informed = rng.next_f64() < self.informed_odds;
        let worker = rng.next_f64() < self.worker_odds;
        (informed, worker)
    }

    /// A scenario and what it was drawn as. The same seed always yields the
    /// same scenario.
    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, WeekendShape) {
        let mut rng = SplitMix64(seed);
        let informed = rng.next_f64() < self.informed_odds;
        let worker = rng.next_f64() < self.worker_odds;
        // The session's length first: it can't date anything.
        let closed = rng.range_u64(self.closed_slots);
        let open = rng.range_u64(self.open_slots);
        let duration = closed + open;
        let sign = |rng: &mut SplitMix64| if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
        let weekend = sign(&mut rng)
            * rng.log_range_f64(if informed {
                self.weekend_pct
            } else {
                self.quiet_weekend_pct
            });
        let open_sd = rng.range_f64(self.open_sd_pct);
        let impact = rng.range_f64(self.impact_pct);
        let half_life = rng.range_f64(self.half_life);
        let decay = 0.5_f64.powf(1.0 / half_life);
        let noise_rate = rng.range_f64(self.noise_rate);
        let drift_sd = rng.range_f64(self.drift_sd_pct);
        let half_spread = round2(rng.range_f64(self.half_spread_bps));
        // Two distinct accounts for the informed trader and the worker.
        let informed_account = (rng.next_u64() % TAKER_ACCOUNTS as u64) as u8;
        let worker_account =
            ((informed_account as u64 + 1 + rng.next_u64() % (TAKER_ACCOUNTS as u64 - 1))
                % TAKER_ACCOUNTS as u64) as u8;
        let informed_from = frac_slot(&mut rng, self.informed_start, closed);
        let informed_pace = rng.range_f64(self.pace_slots);
        let worker_from = frac_slot(&mut rng, self.worker_start, closed);
        let worker_pace = rng.range_f64(self.pace_slots);
        let worker_side = sign(&mut rng);
        let worker_qty = rng.range_f64(self.worker_qty);

        let edge = self.band_pct - 0.15;
        let (mut permanent, mut passing, mut drift) = (0.0_f64, 0.0_f64, 0.0_f64);
        let drift_pull = 0.5_f64.powf(1.0 / self.drift_half_life);
        let mut next_drift = rng.range_u64(self.drift_gap);
        let mut next_informed = informed_from as f64;
        let mut next_worker = worker_from as f64;
        let mut worker_left = worker_qty;
        let mut levels = Vec::with_capacity(closed as usize);
        let mut takers: Vec<TakerOrder> = Vec::new();
        let (mut informed_slots, mut worker_slots) = (Vec::new(), Vec::new());
        // Each slot: what landed in it moves the perp from the next slot on.
        for slot in 0..closed {
            passing *= decay;
            drift *= drift_pull;
            if slot >= next_drift {
                drift += drift_sd * rng.normal();
                next_drift = slot + rng.range_u64(self.drift_gap);
            }
            // Orders go on even slots, where a step ends.
            let tradable = slot.is_multiple_of(2) && slot + 4 < closed;
            if tradable && rng.next_f64() < 2.0 * noise_rate {
                let side = sign(&mut rng);
                let size = self.size(&mut rng, false);
                let limit = half_spread * rng.range_f64(self.noise_limit);
                // Noise comes from everyone but the session's informed
                // trader and worker, who trade only their own orders.
                let account = loop {
                    let a = (rng.next_u64() % TAKER_ACCOUNTS as u64) as u8;
                    if !(informed && a == informed_account) && !(worker && a == worker_account) {
                        break a;
                    }
                };
                takers.push(order(slot, side, size, limit, account));
                passing += side * impact * size * self.jitter(&mut rng);
            }
            if informed && tradable && slot as f64 >= next_informed {
                next_informed = slot as f64 + informed_pace * rng.range_f64((0.5, 1.5));
                let size = self.size(&mut rng, true);
                let jitter = self.jitter(&mut rng);
                let limit = half_spread * rng.range_f64(self.big_limit);
                let gap = weekend - permanent;
                // It stops once the perp is within half an order's impact.
                if gap.abs() > 0.5 * impact * self.big_size_median {
                    let side = gap.signum();
                    takers.push(order(slot, side, size, limit, informed_account));
                    informed_slots.push(slot);
                    let moved = (impact * size * jitter).min(gap.abs());
                    permanent = (permanent + side * moved).clamp(-edge, edge);
                }
            }
            if worker && tradable && slot as f64 >= next_worker && worker_left > 0.0 {
                next_worker = slot as f64 + worker_pace * rng.range_f64((0.5, 1.5));
                // The last order takes the remainder with it rather than
                // leave less than the smallest order behind.
                let (least, most) = self.big_size_clamp;
                let mut size = self.size(&mut rng, true);
                if worker_left - size < least {
                    if worker_left <= most {
                        size = round2(worker_left);
                        worker_left = 0.0;
                    } else {
                        size = round2(worker_left - least);
                        worker_left = least;
                    }
                } else {
                    worker_left -= size;
                }
                let limit = half_spread * rng.range_f64(self.big_limit);
                takers.push(order(slot, worker_side, size, limit, worker_account));
                worker_slots.push(slot);
                passing += worker_side * impact * size * self.jitter(&mut rng);
            }
            levels.push((permanent + passing + drift).clamp(-edge, edge));
        }

        // Phoenix's index: an EMA of the perp, never far behind it.
        let alpha = 1.0 - (-(self.index_every as f64) / self.index_tau_slots).exp();
        let mut index_level = 0.0_f64;
        let mut index = Vec::new();
        let mut index_series = vec![0.0; closed as usize];
        for slot in 0..closed {
            let perp = levels[slot as usize];
            if slot > 0 && slot.is_multiple_of(self.index_every) && slot + 4 < closed {
                let next = round(
                    (index_level + alpha * (perp - index_level))
                        .clamp(perp - MAX_INDEX_GAP_PCT, perp + MAX_INDEX_GAP_PCT)
                        .clamp(-self.band_pct, self.band_pct),
                );
                if next != index_level {
                    index_level = next;
                    index.push(Waypoint::at(slot, index_level));
                }
            }
            index_series[slot as usize] = index_level;
        }

        // The perp's path: a waypoint wherever its level changed, one slot
        // after what moved it, at most one per Phoenix move. Each keeps
        // within the index gap Phoenix takes.
        let mut path = Vec::new();
        let mut shown = 0.0_f64;
        let mut free = 1;
        for slot in 1..closed {
            let target = levels[(slot - 1) as usize];
            let index_here = index_series[slot as usize];
            let target = round(target.clamp(
                index_here - MAX_INDEX_GAP_PCT,
                index_here + MAX_INDEX_GAP_PCT,
            ));
            if slot >= free && (target - shown).abs() >= 0.001 {
                path.push(Waypoint::at(slot, target));
                shown = target;
                free = slot + 3;
            }
        }
        let reopen_slot = closed.max(free);
        let open_level = round(weekend + open_sd * rng.normal());
        path.push(Waypoint::at(reopen_slot, open_level));
        // The reopen lands at once; the regular session after it wobbles
        // around the open.
        let mut slot = reopen_slot + crate::phoenix::SLOTS_BETWEEN_STEPS + 1;
        loop {
            slot += rng.range_u64(self.open_wobble_gap);
            if slot + 4 > duration {
                break;
            }
            path.push(Waypoint::at(
                slot,
                round(open_level + self.open_wobble_pct * (2.0 * rng.next_f64() - 1.0)),
            ));
        }
        let duration = duration.max(reopen_slot + 40);

        let shape = WeekendShape {
            informed: informed && !informed_slots.is_empty(),
            informed_account: informed.then_some(informed_account),
            worker,
            worker_account: worker.then_some(worker_account),
            worker_side: if worker { worker_side } else { 0.0 },
            impact_pct: impact,
            half_life_slots: half_life,
            weekend_pct: weekend,
            open_pct: open_level,
            permanent_pct: permanent,
            informed_slots,
            worker_slots,
            path: path.clone(),
        };
        let description = format!(
            "{} closed for {closed} slots with {} taker orders{}{}; the weekend moved it {weekend:+.2}% and it opens at {open_level:+.2}%",
            self.symbol,
            takers.len(),
            if informed {
                format!(", informed flow from account {informed_account}")
            } else {
                ", no informed flow".to_string()
            },
            if worker {
                format!(
                    ", account {worker_account} working {worker_qty:.0} shares to {}",
                    if worker_side > 0.0 { "buy" } else { "sell" }
                )
            } else {
                String::new()
            },
        );
        let scenario = Scenario {
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
            offhours: Some(OffHours {
                reopen_slot,
                band_pct: round(self.band_pct),
                half_spread_bps: half_spread,
                index,
                takers,
            }),
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
            reference_max_drawdown_usd: None,
            informed_max_drawdown_usd: None,
            fair_path: None,
            market_model: None,
            raw_sends: None,
            tx_layer: None,
            stop_out: None,
            liquidator: None,
        };
        (scenario, shape)
    }

    /// `count` scenarios named `<prefix>-<n>`, each mix of informed trader and
    /// worker in proportion to its odds (largest remainder). Seeds derive
    /// from `seed`; scenarios keep their draw order.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, WeekendShape)> {
        let mut cells: Vec<((bool, bool), usize, f64)> =
            [(true, true), (true, false), (false, true), (false, false)]
                .iter()
                .map(|&(i, w)| {
                    let p = if i {
                        self.informed_odds
                    } else {
                        1.0 - self.informed_odds
                    } * if w {
                        self.worker_odds
                    } else {
                        1.0 - self.worker_odds
                    };
                    let exact = p * count as f64;
                    ((i, w), exact.floor() as usize, exact - exact.floor())
                })
                .collect();
        let short = count - cells.iter().map(|c| c.1).sum::<usize>();
        let mut by_remainder: Vec<usize> = (0..cells.len()).collect();
        by_remainder.sort_by(|&a, &b| cells[b].2.total_cmp(&cells[a].2));
        for &i in by_remainder.iter().take(short) {
            cells[i].1 += 1;
        }
        let mut scenarios = Vec::with_capacity(count);
        let mut draw = 0u64;
        while scenarios.len() < count {
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let kind = self.kind(draw_seed);
            let cell = cells.iter_mut().find(|c| c.0 == kind).expect("every cell");
            if cell.1 == 0 {
                continue;
            }
            cell.1 -= 1;
            let name = format!("{prefix}-{}", scenarios.len() + 1);
            scenarios.push(self.draw(&name, draw_seed));
        }
        scenarios
    }

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(s, _)| s)
            .collect()
    }

    fn size(&self, rng: &mut SplitMix64, big: bool) -> f64 {
        let (median, sigma, (lo, hi)) = if big {
            (
                self.big_size_median,
                self.big_size_sigma,
                self.big_size_clamp,
            )
        } else {
            (
                self.noise_size_median,
                self.noise_size_sigma,
                self.noise_size_clamp,
            )
        };
        round2((median * (sigma * rng.normal()).exp()).clamp(lo, hi))
    }

    fn jitter(&self, rng: &mut SplitMix64) -> f64 {
        (1.0 + self.impact_jitter * rng.normal()).clamp(0.5, 1.5)
    }
}

fn frac_slot(rng: &mut SplitMix64, (lo, hi): (f64, f64), closed: u64) -> u64 {
    (rng.range_f64((lo, hi)) * closed as f64) as u64
}

fn order(slot: u64, side: f64, size: f64, limit_bps: f64, account: u8) -> TakerOrder {
    TakerOrder {
        slot,
        side: if side > 0.0 {
            TakerSide::Buy
        } else {
            TakerSide::Sell
        },
        size,
        limit_bps: round2(limit_bps),
        account,
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

    #[test]
    fn sampled_scenarios_are_valid_and_deterministic() {
        let family = WeekendFamily::spcx();
        for seed in 0..300 {
            let scenario = family.sample("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}");
            assert_eq!(scenario, family.sample("s", seed));
        }
    }

    #[test]
    fn sets_are_stratified_by_who_trades() {
        let family = WeekendFamily::spcx();
        let set = family.draw_set("h", 72, 9);
        let n = |i: bool, w: bool| {
            set.iter()
                .filter(|(_, s)| (s.informed_account.is_some(), s.worker) == (i, w))
                .count()
        };
        assert_eq!(
            [
                n(true, true),
                n(true, false),
                n(false, true),
                n(false, false)
            ],
            [26, 17, 17, 12]
        );
    }

    /// Informed sessions and the others carry about as much flow: the
    /// number of orders doesn't say who's trading.
    #[test]
    fn the_perp_stays_in_the_band_and_near_its_index() {
        let family = WeekendFamily::spcx();
        for seed in 0..200 {
            let (scenario, _) = family.draw("s", seed);
            let off = scenario.offhours.as_ref().unwrap();
            for w in scenario.path.iter().filter(|w| w.slot < off.reopen_slot) {
                assert!(w.level_pct.abs() <= family.band_pct);
                assert!(
                    (w.level_pct - crate::making::level_at(&off.index, w.slot)).abs()
                        <= MAX_INDEX_GAP_PCT + 0.5
                );
            }
        }
    }

    /// The worker's orders, its last included, come in the informed
    /// trader's sizes: a small remainder would mark it.
    #[test]
    fn worker_orders_stay_in_the_clamp() {
        let family = WeekendFamily::spcx();
        for seed in 0..300 {
            let (scenario, shape) = family.draw("s", seed);
            let Some(account) = shape.worker_account else {
                continue;
            };
            for o in &scenario.offhours.as_ref().unwrap().takers {
                if o.account == account {
                    assert!(
                        o.size >= family.big_size_clamp.0 - 1e-9
                            && o.size <= family.big_size_clamp.1 + 1e-9,
                        "seed {seed}: {}",
                        o.size
                    );
                }
            }
        }
    }

    #[test]
    fn the_informed_trader_moves_the_perp_toward_the_open() {
        let family = WeekendFamily::spcx();
        let mut right = 0;
        let mut n = 0;
        for seed in 0..400 {
            let (_, shape) = family.draw("s", seed);
            if shape.informed && shape.weekend_pct.abs() > 1.0 {
                n += 1;
                right += (shape.permanent_pct * shape.weekend_pct > 0.0) as u32;
            }
        }
        assert_eq!(right, n);
    }
}
