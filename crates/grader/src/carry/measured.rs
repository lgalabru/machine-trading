//! The measured funding-carry family (0.15.0): Phoenix's SOL perp, its
//! premium and funding as Phoenix's public API recorded them, its takers as
//! luthor's tape recorded them, its index from SOL's market calibration.
//!
//! Up to 0.14.0 the carry task traded SPCX, with premium episodes of 30 to
//! 150 bp that the generator scripted, crowded or informed. Nothing measured
//! on SOL looks like that. Over 90 days of Phoenix's hourly funding
//! (`calibration/funding/phoenix-funding.sol.20261125.248a28ea.json`) the
//! premium it implies averaged +1.43 bp, wandered 0.84 bp around a five-day
//! level with a half-life of 3.6 hours, never passed 8.3 bp, and never
//! reached the cap. Longs paid shorts 92% of the hours. That is the carry
//! here: a small, slow, nearly always positive basis, worth about a sixth
//! of a basis point an hour, against a 3.5 bp taker fee each way.
//!
//! A session plays about five days of that funding: the funding interval is
//! 3 or 4 seconds instead of 3,600 (stated departure, [`FUNDING_INTERVAL_S`]),
//! the period stays eight intervals as on chain, the cap is the snapshot's
//! own per interval, and the premium's clock runs with funding's. The index
//! does not: it is SOL's fair value from its calibration through the market
//! model v2, at its own pace, so a session's price risk is minutes of SOL,
//! not days.
//!
//! The book moves with the index plus the premium. Against it trade:
//!
//! - **noise** and **informed** takers from the actor library, at the pace
//!   and sizes the tape measured in the session's kind of hours (regular,
//!   off, weekend), the informed share fitted so the average taker's
//!   one-minute markout is the tape's ([`INFORMED_SHARE`]);
//! - an **arbitrageur** that sells a bid over the index by its edge and buys
//!   an ask under it, within a slot or two, as the exchanges lead Phoenix:
//!   the basis doesn't sit still for the bot;
//! - **the pinger**, Phoenix's $3 prints, faster than measured so the
//!   book's mark (the engine's mark: see the task's design record) is fresh
//!   every funding interval.
//!
//! Every trade moves the book: price impact, its reach the tape's.
//!
//! Every number that sets what a bot can earn is measured, or a stated
//! departure from a measurement ([`MeasuredCarryFamily`]'s fields say
//! which).

use mtx_kit::Mandate;
use serde::Serialize;

use super::BOOK_MARK;
use crate::{
    actors::library::{self, Arbitrageur, Informed, Library, Noise, Scale},
    impact::Impact,
    making::{TAKER_ACCOUNTS, TakerOrder, TakerSide},
    market::{Calibration, MarketModel, ModelSpec},
    scenario::{
        Funding, SLOT_SECONDS, Scenario, SplitMix64, VENUE, Waypoint, move_slots, shuffled,
    },
};

/// The tape study the flow numbers come from.
pub const TAPE_STUDY: &str = "phoenix-tape.20261125.fc443f1e";
/// The funding study the premium numbers come from.
pub const FUNDING_STUDY: &str = "phoenix-funding.sol.20261125.248a28ea";
/// The market calibration the index is drawn from.
pub const CALIBRATION: &str = "sol-usd.20261112.0dbc6cdf";
const CALIBRATION_TOML: &str =
    include_str!("../../../../calibration/market/sol-usd.20261112.0dbc6cdf.toml");

/// SOL's mark in the snapshot (`snapshots/phoenix-perps-2026-11-13/manifest.json`,
/// `marks.SOL`): what dollar sizes are turned into SOL at.
pub const SNAPSHOT_MARK_USD: f64 = 79.54;
/// The snapshot's SOL funding cap per interval, in quote lots per base lot
/// (`exchange.json`, `maxFundingRatePerInterval`): 0.24% of the price at
/// the snapshot's mark, the calibration's 0.2273% at its fit.
pub const SNAPSHOT_MAX_RATE: i64 = 1931;
/// Phoenix's funding period over its interval for SOL: 28,800 s over
/// 3,600 s (`/exchange`, in the funding study and the snapshot).
pub const PERIOD_INTERVALS: u64 = 8;
/// Mainnet's funding interval, seconds.
pub const MAINNET_INTERVAL_S: u64 = 3_600;
/// Stated departure: a session's funding interval, seconds (mainnet's is
/// 3,600). A session of six minutes then plays about five days of funding.
pub const FUNDING_INTERVAL_S: (u64, u64) = (3, 4);

/// The informed share of orders, fitted by [`fit_informed`] so that the
/// average taker's one-minute markout is the tape's
/// ([`MARKOUT_60S_BPS`]); a test checks the fit gives it.
pub const INFORMED_SHARE: f64 = 0.12;
/// The share of the time an informed wallet sees a move past its edge, from
/// the same fit: its pace while it trades is the share over this.
pub const INFORMED_ACTIVE: f64 = 0.294;
/// The average non-pinger taker's one-minute markout on SOL, bp: the tape's
/// four buckets weighted by their orders (`markets.SOL.markouts`, 60 s:
/// 1.723 over 1,449 orders of $5-100, 1.29 over 3,322 of $100-1k, 0.752
/// over 2,504 of $1k-10k, 8.221 over 147 of $10k+).
pub const MARKOUT_60S_BPS: f64 = 1.330;
/// The move an informed wallet must see before it trades, bp: Phoenix's
/// 3.5 bp taker fee (`venues.phoenix_perp.taker_fee`) in and out.
pub const INFORMED_EDGE_BPS: f64 = 7.0;
/// The most slots a session lasts: what the fit and the tests may assume.
pub const DURATION_MAX_SLOTS: u64 = 2_000;

/// Slots in a minute of the bench's clock.
const SLOTS_PER_MINUTE: f64 = 60.0 / SLOT_SECONDS;
/// The bench's clock over the actor library's: the library turns a rate a
/// minute into a rate a slot at 0.4 s, so a measured rate is handed to it
/// times this.
pub const LIBRARY_RATE_FACTOR: f64 = SLOT_SECONDS / library::SLOT_SECONDS;

/// The kind of hours a session's flow is measured in (the tape study's
/// sessions, US winter time in UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowSession {
    /// Weekdays 14:30 to 21:00 UTC.
    Regular,
    /// Weekday nights and mornings.
    Off,
    /// Friday 22:00 to Sunday 23:00 UTC.
    Weekend,
}

/// SOL's non-pinger taker flow in one kind of hours (`markets.SOL.flow`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Flow {
    pub session: FlowSession,
    /// The tape's minutes of this kind (`tape.minutes_by_session`): the
    /// odds a session draws it.
    pub minutes: u32,
    pub orders_per_minute: f64,
    /// Order sizes in USD: the median and the log-sd of a lognormal.
    pub usd_median: f64,
    pub usd_log_sd: f64,
}

/// SOL's flow, from the tape study.
pub const FLOWS: [Flow; 3] = [
    Flow {
        session: FlowSession::Regular,
        minutes: 780,
        orders_per_minute: 0.9872,
        usd_median: 511.98,
        usd_log_sd: 2.096,
    },
    Flow {
        session: FlowSession::Off,
        minutes: 2_233,
        orders_per_minute: 1.335,
        usd_median: 534.75,
        usd_log_sd: 1.686,
    },
    Flow {
        session: FlowSession::Weekend,
        minutes: 1_395,
        orders_per_minute: 2.6323,
        usd_median: 509.71,
        usd_log_sd: 1.336,
    },
];

/// The family's parameters, every range inclusive.
#[derive(Debug, Clone)]
pub struct MeasuredCarryFamily {
    pub symbol: String,
    /// The session's length, in slots, drawn first.
    pub duration: (u64, u64),
    /// The funding interval, seconds (stated departure), and the period in
    /// intervals and the cap per interval (the snapshot's).
    pub funding_interval_s: (u64, u64),
    pub period_intervals: u64,
    pub max_rate: i64,
    /// The session's premium level, bp: normal, with the mean and spread of
    /// the funding study's five-day windows' means (`windows[hours=120]`:
    /// 1.434 and 0.665), clamped to a range just outside their 1st and
    /// 99th percentiles (-0.044, 2.805) and the three-day windows' 99th
    /// (3.2).
    pub premium_mean_bps: (f64, f64),
    pub premium_mean_clamp_bps: (f64, f64),
    /// How the premium wanders around its level: an AR(1) in funding
    /// intervals with the five-day windows' spread (0.841 bp) and
    /// half-life (3.64 intervals).
    pub premium_sd_bps: f64,
    pub premium_half_life_intervals: f64,
    /// Slots between the path's waypoints when the last move has finished.
    pub waypoint_slots: u64,
    /// The flow by kind of hours ([`FLOWS`]).
    pub flows: [Flow; 3],
    pub noise_wallets: u32,
    /// The informed wallets: their share of the orders, their pace while
    /// they trade (the share over [`INFORMED_ACTIVE`]), the move they must
    /// see, how far ahead they see (60 s, the tape's longest markout but
    /// one) and how often they're right. Their orders are larger than
    /// noise's (the tape's $10k+ orders marked out at +8.2 bp a minute on,
    /// the rest at about 1.3).
    pub informed_wallets: u32,
    pub informed_share: f64,
    pub informed_intensity: f64,
    pub informed_edge_bps: f64,
    pub informed_horizon_slots: u64,
    pub informed_hit_rate: f64,
    pub informed_size_mult: f64,
    /// How far through the book a taker reaches, bp: the tape's p90 worst
    /// fill of a $10k+ order (`markets.SOL.impact`, 20.32).
    pub taker_limit_bps: f64,
    /// The arbitrageur's latency in slots (luthor measured Binance leading
    /// Phoenix's SOL by about 0.5 s and Coinbase by 0.25 s: one to two
    /// slots), and its edge in bp (the 3.5 bp taker fee and up to 1.5 bp
    /// more), drawn per session.
    pub arbitrageur_latency_slots: (u64, u64),
    pub arbitrageur_edge_bps: (f64, f64),
    pub arbitrageur_size_mult: f64,
    pub arbitrageur_inventory_mult: f64,
    /// Price impact. None of it stays: on the tape, the mid's move after a
    /// $100-1k order (the bulk of the flow) is +1.29 bp its way a minute on
    /// and +0.28 bp five minutes on (95% interval -0.63 to 1.13), against
    /// 1.78 bp paid, so what is left after five minutes can't be told from
    /// nothing, and what the informed know is in the index's path already
    /// (they trade on where it goes). The fading part halves in a minute
    /// (the markouts peak at 60 s and are gone by 300 s; the calibration's
    /// `book.impact` defaults, 0.5 permanent and 30 s, are unmeasured). It
    /// moves the book at most the tape's p90 worst fill of $10k+ (20.32
    /// bp), and reads the book at the tape's median order ($512, in SOL).
    pub impact_permanent_share: f64,
    pub impact_half_life_slots: f64,
    pub impact_max_bps: f64,
    pub impact_probe_sol: f64,
    /// The pinger: slots between its prints (stated departure: about 30
    /// times the tape's 1.17 a minute), its size in SOL ($3.18, under the
    /// study's $5 line) and its reach in bp.
    pub pinger_gap_slots: (u64, u64),
    pub pinger_size_sol: f64,
    pub pinger_limit_bps: f64,
    pub max_position: f64,
    pub max_drawdown_usd: f64,
}

/// What a session was drawn as: its flow's kind and the processes its
/// premium, funding and actors come from. All of it is the latent state:
/// none of it is a draw the session plays out.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeasuredCarryShape {
    pub flow_session: FlowSession,
    pub premium_mean_bps: f64,
    pub premium_sd_bps: f64,
    pub premium_half_life_slots: f64,
    pub funding: Funding,
    pub orders_per_minute: f64,
    pub informed_share: f64,
    pub arbitrageur_latency_slots: u64,
    pub arbitrageur_edge_bps: f64,
}

impl MeasuredCarryShape {
    /// What the informed bot is told: the premium's process (its level,
    /// spread and half-life), funding's settings, the flow's pace and the
    /// arbitrageur's. Never the path, the premium as it wanders, or when
    /// anyone trades.
    pub fn latent(&self) -> serde_json::Value {
        serde_json::json!({
            "premium_mean_bps": self.premium_mean_bps,
            "premium_sd_bps": self.premium_sd_bps,
            "premium_half_life_slots": self.premium_half_life_slots,
            "funding": self.funding,
            "orders_per_minute": self.orders_per_minute,
            "informed_share": self.informed_share,
            "arbitrageur_latency_slots": self.arbitrageur_latency_slots,
            "arbitrageur_edge_bps": self.arbitrageur_edge_bps,
        })
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// SOL's market calibration, with its SHA-256 as a loaded file has it.
pub fn calibration() -> Calibration {
    let mut calibration = Calibration::parse(CALIBRATION_TOML).expect("the SOL calibration parses");
    calibration.sha256 = crate::market::calibration::sha256_hex(CALIBRATION_TOML.as_bytes());
    assert_eq!(calibration.meta.calibration_id, CALIBRATION);
    calibration
}

impl MeasuredCarryFamily {
    /// SOL, measured.
    pub fn sol() -> Self {
        Self {
            symbol: "SOL".into(),
            duration: (1_440, 1_920),
            funding_interval_s: FUNDING_INTERVAL_S,
            period_intervals: PERIOD_INTERVALS,
            max_rate: SNAPSHOT_MAX_RATE,
            premium_mean_bps: (1.434, 0.665),
            premium_mean_clamp_bps: (-0.5, 3.5),
            premium_sd_bps: 0.841,
            premium_half_life_intervals: 3.64,
            waypoint_slots: 4,
            flows: FLOWS,
            noise_wallets: 8,
            informed_wallets: 4,
            informed_share: INFORMED_SHARE,
            informed_intensity: round3(INFORMED_SHARE / INFORMED_ACTIVE),
            informed_edge_bps: INFORMED_EDGE_BPS,
            informed_horizon_slots: 240,
            informed_hit_rate: 0.85,
            informed_size_mult: 1.5,
            taker_limit_bps: 20.32,
            arbitrageur_latency_slots: (1, 2),
            arbitrageur_edge_bps: (3.5, 5.0),
            arbitrageur_size_mult: 1.0,
            arbitrageur_inventory_mult: 40.0,
            impact_permanent_share: 0.0,
            impact_half_life_slots: 60.0 / SLOT_SECONDS,
            impact_max_bps: 20.32,
            impact_probe_sol: round2(511.98 / SNAPSHOT_MARK_USD),
            pinger_gap_slots: (4, 10),
            pinger_size_sol: 0.04,
            pinger_limit_bps: 10.0,
            // ~1x the 10,000 USDC of collateral at the snapshot's $79.54.
            max_position: 125.0,
            max_drawdown_usd: 1_000.0,
        }
    }

    /// The practice family is the hidden family: what the generator draws
    /// is what is graded (the levers that vary are drawn per session).
    pub fn practice() -> Self {
        Self::sol()
    }

    fn model() -> MarketModel {
        MarketModel::new(calibration(), ModelSpec::default()).expect("the SOL model builds")
    }

    /// The flow kind a seed's session draws: its first draw.
    fn flow_of(&self, draw: f64) -> Flow {
        let total: u32 = self.flows.iter().map(|f| f.minutes).sum();
        assert!(total > 0);
        let mut at = draw * f64::from(total);
        for flow in &self.flows {
            at -= f64::from(flow.minutes);
            if at < 0.0 {
                return *flow;
            }
        }
        self.flows[self.flows.len() - 1]
    }

    /// A session's premium level, bp: its second and third draws.
    fn premium_mean(&self, rng: &mut SplitMix64) -> f64 {
        let (mean, sd) = self.premium_mean_bps;
        let (low, high) = self.premium_mean_clamp_bps;
        round3((mean + sd * rng.normal()).clamp(low, high))
    }

    /// The session's premium level, as [`Self::draw`] draws it.
    pub fn session_premium_mean(&self, seed: u64) -> f64 {
        let mut rng = SplitMix64(seed);
        let _ = rng.next_f64();
        self.premium_mean(&mut rng)
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, MeasuredCarryShape) {
        assert!(self.duration.1 <= DURATION_MAX_SLOTS);
        assert!(self.period_intervals >= 1);
        let mut rng = SplitMix64(seed);
        let flow = self.flow_of(rng.next_f64());
        let premium_mean_bps = self.premium_mean(&mut rng);
        let duration = rng.range_u64(self.duration);
        let interval_s = rng.range_u64(self.funding_interval_s);
        let interval_slots = (interval_s as f64 / SLOT_SECONDS).round();
        let premium_half_life_slots = round2(self.premium_half_life_intervals * interval_slots);
        let funding = Funding {
            interval_seconds: interval_s,
            period_seconds: interval_s * self.period_intervals,
            max_rate: self.max_rate,
            mark: Some(BOOK_MARK),
        };
        let arbitrageur_latency_slots = rng.range_u64(self.arbitrageur_latency_slots);
        let arbitrageur_edge_bps = round2(rng.range_f64(self.arbitrageur_edge_bps));

        // The index, from the calibration's model, and the premium's wander
        // on funding's clock.
        let model_seed = rng.next_u64();
        let drawn = Self::model().draw(model_seed, duration);
        let path = self.path(
            &mut rng,
            &drawn,
            duration,
            premium_mean_bps,
            premium_half_life_slots,
        );
        let tape = self.pinger(&mut rng, duration);

        let shape = MeasuredCarryShape {
            flow_session: flow.session,
            premium_mean_bps,
            premium_sd_bps: self.premium_sd_bps,
            premium_half_life_slots,
            funding,
            orders_per_minute: flow.orders_per_minute,
            informed_share: self.informed_share,
            arbitrageur_latency_slots,
            arbitrageur_edge_bps,
        };
        let description = format!(
            "{}: Phoenix funding every {interval_s} s over {} s (mainnet: {MAINNET_INTERVAL_S} s over {} s); premium level {premium_mean_bps} bp ({FUNDING_STUDY}); {:?} flow, {} orders a minute ({TAPE_STUDY}); index from {CALIBRATION}",
            self.symbol,
            funding.period_seconds,
            MAINNET_INTERVAL_S * self.period_intervals,
            flow.session,
            flow.orders_per_minute,
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
            faults: None,
            actors: Some(self.actors(&flow, &shape)),
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
            market_model: Some(drawn.record),
            raw_sends: None,
            markets: Vec::new(),
            joint: None,
            start_positions: Default::default(),
            close_out: Default::default(),
            tx_layer: None,
            stop_out: None,
            liquidator: None,
            impact: Some(Impact {
                permanent_share: self.impact_permanent_share,
                half_life_slots: self.impact_half_life_slots,
                max_bps: self.impact_max_bps,
                probe: self.impact_probe_sol,
                resistance: Vec::new(),
            }),
        };
        (scenario, shape)
    }

    /// The index at the model's levels and the book `premium` above it: a
    /// waypoint every `waypoint_slots` once the last move has finished, the
    /// premium an AR(1) around the session's level on funding's clock.
    fn path(
        &self,
        rng: &mut SplitMix64,
        drawn: &crate::market::model::ModelPath,
        duration: u64,
        premium_mean_bps: f64,
        half_life_slots: f64,
    ) -> Vec<Waypoint> {
        assert!(half_life_slots > 0.0);
        assert!(self.waypoint_slots >= 3);
        let mut path: Vec<Waypoint> = Vec::new();
        let mut wander_bps = self.premium_sd_bps * rng.normal();
        let mut last_level = 0.0;
        let mut last_slot = 0;
        let mut free_from = 0;
        let mut slot = self.waypoint_slots;
        while slot < duration {
            let step = (slot - last_slot) as f64;
            let keep = 0.5f64.powf(step / half_life_slots);
            wander_bps =
                keep * wander_bps + self.premium_sd_bps * (1.0 - keep * keep).sqrt() * rng.normal();
            last_slot = slot;
            if slot >= free_from {
                let level = round3(drawn.level_pct(slot));
                let premium = round3((premium_mean_bps + wander_bps) / 100.0);
                let done = slot + move_slots(last_level, level) + 1;
                if done > duration {
                    break;
                }
                let same = path
                    .last()
                    .is_some_and(|w| w.level_pct == level && w.premium_pct == premium);
                if !same {
                    path.push(Waypoint {
                        slot,
                        level_pct: level,
                        premium_pct: premium,
                    });
                    free_from = done;
                    last_level = level;
                }
            }
            slot += self.waypoint_slots;
        }
        assert!(!path.is_empty(), "a session of {duration} slots moves");
        path
    }

    /// The pinger's prints, either side at random, from the shared pool.
    fn pinger(&self, rng: &mut SplitMix64, duration: u64) -> Vec<TakerOrder> {
        let mut orders = Vec::new();
        let mut slot = rng.range_u64(self.pinger_gap_slots);
        while slot < duration {
            let side = if rng.next_f64() < 0.5 {
                TakerSide::Buy
            } else {
                TakerSide::Sell
            };
            orders.push(TakerOrder {
                slot,
                side,
                size: self.pinger_size_sol,
                limit_bps: self.pinger_limit_bps,
                account: (rng.next_u64() % TAKER_ACCOUNTS as u64) as u8,
            });
            slot += rng.range_u64(self.pinger_gap_slots);
        }
        orders
    }

    /// The library's cast for a session of `flow`.
    pub fn actors(&self, flow: &Flow, shape: &MeasuredCarryShape) -> Library {
        let rate = flow.orders_per_minute * LIBRARY_RATE_FACTOR;
        Library {
            scale: Scale {
                trades_per_minute: round3(rate),
                trade_size: round3(flow.usd_median / SNAPSHOT_MARK_USD),
                trade_size_sigma: flow.usd_log_sd,
            },
            noise: vec![Noise {
                wallets: self.noise_wallets,
                flow_mult: round3(1.0 - self.informed_share),
                size_mult: 1.0,
                limit_bps: self.taker_limit_bps,
                active: None,
            }],
            informed: vec![Informed {
                wallets: self.informed_wallets,
                horizon_slots: self.informed_horizon_slots,
                edge_bps: self.informed_edge_bps,
                hit_rate: self.informed_hit_rate,
                view_noise_bps: 0.0,
                intensity_mult: self.informed_intensity,
                size_mult: self.informed_size_mult,
                limit_bps: self.taker_limit_bps,
                active: None,
            }],
            arbitrageur: Some(Arbitrageur {
                latency_slots: shape.arbitrageur_latency_slots,
                edge_bps: shape.arbitrageur_edge_bps,
                size_mult: self.arbitrageur_size_mult,
                max_inventory_mult: self.arbitrageur_inventory_mult,
                // Before the bot's turn, as the carry family was measured.
                after_turn: false,
            }),
            ..Library::default()
        }
    }

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// `count` sessions named `<prefix>-<n>`, stratified by the premium's
    /// level: half below the funding study's mean, half at or above it, so
    /// a set isn't tilted toward rich or thin carry by the luck of the draw,
    /// in a shuffled order.
    pub fn draw_set(
        &self,
        prefix: &str,
        count: usize,
        seed: u64,
    ) -> Vec<(Scenario, MeasuredCarryShape)> {
        let split = self.premium_mean_bps.0;
        let mut quotas = [count / 2, count - count / 2];
        let mut drawn = Vec::with_capacity(count);
        let mut draw = 0u64;
        while drawn.len() < count {
            draw += 1;
            assert!(draw < 1_000 * (count as u64 + 1), "the strata fill");
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let cell = usize::from(self.session_premium_mean(draw_seed) >= split);
            if quotas[cell] == 0 {
                continue;
            }
            quotas[cell] -= 1;
            drawn.push(draw_seed);
        }
        shuffled(drawn, seed)
            .into_iter()
            .enumerate()
            .map(|(i, draw_seed)| self.draw(&format!("{prefix}-{}", i + 1), draw_seed))
            .collect()
    }
}

/// The calm sessions the informed share is fitted on, and their seeds.
const FIT_SESSIONS: u64 = 200;
const FIT_SEED: u64 = 0x6361_7272_7966_6974;

/// The informed share of orders that gives the tape's average one-minute
/// markout, and the share of the time an informed wallet trades. On the
/// model's paths an informed order's markout is `(2 h - 1)` times the move
/// it saw ahead, and it trades only while that move is past its edge;
/// noise's markout is 0. Clamped to [0.05, 0.9] so neither kind vanishes.
pub fn fit_informed(family: &MeasuredCarryFamily) -> (f64, f64) {
    let model = MeasuredCarryFamily::model();
    let (mut seen_bps, mut active, mut slots) = (0.0, 0u64, 0u64);
    for n in 0..FIT_SESSIONS {
        let duration = family.duration.1;
        let drawn = model.draw(FIT_SEED + n, duration);
        let horizon = family.informed_horizon_slots;
        for slot in (0..duration.saturating_sub(horizon)).step_by(2) {
            let now = drawn.level_pct(slot);
            let ahead = drawn.level_pct(slot + horizon);
            let ahead_bps = ((1.0 + ahead / 100.0) / (1.0 + now / 100.0) - 1.0).abs() * 10_000.0;
            slots += 1;
            if ahead_bps > family.informed_edge_bps {
                active += 1;
                seen_bps += ahead_bps;
            }
        }
    }
    assert!(
        active > 0 && slots > 0,
        "an informed wallet sees a move past its edge"
    );
    let markout = (2.0 * family.informed_hit_rate - 1.0) * seen_bps / active as f64;
    let share = (MARKOUT_60S_BPS / markout).clamp(0.05, 0.9);
    (share, active as f64 / slots as f64)
}

/// Orders a session minute the library sends, all kinds.
pub fn library_orders_per_minute(library: &Library) -> f64 {
    library.scale.per_slot() * SLOTS_PER_MINUTE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_valid_and_deterministic() {
        let family = MeasuredCarryFamily::sol();
        for seed in 0..60 {
            let (scenario, shape) = family.draw("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}");
            assert_eq!(scenario, family.sample("s", seed));
            assert!(!scenario.tape.is_empty());
            let actors = scenario.actors.as_ref().unwrap();
            actors.validate().unwrap();
            assert_eq!(scenario.symbol, "SOL");
            assert!(scenario.impact.is_some());
            assert!(scenario.market_model.is_some());
            assert_eq!(scenario.funding, Some(shape.funding));
        }
    }

    /// The funding study's numbers, as the family holds them.
    #[test]
    fn the_premium_and_funding_are_the_studies() {
        let family = MeasuredCarryFamily::sol();
        assert_eq!(family.premium_mean_bps, (1.434, 0.665));
        assert_eq!(family.premium_sd_bps, 0.841);
        assert_eq!(family.premium_half_life_intervals, 3.64);
        assert_eq!(family.max_rate, 1931);
        assert_eq!(family.period_intervals, 8);
        let study: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../calibration/funding/phoenix-funding.sol.20261125.248a28ea.json"
        ))
        .unwrap();
        assert_eq!(study["id"], FUNDING_STUDY);
        assert_eq!(study["market"]["period_seconds"], 28_800);
        assert_eq!(study["market"]["interval_seconds"], 3_600);
        let five_days = study["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["hours"] == 120)
            .unwrap();
        assert_eq!(five_days["mean_bps"]["mean"], 1.434);
        assert_eq!(five_days["mean_bps"]["sd"], 0.665);
        assert_eq!(five_days["within_sd_bps"], 0.841);
        assert_eq!(five_days["within_half_life_intervals"], 3.64);
        assert_eq!(study["all"]["cap"]["at_cap_share"], 0.0);
        for seed in 0..40 {
            let (scenario, shape) = family.draw("s", seed);
            let funding = scenario.funding.unwrap();
            assert_eq!(funding.period_seconds, 8 * funding.interval_seconds);
            assert!((3..=4).contains(&funding.interval_seconds));
            // A session plays 90 to 160 intervals: four to seven days.
            let intervals =
                scenario.duration_slots as f64 * SLOT_SECONDS / funding.interval_seconds as f64;
            assert!((90.0..=160.0).contains(&intervals), "{intervals}");
            assert!(shape.premium_mean_bps >= -0.5 && shape.premium_mean_bps <= 3.5);
        }
    }

    /// Across sessions the premium's level has the study's spread, and
    /// within one the book wanders around it with the study's spread.
    #[test]
    fn the_premium_has_the_study_s_level_and_wander() {
        let family = MeasuredCarryFamily::sol();
        let (mut levels, mut wander) = (Vec::new(), Vec::new());
        for seed in 0..300 {
            let (scenario, shape) = family.draw("s", seed);
            levels.push(shape.premium_mean_bps);
            for w in &scenario.path {
                wander.push(w.premium_pct * 100.0 - shape.premium_mean_bps);
            }
        }
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        let sd = |v: &[f64]| {
            let m = mean(v);
            (v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / v.len() as f64).sqrt()
        };
        assert!((mean(&levels) - 1.434).abs() < 0.1, "{}", mean(&levels));
        assert!((sd(&levels) - 0.665).abs() < 0.1, "{}", sd(&levels));
        assert!(mean(&wander).abs() < 0.1, "{}", mean(&wander));
        assert!((sd(&wander) - 0.841).abs() < 0.08, "{}", sd(&wander));
        // Longs pay shorts in most sessions, as in 92% of the hours.
        let positive = levels.iter().filter(|l| **l > 0.0).count();
        assert!(positive as f64 > 0.95 * levels.len() as f64);
    }

    /// The premium halves its distance to the level in the study's 3.64
    /// intervals, on the session's clock.
    #[test]
    fn the_premium_s_half_life_is_the_study_s_on_funding_s_clock() {
        let family = MeasuredCarryFamily::sol();
        let (mut num, mut den) = (0.0, 0.0);
        let mut lag_slots = Vec::new();
        for seed in 0..200 {
            let (scenario, shape) = family.draw("s", seed);
            let interval_slots = shape.funding.interval_seconds as f64 / SLOT_SECONDS;
            assert_eq!(shape.premium_half_life_slots, round2(3.64 * interval_slots));
            let x: Vec<f64> = scenario
                .path
                .iter()
                .map(|w| w.premium_pct * 100.0 - shape.premium_mean_bps)
                .collect();
            for pair in scenario.path.windows(2) {
                lag_slots
                    .push((pair[1].slot - pair[0].slot) as f64 / shape.premium_half_life_slots);
            }
            for i in 1..x.len() {
                num += x[i] * x[i - 1];
                den += x[i - 1] * x[i - 1];
            }
        }
        // One waypoint step's autocorrelation, against the half-life's.
        let step = lag_slots.iter().sum::<f64>() / lag_slots.len() as f64;
        let want = 0.5f64.powf(step);
        let got = num / den;
        assert!((got - want).abs() < 0.05, "{got} against {want}");
    }

    /// The flow is the tape's, by kind of hours, at the library's clock.
    #[test]
    fn the_flow_is_the_tape_s_by_kind_of_hours() {
        let family = MeasuredCarryFamily::sol();
        let mut kinds = [0usize; 3];
        for seed in 0..600 {
            let (scenario, shape) = family.draw("s", seed);
            let actors = scenario.actors.as_ref().unwrap();
            let flow = FLOWS
                .iter()
                .find(|f| f.session == shape.flow_session)
                .unwrap();
            let per_minute = library_orders_per_minute(actors);
            assert!(
                (per_minute - flow.orders_per_minute).abs() < 0.01,
                "{per_minute}"
            );
            assert!((actors.scale.trade_size - flow.usd_median / SNAPSHOT_MARK_USD).abs() < 1e-3);
            assert_eq!(actors.scale.trade_size_sigma, flow.usd_log_sd);
            let noise = actors.noise[0].flow_mult;
            assert!((noise + INFORMED_SHARE - 1.0).abs() < 1e-9);
            let arb = actors.arbitrageur.as_ref().unwrap();
            assert!((1..=2).contains(&arb.latency_slots));
            assert!((3.5..=5.0).contains(&arb.edge_bps));
            kinds[match shape.flow_session {
                FlowSession::Regular => 0,
                FlowSession::Off => 1,
                FlowSession::Weekend => 2,
            }] += 1;
        }
        // In proportion to the tape's minutes: 780, 2,233 and 1,395 of 4,408.
        let share = |k: usize| kinds[k] as f64 / 600.0;
        assert!((share(0) - 0.177).abs() < 0.05, "{kinds:?}");
        assert!((share(1) - 0.507).abs() < 0.05, "{kinds:?}");
        assert!((share(2) - 0.316).abs() < 0.05, "{kinds:?}");
    }

    /// The tape study's flow, markouts and impact, as the family holds them.
    #[test]
    fn the_numbers_are_the_tape_study_s() {
        let study: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../calibration/tape/phoenix-tape.20261125.fc443f1e.json"
        ))
        .unwrap();
        assert_eq!(study["id"], TAPE_STUDY);
        let sol = &study["markets"]["SOL"];
        for (flow, kind) in FLOWS.iter().zip(["regular", "off", "weekend"]) {
            let measured = &sol["flow"][kind];
            assert_eq!(measured["orders_per_minute"], flow.orders_per_minute);
            assert_eq!(measured["usd_median"], flow.usd_median);
            assert_eq!(measured["log_sd"], flow.usd_log_sd);
            assert_eq!(study["tape"]["minutes_by_session"][kind], flow.minutes);
        }
        // The average taker's one-minute markout, weighted by orders.
        let (mut sum, mut n) = (0.0, 0.0);
        for bucket in ["5-100", "100-1000", "1000-10000", "10000-inf"] {
            let m = &sol["markouts"][bucket]["markout_bps"]["60s"];
            sum += m["mean"].as_f64().unwrap() * m["n"].as_f64().unwrap();
            n += m["n"].as_f64().unwrap();
        }
        assert!((sum / n - MARKOUT_60S_BPS).abs() < 0.001, "{}", sum / n);
        assert_eq!(sol["impact"]["10000-inf"]["p90_bps"], 20.32);
        // What is left of a $100-1k order's move five minutes on can't be
        // told from nothing: the impact fades whole.
        let small = &sol["markouts"]["100-1000"]["markout_bps"];
        assert_eq!(small["300s"]["mean"], 0.275);
        assert!(small["300s"]["low"].as_f64().unwrap() < 0.0);
        assert!(small["60s"]["mean"].as_f64().unwrap() > small["300s"]["mean"].as_f64().unwrap());
        let family = MeasuredCarryFamily::sol();
        assert_eq!(family.taker_limit_bps, 20.32);
        assert_eq!(family.impact_max_bps, 20.32);
        assert_eq!(family.impact_permanent_share, 0.0);
        assert_eq!(family.impact_half_life_slots, 240.0);
        // The pinger: under $5 a print, and about 30 times its measured pace
        // (5,134 prints in the tape's 4,408 minutes).
        assert!(family.pinger_size_sol * SNAPSHOT_MARK_USD < 5.0);
        let measured = sol["pinger_orders"].as_f64().unwrap() / 4_408.0;
        let gap = (family.pinger_gap_slots.0 + family.pinger_gap_slots.1) as f64 / 2.0;
        let ours = SLOTS_PER_MINUTE / gap;
        assert!(
            (ours / measured - 29.0).abs() < 3.0,
            "{} times",
            ours / measured
        );
    }

    /// [`INFORMED_SHARE`] and [`INFORMED_ACTIVE`] are what the fit gives.
    #[test]
    fn the_informed_share_is_the_fit() {
        let (share, active) = fit_informed(&MeasuredCarryFamily::sol());
        eprintln!("informed share {share:.4}, active {active:.4}");
        assert!((share - INFORMED_SHARE).abs() < 0.01, "{share}");
        assert!((active - INFORMED_ACTIVE).abs() < 0.01, "{active}");
    }

    #[test]
    fn sets_are_stratified_by_the_premium_s_level() {
        let family = MeasuredCarryFamily::sol();
        let set = family.draw_set("h", 12, 9);
        let low = set
            .iter()
            .filter(|(_, s)| s.premium_mean_bps < 1.434)
            .count();
        assert_eq!(low, 6);
        let again = family.draw_set("h", 12, 9);
        assert_eq!(set, again);
    }

    /// The latent state is process facts only: no path, no premium as it
    /// wanders, no order.
    #[test]
    fn the_latent_state_is_the_process() {
        let (_, shape) = MeasuredCarryFamily::sol().draw("s", 3);
        let latent = shape.latent();
        let text = latent.to_string();
        for draw in ["path", "slot\"", "waypoint", "tape", "level_pct"] {
            assert!(!text.contains(draw), "{draw} in {text}");
        }
        assert!(latent["premium_mean_bps"].is_number());
        assert!(latent["funding"]["interval_seconds"].is_number());
    }
}
