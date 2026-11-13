//! The funding-carry family: Phoenix's SPCX perp trades at a premium (or a
//! discount) to its index in episodes, and Phoenix's funding moves money
//! between longs and shorts while it does.
//!
//! Every episode opens the same way: the book moves away from the index.
//! What follows is one of two things, drawn per episode with odds drawn per
//! session:
//!
//! - **Crowded** (positioning): the premium holds a while, and funding is
//!   paid to the side against it, then the book comes back to the index.
//!   Fading it earns the premium and the funding.
//! - **Informed** (price discovery): after a lag, the index starts to
//!   follow the book, and the book keeps its lead while both move on; once
//!   the move is done the index catches up with the book. The gap closes
//!   here too, but by the index moving: fading it loses the move.
//!
//! Three things are drawn per session and shown nowhere but in the session
//! itself: the share of episodes that are informed, how long the index lags
//! the book when it follows, and how long a crowded premium holds. Funding's
//! interval, period and cap are drawn per session too; those are on chain.
//!
//! No tells: both kinds open with the same moves, sizes and timing; the index
//! wobbles throughout and carries fizzles (moves it gives back, the book
//! moving with it) in quiet stretches and in a crowded premium's hold; a
//! crowded premium sometimes widens before it fades. The session's length
//! is drawn first, and the path is cut where it ends.

use mtx_kit::Mandate;
use serde::Serialize;

use crate::{
    making::{TAKER_ACCOUNTS, TakerOrder, TakerSide},
    phoenix::MarkConfig,
    scenario::{Funding, Scenario, SplitMix64, VENUE, Waypoint},
};

/// What an episode turns out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The book comes back to the index.
    Crowded,
    /// The index follows the book, and both move on.
    Informed,
}

/// One episode, as drawn: what the ladder's told rung is told.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Episode {
    pub kind: Kind,
    /// Slot the book starts to move away from the index.
    pub start: u64,
    /// Slot the premium is fully open.
    pub open: u64,
    /// The premium it opens to, in percent of the index; negative is a
    /// discount.
    pub premium_pct: f64,
    /// Informed: the slot the index starts to follow, and how far the book
    /// moves on after the premium opened, in percent (the premium's sign).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift_pct: Option<f64>,
    /// Slot the premium starts to close, and the slot it's closed.
    pub close: u64,
    pub end: u64,
}

/// A session, beneath its path.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    pub informed_share: f64,
    pub lag_slots: u64,
    pub hold_slots: u64,
    pub funding: Funding,
    /// The cap on funding as a premium, in percent of the price.
    pub cap_pct: f64,
    /// The index's wobble per tick, in percent.
    pub noise_pct: f64,
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone)]
pub struct CarryFamily {
    pub symbol: String,
    /// The session's length, in slots, drawn first.
    pub duration: (u64, u64),
    /// Slot the first episode starts, and the quiet stretch between one
    /// episode's end and the next one's start.
    pub first_episode: (u64, u64),
    pub episode_gap: (u64, u64),
    /// Per session: the share of episodes that are informed, from the
    /// lower range (a positioning day) or the upper one (a discovery day),
    /// with even odds.
    pub informed_share: [(f64, f64); 2],
    /// Per session: how many slots the index lags the book in an informed
    /// episode, and each episode's spread around it.
    pub lag: (u64, u64),
    pub lag_jitter: (f64, f64),
    /// Per session: how many slots a crowded premium takes to halve once it
    /// starts to bleed out, and each episode's spread around it.
    pub hold: (u64, u64),
    pub hold_jitter: (f64, f64),
    /// The premium an episode opens to, in percent: log-uniform, either sign.
    pub premium_pct: (f64, f64),
    /// Odds the premium opens over a few ticks rather than at once, and over
    /// how many slots.
    pub ramp_odds: f64,
    pub ramp_slots: (u64, u64),
    /// Odds a crowded premium widens during its hold, and by how much.
    pub widen_odds: f64,
    pub widen: (f64, f64),
    /// The premium's tick-to-tick noise, as a share of it.
    pub premium_noise: f64,
    /// Informed: how far the book moves on after the premium opened, as a
    /// multiple of the premium (log-uniform), over how many slots, and how
    /// many slots the index takes to catch up at the end.
    pub drift_multiple: (f64, f64),
    pub drift_slots: (u64, u64),
    /// Informed: the book's lead over the index while both move, as a share
    /// of the premium it opened at.
    pub drift_lead: (f64, f64),
    pub catchup_slots: (u64, u64),
    /// Fizzles: odds a quiet stretch or a crowded hold carries one, its size
    /// in percent (either way; the book moves with the index), and its hold.
    pub fizzle_odds: f64,
    pub fizzle_pct: (f64, f64),
    pub fizzle_hold: (u64, u64),
    /// The index's wobble per tick, in percent, drawn per session from this
    /// range, and the slots between ticks. Fizzles scale with it (their sizes
    /// above are at [`BASE_NOISE_PCT`]).
    pub noise_pct: (f64, f64),
    pub tick_gap: (u64, u64),
    /// Funding, per session: interval and period in seconds, and the cap on
    /// one interval's funding, as a premium in percent of the price.
    pub funding_interval: (u64, u64),
    pub funding_period: (u64, u64),
    pub funding_cap_pct: (f64, f64),
    /// The price the cap converts to Phoenix's units at: the snapshot's.
    pub nominal_price_usd: f64,
    /// The tape: slots between taker orders, their sizes (lognormal around a
    /// median, clamped), and their limit in basis points from the book.
    pub tape_gap: (u64, u64),
    pub tape_size_median: f64,
    pub tape_size_sigma: f64,
    pub tape_size_clamp: (f64, f64),
    pub tape_limit_bps: f64,
    pub max_position: f64,
    pub max_drawdown_usd: f64,
}

/// The mark the family's sessions price: Phoenix's own book (the mid at its
/// last trade), not the oracles. See the task's design record: on a
/// surfnet, the oracle-weighted mark can't follow a book premium.
pub const BOOK_MARK: MarkConfig = MarkConfig {
    spot_weight: 0,
    perp_weight: 0,
    book_weight: 100,
    ema_ticks: 0,
};

/// The index's wobble per tick that fizzle sizes are stated at.
pub const BASE_NOISE_PCT: f64 = 0.06;

/// Quote lots per base lot of one USD per SPCX: 10^6 quote lots per USD over
/// 10^3 base lots per SPCX.
const QUOTE_LOTS_PER_BASE_LOT_PER_USD: f64 = 1_000.0;

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn sign(rng: &mut SplitMix64) -> f64 {
    if rng.next_f64() < 0.5 { -1.0 } else { 1.0 }
}

/// A keyframe of the path: the index's base level and the premium, in
/// percent, at a slot; the path interpolates between keyframes.
#[derive(Debug, Clone, Copy)]
struct Key {
    slot: u64,
    base: f64,
    premium: f64,
}

impl CarryFamily {
    pub fn spcx() -> Self {
        Self {
            symbol: "SPCX".into(),
            duration: (900, 1500),
            first_episode: (20, 120),
            episode_gap: (30, 160),
            informed_share: [(0.05, 0.3), (0.7, 0.95)],
            lag: (6, 40),
            lag_jitter: (0.7, 1.3),
            hold: (20, 120),
            hold_jitter: (0.6, 1.4),
            premium_pct: (0.3, 1.5),
            ramp_odds: 0.35,
            ramp_slots: (8, 24),
            widen_odds: 0.3,
            widen: (1.15, 1.5),
            premium_noise: 0.1,
            drift_multiple: (1.5, 6.0),
            drift_slots: (60, 240),
            drift_lead: (0.4, 1.0),
            catchup_slots: (8, 60),
            fizzle_odds: 0.5,
            fizzle_pct: (0.15, 0.6),
            fizzle_hold: (8, 40),
            noise_pct: (0.03, 0.25),
            tick_gap: (4, 10),
            funding_interval: (2, 6),
            funding_period: (16, 60),
            funding_cap_pct: (0.6, 1.6),
            nominal_price_usd: 143.0,
            tape_gap: (3, 8),
            tape_size_median: 0.4,
            tape_size_sigma: 0.6,
            tape_size_clamp: (0.05, 2.0),
            tape_limit_bps: 60.0,
            // ~1x the 10,000 USDC of collateral at the snapshot's ~$143.
            max_position: 70.0,
            max_drawdown_usd: 1_000.0,
        }
    }

    /// The practice family: the hidden family with the index's wobble fixed
    /// at [`BASE_NOISE_PCT`]. `mtx scenario generate` and the practice files
    /// draw from it; hidden sets draw from [`Self::spcx`], whose wobble each
    /// session draws, so a bot fitted to generated scenarios has only ever
    /// seen one level of noise.
    pub fn practice() -> Self {
        Self {
            noise_pct: (BASE_NOISE_PCT, BASE_NOISE_PCT),
            ..Self::spcx()
        }
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    /// The session's informed share, the first two draws of [`Self::draw`].
    pub fn informed_share(&self, seed: u64) -> f64 {
        let mut rng = SplitMix64(seed);
        Self::share(&self.informed_share, &mut rng)
    }

    fn share(ranges: &[(f64, f64); 2], rng: &mut SplitMix64) -> f64 {
        let range = if rng.next_f64() < 0.5 {
            ranges[0]
        } else {
            ranges[1]
        };
        rng.range_f64(range)
    }

    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        let mut rng = SplitMix64(seed);
        let informed_share = Self::share(&self.informed_share, &mut rng);
        let duration = rng.range_u64(self.duration);
        let lag_slots = rng.range_u64(self.lag);
        let hold_slots = rng.range_u64(self.hold);
        let interval = rng.range_u64(self.funding_interval);
        let period = rng.range_u64(self.funding_period).max(interval);
        let cap_pct = round3(rng.range_f64(self.funding_cap_pct));
        let max_rate = (cap_pct / 100.0
            * self.nominal_price_usd
            * QUOTE_LOTS_PER_BASE_LOT_PER_USD
            * interval as f64
            / period as f64)
            .round()
            .max(1.0) as i64;
        let funding = Funding {
            interval_seconds: interval,
            period_seconds: period,
            max_rate,
            mark: Some(BOOK_MARK),
        };

        // Episodes, as keyframes of the index's base and the premium.
        let mut keys = vec![Key {
            slot: 0,
            base: 0.0,
            premium: 0.0,
        }];
        let mut episodes = Vec::new();
        let mut base = 0.0;
        let mut t = rng.range_u64(self.first_episode);
        // Quiet stretches carry fizzles; so do crowded holds.
        let mut fizzles: Vec<(u64, u64, f64)> = Vec::new();
        let mut quiet_from = 0;
        while t < duration {
            self.maybe_fizzle(&mut rng, quiet_from, t, &mut fizzles);
            let kind = if rng.next_f64() < informed_share {
                Kind::Informed
            } else {
                Kind::Crowded
            };
            let q0 = sign(&mut rng) * rng.log_range_f64(self.premium_pct);
            let ramp = if rng.next_f64() < self.ramp_odds {
                rng.range_u64(self.ramp_slots)
            } else {
                1
            };
            let start = t;
            let open = start + ramp;
            keys.push(Key {
                slot: start,
                base,
                premium: 0.0,
            });
            keys.push(Key {
                slot: open,
                base,
                premium: q0,
            });
            let episode = match kind {
                Kind::Crowded => {
                    let life = (hold_slots as f64 * rng.range_f64(self.hold_jitter)).max(6.0);
                    // Sometimes the crowd keeps piling in first.
                    let (close, peak) = if rng.next_f64() < self.widen_odds {
                        let at = open + rng.range_u64((6, 30));
                        let wide = q0 * rng.range_f64(self.widen);
                        keys.push(Key {
                            slot: at,
                            base,
                            premium: wide,
                        });
                        (at, wide)
                    } else {
                        (open, q0)
                    };
                    // Then it bleeds out: halving every `life` slots.
                    for j in [0.35, 0.7, 1.0, 1.5, 2.2, 3.0] {
                        keys.push(Key {
                            slot: close + (life * j).round() as u64 + 1,
                            base,
                            premium: peak * 0.5f64.powf(j),
                        });
                    }
                    let end = close + (life * 3.0).round() as u64 + rng.range_u64((4, 16));
                    keys.push(Key {
                        slot: end,
                        base,
                        premium: 0.0,
                    });
                    self.maybe_fizzle(&mut rng, open, end, &mut fizzles);
                    Episode {
                        kind,
                        start,
                        open,
                        premium_pct: round3(q0),
                        follow: None,
                        drift_pct: None,
                        close,
                        end,
                    }
                }
                Kind::Informed => {
                    let lag = (lag_slots as f64 * rng.range_f64(self.lag_jitter)).round() as u64;
                    let follow = open + lag.max(2);
                    keys.push(Key {
                        slot: follow,
                        base,
                        premium: q0,
                    });
                    let drift = q0 * rng.log_range_f64(self.drift_multiple);
                    let span = rng.range_u64(self.drift_slots);
                    // Uneven legs: random weights, each a share of the
                    // drift over a share of the span.
                    let legs = 2 + rng.range_u64((0, 2));
                    // The first leg is the largest: news moves fast.
                    let weights: Vec<f64> = (0..legs)
                        .map(|leg| if leg == 0 { 1.0 } else { 0.3 } + rng.next_f64())
                        .collect();
                    let total: f64 = weights.iter().sum();
                    let mut done = 0.0;
                    let mut at = follow;
                    for w in &weights {
                        done += w / total;
                        at += (span as f64 * w / total).round() as u64;
                        // The book keeps a lead, noisily, as the index
                        // closes some of it.
                        let lead = q0 * rng.range_f64(self.drift_lead);
                        keys.push(Key {
                            slot: at,
                            base: base + drift * done,
                            premium: lead,
                        });
                    }
                    let close = at;
                    let lead = keys.last().map_or(q0, |k| k.premium);
                    base += drift;
                    let end = close + rng.range_u64(self.catchup_slots);
                    // The index catches up with the book: the book stays.
                    base = (1.0 + base / 100.0) * (1.0 + lead / 100.0) * 100.0 - 100.0;
                    keys.push(Key {
                        slot: end,
                        base,
                        premium: 0.0,
                    });
                    Episode {
                        kind,
                        start,
                        open,
                        premium_pct: round3(q0),
                        follow: Some(follow),
                        drift_pct: Some(round3(drift)),
                        close,
                        end,
                    }
                }
            };
            quiet_from = episode.end;
            t = episode.end + rng.range_u64(self.episode_gap);
            episodes.push(episode);
        }
        self.maybe_fizzle(&mut rng, quiet_from, duration, &mut fizzles);
        keys.push(Key {
            slot: duration,
            base,
            premium: 0.0,
        });

        // The session's wobble, drawn here (and only when it varies), so a
        // family with a fixed wobble draws the same sessions as before.
        let noise = if self.noise_pct.0 < self.noise_pct.1 {
            rng.range_f64(self.noise_pct)
        } else {
            self.noise_pct.0
        };
        let fizzle_scale = noise / BASE_NOISE_PCT;
        // Ticks: interpolate the keyframes, wobble the index, add fizzles
        // and the premium's noise.
        let mut path: Vec<Waypoint> = Vec::new();
        let mut offset = 0.0;
        let mut slot = 0;
        let mut k = 0;
        loop {
            slot += rng.range_u64(self.tick_gap);
            // A waypoint's move takes 2 slots and the next must start after.
            if slot + 3 > duration {
                break;
            }
            while k + 1 < keys.len() && keys[k + 1].slot <= slot {
                k += 1;
            }
            let (b, q) = match keys.get(k + 1) {
                Some(next) if next.slot > keys[k].slot => {
                    let f = (slot - keys[k].slot) as f64 / (next.slot - keys[k].slot) as f64;
                    (
                        keys[k].base + f * (next.base - keys[k].base),
                        keys[k].premium + f * (next.premium - keys[k].premium),
                    )
                }
                _ => (keys[k].base, keys[k].premium),
            };
            offset = 0.5 * offset + noise * (2.0 * rng.next_f64() - 1.0);
            let fizzle: f64 = fizzles
                .iter()
                .filter(|(from, to, _)| (*from..*to).contains(&slot))
                .map(|(_, _, size)| size * fizzle_scale)
                .sum();
            let jitter = 1.0 + self.premium_noise * (2.0 * rng.next_f64() - 1.0);
            let premium = round3(q * jitter);
            let level = round3(b + offset + fizzle);
            if path
                .last()
                .is_some_and(|w| w.level_pct == level && w.premium_pct == premium)
            {
                continue;
            }
            path.push(Waypoint {
                slot,
                level_pct: level,
                premium_pct: premium,
            });
        }

        let tape = self.tape(&mut rng, duration);
        let informed = episodes.iter().filter(|e| e.kind == Kind::Informed).count();
        let description = format!(
            "{}: {} premium episodes ({} crowded, {informed} informed; informed share {:.2}, lag {lag_slots} slots, hold {hold_slots} slots); funding every {interval} s over {period} s, capped at {cap_pct}%",
            self.symbol,
            episodes.len(),
            episodes.len() - informed,
            informed_share,
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
            offhours: None,
            funding: Some(funding),
            tape,
            spot: None,
            stake: None,
            faults: None,
            turn_data: None,
            order: None,
            participation: None,
            net: None,
            reference_pnl_usd: None,
            fair_path: None,
            raw_sends: None,
        };
        let shape = Shape {
            informed_share,
            lag_slots,
            hold_slots,
            funding,
            cap_pct,
            noise_pct: round3(noise),
            episodes,
        };
        (scenario, shape)
    }

    /// With the family's odds, a fizzle somewhere in `[from, to)`.
    fn maybe_fizzle(
        &self,
        rng: &mut SplitMix64,
        from: u64,
        to: u64,
        fizzles: &mut Vec<(u64, u64, f64)>,
    ) {
        if rng.next_f64() >= self.fizzle_odds {
            return;
        }
        let size = sign(rng) * rng.range_f64(self.fizzle_pct);
        let hold = rng.range_u64(self.fizzle_hold);
        if to <= from + hold + 8 {
            return;
        }
        let at = from + 4 + rng.range_u64((0, to - from - hold - 8));
        fizzles.push((at, at + hold, size));
    }

    /// Noise takers, either side at random, from the shared pool.
    fn tape(&self, rng: &mut SplitMix64, duration: u64) -> Vec<TakerOrder> {
        let mut orders = Vec::new();
        let mut slot = rng.range_u64(self.tape_gap);
        while slot < duration {
            let side = if rng.next_f64() < 0.5 {
                TakerSide::Buy
            } else {
                TakerSide::Sell
            };
            let (lo, hi) = self.tape_size_clamp;
            let size =
                (self.tape_size_median * (self.tape_size_sigma * rng.normal()).exp()).clamp(lo, hi);
            orders.push(TakerOrder {
                slot,
                side,
                size: (size * 100.0).round() / 100.0,
                limit_bps: self.tape_limit_bps,
                account: (rng.next_u64() % TAKER_ACCOUNTS as u64) as u8,
            });
            slot += rng.range_u64(self.tape_gap);
        }
        orders
    }

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// `count` sessions named `<prefix>-<n>`, stratified by the day: half
    /// positioning days, half discovery days, so a set isn't tilted toward
    /// fading or following by the luck of the draw.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let split = (self.informed_share[0].1 + self.informed_share[1].0) / 2.0;
        let day = |share: f64| usize::from(share >= split);
        let mut quotas = [count / 2, count - count / 2];
        let mut sessions = Vec::with_capacity(count);
        let mut draw = 0u64;
        while sessions.len() < count {
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let cell = day(self.informed_share(draw_seed));
            if quotas[cell] == 0 {
                continue;
            }
            quotas[cell] -= 1;
            let name = format!("{prefix}-{}", sessions.len() + 1);
            sessions.push(self.draw(&name, draw_seed));
        }
        sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_valid_and_deterministic() {
        let family = CarryFamily::spcx();
        for seed in 0..200 {
            let (scenario, shape) = family.draw("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}");
            assert_eq!(scenario, family.sample("s", seed));
            assert!(!shape.episodes.is_empty(), "seed {seed}");
            assert!(!scenario.tape.is_empty());
        }
    }

    /// Practice is the hidden family with one level of wobble; hidden
    /// sessions draw theirs, and a session's paths differ only from where the
    /// wobble is drawn.
    #[test]
    fn practice_is_the_hidden_family_at_one_wobble() {
        let (hidden, practice) = (CarryFamily::spcx(), CarryFamily::practice());
        let mut noises = Vec::new();
        for seed in 0..200 {
            let (h, hs) = hidden.draw("s", seed);
            let (p, ps) = practice.draw("s", seed);
            assert_eq!(ps.noise_pct, BASE_NOISE_PCT);
            assert_eq!(
                (hs.episodes, h.duration_slots, h.funding),
                (ps.episodes, p.duration_slots, p.funding)
            );
            assert_eq!(p.validate(), Ok(()));
            assert_eq!(h.validate(), Ok(()), "seed {seed}");
            noises.push(hs.noise_pct);
        }
        let low = noises.iter().filter(|n| **n < 0.08).count();
        let high = noises.iter().filter(|n| **n > 0.2).count();
        assert!(low > 10 && high > 10, "{low} low, {high} high");
    }

    #[test]
    fn sets_are_stratified_by_the_day() {
        let family = CarryFamily::spcx();
        let set = family.draw_set("h", 12, 9);
        let low = set.iter().filter(|(_, s)| s.informed_share < 0.5).count();
        assert_eq!(low, 6);
    }

    /// Crowded and informed episodes open alike: the same sizes and ramps.
    #[test]
    fn both_kinds_open_alike() {
        let family = CarryFamily::spcx();
        let (mut crowded, mut informed) = (Vec::new(), Vec::new());
        for seed in 0..600 {
            for e in family.draw("s", seed).1.episodes {
                let size = e.premium_pct.abs();
                match e.kind {
                    Kind::Crowded => crowded.push((size, e.open - e.start)),
                    Kind::Informed => informed.push((size, e.open - e.start)),
                }
            }
        }
        let mean = |v: &[(f64, u64)], f: &dyn Fn(&(f64, u64)) -> f64| {
            v.iter().map(f).sum::<f64>() / v.len() as f64
        };
        assert!((mean(&crowded, &|e| e.0) - mean(&informed, &|e| e.0)).abs() < 0.05);
        assert!((mean(&crowded, &|e| e.1 as f64) - mean(&informed, &|e| e.1 as f64)).abs() < 1.5);
    }

    /// Informed episodes close by the index moving: the book ends where the
    /// drift took it. Crowded ones close by the book coming back.
    #[test]
    fn informed_gaps_close_by_the_index() {
        // At one level of wobble, so a fizzle can't blur the book's close.
        let family = CarryFamily::practice();
        for seed in 0..100 {
            let (scenario, shape) = family.draw("s", seed);
            for e in &shape.episodes {
                if e.end + 12 >= scenario.duration_slots {
                    continue;
                }
                let book = |slot: u64| crate::making::book_at(&scenario.path, slot);
                let before = book(e.start.saturating_sub(1));
                let after = book(e.end + 10);
                match e.kind {
                    Kind::Crowded => assert!(
                        (after - before).abs() < 1.2,
                        "seed {seed}: {e:?} {before} {after}"
                    ),
                    Kind::Informed => assert!(
                        (after - before) * e.premium_pct.signum()
                            > 0.4 * e.drift_pct.unwrap().abs(),
                        "seed {seed}: {e:?} {before} {after}"
                    ),
                }
            }
        }
    }
}
