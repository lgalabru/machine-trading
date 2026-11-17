//! The strategy zoo: generic strong strategies that are told nothing.
//!
//! A task is only as hard as its best shortcut. Each rung here is a
//! strategy a trader would try first on a task he doesn't understand yet,
//! reading only what any bot sees (the oracle, the book, the takes on the
//! chain) and adapting to the task through its mandate and order
//! ([`desk::Desk`]): the side and size of the job, the position range, the
//! price limits, a broker's haircut. The ladder (`bench/checks/ladder`)
//! grades every rung on the task's sets, and `mtx gates` fails a task where
//! one comes near the informed bot or matches the reference (README.md).
//!
//! Rungs (`MTX_ZOO`). With a job (an order to buy or sell a quantity, or a
//! one-sided mandate's starting position to unwind) each works the job and
//! finishes what is left in the last 3% of the session (only at a price
//! better than the broker's where a broker takes the remainder). Without a
//! job the price and flow rungs take `MTX_ZOO_SIZE` (0.5) of the cap the
//! way their signal points and hold it `MTX_ZOO_HOLD` (300) slots past the
//! last signal, and the passive rungs quote both allowed sides.
//!
//! - Pacing, a job only: `twap` (evenly, crossing), `random-twap` (the same
//!   schedule, caught up at seeded random steps), `dump` (all at once),
//!   `late` (nothing until 80% of the session, then evenly), `front-load`
//!   (a schedule that falls with the square of the time left), `wait-close`
//!   (everything in the last 3%).
//! - Passive: `rest-touch` (rests at the touch on its side), `odd-lot`
//!   (rests clips of `MTX_ZOO_CLIP`, 0.95 base units, at the touch: under a
//!   maker's reaction size), `inside-maker` (one tick inside the touch),
//!   `rest-mid` (at the mid).
//! - Patience: `good-price` (takes only when the touch, after fees, beats
//!   the oracle, or the broker's price where there is one; without a job it
//!   buys under the oracle and sells over it).
//! - Price breaks: `range-break` (the oracle `MTX_ZOO_BREAK_BPS`, 25 bp,
//!   from where the session started), `momentum` (a move over
//!   `MTX_ZOO_LOOK`, 120 slots, of two standard deviations of the path's own
//!   noise), `fade` (against it).
//! - Flow: `follow-flow` and `fade-flow` (the takes' net size over the last
//!   60 slots, once it is three median takes), `most-active` and
//!   `biggest-trader` (the net of the wallet with the most takes, or the
//!   most volume), `copy-everyone` (every take, at its size), `size-follow`
//!   and `size-fade` (a take of three median takes or more: the size tell).
//!
//! With a job a signal is read as where the price goes next: a rung that
//! expects the price to move against the job (up for a buyer) works the rest
//! at once; otherwise it waits as `late` does. `copy-everyone` trades along
//! with every take on the job's side, at its size.
//!
//! [`hybrid`] runs a task's informed bot with a zoo rung in the steps it
//! leaves idle: the ladder's anchor-strength check.

pub mod desk;
pub mod hybrid;
pub mod signals;

use anyhow::{Context, Result, bail};
use desk::{Desk, Goal};
use mtx_kit::{BotResult, Market, Orders, Session, Strategy, log};
use mtx_probes::draw;
use signals::{Tape, Trail};

/// The share of the session by which a schedule is done, and the end phase
/// starts: what is left is finished then.
const END: f64 = 0.97;
/// Where `late` starts.
const LATE: f64 = 0.8;
/// Taker fee a good price must clear, in basis points (Phoenix's is 3.5).
const FEE_BPS: f64 = 5.0;
/// `random-twap`: the chance a step catches up with the schedule.
const CATCH_UP: f64 = 0.25;
/// `good-price`: the most of the job one step takes.
const GOOD_SHARE: f64 = 0.1;
/// Flow: the window of the net, in slots; the takes seen before a median
/// counts; the multiple of the median that makes a signal or a big take.
const FLOW_SLOTS: u64 = 60;
const MIN_TAKES: usize = 10;
const SIZE_TELL: f64 = 3.0;
/// The takes a wallet needs before it can lead.
const MIN_WALLET_TAKES: u64 = 3;
/// Momentum: standard deviations, and the steps of noise seen first.
const MOMENTUM_Z: f64 = 2.0;
const MIN_STEPS: usize = 30;
/// A free passive rung's quote: this share of its size.
const QUOTE_SHARE: f64 = 0.25;

const TAG_RANDOM: u64 = 0x2007;

/// A rung of the zoo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Twap,
    RandomTwap,
    Dump,
    Late,
    FrontLoad,
    WaitClose,
    RestTouch,
    OddLot,
    InsideMaker,
    RestMid,
    GoodPrice,
    RangeBreak,
    Momentum,
    Fade,
    FollowFlow,
    FadeFlow,
    MostActive,
    BiggestTrader,
    CopyEveryone,
    SizeFollow,
    SizeFade,
}

impl Kind {
    /// Every rung, in the order the ladder lists them.
    pub const ALL: [Kind; 21] = [
        Self::Twap,
        Self::RandomTwap,
        Self::Dump,
        Self::Late,
        Self::FrontLoad,
        Self::WaitClose,
        Self::RestTouch,
        Self::OddLot,
        Self::InsideMaker,
        Self::RestMid,
        Self::GoodPrice,
        Self::RangeBreak,
        Self::Momentum,
        Self::Fade,
        Self::FollowFlow,
        Self::FadeFlow,
        Self::MostActive,
        Self::BiggestTrader,
        Self::CopyEveryone,
        Self::SizeFollow,
        Self::SizeFade,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Twap => "twap",
            Self::RandomTwap => "random-twap",
            Self::Dump => "dump",
            Self::Late => "late",
            Self::FrontLoad => "front-load",
            Self::WaitClose => "wait-close",
            Self::RestTouch => "rest-touch",
            Self::OddLot => "odd-lot",
            Self::InsideMaker => "inside-maker",
            Self::RestMid => "rest-mid",
            Self::GoodPrice => "good-price",
            Self::RangeBreak => "range-break",
            Self::Momentum => "momentum",
            Self::Fade => "fade",
            Self::FollowFlow => "follow-flow",
            Self::FadeFlow => "fade-flow",
            Self::MostActive => "most-active",
            Self::BiggestTrader => "biggest-trader",
            Self::CopyEveryone => "copy-everyone",
            Self::SizeFollow => "size-follow",
            Self::SizeFade => "size-fade",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    /// Pacing rungs: they only work a job.
    pub fn needs_job(self) -> bool {
        matches!(
            self,
            Self::Twap
                | Self::RandomTwap
                | Self::Dump
                | Self::Late
                | Self::FrontLoad
                | Self::WaitClose
        )
    }

    fn passive(self) -> bool {
        matches!(
            self,
            Self::RestTouch | Self::OddLot | Self::InsideMaker | Self::RestMid
        )
    }

    fn reads_tape(self) -> bool {
        matches!(
            self,
            Self::FollowFlow
                | Self::FadeFlow
                | Self::MostActive
                | Self::BiggestTrader
                | Self::CopyEveryone
                | Self::SizeFollow
                | Self::SizeFade
        )
    }
}

/// A rung's settings, from `MTX_ZOO_*`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub kind: Kind,
    /// A free rung's position, as a share of the cap.
    pub size: f64,
    /// `odd-lot`'s clip, base units.
    pub clip: f64,
    pub break_bps: f64,
    pub look: u64,
    pub hold: u64,
    pub salt: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::parse(|name| std::env::var(name).ok())
    }

    /// The rung `get` describes, `get` naming a variable's value if set.
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let name = get("MTX_ZOO").unwrap_or_default();
        let kind = Kind::parse(&name).with_context(|| format!("MTX_ZOO: unknown rung {name:?}"))?;
        Self::parse_kind(kind, get)
    }

    fn parse_kind(kind: Kind, get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let number = |name: &str, default: f64| -> Result<f64> {
            match get(name) {
                None => Ok(default),
                Some(v) => v
                    .parse::<f64>()
                    .ok()
                    .filter(|x| x.is_finite() && *x >= 0.0)
                    .with_context(|| format!("{name}: not a number at least 0: {v:?}")),
            }
        };
        let config = Self {
            kind,
            size: number("MTX_ZOO_SIZE", 0.5)?,
            clip: number("MTX_ZOO_CLIP", 0.95)?,
            break_bps: number("MTX_ZOO_BREAK_BPS", 25.0)?,
            look: number("MTX_ZOO_LOOK", 120.0)? as u64,
            hold: number("MTX_ZOO_HOLD", 300.0)? as u64,
            salt: number("MTX_ZOO_SALT", 0.0)? as u64,
        };
        if config.size == 0.0 || config.size > 1.0 {
            bail!("MTX_ZOO_SIZE: a share of the cap in (0, 1]");
        }
        if config.clip == 0.0 || config.look == 0 || config.break_bps == 0.0 {
            bail!("MTX_ZOO_CLIP, MTX_ZOO_LOOK and MTX_ZOO_BREAK_BPS: more than 0");
        }
        Ok(config)
    }
}

/// What the rung has resting, as it last sent it: bid and ask prices and
/// sizes.
type Resting = (Option<(f64, f64)>, Option<(f64, f64)>);

/// One zoo rung.
pub struct Zoo {
    config: Config,
    desk: Desk,
    trail: Trail,
    tape: Tape,
    seed: u64,
    /// A free rung's signal and the slot it holds until.
    latched: Option<(f64, u64)>,
    /// A job rung that has been told to finish now.
    urgent: bool,
    resting: Option<Resting>,
    /// This step's new takes: their net, and the side of the last big one.
    fresh_net: f64,
    fresh_big: Option<f64>,
    idle_said: bool,
}

impl Zoo {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            desk: Desk::default(),
            trail: Trail::default(),
            tape: Tape::default(),
            seed: 0,
            latched: None,
            urgent: false,
            resting: None,
            fresh_net: 0.0,
            fresh_big: None,
            idle_said: false,
        }
    }

    pub fn kind(&self) -> Kind {
        self.config.kind
    }

    /// Where the rung expects the price to go: 1 up, -1 down, None no view.
    fn signal(&self, market: &Market) -> Option<f64> {
        let c = &self.config;
        let since = |slots: u64| market.slot.saturating_sub(slots);
        let flow = || -> Option<f64> {
            let median = self.tape.median_size(MIN_TAKES)?;
            let net = self.tape.net(since(FLOW_SLOTS), None);
            (net.abs() >= SIZE_TELL * median).then_some(net.signum())
        };
        let wallet = |by_volume: bool| -> Option<f64> {
            let w = self.tape.leader(by_volume, MIN_WALLET_TAKES)?;
            let net = self.tape.net(since(c.hold), Some(w));
            (net != 0.0).then_some(net.signum())
        };
        match c.kind {
            Kind::RangeBreak => {
                let start = if self.desk.start_oracle > 0.0 {
                    self.desk.start_oracle
                } else {
                    market.oracle
                };
                let r = market.oracle / start - 1.0;
                (r.abs() * 1e4 >= c.break_bps).then_some(r.signum())
            }
            Kind::Momentum => self.trail.moved(c.look, MOMENTUM_Z, MIN_STEPS),
            Kind::Fade => self.trail.moved(c.look, MOMENTUM_Z, MIN_STEPS).map(|s| -s),
            Kind::FollowFlow => flow(),
            Kind::FadeFlow => flow().map(|s| -s),
            Kind::MostActive => wallet(false),
            Kind::BiggestTrader => wallet(true),
            Kind::SizeFollow => self.fresh_big,
            Kind::SizeFade => self.fresh_big.map(|s| -s),
            _ => None,
        }
    }

    fn read_tape(&mut self, market: &Market) {
        let median = self.tape.median_size(MIN_TAKES);
        let fresh = self.tape.read(market);
        self.fresh_net = fresh.iter().map(|t| t.signed).sum();
        self.fresh_big = median.and_then(|m| {
            fresh
                .iter()
                .rev()
                .find(|t| t.signed.abs() >= SIZE_TELL * m)
                .map(|t| t.signed.signum())
        });
    }

    /// Rest `size` (signed) at `price` unless that is what already rests.
    fn rest_once(&mut self, market: &Market, orders: &mut Orders, quotes: Resting) {
        let resting = market.resting_bids + market.resting_asks > 0.0;
        if resting && self.resting == Some(quotes) {
            return;
        }
        orders.cancel_all();
        if let Some((price, size)) = quotes.0 {
            self.desk.rest(market, orders, size, price);
        }
        if let Some((price, size)) = quotes.1 {
            self.desk.rest(market, orders, -size, price);
        }
        self.resting = Some(quotes);
    }

    fn unrest(&mut self, market: &Market, orders: &mut Orders) {
        if self.resting.take().is_some() || market.resting_bids + market.resting_asks > 0.0 {
            orders.cancel_all();
        }
    }

    /// The passive price on `side` (1 a bid, -1 an ask), by the rung's rule.
    fn passive_price(&self, market: &Market, side: f64) -> Option<f64> {
        let (bid, ask) = (market.best_bid?, market.best_ask?);
        let tick = self.desk.tick;
        Some(match self.config.kind {
            Kind::InsideMaker if side > 0.0 => {
                if bid + tick < ask - 1e-9 {
                    bid + tick
                } else {
                    bid
                }
            }
            Kind::InsideMaker => {
                if ask - tick > bid + 1e-9 {
                    ask - tick
                } else {
                    ask
                }
            }
            Kind::RestMid => (bid + ask) / 2.0,
            _ if side > 0.0 => bid,
            _ => ask,
        })
    }

    /// Trade toward a schedule that leaves `left` (unsigned) of the job.
    fn keep_to(&self, market: &Market, orders: &mut Orders, remaining: f64, left: f64) {
        let behind = remaining.abs() - left.max(0.0);
        if behind >= self.desk.lot {
            self.desk.take(market, orders, behind.copysign(remaining));
        }
    }

    /// What `late` leaves of the job at progress `p`.
    fn late_left(&self, p: f64) -> f64 {
        self.desk.total * (1.0 - ((p - LATE) / (END - LATE)).clamp(0.0, 1.0))
    }

    /// Finish the job: at any price, or only above the broker's where a
    /// broker takes what is left.
    fn finish(&self, market: &Market, orders: &mut Orders, remaining: f64, dir: f64) {
        let good = match self.desk.broker_price(market.oracle, dir) {
            None => true,
            Some(broker) => beats(market, dir, broker),
        };
        if good {
            self.desk.take(market, orders, remaining);
        }
    }

    fn work_job(&mut self, market: &Market, orders: &mut Orders, dir: f64, signal: Option<f64>) {
        let remaining = self.desk.remaining(market.position.size);
        if remaining.abs() < self.desk.lot {
            self.unrest(market, orders);
            return;
        }
        let p = self.desk.progress(market.slot);
        let total = self.desk.total;
        if p >= END || market.slots_left <= 8 {
            self.unrest(market, orders);
            self.finish(market, orders, remaining, dir);
            return;
        }
        let schedule = 1.0 - (p / END).min(1.0);
        let kind = self.config.kind;
        match kind {
            Kind::Twap => self.keep_to(market, orders, remaining, total * schedule),
            Kind::RandomTwap => {
                if draw(self.seed, self.config.salt, TAG_RANDOM, market.k) < CATCH_UP {
                    self.keep_to(market, orders, remaining, total * schedule);
                }
            }
            Kind::Dump => self.desk.take(market, orders, remaining),
            Kind::Late => self.keep_to(market, orders, remaining, self.late_left(p)),
            Kind::FrontLoad => self.keep_to(market, orders, remaining, total * schedule.powi(2)),
            Kind::WaitClose => {}
            _ if kind.passive() => {
                let Some(price) = self.passive_price(market, dir) else {
                    return;
                };
                let size = if kind == Kind::OddLot {
                    self.config.clip.min(remaining.abs())
                } else {
                    remaining.abs()
                };
                let quote = Some((price, size));
                let quotes = if dir > 0.0 {
                    (quote, None)
                } else {
                    (None, quote)
                };
                self.rest_once(market, orders, quotes);
            }
            Kind::GoodPrice => {
                let fair = self
                    .desk
                    .broker_price(market.oracle, dir)
                    .unwrap_or(market.oracle);
                if beats(market, dir, fair) {
                    let step = (total * GOOD_SHARE).max(self.desk.lot);
                    self.desk.take(
                        market,
                        orders,
                        remaining.signum() * step.min(remaining.abs()),
                    );
                }
            }
            Kind::CopyEveryone => {
                if self.fresh_net * dir > 0.0 {
                    let size = self.fresh_net.abs().min(remaining.abs());
                    self.desk.take(market, orders, size.copysign(remaining));
                } else {
                    self.keep_to(market, orders, remaining, self.late_left(p));
                }
            }
            _ => {
                if signal.is_some_and(|s| s * dir > 0.0) {
                    self.urgent = true;
                }
                if self.urgent {
                    self.desk.take(market, orders, remaining);
                } else {
                    self.keep_to(market, orders, remaining, self.late_left(p));
                }
            }
        }
    }

    fn trade_free(&mut self, market: &Market, orders: &mut Orders, signal: Option<f64>) {
        let kind = self.config.kind;
        let size = self.config.size * market.cap.abs();
        let p = self.desk.progress(market.slot);
        if kind.needs_job() {
            if !self.idle_said {
                log(&format!(
                    "zoo {}: the session hands no job, so this rung trades nothing",
                    kind.name()
                ));
                self.idle_said = true;
            }
            return;
        }
        if kind.passive() {
            if p >= END || market.slots_left <= 8 {
                self.unrest(market, orders);
                self.desk.target(market, orders, 0.0);
                return;
            }
            let q = if kind == Kind::OddLot {
                self.config.clip
            } else {
                size * QUOTE_SHARE
            };
            let held = market.position.size;
            let bid = (held + q <= size + 1e-9)
                .then(|| self.passive_price(market, 1.0))
                .flatten()
                .map(|p| (p, q));
            let ask = (held - q >= -size - 1e-9)
                .then(|| self.passive_price(market, -1.0))
                .flatten()
                .map(|p| (p, q));
            self.rest_once(market, orders, (bid, ask));
            return;
        }
        let target = match kind {
            Kind::GoodPrice => {
                let fee = FEE_BPS / 1e4;
                if market
                    .best_ask
                    .is_some_and(|a| a < market.oracle * (1.0 - fee))
                {
                    size
                } else if market
                    .best_bid
                    .is_some_and(|b| b > market.oracle * (1.0 + fee))
                {
                    -size
                } else {
                    market.position.size
                }
            }
            Kind::CopyEveryone => {
                let net = self
                    .tape
                    .net(market.slot.saturating_sub(self.config.hold), None);
                net.clamp(-size, size)
            }
            _ => {
                if let Some(s) = signal {
                    self.latched = Some((s, market.slot + self.config.hold));
                }
                match self.latched {
                    Some((s, until)) if market.slot <= until => s * size,
                    _ => 0.0,
                }
            }
        };
        self.desk.target(market, orders, target);
    }
}

/// Whether the touch on the job's side beats `price` after the fee: a bid
/// over it for a seller (`dir` -1), an ask under it for a buyer.
fn beats(market: &Market, dir: f64, price: f64) -> bool {
    let fee = FEE_BPS / 1e4;
    if dir > 0.0 {
        market.best_ask.is_some_and(|a| a * (1.0 + fee) < price)
    } else {
        market.best_bid.is_some_and(|b| b * (1.0 - fee) > price)
    }
}

impl Strategy for Zoo {
    fn start(&mut self, session: &Session) -> BotResult {
        self.desk = Desk::of(session);
        self.seed = session.seed;
        if self.config.kind.reads_tape() {
            self.tape = Tape::of(session);
        }
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.desk.begin(market.position.size);
        self.trail.push(market.slot, market.oracle);
        if self.config.kind.reads_tape() {
            self.read_tape(market);
        }
        let signal = self.signal(market);
        match self.desk.goal {
            Goal::Execute { dir, .. } => self.work_job(market, orders, dir, signal),
            Goal::Free => self.trade_free(market, orders, signal),
        }
        Ok(())
    }

    fn end(&mut self, _: Option<&Market>) {
        log(&format!(
            "zoo {}: {:?}, {} takes read from {} wallets",
            self.config.kind.name(),
            self.desk.goal,
            self.tape.takes.len(),
            self.tape.wallets.len()
        ));
    }
}

/// The zoo rung `MTX_ZOO` names, on `mtx-kit`'s run loop.
pub fn main() -> BotResult {
    let config = Config::from_env()?;
    mtx_kit::run(Zoo::new(config))
}

/// The anchor-strength hybrid ([`hybrid`]): `informed`, the task's informed
/// bot, with the zoo rung `MTX_ZOO` names in its idle steps. A task's
/// `checks/rungs/hybrid` calls it.
pub fn hybrid_main(informed: Box<dyn Strategy>) -> BotResult {
    hybrid_main_with(informed, |_| None)
}

/// [`hybrid_main`], with the task's own rungs: `extra` builds the rung
/// `MTX_ZOO` names when it isn't one of the zoo's.
pub fn hybrid_main_with(
    informed: Box<dyn Strategy>,
    extra: impl Fn(&str) -> Option<Box<dyn Strategy>>,
) -> BotResult {
    let name = std::env::var("MTX_ZOO").unwrap_or_default();
    let rung: Box<dyn Strategy> = match Kind::parse(&name) {
        Some(_) => Box::new(Zoo::new(Config::from_env()?)),
        None => extra(&name).with_context(|| format!("MTX_ZOO: unknown rung {name:?}"))?,
    };
    mtx_kit::run(hybrid::Hybrid::new(informed, rung))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mtx_kit::{Intent, Position};

    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config> {
        let vars: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::parse(|name| vars.get(name).cloned())
    }

    fn job(kind: Kind, dir: f64) -> Zoo {
        let mut zoo = Zoo::new(config(&[("MTX_ZOO", kind.name())]).unwrap());
        zoo.desk = Desk {
            max_position: 10.0,
            min_position: 0.0,
            buy: dir > 0.0,
            sell: dir < 0.0,
            lot: 0.01,
            tick: 0.01,
            start_slot: 0,
            end_slot: 1000,
            start_oracle: 100.0,
            order: Some(desk::Order {
                dir,
                quantity: 10.0,
            }),
            ..Desk::default()
        };
        zoo
    }

    fn market(slot: u64, position: f64) -> Market {
        Market {
            k: slot / 2,
            slot,
            slots_left: 1000u64.saturating_sub(slot),
            position: Position {
                size: position,
                entry_usd: 0.0,
            },
            mark: 100.0,
            oracle: 100.0,
            best_bid: Some(99.95),
            best_ask: Some(100.05),
            cap: 10.0,
            ..Market::default()
        }
    }

    fn traded(intents: &[Intent]) -> f64 {
        intents
            .iter()
            .map(|i| match i {
                Intent::Market { side, size, .. } => match side {
                    mtx_kit::Side::Bid => *size,
                    mtx_kit::Side::Ask => -size,
                },
                _ => 0.0,
            })
            .sum()
    }

    fn step(zoo: &mut Zoo, m: &Market) -> Vec<Intent> {
        let mut orders = Orders::new();
        zoo.step(m, &mut orders).unwrap();
        orders.intents().to_vec()
    }

    #[test]
    fn every_rung_has_one_name_and_settings_are_checked() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.name()), Some(kind));
        }
        let names: std::collections::BTreeSet<_> = Kind::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(names.len(), Kind::ALL.len());
        assert!(config(&[]).is_err());
        assert!(config(&[("MTX_ZOO", "nope")]).is_err());
        assert!(config(&[("MTX_ZOO", "twap"), ("MTX_ZOO_SIZE", "1.5")]).is_err());
        assert!(config(&[("MTX_ZOO", "twap"), ("MTX_ZOO_SIZE", "-0.5")]).is_err());
        assert!(config(&[("MTX_ZOO", "twap"), ("MTX_ZOO_LOOK", "0")]).is_err());
        assert!(config(&[("MTX_ZOO", "twap"), ("MTX_ZOO_CLIP", "x")]).is_err());
        let c = config(&[("MTX_ZOO", "odd-lot"), ("MTX_ZOO_CLIP", "0.5")]).unwrap();
        assert_eq!(
            (c.kind, c.clip, c.size, c.hold),
            (Kind::OddLot, 0.5, 0.5, 300)
        );
    }

    #[test]
    fn a_seller_twap_keeps_to_its_schedule_and_dump_sells_at_once() {
        let mut twap = job(Kind::Twap, -1.0);
        twap.desk.begin(10.0);
        // A quarter of the way to the end of the schedule: a quarter sold.
        let sold = traded(&step(&mut twap, &market(242, 10.0)));
        assert!((sold + 2.49).abs() < 0.02, "{sold}");
        assert_eq!(traded(&step(&mut twap, &market(242, 7.5))), 0.0);

        let mut dump = job(Kind::Dump, -1.0);
        dump.desk.begin(10.0);
        assert!((traded(&step(&mut dump, &market(0, 10.0))) + 10.0).abs() < 1e-9);
    }

    #[test]
    fn late_and_wait_close_hold_then_finish() {
        let mut late = job(Kind::Late, 1.0);
        late.desk.begin(0.0);
        assert_eq!(traded(&step(&mut late, &market(500, 0.0))), 0.0);
        assert!(traded(&step(&mut late, &market(900, 0.0))) > 4.0);
        let mut wait = job(Kind::WaitClose, 1.0);
        wait.desk.begin(0.0);
        assert_eq!(traded(&step(&mut wait, &market(900, 0.0))), 0.0);
        assert!((traded(&step(&mut wait, &market(980, 0.0))) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_broker_job_finishes_only_above_the_brokers_price() {
        let mut wait = job(Kind::WaitClose, -1.0);
        wait.desk.haircut_bps = Some(5.0);
        wait.desk.begin(10.0);
        // The bid less the fee is under the oracle less 5 bp.
        assert_eq!(traded(&step(&mut wait, &market(980, 10.0))), 0.0);
        wait.desk.haircut_bps = Some(15.0);
        assert!(traded(&step(&mut wait, &market(980, 10.0))) < -9.9);
    }

    #[test]
    fn the_passive_rungs_rest_on_the_jobs_side_and_do_not_churn() {
        let mut odd = job(Kind::OddLot, 1.0);
        odd.desk.begin(0.0);
        let first = step(&mut odd, &market(10, 0.0));
        assert!(
            matches!(first[..], [Intent::CancelAll, Intent::Limit { side: mtx_kit::Side::Bid, price, size }]
            if (price - 99.95).abs() < 1e-9 && (size - 0.95).abs() < 1e-9)
        );
        let resting = Market {
            resting_bids: 0.95,
            ..market(12, 0.0)
        };
        assert!(step(&mut odd, &resting).is_empty());

        let mut inside = job(Kind::InsideMaker, -1.0);
        inside.desk.begin(10.0);
        let quote = step(&mut inside, &market(10, 10.0));
        assert!(
            matches!(quote[..], [Intent::CancelAll, Intent::Limit { side: mtx_kit::Side::Ask, price, size }]
            if (price - 100.04).abs() < 1e-9 && (size - 10.0).abs() < 1e-9)
        );
    }

    #[test]
    fn a_signal_against_the_job_finishes_it_and_one_for_it_waits() {
        let mut breaker = job(Kind::RangeBreak, -1.0);
        breaker.desk.begin(10.0);
        let up = Market {
            oracle: 100.3,
            ..market(100, 10.0)
        };
        assert_eq!(traded(&step(&mut breaker, &up)), 0.0);
        let down = Market {
            oracle: 99.7,
            ..market(102, 10.0)
        };
        assert!((traded(&step(&mut breaker, &down)) + 10.0).abs() < 1e-9);
        // Once told, it stays told.
        assert!(
            traded(&step(
                &mut breaker,
                &Market {
                    oracle: 100.0,
                    ..market(104, 4.0)
                }
            )) < -3.9
        );
    }

    #[test]
    fn a_free_rung_holds_its_signal_for_the_hold_and_stays_in_the_mandate() {
        let mut zoo = job(Kind::RangeBreak, 1.0);
        zoo.desk.order = None;
        zoo.desk.buy = true;
        zoo.desk.sell = true;
        zoo.desk.min_position = -10.0;
        zoo.desk.begin(0.0);
        assert_eq!(zoo.desk.goal, Goal::Free);
        let down = Market {
            oracle: 99.7,
            ..market(100, 0.0)
        };
        assert!((traded(&step(&mut zoo, &down)) + 5.0).abs() < 1e-9);
        let back = Market {
            oracle: 100.0,
            ..market(300, -5.0)
        };
        assert_eq!(traded(&step(&mut zoo, &back)), 0.0);
        assert!((traded(&step(&mut zoo, &market(402, -5.0))) - 5.0).abs() < 1e-9);

        // On a long-only mandate the short is cut to nothing.
        let mut long_only = job(Kind::RangeBreak, 1.0);
        long_only.desk.order = None;
        long_only.desk.sell = true;
        long_only.desk.begin(0.0);
        assert_eq!(traded(&step(&mut long_only, &down)), 0.0);
    }

    #[test]
    fn a_pacing_rung_without_a_job_trades_nothing() {
        let mut zoo = job(Kind::Twap, 1.0);
        zoo.desk.order = None;
        zoo.desk.sell = true;
        zoo.desk.begin(0.0);
        assert!(step(&mut zoo, &market(500, 0.0)).is_empty());
    }
}
