//! The measured market-making family (0.15.0): a market built from a profile
//! of a real market's taker flow, measured on a Phoenix tape
//! (`calibration/tape/phoenix-tape.20261125.fc443f1e.json`, written by
//! `calibration/fitter/tape/study.py`), traded on the SPCX snapshot.
//!
//! Up to 0.14.0 the family's takers were the generator's own: noise at 150
//! times SPCX's real rate, and informed orders only ahead of jumps, so a maker
//! earned the spread from flow that knew nothing. On Phoenix the average
//! taker is a little informed: a minute on, the mid has moved its way by
//! about what it paid to cross (RFD 0007). This family's takers come from the
//! actor library ([`crate::actors::library`]), their pace and sizes from the
//! profile ([`MarketProfile`]):
//!
//! - **noise** wallets, which know nothing;
//! - **informed** wallets, which see the price a minute ahead, right 85% of
//!   the time, in the share of the flow that makes the average taker's
//!   one-minute markout the profile's ([`MarketProfile::markout_60s_bps`]);
//! - an **arbitrageur** that, after each move and before the bot's turn,
//!   takes any quote left stale against Phoenix's index by more than its
//!   edge: where luthor's paper makers on mainnet lose (exchanges lead
//!   Phoenix by 1 to 2 slots, faster than a turn);
//! - the grader's **reactive competitor**, at the touch the profile measured.
//!
//! Every number that sets what a bot can earn is the profile's, or a stated
//! departure from it ([`MeasuredFamily`]'s fields say which).

use mtx_core::mandate::Taking;

use super::{Competition, Making, Reactive, round2, round3};
use crate::{
    actors::library::{self, Arbitrageur, IndexChaser, Informed, Library, Noise, Scale},
    obligations::Obligations,
    scenario::{Scenario, SplitMix64, VENUE, Waypoint, move_slots, shuffled},
};

/// The tape study every profile number comes from: the tape of
/// `phoenix-tape.20261125.fc443f1e` (the same 543,438 rows, exported again),
/// studied with CBRS's markouts and impact too (`--markets CBRS,SPCX,ANT`);
/// its SPCX and ANT numbers equal the first study's.
pub const STUDY: &str = "phoenix-tape.20261125.3cb49758";

/// Luthor's top-of-book reads of the touch's half-spread, on mtx-alpha
/// (`history.sqlite`, table `quotes`), from 2026-11-25 ~01:40 UTC: ANT
/// averaged 2.7 bp over 106 minutes (least 0.6, most 12); SPCX 0.4 bp (one
/// tick: 125.64 by 125.65) in the first minute. A guide, not a fit.
pub const QUOTES: &str = "luthor quotes, 106 minutes from 2026-11-25 01:40 UTC";

/// The quoting obligation's band, bp: twice the calibration's default book
/// half-spread (`book.half_spread_bps`, 5 bp).
pub const OBLIGATION_HALF_SPREAD_BPS: f64 = 10.0;

/// The market a session trades: a snapshot's perp, its mark (what a
/// profile's dollar sizes are turned into base units at), its tick, and the
/// mandate's cap in its base units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapshotMarket {
    pub symbol: &'static str,
    pub mark_usd: f64,
    pub tick_usd: f64,
    pub max_position: f64,
}

impl SnapshotMarket {
    /// Phoenix's ANT (`snapshots/phoenix-ant-2026-11-25`): $79.99, a $0.01
    /// tick (1.25 bp), and a cap of 35 ANT, about $2,800, the 0.3x of
    /// collateral SPCX's 20 was.
    pub fn ant() -> Self {
        Self {
            symbol: "ANT",
            mark_usd: 79.99,
            tick_usd: 0.01,
            max_position: 35.0,
        }
    }

    /// Phoenix's CBRS, the Cerebras perp (`snapshots/phoenix-cbrs-2026-11-25`,
    /// cross-margin): $84.37, a $0.01 tick (1.19 bp), and a cap of 33 CBRS,
    /// about $2,800, the 0.3x of collateral the others' caps are.
    pub fn cbrs() -> Self {
        Self {
            symbol: "CBRS",
            mark_usd: 84.37,
            tick_usd: 0.01,
            max_position: 33.0,
        }
    }

    /// Phoenix's SPCX (`snapshots/phoenix-spcx-2026-10-29`): $143.05, a
    /// $0.01 tick, a cap of 20 SPCX.
    pub fn spcx() -> Self {
        Self {
            symbol: "SPCX",
            mark_usd: 143.05,
            tick_usd: 0.01,
            max_position: 20.0,
        }
    }

    /// One tick in bp of the mark: no quote rests closer to the level.
    pub fn tick_bps(&self) -> f64 {
        self.tick_usd / self.mark_usd * 10_000.0
    }
}

/// The bench's clock (`scenario::SLOT_SECONDS`) over the actor library's
/// (`actors::library::SLOT_SECONDS`, mainnet's 0.4 s): the library turns a
/// rate a minute into a rate a slot at 0.4 s, and a session's minute is 240
/// slots, so a measured rate is handed to it times this.
pub const LIBRARY_RATE_FACTOR: f64 = crate::scenario::SLOT_SECONDS / library::SLOT_SECONDS;

/// Slots in a minute of the bench's clock.
const SLOTS_PER_MINUTE: f64 = 60.0 / crate::scenario::SLOT_SECONDS;

/// One size bucket of a profile's takers' markouts: orders of
/// `[usd_low, usd_high)`, what they paid against the mid just before them,
/// and how far the mid moved their way 10 and 60 seconds on (bp, means).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub usd_low: f64,
    pub usd_high: f64,
    pub orders: u32,
    pub paid_bps: f64,
    pub markout_10s_bps: f64,
    pub markout_60s_bps: f64,
}

/// A market's taker flow in US regular hours, as the tape study measured it
/// (non-pinger orders: an order is the fills in one slot on one side, and
/// one under $5 is the pinger's).
#[derive(Debug, Clone, PartialEq)]
pub struct MarketProfile {
    /// The market measured.
    pub market: &'static str,
    /// Orders a minute, regular hours (`markets.<m>.flow.regular`).
    pub orders_per_minute: f64,
    /// Order sizes in USD: median, the log-sd of a lognormal, and p90.
    pub order_usd_median: f64,
    pub order_usd_log_sd: f64,
    pub order_usd_p90: f64,
    /// The chance the next order is on the same side.
    pub same_side_next: f64,
    /// The touch's half-spread, bp ([`QUOTES`]).
    pub touch_half_spread_bps: f64,
    /// The worst fill's distance from the mid, p90, bp, over the orders of
    /// $100 to $10k (`markets.<m>.impact`): how deep big orders reach.
    pub impact_p90_bps: f64,
    /// The mid's volatility in US regular hours, bp a second: the sd of 60 s
    /// returns of the tape's mid, over the root of 60 (measured for 0.15.0
    /// by the study's rules; SPCX's 1.55 equals its calibration's
    /// `per_second_sd`, which checks the method).
    pub vol_bps_per_second: f64,
    /// The pinger: a bot that prints a few dollars about once a minute on
    /// every Phoenix market, uninformed (the study leaves orders under $5
    /// out of the flow). Its prints a minute and their size in USD.
    pub pinger_per_minute: f64,
    pub pinger_usd: f64,
    /// The takers' markouts by size (`markets.<m>.markouts`), buckets with
    /// fewer than [`Bucket`]s of 30 orders left out.
    pub buckets: &'static [Bucket],
}

/// SPCX's buckets: $10k+ held 1 order and is left out.
const SPCX_BUCKETS: &[Bucket] = &[
    Bucket {
        usd_low: 5.0,
        usd_high: 100.0,
        orders: 52,
        paid_bps: 6.196,
        markout_10s_bps: 3.189,
        markout_60s_bps: 4.887,
    },
    Bucket {
        usd_low: 100.0,
        usd_high: 1_000.0,
        orders: 188,
        paid_bps: 0.704,
        markout_10s_bps: 0.01,
        markout_60s_bps: 1.078,
    },
    Bucket {
        usd_low: 1_000.0,
        usd_high: 10_000.0,
        orders: 229,
        paid_bps: -2.097,
        markout_10s_bps: -0.537,
        markout_60s_bps: 0.779,
    },
];

/// CBRS's buckets: $10k+ held no order.
const CBRS_BUCKETS: &[Bucket] = &[
    Bucket {
        usd_low: 5.0,
        usd_high: 100.0,
        orders: 47,
        paid_bps: -2.694,
        markout_10s_bps: -4.193,
        markout_60s_bps: -2.634,
    },
    Bucket {
        usd_low: 100.0,
        usd_high: 1_000.0,
        orders: 109,
        paid_bps: -6.328,
        markout_10s_bps: -3.196,
        markout_60s_bps: -9.978,
    },
    Bucket {
        usd_low: 1_000.0,
        usd_high: 10_000.0,
        orders: 12,
        paid_bps: 12.508,
        markout_10s_bps: 16.663,
        markout_60s_bps: -5.024,
    },
];

const ANT_BUCKETS: &[Bucket] = &[
    Bucket {
        usd_low: 5.0,
        usd_high: 100.0,
        orders: 11_536,
        paid_bps: 2.355,
        markout_10s_bps: 2.885,
        markout_60s_bps: 5.427,
    },
    Bucket {
        usd_low: 100.0,
        usd_high: 1_000.0,
        orders: 10_567,
        paid_bps: 2.069,
        markout_10s_bps: 1.438,
        markout_60s_bps: 3.423,
    },
    Bucket {
        usd_low: 1_000.0,
        usd_high: 10_000.0,
        orders: 2_925,
        paid_bps: 3.26,
        markout_10s_bps: 1.504,
        markout_60s_bps: 3.509,
    },
    Bucket {
        usd_low: 10_000.0,
        usd_high: f64::INFINITY,
        orders: 159,
        paid_bps: 6.298,
        markout_10s_bps: 6.613,
        markout_60s_bps: 13.65,
    },
];

impl MarketProfile {
    /// SPCX, measured ([`STUDY`], `markets.SPCX`; touch from [`QUOTES`]).
    /// Its markouts are too few to say much (229 orders of $1k to $10k: 60 s
    /// +0.8 bp, 95% interval -3.1 to 4.1).
    pub fn spcx() -> Self {
        Self {
            market: "SPCX",
            orders_per_minute: 0.3333,
            order_usd_median: 1_000.5,
            order_usd_log_sd: 1.315,
            order_usd_p90: 3_483.1,
            same_side_next: 0.467,
            vol_bps_per_second: 1.55,
            pinger_per_minute: 0.0,
            pinger_usd: 3.0,
            touch_half_spread_bps: 0.4,
            // 100-1000: 17.43, 1000-10000: 15.53.
            impact_p90_bps: 15.53,
            buckets: SPCX_BUCKETS,
        }
    }

    /// ANT, the Anthropic perp, measured ([`STUDY`], `markets.ANT`; touch
    /// from [`QUOTES`]): the Phoenix market with real flow.
    pub fn ant() -> Self {
        Self {
            market: "ANT",
            orders_per_minute: 11.6436,
            order_usd_median: 99.61,
            order_usd_log_sd: 1.522,
            order_usd_p90: 1_000.99,
            same_side_next: 0.562,
            vol_bps_per_second: 4.45,
            pinger_per_minute: 0.0,
            pinger_usd: 3.0,
            touch_half_spread_bps: 2.7,
            // 100-1000: 11.42, 1000-10000: 16.54.
            impact_p90_bps: 16.54,
            buckets: ANT_BUCKETS,
        }
    }

    /// CBRS, the Cerebras perp, measured ([`STUDY`], `markets.CBRS`; touch
    /// from 4.8 hours of luthor's quotes; volatility from 60 s returns of the
    /// tape's mid, regular hours, an upper bound: the wide book's mid bounces
    /// between prints, robust sd 2.05). A thin, wide book: 96% of its orders
    /// are the pinger's. Its other orders are nearly one size (median $298,
    /// p90 $308: another bot, most likely), and its takers lose: a minute
    /// on, the mid has moved 7.6 bp against them on average.
    pub fn cbrs() -> Self {
        Self {
            market: "CBRS",
            orders_per_minute: 0.1846,
            order_usd_median: 298.27,
            order_usd_log_sd: 1.295,
            order_usd_p90: 308.23,
            same_side_next: 0.608,
            vol_bps_per_second: 3.44,
            pinger_per_minute: 1.0,
            pinger_usd: 3.0,
            touch_half_spread_bps: 13.9,
            // 5-100: 28.87, 100-1000: 30.07 (1000-10000: 99.79, 12 orders).
            impact_p90_bps: 30.07,
            buckets: CBRS_BUCKETS,
        }
    }

    /// The average taker's one-minute markout, bp: the buckets' means
    /// weighted by their orders.
    pub fn markout_60s_bps(&self) -> f64 {
        let orders: f64 = self.buckets.iter().map(|b| f64::from(b.orders)).sum();
        assert!(orders > 0.0, "a profile has measured buckets");
        self.buckets
            .iter()
            .map(|b| f64::from(b.orders) * b.markout_60s_bps)
            .sum::<f64>()
            / orders
    }

    /// The average taker's ten-second markout, bp, weighted as
    /// [`MarketProfile::markout_60s_bps`].
    pub fn markout_10s_bps(&self) -> f64 {
        let orders: f64 = self.buckets.iter().map(|b| f64::from(b.orders)).sum();
        assert!(orders > 0.0, "a profile has measured buckets");
        self.buckets
            .iter()
            .map(|b| f64::from(b.orders) * b.markout_10s_bps)
            .sum::<f64>()
            / orders
    }

    /// The median order in base units at `market`'s mark.
    pub fn order_size(&self, market: &SnapshotMarket) -> f64 {
        self.order_usd_median / market.mark_usd
    }
}

/// A session's kind. The family stratifies its sets by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasuredRegime {
    /// The walk alone.
    Calm,
    /// The walk with a drift.
    Trend,
    /// The walk and one jump.
    Jump,
}

/// The family's parameters, every range inclusive. The profile's numbers
/// are the market; the rest is how a session is cut from it.
#[derive(Debug, Clone)]
pub struct MeasuredFamily {
    pub profile: MarketProfile,
    pub market: SnapshotMarket,
    /// Stated departure: the measured rate times this. SPCX trades one
    /// order every three minutes in regular hours; a session of two to three
    /// minutes would hold none.
    pub flow_mult: f64,
    pub calm_odds: f64,
    pub trend_odds: f64,
    pub duration: (u64, u64),
    /// Each walk step's standard deviation, in percent, drawn log-uniform
    /// per session, and the slots between steps. The calibration
    /// (`calibration/market/spcx.20261112.d1f48be2.toml`) puts SPCX's
    /// regular-hours sd at 1.55 bp a second (`per_second_sd`), and the
    /// session's level at a log-sd of 0.54 (`log_sd_within`): about 1.9 bp
    /// a step of six slots, 0.011% to 0.034% across sessions.
    pub step_vol_pct: (f64, f64),
    pub step_gap: (u64, u64),
    /// A trend's drift per walk step, in percent (not measured: a trend is
    /// a session the informed flow leans through).
    pub trend_drift_pct: (f64, f64),
    /// A jump's size, in percent: the tape's 93 jumps of 1.5% or more had
    /// a median of 2.0% and a p90 of 4.17% ([`STUDY`], `jumps.events`).
    pub jump_pct: (f64, f64),
    pub jump_margin: u64,
    /// The share of the orders that is informed (fitted, see
    /// [`MeasuredFamily::fit_informed`]), the pace an informed wallet trades
    /// at while it trades (the share over the time it does, in multiples of
    /// the flow's), the move it must see before it trades (twice Phoenix's
    /// 3.5 bp taker fee: an informed trader takes when the move pays the
    /// fee in and out), the slots it sees ahead (60 s: the profile's
    /// longest markout), and how often it's right.
    pub informed_share: f64,
    pub informed_intensity: f64,
    pub informed_edge_bps: f64,
    /// Index chasers (the public mechanism): the share of the orders that
    /// takes the way the index has moved ahead of the book, past
    /// `index_trigger_bps` (the measured touch: the gap must pay the cost to
    /// cross), and their pace while the gap is open. Fitted with the
    /// informed share to the profile's 10 s and 60 s markouts
    /// ([`MeasuredFamily::fit_flow`]).
    pub index_share: f64,
    pub index_intensity: f64,
    pub index_trigger_bps: f64,
    pub index_wallets: u32,
    /// How far the book (Phoenix's makers) lags the index, slots, drawn per
    /// session: 1 to 2 s, as Binance leads Phoenix's SOL by about 0.5 s and
    /// Coinbase by 0.25 s, and the index by a second or two (luthor RFD 0001).
    pub book_lead_slots: (u64, u64),
    pub informed_horizon_slots: u64,
    pub informed_hit_rate: f64,
    /// Informed orders are larger than noise by this: the profile's big
    /// orders are the more informed ones (ANT's $10k+ +13.7 bp a minute on).
    pub informed_size_mult: f64,
    pub noise_wallets: u32,
    pub informed_wallets: u32,
    /// How far through the price a taker reaches, bp: the profile's p90
    /// worst fill.
    pub taker_limit_bps: f64,
    /// The arbitrageur's latency in slots and its edge in bp (Phoenix's
    /// 3.5 bp taker fee, `venues.phoenix_perp.taker_fee`, and up to 1.5 bp
    /// more), drawn per session.
    pub arbitrageur_latency_slots: (u64, u64),
    pub arbitrageur_edge_bps: (f64, f64),
    /// The competitor's half-spread and floor in bp, its quote in SPCX, how
    /// often it looks, and where the snapshot's makers stand behind it.
    pub competitor_bps: (f64, f64),
    pub floor_share: f64,
    pub competitor_size: (f64, f64),
    pub react_slots: (u64, u64),
    pub makers_bps: Option<(f64, f64)>,
    pub max_position: f64,
    pub max_drawdown_usd: f64,
}

impl MeasuredFamily {
    /// The family on `profile`'s flow, times `flow_mult` (1 for a market
    /// whose own flow fills a session), traded on `market`.
    pub fn new(profile: MarketProfile, flow_mult: f64, market: SnapshotMarket) -> Self {
        assert!(flow_mult >= 1.0 && flow_mult.is_finite());
        let tick_bps = market.tick_bps();
        let touch = profile.touch_half_spread_bps.max(tick_bps);
        // A touch of a tick or so (SPCX) can't be undercut: its depth is
        // thin, and the snapshot's makers stand behind it, as SPCX's worst
        // fills say (median 0.5 bp, p90 15.5 bp).
        let thin = touch < 2.0 * tick_bps;
        let mut family = Self {
            flow_mult,
            // Jumps at most 4 of 72 sessions: a stated stress share, about
            // 70 times the tape's rate (0.0115 per market-hour), not 0.14's
            // half or the first 0.15 draft's sixth.
            calm_odds: 0.55,
            trend_odds: 0.39,
            duration: (480, 800),
            // SPCX's calibration, scaled by the profile's measured volatility
            // over SPCX's (ANT: 4.45 / 1.55, 2.9x): ANT has no calibration.
            step_vol_pct: (
                round4(0.011 * profile.vol_bps_per_second / SPCX_VOL_BPS_PER_SECOND),
                round4(0.034 * profile.vol_bps_per_second / SPCX_VOL_BPS_PER_SECOND),
            ),
            step_gap: (3, 9),
            trend_drift_pct: (0.002, 0.006),
            jump_pct: (1.5, 4.2),
            jump_margin: 60,
            informed_share: 0.0,
            informed_intensity: 0.0,
            informed_edge_bps: INFORMED_EDGE_BPS,
            index_share: 0.0,
            index_intensity: 0.0,
            index_trigger_bps: round2(touch),
            index_wallets: 4,
            book_lead_slots: (4, 8),
            informed_horizon_slots: 240,
            informed_hit_rate: 0.85,
            informed_size_mult: 1.5,
            noise_wallets: 8,
            informed_wallets: 4,
            taker_limit_bps: round2(profile.impact_p90_bps),
            arbitrageur_latency_slots: (1, 2),
            arbitrageur_edge_bps: (3.5, 5.0),
            competitor_bps: if thin {
                (tick_bps, 2.0 * tick_bps)
            } else {
                (0.75 * touch, 1.3 * touch)
            },
            floor_share: 0.5,
            competitor_size: if thin { (0.5, 2.0) } else { (2.0, 5.0) },
            react_slots: (2, 8),
            makers_bps: if thin { Some((3.0, 8.0)) } else { None },
            max_position: market.max_position,
            max_drawdown_usd: 250.0,
            market,
            profile,
        };
        let fit = family.fit_flow();
        family.informed_share = fit.informed_share;
        family.informed_intensity =
            round3(fit.informed_share / fit.informed_active.max(f64::MIN_POSITIVE));
        family.index_share = fit.index_share;
        family.index_intensity = round3(fit.index_share / fit.index_active.max(f64::MIN_POSITIVE));
        family
    }

    /// The informed and index-chasing shares of the orders that give the
    /// profile's average 10 s and 60 s markouts, against the book's mid as
    /// the tape measures them, on calm paths of this family (the tape's ANT
    /// had four jumps in three days, SPCX none). An index chaser's markout
    /// is the gap it trades on, closed within the book's lag; an informed
    /// trader's builds over its minute, `(2 h - 1)` times the move it saw.
    /// Noise's is 0. Each share is clamped to [0, 0.9], and both to 0.95.
    fn fit_flow(&self) -> Fit {
        let mut informed = Tally::default();
        let mut index = Tally::default();
        let mut drawn = 0u64;
        let mut seed = FIT_SEED;
        let horizon = self.informed_horizon_slots;
        let hit = 2.0 * self.informed_hit_rate - 1.0;
        while drawn < FIT_SESSIONS {
            seed += 1;
            assert!(
                seed < FIT_SEED + 100 * FIT_SESSIONS,
                "calm sessions are drawn"
            );
            if self.regime(seed) != MeasuredRegime::Calm {
                continue;
            }
            drawn += 1;
            let (path, duration) = self.calm_path(seed);
            let book = |t: u64| super::book_at(&path, t.min(duration));
            let level = |t: u64| super::level_at(&path, t.min(duration));
            for slot in (0..duration).step_by(2) {
                informed.slots += 1;
                index.slots += 1;
                let ahead_bps = (level(slot + horizon) - level(slot)) * 100.0;
                if ahead_bps.abs() > self.informed_edge_bps {
                    let side = ahead_bps.signum() * hit;
                    informed.add(
                        side * (book(slot + 40) - book(slot)) * 100.0,
                        side * (book(slot + 240) - book(slot)) * 100.0,
                    );
                }
                let gap_bps = (level(slot) - book(slot)) * 100.0;
                if gap_bps.abs() > self.index_trigger_bps {
                    let side = gap_bps.signum();
                    index.add(
                        side * (book(slot + 40) - book(slot)) * 100.0,
                        side * (book(slot + 240) - book(slot)) * 100.0,
                    );
                }
            }
        }
        let (p10, p60) = informed.means();
        let (g10, g60) = index.means();
        let (m10, m60) = (
            self.profile.markout_10s_bps(),
            self.profile.markout_60s_bps(),
        );
        // m10 = i g10 + s p10, m60 = i g60 + s p60.
        let det = g10 * p60 - g60 * p10;
        assert!(
            det.abs() > 1e-9,
            "the two mechanisms' markouts differ in shape"
        );
        let index_share = ((m10 * p60 - m60 * p10) / det).clamp(0.0, 0.9);
        let informed_share = ((m60 - index_share * g60) / p60).clamp(0.0, 0.9);
        let scale = (0.95 / (index_share + informed_share)).min(1.0);
        Fit {
            informed_share: round3(informed_share * scale),
            informed_active: informed.active as f64 / informed.slots as f64,
            index_share: round3(index_share * scale),
            index_active: index.active as f64 / index.slots as f64,
        }
    }

    /// The calm path `sample` draws from `seed`, and its duration.
    fn calm_path(&self, seed: u64) -> (Vec<Waypoint>, u64) {
        let mut rng = SplitMix64(seed);
        let _ = rng.next_f64(); // The regime's draw, as `sample` takes it.
        let duration = rng.range_u64(self.duration);
        (
            self.path(&mut rng, MeasuredRegime::Calm, duration).0,
            duration,
        )
    }

    /// CBRS's measured market on Phoenix's CBRS: the task's
    /// (`market-making-cbrs`). Its flow is thin (0.18 orders a minute in
    /// regular hours), so sessions run twice as long (4 to 6.7 minutes) and
    /// the flow is [`CBRS_FLOW_MULT`] times the measured (stated).
    pub fn cbrs() -> Self {
        let mut family = Self::new(
            MarketProfile::cbrs(),
            CBRS_FLOW_MULT,
            SnapshotMarket::cbrs(),
        );
        family.duration = (960, 1600);
        family
    }

    /// ANT's measured market on Phoenix's ANT, at its own pace: the profile
    /// the task was drawn on before the owner moved it to CBRS, kept to
    /// compare.
    pub fn ant() -> Self {
        Self::new(MarketProfile::ant(), 1.0, SnapshotMarket::ant())
    }

    /// SPCX's measured market on Phoenix's SPCX, its flow times
    /// [`SPCX_FLOW_MULT`]: the profile the task was drawn on before the
    /// owner moved it to ANT, kept to compare them.
    pub fn spcx() -> Self {
        Self::new(
            MarketProfile::spcx(),
            SPCX_FLOW_MULT,
            SnapshotMarket::spcx(),
        )
    }

    /// Orders a session minute: the profile's times the multiplier.
    pub fn orders_per_minute(&self) -> f64 {
        self.profile.orders_per_minute * self.flow_mult
    }

    pub fn regime(&self, seed: u64) -> MeasuredRegime {
        regime_of(SplitMix64(seed).next_f64(), self.calm_odds, self.trend_odds)
    }

    /// One session, with its latent state.
    pub fn sample(&self, name: &str, seed: u64) -> (Scenario, MeasuredShape) {
        let mut rng = SplitMix64(seed);
        let regime = regime_of(rng.next_f64(), self.calm_odds, self.trend_odds);
        let duration = rng.range_u64(self.duration);
        let (path, book_lead_slots) = self.path(&mut rng, regime, duration);
        let competitor_bps = round2(rng.range_f64(self.competitor_bps));
        let competition = Competition {
            floor_bps: round2((self.floor_share * competitor_bps).max(0.01)),
            react_slots: rng.range_u64(self.react_slots),
            size: round2(rng.range_f64(self.competitor_size)),
            reactive: Some(Reactive::default()),
        };
        let makers_bps = self
            .makers_bps
            .map(|range| round2(rng.range_f64(range).max(competitor_bps)));
        let shape = MeasuredShape {
            regime,
            informed_share: self.informed_share,
            informed_horizon_slots: self.informed_horizon_slots,
            informed_hit_rate: self.informed_hit_rate,
            informed_edge_bps: self.informed_edge_bps,
            orders_per_minute: round3(self.orders_per_minute()),
            competitor_bps,
            competitor_floor_bps: competition.floor_bps,
            competitor_react_slots: competition.react_slots,
            index_share: self.index_share,
            index_trigger_bps: self.index_trigger_bps,
            book_lead_slots,
            arbitrageur_latency_slots: rng.range_u64(self.arbitrageur_latency_slots),
            arbitrageur_edge_bps: round2(rng.range_f64(self.arbitrageur_edge_bps)),
        };
        let making = Making {
            competitor_bps,
            competition,
            takers: Vec::new(),
            // Reported, never a breach: both sides within 10 bp (twice the
            // calibration's default 5 bp book), half the session, at a tenth
            // of a median order.
            obligations: Some(Obligations {
                presence: 0.5,
                max_half_spread_bps: OBLIGATION_HALF_SPREAD_BPS.max(3.0 * competitor_bps),
                min_size: round2((0.1 * self.profile.order_size(&self.market)).max(0.05)),
            }),
            makers_bps,
        };
        let description = format!(
            "{} on {}'s measured flow ({STUDY}), {:.1} orders a minute; {}",
            self.market.symbol,
            self.profile.market,
            self.orders_per_minute(),
            match regime {
                MeasuredRegime::Calm => "a walk",
                MeasuredRegime::Trend => "a drift",
                MeasuredRegime::Jump => "a jump",
            }
        );
        let scenario = self.scenario(name, description, duration, path, making, &shape);
        (scenario, shape)
    }

    /// The library's cast for a session of `shape`.
    pub fn actors(&self, shape: &MeasuredShape) -> Library {
        let rate = self.orders_per_minute() * LIBRARY_RATE_FACTOR;
        // Informed orders are larger, so their count share is less than
        // their size-weighted share would be: the scale is the noise's.
        Library {
            scale: Scale {
                trades_per_minute: round3(rate),
                trade_size: round3(self.profile.order_size(&self.market)),
                trade_size_sigma: self.profile.order_usd_log_sd,
            },
            noise: self.noise(),
            informed: if self.informed_share > 0.0 {
                vec![Informed {
                    wallets: self.informed_wallets,
                    horizon_slots: self.informed_horizon_slots,
                    edge_bps: self.informed_edge_bps,
                    hit_rate: self.informed_hit_rate,
                    view_noise_bps: 0.0,
                    intensity_mult: self.informed_intensity,
                    size_mult: self.informed_size_mult,
                    limit_bps: self.taker_limit_bps,
                    active: None,
                }]
            } else {
                Vec::new()
            },
            index_chasers: if self.index_share > 0.0 {
                vec![IndexChaser {
                    wallets: self.index_wallets,
                    trigger_bps: self.index_trigger_bps,
                    intensity_mult: self.index_intensity,
                    size_mult: 1.0,
                    limit_bps: self.taker_limit_bps,
                    active: None,
                }]
            } else {
                Vec::new()
            },
            arbitrageur: Some(Arbitrageur {
                latency_slots: shape.arbitrageur_latency_slots,
                edge_bps: shape.arbitrageur_edge_bps,
                size_mult: 1.0,
                max_inventory_mult: 40.0,
                // Its latency, from the index: a 1-slot arbitrageur looks right
                // after the move, before the bot's turn; a 2-slot one a step
                // later, after it. Mainnet's lead is 1 to 2 slots.
                after_turn: shape.arbitrageur_latency_slots >= 2,
            }),
            ..Library::default()
        }
    }

    /// The noise takers, and the pinger where the profile has one: its own
    /// wallet, its measured prints a minute (not multiplied: one bot), its
    /// few dollars a print.
    fn noise(&self) -> Vec<Noise> {
        let mut noise = vec![Noise {
            wallets: self.noise_wallets,
            flow_mult: round3(1.0 - self.informed_share - self.index_share),
            size_mult: 1.0,
            limit_bps: self.taker_limit_bps,
            active: None,
        }];
        if self.profile.pinger_per_minute > 0.0 {
            noise.push(Noise {
                wallets: 1,
                flow_mult: round4(self.profile.pinger_per_minute / self.orders_per_minute()),
                size_mult: round4(self.profile.pinger_usd / self.profile.order_usd_median),
                limit_bps: self.taker_limit_bps,
                active: None,
            });
        }
        noise
    }

    /// `count` sessions named `<prefix>-<n>`, each kind in proportion to its
    /// odds (calm and trend rounded, the jumps the rest: 40, 28, 4 of 72),
    /// shuffled. Seeds derive from `seed`.
    pub fn sample_set(
        &self,
        prefix: &str,
        count: usize,
        seed: u64,
    ) -> Vec<(Scenario, MeasuredShape)> {
        let calm = (self.calm_odds * count as f64).round() as usize;
        let trend = (self.trend_odds * count as f64).round() as usize;
        assert!(calm + trend <= count);
        let mut quotas = [
            (MeasuredRegime::Calm, calm),
            (MeasuredRegime::Trend, trend),
            (MeasuredRegime::Jump, count - calm - trend),
        ];
        let mut drawn = Vec::with_capacity(count);
        let mut draw = 0u64;
        // A jump session is drawn once in about 17, so a set fills well
        // within a thousand times its count of draws.
        while drawn.len() < count {
            draw += 1;
            assert!(draw < 1_000 * (count as u64 + 1), "the strata fill");
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
            drawn.push(draw_seed);
        }
        shuffled(drawn, seed)
            .into_iter()
            .enumerate()
            .map(|(i, s)| self.sample(&format!("{prefix}-{}", i + 1), s))
            .collect()
    }

    /// A session's path, the book `lead` slots behind the index, and the lead.
    fn path(
        &self,
        rng: &mut SplitMix64,
        regime: MeasuredRegime,
        duration: u64,
    ) -> (Vec<Waypoint>, u64) {
        let lead = rng.range_u64(self.book_lead_slots);
        let vol = rng.log_range_f64(self.step_vol_pct);
        let drift = match regime {
            MeasuredRegime::Trend => {
                let sign = if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
                sign * rng.range_f64(self.trend_drift_pct)
            }
            _ => 0.0,
        };
        let jump = match regime {
            MeasuredRegime::Jump => {
                let slot = rng.range_u64((self.jump_margin, duration - self.jump_margin));
                let sign = if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
                Some((slot, sign * rng.range_f64(self.jump_pct)))
            }
            _ => None,
        };
        let mut pending = jump.into_iter().peekable();
        let mut path = Vec::new();
        let mut level = 0.0;
        let mut slot = rng.range_u64(self.step_gap);
        while slot + 4 < duration {
            let previous = level;
            level = match pending.next_if(|(at, _)| *at <= slot) {
                Some((_, size)) => level + size,
                None => level + drift + vol * rng.normal(),
            };
            level = round3(level);
            path.push(Waypoint::at(slot, level));
            // A jump can take more than one of Phoenix's steps, for the
            // index and then for the book behind it: wait them out.
            let moving = move_slots(previous, level) + 1;
            let wait = if moving > 2 { lead + moving } else { moving };
            slot = (slot + rng.range_u64(self.step_gap)).max(slot + wait);
        }
        assert!(!path.is_empty(), "a session of {duration} slots walks");
        (lag_book(&path, lead, duration), lead)
    }

    fn scenario(
        &self,
        name: &str,
        description: String,
        duration: u64,
        path: Vec<Waypoint>,
        making: Making,
        shape: &MeasuredShape,
    ) -> Scenario {
        let mut mandate = mtx_core::Mandate::perp(
            VENUE,
            self.market.symbol,
            self.max_position,
            self.max_drawdown_usd,
        );
        assert_eq!(
            mandate.universe.len(),
            1,
            "a perp mandate has one allowance"
        );
        // A maker's desk takes only to reduce (0.13.0, ADR 0003).
        mandate.universe[0].taking = Some(Taking::ReduceOnly);
        mandate.version = mandate.minimal_version();
        Scenario {
            name: name.to_string(),
            description,
            symbol: self.market.symbol.into(),
            duration_slots: duration,
            mandate,
            path,
            making: Some(making),
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            actors: Some(self.actors(shape)),
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
}

/// Stated departure: SPCX's 0.33 orders a minute times 35 is ANT's pace,
/// 11.7 a minute, about 30 orders a session. At its own pace a session would
/// hold one.
pub const SPCX_FLOW_MULT: f64 = 35.0;

/// Stated departure: CBRS's 0.185 orders a minute times 30 is 5.5 a minute,
/// about 30 orders in a session of 4 to 6.7 minutes. At its own pace a
/// session would hold one.
pub const CBRS_FLOW_MULT: f64 = 30.0;

/// The move an informed wallet must see before it trades, bp: Phoenix's
/// 3.5 bp taker fee (`venues.phoenix_perp.taker_fee`) in and out.
pub const INFORMED_EDGE_BPS: f64 = 7.0;

/// The calm sessions the informed share is fitted on, and their seeds.
const FIT_SESSIONS: u64 = 200;
const FIT_SEED: u64 = 0x6d6d_6669_7400;

/// What a session was drawn as: its kind, and the process its flow comes
/// from (the latent state's process facts).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct MeasuredShape {
    #[serde(skip)]
    pub regime: MeasuredRegime,
    pub informed_share: f64,
    pub informed_horizon_slots: u64,
    pub informed_hit_rate: f64,
    pub informed_edge_bps: f64,
    pub orders_per_minute: f64,
    /// The grader's maker: its half-spread, the floor it never undercuts
    /// past, and the slots between its looks (its reaction rule).
    pub competitor_bps: f64,
    pub competitor_floor_bps: f64,
    pub competitor_react_slots: u64,
    pub index_share: f64,
    pub index_trigger_bps: f64,
    pub book_lead_slots: u64,
    pub arbitrageur_latency_slots: u64,
    pub arbitrageur_edge_bps: f64,
}

impl MeasuredShape {
    /// What the informed bot is told: who the informed takers and the
    /// arbitrageur are (their wallets, filled in as the session starts), and
    /// the process (the informed share, horizon and hit rate, the flow's
    /// pace, the arbitrageur's latency and edge). Never the path, a jump, or
    /// when anyone trades.
    pub fn latent(&self) -> serde_json::Value {
        let mut roster = Vec::new();
        if self.informed_share > 0.0 {
            roster.push("informed");
        }
        if self.index_share > 0.0 {
            roster.push("index-chaser");
        }
        roster.push("arbitrageur");
        serde_json::json!({
            crate::latent::ROSTER: roster,
            "informed_share": self.informed_share,
            "informed_horizon_slots": self.informed_horizon_slots,
            "informed_hit_rate": self.informed_hit_rate,
            "informed_edge_bps": self.informed_edge_bps,
            "orders_per_minute": self.orders_per_minute,
            "competitor_bps": self.competitor_bps,
            "competitor_floor_bps": self.competitor_floor_bps,
            "competitor_react_slots": self.competitor_react_slots,
            "index_share": self.index_share,
            "index_trigger_bps": self.index_trigger_bps,
            "book_lead_slots": self.book_lead_slots,
            "arbitrageur_latency_slots": self.arbitrageur_latency_slots,
            "arbitrageur_edge_bps": self.arbitrageur_edge_bps,
        })
    }

    /// The regime's name, for annotations.
    pub fn regime_name(&self) -> &'static str {
        match self.regime {
            MeasuredRegime::Calm => "calm",
            MeasuredRegime::Trend => "trend",
            MeasuredRegime::Jump => "jump",
        }
    }
}

/// The path with the book `lead` slots behind the index: at each index
/// move the makers stay where they were (a premium of the old level over
/// the new), and `lead` slots later they catch up.
fn lag_book(levels: &[Waypoint], lead: u64, duration: u64) -> Vec<Waypoint> {
    assert!(levels.iter().all(|w| w.premium_pct == 0.0));
    let mut slots: Vec<u64> = levels
        .iter()
        .flat_map(|w| [w.slot, w.slot + lead])
        .filter(|s| *s + 4 < duration)
        .collect();
    slots.sort_unstable();
    slots.dedup();
    let path: Vec<Waypoint> = slots
        .into_iter()
        .map(|slot| {
            let level = super::level_at(levels, slot);
            let book = super::level_at(levels, slot.saturating_sub(lead));
            let book = if slot < lead { 0.0 } else { book };
            let premium = ((1.0 + book / 100.0) / (1.0 + level / 100.0) - 1.0) * 100.0;
            Waypoint {
                premium_pct: (premium * 1e6).round() / 1e6,
                ..Waypoint::at(slot, level)
            }
        })
        .collect();
    assert!(path.windows(2).all(|w| w[0].slot < w[1].slot));
    path
}

/// What [`MeasuredFamily::fit_flow`] finds: each mechanism's share of the
/// orders and the share of the time it trades.
struct Fit {
    informed_share: f64,
    informed_active: f64,
    index_share: f64,
    index_active: f64,
}

/// A mechanism's slots, the ones it trades in, and its markouts there.
#[derive(Default)]
struct Tally {
    slots: u64,
    active: u64,
    markout_10s_bps: f64,
    markout_60s_bps: f64,
}

impl Tally {
    fn add(&mut self, markout_10s_bps: f64, markout_60s_bps: f64) {
        self.active += 1;
        self.markout_10s_bps += markout_10s_bps;
        self.markout_60s_bps += markout_60s_bps;
    }

    fn means(&self) -> (f64, f64) {
        assert!(self.active > 0, "a mechanism trades in some slots");
        let n = self.active as f64;
        (self.markout_10s_bps / n, self.markout_60s_bps / n)
    }
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// SPCX's regular-hours volatility, bp a second: the calibration's
/// `per_second_sd` (`spcx.20261112.d1f48be2`), which the walk's step sizes
/// are drawn from.
const SPCX_VOL_BPS_PER_SECOND: f64 = 1.55;

fn regime_of(draw: f64, calm_odds: f64, trend_odds: f64) -> MeasuredRegime {
    if draw < calm_odds {
        MeasuredRegime::Calm
    } else if draw < calm_odds + trend_odds {
        MeasuredRegime::Trend
    } else {
        MeasuredRegime::Jump
    }
}

/// One simulated taker order: its session's kind, its size in USD, and its
/// markouts against the book's mid 10 s and 60 s on, bp.
#[derive(Debug, Clone, Copy)]
pub struct SimulatedOrder {
    pub regime: MeasuredRegime,
    pub usd: f64,
    pub markout_10s_bps: f64,
    pub markout_60s_bps: f64,
}

/// Simulated markouts, for the profile checks: draws `sessions` sessions'
/// paths and flow by the library's rules (arrivals at the scale's pace;
/// noise either side; index chasers the way the index leads the book past
/// their trigger; informed wallets one view each, held a horizon, right
/// with the hit rate, past their edge) and marks each order out against the
/// book's mid, as the tape does.
pub fn simulate_markouts(family: &MeasuredFamily, sessions: u64, seed: u64) -> Vec<SimulatedOrder> {
    let mut out = Vec::new();
    let mut rng = SplitMix64(seed);
    let per_pair = 2.0 * family.orders_per_minute() / SLOTS_PER_MINUTE;
    let horizon = family.informed_horizon_slots;
    for n in 0..sessions {
        let (scenario, shape) = family.sample("sim", seed.wrapping_add(n));
        let end = scenario.duration_slots;
        let level = |slot: u64| super::level_at(&scenario.path, slot.min(end));
        let book = |slot: u64| super::book_at(&scenario.path, slot.min(end));
        let mut views = vec![(0u64, false); family.informed_wallets as usize];
        let mut push = |rng: &mut SplitMix64, slot: u64, side: f64, mult: f64| {
            let usd = family.profile.order_usd_median
                * mult
                * (family.profile.order_usd_log_sd * rng.normal()).exp();
            out.push(SimulatedOrder {
                regime: shape.regime,
                usd,
                markout_10s_bps: side * (book(slot + 40) - book(slot)) * 100.0,
                markout_60s_bps: side * (book(slot + 240) - book(slot)) * 100.0,
            });
        };
        // Bernoulli per slot pair: the means are far under 1.
        let arrives = |rng: &mut SplitMix64, mean: f64| rng.next_f64() < mean;
        for slot in (0..end).step_by(2) {
            let noise = 1.0 - family.informed_share - family.index_share;
            if arrives(&mut rng, per_pair * noise) {
                let side = if rng.next_f64() < 0.5 { 1.0 } else { -1.0 };
                push(&mut rng, slot, side, 1.0);
            }
            let gap_bps = (level(slot) - book(slot)) * 100.0;
            if gap_bps.abs() > family.index_trigger_bps
                && arrives(&mut rng, per_pair * family.index_intensity)
            {
                push(&mut rng, slot, gap_bps.signum(), 1.0);
            }
            let ahead = level(slot + horizon) - level(slot);
            let wallets = views.len() as f64;
            for view in views.iter_mut() {
                if slot >= view.0 {
                    *view = (slot + horizon, rng.next_f64() >= family.informed_hit_rate);
                }
                if (ahead * 100.0).abs() <= family.informed_edge_bps {
                    continue;
                }
                if arrives(&mut rng, per_pair * family.informed_intensity / wallets) {
                    let side = if view.1 {
                        -ahead.signum()
                    } else {
                        ahead.signum()
                    };
                    push(&mut rng, slot, side, family.informed_size_mult);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mean(values: impl Iterator<Item = f64>) -> f64 {
        let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
        assert!(n > 0);
        sum / n as f64
    }

    #[test]
    fn the_profiles_hold_the_tape_s_numbers() {
        let spcx = MarketProfile::spcx();
        assert_eq!(spcx.orders_per_minute, 0.3333);
        assert_eq!(spcx.order_usd_median, 1_000.5);
        assert_eq!(spcx.touch_half_spread_bps, 0.4);
        assert!(
            (spcx.markout_60s_bps() - 1.35).abs() < 0.01,
            "{}",
            spcx.markout_60s_bps()
        );
        let ant = MarketProfile::ant();
        assert_eq!(ant.orders_per_minute, 11.6436);
        assert_eq!(ant.order_usd_median, 99.61);
        assert_eq!(ant.touch_half_spread_bps, 2.7);
        assert!(
            (ant.markout_60s_bps() - 4.43).abs() < 0.02,
            "{}",
            ant.markout_60s_bps()
        );
        // Big ANT takers are the more informed.
        let big = ant.buckets.last().unwrap();
        assert!(big.markout_60s_bps > 2.0 * ant.markout_60s_bps());
    }

    #[test]
    fn the_flow_is_the_profile_s_pace_times_the_stated_multiplier() {
        let spcx = MeasuredFamily::spcx();
        assert_eq!(spcx.flow_mult, SPCX_FLOW_MULT);
        assert!((spcx.orders_per_minute() - 11.67).abs() < 0.01);
        let ant = MeasuredFamily::ant();
        assert_eq!(ant.flow_mult, 1.0);
        let (scenario, shape) = ant.sample("s", 3);
        let actors = scenario.actors.as_ref().unwrap();
        // The library counts 0.4 s a slot: a session minute's 240 slots
        // must see the profile's 11.6 orders.
        let per_session_minute = actors.scale.per_slot() * SLOTS_PER_MINUTE;
        assert!(
            (per_session_minute - 11.64).abs() < 0.01,
            "{per_session_minute}"
        );
        let noise = actors.noise[0].flow_mult;
        assert!((noise + ant.informed_share + ant.index_share - 1.0).abs() < 1e-3);
        assert_eq!(actors.index_chasers.len(), 1);
        assert_eq!(actors.index_chasers[0].trigger_bps, 2.7);
        // Informed wallets trade only while they see a move past their
        // edge, faster then, so their count share is the fitted one.
        assert_eq!(actors.informed[0].edge_bps, INFORMED_EDGE_BPS);
        assert!(actors.informed[0].intensity_mult > ant.informed_share);
        assert!((actors.scale.trade_size - 99.61 / 79.99).abs() < 1e-3);
        assert_eq!(shape.orders_per_minute, 11.644);
    }

    #[test]
    fn every_session_has_noise_informed_takers_an_arbitrageur_and_a_reactive_competitor() {
        for family in [MeasuredFamily::spcx(), MeasuredFamily::ant()] {
            for seed in 0..20 {
                let (scenario, shape) = family.sample("s", seed);
                scenario.validate().unwrap();
                let actors = scenario.actors.as_ref().unwrap();
                actors.validate().unwrap();
                assert_eq!(actors.noise.len(), 1);
                assert_eq!(actors.informed.len(), 1);
                assert_eq!(actors.informed[0].horizon_slots, 240);
                let arb = actors.arbitrageur.as_ref().unwrap();
                assert!((1..=2).contains(&arb.latency_slots));
                assert!(arb.edge_bps >= 3.5 && arb.edge_bps <= 5.0);
                assert_eq!(arb.latency_slots, shape.arbitrageur_latency_slots);
                let making = scenario.making.as_ref().unwrap();
                assert!(making.takers.is_empty(), "every taker is the library's");
                assert!(making.competition.reactive.is_some());
                assert!(making.obligations.is_some());
                let wallets = actors.wallets();
                assert!(wallets.iter().any(|(r, _)| *r == "arbitrageur"));
                assert_eq!(wallets.iter().filter(|(r, _)| *r == "informed").count(), 4);
                let chasers = wallets.iter().filter(|(r, _)| *r == "index-chaser").count();
                assert_eq!(chasers > 0, family.index_share > 0.0);
            }
        }
    }

    #[test]
    fn the_competitor_quotes_the_measured_touch() {
        // SPCX: one to two ticks (the touch was one tick wide), thin, with
        // the snapshot's makers behind it.
        for seed in 0..30 {
            let (s, _) = MeasuredFamily::spcx().sample("s", seed);
            let m = s.making.unwrap();
            assert!(
                m.competitor_bps >= 0.69 && m.competitor_bps <= 1.41,
                "{}",
                m.competitor_bps
            );
            assert!(m.competition.size <= 2.0);
            assert!(m.makers_bps.unwrap() >= 3.0);
            let (s, _) = MeasuredFamily::ant().sample("s", seed);
            assert_eq!(s.symbol, "ANT");
            assert_eq!(s.mandate.universe[0].max_position, 35.0);
            let m = s.making.unwrap();
            assert!(
                m.competitor_bps >= 2.0 && m.competitor_bps <= 3.52,
                "{}",
                m.competitor_bps
            );
            assert!(m.makers_bps.is_none());
        }
    }

    #[test]
    fn sets_are_stratified_40_28_4() {
        for family in [MeasuredFamily::spcx(), MeasuredFamily::ant()] {
            let set = family.sample_set("hidden", 72, 9);
            let count = |r| set.iter().filter(|(_, s)| s.regime == r).count();
            assert_eq!(count(MeasuredRegime::Calm), 40);
            assert_eq!(count(MeasuredRegime::Trend), 28);
            assert_eq!(count(MeasuredRegime::Jump), 4);
            let jumps = set
                .iter()
                .filter(|(_, s)| s.regime == MeasuredRegime::Jump)
                .all(|(sc, _)| {
                    sc.path
                        .windows(2)
                        .any(|w| (w[1].level_pct - w[0].level_pct).abs() >= 1.5)
                });
            assert!(jumps);
            let again = family.sample_set("hidden", 72, 9);
            assert_eq!(
                serde_json::to_string(&set.iter().map(|s| &s.0).collect::<Vec<_>>()).unwrap(),
                serde_json::to_string(&again.iter().map(|s| &s.0).collect::<Vec<_>>()).unwrap()
            );
        }
    }

    /// The average taker's 10 s and 60 s markouts against the book come out
    /// near the profile's on calm sessions, and the big orders are the more
    /// informed.
    #[test]
    fn simulated_markouts_match_the_profile() {
        for family in [MeasuredFamily::spcx(), MeasuredFamily::ant()] {
            let orders: Vec<SimulatedOrder> = simulate_markouts(&family, 2_000, 17)
                .into_iter()
                .filter(|o| o.regime == MeasuredRegime::Calm)
                .collect();
            let m10 = mean(orders.iter().map(|o| o.markout_10s_bps));
            let m60 = mean(orders.iter().map(|o| o.markout_60s_bps));
            let (t10, t60) = (
                family.profile.markout_10s_bps(),
                family.profile.markout_60s_bps(),
            );
            eprintln!(
                "{}: 10 s {m10:.2} (target {t10:.2}), 60 s {m60:.2} (target {t60:.2}); informed {}, index {}",
                family.profile.market, family.informed_share, family.index_share
            );
            let market = family.profile.market;
            assert!(
                (m60 - t60).abs() < 0.6 + 0.25 * t60.abs(),
                "{market}: 60 s {m60:.2}"
            );
            if family.index_share > 0.0 {
                assert!(
                    (m10 - t10).abs() < 0.6 + 0.3 * t10.abs(),
                    "{market}: 10 s {m10:.2}"
                );
            }
            let median = family.profile.order_usd_median;
            let small = mean(
                orders
                    .iter()
                    .filter(|o| o.usd < median)
                    .map(|o| o.markout_60s_bps),
            );
            let big = mean(
                orders
                    .iter()
                    .filter(|o| o.usd >= 2.0 * median)
                    .map(|o| o.markout_60s_bps),
            );
            assert!(big > small, "{market}: big {big:.2}, small {small:.2}");
        }
    }

    /// CBRS's takers lose on average (the mid moves against them): no
    /// mechanism that knows something fits, so its flow is noise and the
    /// pinger, and the arbitrageur picks off what a move leaves behind.
    #[test]
    fn cbrs_is_a_thin_wide_book_of_uninformed_takers() {
        let family = MeasuredFamily::cbrs();
        assert!(family.profile.markout_60s_bps() < -7.0);
        assert_eq!(family.informed_share, 0.0);
        assert_eq!(family.index_share, 0.0);
        assert_eq!(family.duration, (960, 1600));
        assert!((family.orders_per_minute() - 5.538).abs() < 1e-3);
        for seed in 0..10 {
            let (s, shape) = family.sample("s", seed);
            s.validate().unwrap();
            assert_eq!(s.symbol, "CBRS");
            assert_eq!(s.mandate.universe[0].max_position, 33.0);
            let actors = s.actors.as_ref().unwrap();
            actors.validate().unwrap();
            assert!(actors.informed.is_empty() && actors.index_chasers.is_empty());
            // The noise takers, and the pinger: about one $3 print a minute.
            assert_eq!(actors.noise.len(), 2);
            let pinger = &actors.noise[1];
            let per_minute = actors.scale.per_slot() * SLOTS_PER_MINUTE * pinger.flow_mult;
            assert!((per_minute - 1.0).abs() < 0.01, "{per_minute}");
            assert!((actors.scale.trade_size * 84.37 * pinger.size_mult - 3.0).abs() < 0.05);
            let m = s.making.unwrap();
            assert!(
                m.competitor_bps >= 10.0 && m.competitor_bps <= 18.1,
                "{}",
                m.competitor_bps
            );
            assert_eq!(shape.latent()["roster"], serde_json::json!(["arbitrageur"]));
        }
    }

    #[test]
    fn the_book_lags_the_index_by_one_to_two_seconds() {
        for seed in 0..20 {
            let (s, shape) = MeasuredFamily::ant().sample("s", seed);
            assert!((4..=8).contains(&shape.book_lead_slots));
            let lead = shape.book_lead_slots;
            for w in s
                .path
                .iter()
                .filter(|w| w.premium_pct == 0.0 && w.slot > lead)
            {
                // Where the book has caught up, it is where the index was
                // `lead` slots before.
                let before = super::super::level_at(&s.path, w.slot - lead);
                assert!((w.book_pct() - before).abs() < 1e-3 || w.level_pct == before);
            }
            let leads = s.path.iter().filter(|w| w.premium_pct != 0.0).count();
            assert!(leads > 0, "the index moves first");
        }
    }

    #[test]
    fn the_latent_state_names_the_informed_and_the_arbitrageur_and_no_draw() {
        let (_, shape) = MeasuredFamily::spcx().sample("s", 1);
        let latent = shape.latent();
        // SPCX's flow has no index chasers: the roster names only kinds the
        // session seats.
        assert_eq!(
            latent["roster"],
            serde_json::json!(["informed", "arbitrageur"])
        );
        let (_, shape) = MeasuredFamily::ant().sample("s", 1);
        assert_eq!(
            shape.latent()["roster"],
            serde_json::json!(["informed", "index-chaser", "arbitrageur"])
        );
        let text = latent.to_string();
        for draw in ["jump", "slot\"", "path", "trend", "regime"] {
            assert!(!text.contains(draw), "{draw} in {text}");
        }
    }
}
