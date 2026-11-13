//! The probe bots: reward-hacking checks for a task's scoring, as real bots.
//!
//! Scoring v2 is meant to pay for skill and nothing else. A probe is a bot
//! that has no skill (or a known, small amount, or a way to game the rule),
//! graded on the task's hidden sets; the ladder (`bench/checks/ladder`)
//! says where each must land, and `mtx gates` fails the task when one lands
//! elsewhere. Every probe is a [`Strategy`] on `mtx-kit`'s run loop, chosen
//! and tuned by environment variables the ladder sets per rung
//! (`MTX_PROBE_*`, below), so any perp task reuses them. A probe that
//! wraps the task's reference needs the task's own rung crate
//! (`checks/rungs/probe`, five lines around [`main_with`]).
//!
//! Kinds (`MTX_PROBE`), each a position over `blocks` equal parts of the
//! session, sized as a share of the mandate's cap:
//!
//! - `coin-flip`: a seeded coin picks the side of each block; zero edge.
//!   `MTX_PROBE_SIZE` (default 1, the full cap), `MTX_PROBE_BLOCKS` (1).
//! - `martingale`: a coin per block, starting at `MTX_PROBE_SIZE` (0.125)
//!   of the cap and doubling after each losing block, up to the cap; back to
//!   the start after a win. `MTX_PROBE_BLOCKS` (8).
//! - `stop-lotto`: a coin per block at `MTX_PROBE_SIZE` (1), with a
//!   trailing stop at `MTX_PROBE_STOP` (0.15) of the drawdown limit from the
//!   block's peak PnL; once stopped it waits for the next block.
//!   `MTX_PROBE_BLOCKS` (4).
//! - `hugger`: rests a bid and an ask at the best prices each step, of
//!   `MTX_PROBE_SIZE` (0.1) of the cap; flat for the last tenth.
//! - `reference`: the task's reference, as it is.
//! - `honest`: a session follows the reference with probability
//!   `MTX_PROBE_BLEND`, else flips a coin at the full cap held through the
//!   session. A mixture of a bot with edge and one without: its per-session
//!   Sharpe falls between the two, and `calibrate.py` picks the blend for a
//!   target (see README.md).
//!
//! Wrappers, applied in this order to any kind:
//!
//! - `MTX_PROBE_LEVER=m`: the strategy sees a market with its position
//!   divided by `m` and its orders are multiplied by `m`: the reference's
//!   decisions, `m` times the size, clipped to the cap by the run loop.
//! - `MTX_PROBE_BREACHES=b`: in each session, with probability `b` over
//!   `MTX_PROBE_SESSIONS` (the ladder sets it to the set's count), the bot
//!   goes 20% over the position cap partway through and stays there, a
//!   mandate breach on purpose; it sends its orders to the venue directly,
//!   the run loop would have cut them.
//! - `MTX_PROBE_CRASH_BELOW=f`: the bot exits once its PnL is `f` of the
//!   drawdown limit under water. `MTX_PROBE_CRASH=fatal` first makes a call
//!   the RPC guard refuses (`getHealth`), so the grade is a fatal one.
//!
//! `MTX_PROBE_SALT` changes the coins without changing the rest.

use anyhow::{Context, Result, bail};
use mtx_kit::{
    BotEnv, BotResult, Execution, Intent, Market, Orders, Session, Side, Strategy, Trader,
};

/// Largest market order, in base units: one order that crosses much of the
/// book doesn't fit a transaction's compute budget.
const SLICE: f64 = 10.0;
/// How far past the position cap a breacher goes, as a share of it.
const OVERSHOOT: f64 = 1.2;
/// The last share of a session in which the hugger flattens.
const FLATTEN_SHARE: f64 = 0.1;

const TAG_COIN: u64 = 1;
const TAG_BLEND: u64 = 2;
const TAG_BREACH: u64 = 3;
const TAG_BREACH_AT: u64 = 4;
const TAG_HONEST_COIN: u64 = 5;

/// What a probe does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    CoinFlip,
    Martingale,
    StopLotto,
    Hugger,
    Reference,
    Honest,
}

impl Kind {
    fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "coin-flip" => Self::CoinFlip,
            "martingale" => Self::Martingale,
            "stop-lotto" => Self::StopLotto,
            "hugger" => Self::Hugger,
            "reference" => Self::Reference,
            "honest" => Self::Honest,
            other => bail!("MTX_PROBE: unknown kind {other:?}"),
        })
    }

    fn needs_reference(self) -> bool {
        matches!(self, Self::Reference | Self::Honest)
    }
}

/// How a crashing probe goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crash {
    /// Exits with a failure.
    Exit,
    /// Calls a method the RPC guard refuses, then exits: a fatal grade.
    Fatal,
}

/// One probe, as the environment describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub kind: Kind,
    pub salt: u64,
    pub size: Option<f64>,
    pub blocks: Option<u64>,
    pub stop: f64,
    pub blend: f64,
    pub lever: f64,
    pub breaches: Option<f64>,
    pub sessions: f64,
    pub crash_below: Option<f64>,
    pub crash: Crash,
}

impl Config {
    /// Read the probe from the process's environment.
    pub fn from_env() -> Result<Self> {
        Self::parse(|name| std::env::var(name).ok())
    }

    /// Read the probe from `get`, which names a variable's value if set.
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let number = |name: &str| -> Result<Option<f64>> {
            get(name)
                .map(|v| {
                    v.parse::<f64>()
                        .ok()
                        .filter(|x| x.is_finite())
                        .with_context(|| format!("{name}: not a number: {v:?}"))
                })
                .transpose()
        };
        let kind = Kind::parse(&get("MTX_PROBE").unwrap_or_default())?;
        let config = Self {
            kind,
            salt: number("MTX_PROBE_SALT")?.unwrap_or(0.0) as u64,
            size: number("MTX_PROBE_SIZE")?,
            blocks: number("MTX_PROBE_BLOCKS")?.map(|b| b as u64),
            stop: number("MTX_PROBE_STOP")?.unwrap_or(0.15),
            blend: number("MTX_PROBE_BLEND")?.unwrap_or(1.0),
            lever: number("MTX_PROBE_LEVER")?.unwrap_or(1.0),
            breaches: number("MTX_PROBE_BREACHES")?,
            sessions: number("MTX_PROBE_SESSIONS")?.unwrap_or(72.0),
            crash_below: number("MTX_PROBE_CRASH_BELOW")?,
            crash: match get("MTX_PROBE_CRASH").as_deref() {
                None | Some("exit") => Crash::Exit,
                Some("fatal") => Crash::Fatal,
                Some(other) => bail!("MTX_PROBE_CRASH: unknown mode {other:?}"),
            },
        };
        if !(0.0..=1.0).contains(&config.blend) {
            bail!("MTX_PROBE_BLEND: a share in [0, 1]");
        }
        if config.lever < 1.0 {
            bail!("MTX_PROBE_LEVER: at least 1");
        }
        if config.blocks == Some(0) {
            bail!("MTX_PROBE_BLOCKS: at least 1");
        }
        if config.sessions < 1.0 {
            bail!("MTX_PROBE_SESSIONS: at least 1");
        }
        Ok(config)
    }
}

/// SplitMix64's finaliser: a well-mixed word from any word.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A number in [0, 1) fixed by the session's seed, the probe's salt, what
/// it is for and which one.
pub fn draw(seed: u64, salt: u64, tag: u64, index: u64) -> f64 {
    let h = mix(mix(mix(seed ^ salt.rotate_left(17)) ^ tag) ^ index);
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// -1 or 1, evenly.
pub fn coin(seed: u64, salt: u64, tag: u64, index: u64) -> f64 {
    if draw(seed, salt, tag, index) < 0.5 {
        -1.0
    } else {
        1.0
    }
}

/// The block step `k` of `steps` falls in, of `blocks` equal ones.
pub fn block_of(k: u64, steps: u64, blocks: u64) -> u64 {
    (k.saturating_mul(blocks) / steps.max(1)).min(blocks.saturating_sub(1))
}

/// What the strategies read of the session.
#[derive(Debug, Clone, Copy, Default)]
struct Frame {
    steps: u64,
    cap: f64,
    limit: f64,
    seed: u64,
    salt: u64,
}

impl Frame {
    fn of(session: &Session, salt: u64) -> Self {
        Self {
            steps: session.steps,
            cap: session.cap,
            limit: session.max_drawdown,
            seed: session.seed,
            salt,
        }
    }
}

/// A coin per block, held at a share of the cap.
struct CoinFlip {
    frame: Frame,
    tag: u64,
    size: f64,
    blocks: u64,
}

impl CoinFlip {
    fn target(&self, k: u64) -> f64 {
        let block = block_of(k, self.frame.steps, self.blocks);
        coin(self.frame.seed, self.frame.salt, self.tag, block) * self.size * self.frame.cap
    }
}

/// Doubles after a losing block, a coin per block.
struct Martingale {
    frame: Frame,
    start: f64,
    blocks: u64,
    share: f64,
    block: Option<u64>,
    block_pnl: f64,
}

impl Martingale {
    fn target(&mut self, market: &Market) -> f64 {
        let block = block_of(market.k, self.frame.steps, self.blocks);
        if self.block != Some(block) {
            if self.block.is_some() {
                self.share = if market.pnl < self.block_pnl {
                    (self.share * 2.0).min(1.0)
                } else {
                    self.start
                };
            }
            self.block = Some(block);
            self.block_pnl = market.pnl;
        }
        coin(self.frame.seed, self.frame.salt, TAG_COIN, block) * self.share * self.frame.cap
    }
}

/// A coin per block with a trailing stop.
struct StopLotto {
    frame: Frame,
    size: f64,
    stop: f64,
    blocks: u64,
    block: Option<u64>,
    peak: f64,
    stopped: bool,
}

impl StopLotto {
    fn target(&mut self, market: &Market) -> f64 {
        let block = block_of(market.k, self.frame.steps, self.blocks);
        if self.block != Some(block) {
            self.block = Some(block);
            self.peak = market.pnl;
            self.stopped = false;
        }
        self.peak = self.peak.max(market.pnl);
        if self.peak - market.pnl >= self.stop * self.frame.limit {
            self.stopped = true;
        }
        if self.stopped {
            0.0
        } else {
            coin(self.frame.seed, self.frame.salt, TAG_COIN, block) * self.size * self.frame.cap
        }
    }
}

/// Orders at the best prices.
struct Hugger {
    frame: Frame,
    size: f64,
}

impl Strategy for Hugger {
    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let flat_from = self.frame.steps - (self.frame.steps as f64 * FLATTEN_SHARE) as u64;
        if market.k >= flat_from {
            orders.cancel_all();
            orders.target_position(0.0);
        } else if let (Some(bid), Some(ask)) = (market.best_bid, market.best_ask) {
            orders.quote(bid, ask, self.size * self.frame.cap);
        }
        Ok(())
    }

    fn start(&mut self, session: &Session) -> BotResult {
        self.frame = Frame::of(session, self.frame.salt);
        Ok(())
    }
}

macro_rules! held_target {
    ($name:ident) => {
        impl Strategy for $name {
            fn start(&mut self, session: &Session) -> BotResult {
                self.frame = Frame::of(session, self.frame.salt);
                Ok(())
            }

            fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
                let target = self.target(market);
                orders.target_position(target);
                Ok(())
            }
        }
    };
}

impl CoinFlip {
    fn target_of(&self, market: &Market) -> f64 {
        self.target(market.k)
    }
}

impl Strategy for CoinFlip {
    fn start(&mut self, session: &Session) -> BotResult {
        self.frame = Frame::of(session, self.frame.salt);
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        orders.target_position(self.target_of(market));
        Ok(())
    }
}

held_target!(Martingale);
held_target!(StopLotto);

/// The strategies, behind one pointer.
pub struct Boxed(pub Box<dyn Strategy>);

impl Strategy for Boxed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.0.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.0.step(market, orders)
    }

    fn end(&mut self, last: Option<&Market>) {
        self.0.end(last)
    }
}

/// Follows the reference in some sessions and flips a coin in the rest.
struct Honest {
    reference: Boxed,
    coin: CoinFlip,
    blend: f64,
    follows: bool,
}

impl Strategy for Honest {
    fn start(&mut self, session: &Session) -> BotResult {
        let frame = Frame::of(session, self.coin.frame.salt);
        self.follows = draw(frame.seed, frame.salt, TAG_BLEND, 0) < self.blend;
        self.coin.frame = frame;
        if self.follows {
            self.reference.start(session)
        } else {
            Ok(())
        }
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        if self.follows {
            self.reference.step(market, orders)
        } else {
            orders.target_position(self.coin.target_of(market));
            Ok(())
        }
    }
}

/// The inner strategy at `m` times the size.
struct Lever {
    inner: Boxed,
    m: f64,
}

impl Lever {
    /// The market the inner strategy sees: its own size's worth.
    fn scaled_down(&self, market: &Market) -> Market {
        let mut seen = market.clone();
        seen.position.size /= self.m;
        seen.resting_bids /= self.m;
        seen.resting_asks /= self.m;
        seen.pnl /= self.m;
        seen.peak_pnl /= self.m;
        seen.drawdown /= self.m;
        seen
    }

    /// The inner strategy's sheet, `m` times as large.
    fn scaled_up(&self, inner: Orders, orders: &mut Orders) {
        for (intent, options) in inner.into_lines() {
            match intent {
                Intent::Target {
                    position,
                    execution,
                } => {
                    orders.push(
                        Intent::Target {
                            position: position * self.m,
                            execution: Execution {
                                max_per_step: execution.max_per_step * self.m,
                                ..execution
                            },
                        },
                        options,
                    );
                }
                Intent::Market { side, size, limit } => {
                    let mut left = (size * self.m).round().max(1.0);
                    while left >= 1.0 {
                        let slice = left.min(SLICE);
                        orders.push(
                            Intent::Market {
                                side,
                                size: slice,
                                limit,
                            },
                            options.clone(),
                        );
                        left -= slice;
                    }
                }
                Intent::Limit { side, price, size } => {
                    orders.push(
                        Intent::Limit {
                            side,
                            price,
                            size: size * self.m,
                        },
                        options,
                    );
                }
                Intent::Quote { bid, ask, size } => {
                    orders.push(
                        Intent::Quote {
                            bid,
                            ask,
                            size: size * self.m,
                        },
                        options,
                    );
                }
                other => {
                    orders.push(other, options);
                }
            }
        }
    }
}

impl Strategy for Lever {
    fn start(&mut self, session: &Session) -> BotResult {
        self.inner.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let mut inner = Orders::new();
        self.inner.step(&self.scaled_down(market), &mut inner)?;
        self.scaled_up(inner, orders);
        Ok(())
    }
}

/// In some sessions, goes over the position cap partway through.
struct Breacher {
    inner: Boxed,
    share: f64,
    salt: u64,
    chosen: bool,
    at: u64,
    done: bool,
    trader: Option<Trader>,
}

impl Breacher {
    /// Whether a session with `seed` is one of the breaching ones.
    pub fn chooses(seed: u64, salt: u64, share: f64) -> bool {
        draw(seed, salt, TAG_BREACH, 0) < share
    }

    /// The step the breach goes in at: between a quarter and half way.
    pub fn step_of(seed: u64, salt: u64, steps: u64) -> u64 {
        let quarter = (steps / 4).max(1);
        quarter + (draw(seed, salt, TAG_BREACH_AT, 0) * quarter as f64) as u64
    }

    fn overshoot(&self, market: &Market) -> Result<()> {
        let trader = self.trader.as_ref().context("no trader")?;
        let side = if market.position.size != 0.0 {
            market.position.size.signum()
        } else {
            coin(0, self.salt, TAG_BREACH, market.k)
        };
        let target = side * OVERSHOOT * market.cap;
        let mut missing = target - market.position.size;
        let handle = tokio::runtime::Handle::current();
        while missing.abs() >= 1.0 {
            let size = missing.abs().floor().min(SLICE);
            let (order, limit) = if missing > 0.0 {
                (Side::Bid, market.mark * 1.02)
            } else {
                (Side::Ask, market.mark * 0.98)
            };
            let sent = tokio::task::block_in_place(|| {
                handle.block_on(trader.place_market(order, size, (limit * 100.0).round() / 100.0))
            });
            if let Err(e) = sent {
                tracing::warn!(%e, "overshoot order failed");
                break;
            }
            missing -= size.copysign(missing);
        }
        Ok(())
    }
}

impl Strategy for Breacher {
    fn start(&mut self, session: &Session) -> BotResult {
        self.chosen = Self::chooses(session.seed, self.salt, self.share);
        self.at = Self::step_of(session.seed, self.salt, session.steps);
        if self.chosen {
            self.trader = Some(Trader::connect(&BotEnv::from_env()?)?);
        }
        self.inner.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        if self.chosen && market.k >= self.at {
            if !self.done {
                self.done = true;
                self.overshoot(market)?;
            }
            return Ok(());
        }
        self.inner.step(market, orders)
    }
}

/// Exits once it is behind.
struct Crasher {
    inner: Boxed,
    below: f64,
    mode: Crash,
    trader: Option<Trader>,
}

/// Whether a PnL of `pnl` is `fraction` of `limit` under water.
pub fn behind(pnl: f64, limit: f64, fraction: f64) -> bool {
    limit > 0.0 && pnl <= -fraction * limit
}

impl Strategy for Crasher {
    fn start(&mut self, session: &Session) -> BotResult {
        if self.mode == Crash::Fatal {
            self.trader = Some(Trader::connect(&BotEnv::from_env()?)?);
        }
        self.inner.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        if behind(market.pnl, market.max_drawdown, self.below) {
            mtx_kit::log("behind: crashing on purpose");
            if let Some(trader) = &self.trader {
                // Any method the guard refuses: the grade is fatal.
                let _ = trader.rpc.get_health();
            }
            std::process::exit(1);
        }
        self.inner.step(market, orders)
    }
}

/// The probe `config` describes, around `reference` where it needs one.
pub fn build(config: &Config, reference: Option<Box<dyn Strategy>>) -> Result<Boxed> {
    let frame = Frame {
        salt: config.salt,
        ..Frame::default()
    };
    let reference = || {
        reference
            .context("this probe wraps the task's reference: start it from the task's `checks/rungs/probe`")
    };
    let mut base: Boxed = match config.kind {
        Kind::CoinFlip => Boxed(Box::new(CoinFlip {
            frame,
            tag: TAG_COIN,
            size: config.size.unwrap_or(1.0),
            blocks: config.blocks.unwrap_or(1),
        })),
        Kind::Martingale => {
            let start = config.size.unwrap_or(0.125);
            Boxed(Box::new(Martingale {
                frame,
                start,
                blocks: config.blocks.unwrap_or(8),
                share: start,
                block: None,
                block_pnl: 0.0,
            }))
        }
        Kind::StopLotto => Boxed(Box::new(StopLotto {
            frame,
            size: config.size.unwrap_or(1.0),
            stop: config.stop,
            blocks: config.blocks.unwrap_or(4),
            block: None,
            peak: 0.0,
            stopped: false,
        })),
        Kind::Hugger => Boxed(Box::new(Hugger {
            frame,
            size: config.size.unwrap_or(0.1),
        })),
        Kind::Reference => Boxed(reference()?),
        Kind::Honest => Boxed(Box::new(Honest {
            reference: Boxed(reference()?),
            coin: CoinFlip {
                frame,
                tag: TAG_HONEST_COIN,
                size: config.size.unwrap_or(1.0),
                blocks: config.blocks.unwrap_or(1),
            },
            blend: config.blend,
            follows: true,
        })),
    };
    if config.lever > 1.0 {
        base = Boxed(Box::new(Lever {
            inner: base,
            m: config.lever,
        }));
    }
    if let Some(b) = config.breaches {
        base = Boxed(Box::new(Breacher {
            inner: base,
            share: (b / config.sessions).clamp(0.0, 1.0),
            salt: config.salt,
            chosen: false,
            at: 0,
            done: false,
            trader: None,
        }));
    }
    if let Some(below) = config.crash_below {
        base = Boxed(Box::new(Crasher {
            inner: base,
            below,
            mode: config.crash,
            trader: None,
        }));
    }
    Ok(base)
}

/// The whole bot: the probe the environment describes, as a process, around
/// the task's `reference` if it has one to give.
pub fn main_with(reference: Option<Box<dyn Strategy>>) -> BotResult {
    let config = Config::from_env()?;
    let strategy = build(&config, reference)?;
    mtx_kit::run(strategy)
}

/// Whether `kind` wraps the reference: for a rung that has none to give.
pub fn wraps_reference(name: &str) -> bool {
    Kind::parse(name).is_ok_and(Kind::needs_reference)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config> {
        let vars: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::parse(|name| vars.get(name).cloned())
    }

    fn frame(steps: u64) -> Frame {
        Frame {
            steps,
            cap: 70.0,
            limit: 600.0,
            seed: 11,
            salt: 0,
        }
    }

    fn market(k: u64, pnl: f64) -> Market {
        Market {
            k,
            pnl,
            peak_pnl: pnl.max(0.0),
            cap: 70.0,
            max_drawdown: 600.0,
            ..Market::default()
        }
    }

    #[test]
    fn a_probe_needs_a_known_kind_and_sane_numbers() {
        assert!(config(&[]).is_err());
        assert!(config(&[("MTX_PROBE", "nope")]).is_err());
        assert!(config(&[("MTX_PROBE", "coin-flip"), ("MTX_PROBE_BLEND", "1.5")]).is_err());
        assert!(config(&[("MTX_PROBE", "coin-flip"), ("MTX_PROBE_LEVER", "0.5")]).is_err());
        assert!(config(&[("MTX_PROBE", "coin-flip"), ("MTX_PROBE_BLOCKS", "0")]).is_err());
        assert!(config(&[("MTX_PROBE", "coin-flip"), ("MTX_PROBE_SIZE", "big")]).is_err());
        assert!(config(&[("MTX_PROBE", "coin-flip"), ("MTX_PROBE_CRASH", "burn")]).is_err());
        let c = config(&[
            ("MTX_PROBE", "honest"),
            ("MTX_PROBE_BLEND", "0.4"),
            ("MTX_PROBE_CRASH_BELOW", "0.3"),
            ("MTX_PROBE_CRASH", "fatal"),
        ])
        .unwrap();
        assert_eq!(c.kind, Kind::Honest);
        assert_eq!(c.blend, 0.4);
        assert_eq!(c.crash_below, Some(0.3));
        assert_eq!(c.crash, Crash::Fatal);
    }

    #[test]
    fn the_reference_probes_say_so_without_one() {
        assert!(wraps_reference("honest"));
        assert!(wraps_reference("reference"));
        assert!(!wraps_reference("coin-flip"));
        let c = config(&[("MTX_PROBE", "honest")]).unwrap();
        assert!(build(&c, None).is_err());
    }

    #[test]
    fn draws_repeat_and_spread() {
        assert_eq!(draw(5, 0, 1, 0), draw(5, 0, 1, 0));
        assert_ne!(draw(5, 0, 1, 0), draw(5, 1, 1, 0));
        assert_ne!(draw(5, 0, 1, 0), draw(6, 0, 1, 0));
        let ups = (0..2000).filter(|s| coin(*s, 0, TAG_COIN, 0) > 0.0).count();
        assert!((900..1100).contains(&ups), "{ups} of 2000 up");
        assert!((0..2000).all(|s| (0.0..1.0).contains(&draw(s, 3, 2, 1))));
    }

    #[test]
    fn blocks_cover_the_session_evenly() {
        let counts: Vec<usize> = (0..4)
            .map(|b| (0..120).filter(|k| block_of(*k, 120, 4) == b).count())
            .collect();
        assert_eq!(counts, [30, 30, 30, 30]);
        assert_eq!(block_of(119, 120, 1), 0);
        assert_eq!(block_of(5, 0, 3), 2);
    }

    #[test]
    fn the_full_cap_coin_holds_one_side_per_block() {
        let coin = CoinFlip {
            frame: frame(100),
            tag: TAG_COIN,
            size: 1.0,
            blocks: 1,
        };
        let first = coin.target(0);
        assert_eq!(first.abs(), 70.0);
        assert!((0..100).all(|k| coin.target(k) == first));
        let half = CoinFlip { size: 0.5, ..coin };
        assert_eq!(half.target(3).abs(), 35.0);
    }

    #[test]
    fn the_martingale_doubles_after_a_loss_and_resets_after_a_win() {
        let mut m = Martingale {
            frame: frame(80),
            start: 0.125,
            blocks: 8,
            share: 0.125,
            block: None,
            block_pnl: 0.0,
        };
        let size = |m: &mut Martingale, k, pnl| m.target(&market(k, pnl)).abs() / 70.0;
        assert_eq!(size(&mut m, 0, 0.0), 0.125);
        assert_eq!(size(&mut m, 10, -50.0), 0.25);
        assert_eq!(size(&mut m, 20, -90.0), 0.5);
        assert_eq!(size(&mut m, 30, -120.0), 1.0);
        assert_eq!(size(&mut m, 40, -200.0), 1.0);
        assert_eq!(size(&mut m, 50, -100.0), 0.125);
    }

    #[test]
    fn the_stop_lotto_goes_flat_until_the_next_block() {
        let mut s = StopLotto {
            frame: frame(100),
            size: 1.0,
            stop: 0.15,
            blocks: 4,
            block: None,
            peak: 0.0,
            stopped: false,
        };
        assert_eq!(s.target(&market(0, 0.0)).abs(), 70.0);
        assert_eq!(s.target(&market(5, 100.0)).abs(), 70.0);
        // 0.15 of the limit is $90 below the block's peak.
        assert_eq!(s.target(&market(10, 20.0)).abs(), 70.0);
        assert_eq!(s.target(&market(11, 5.0)), 0.0);
        assert_eq!(s.target(&market(20, 90.0)), 0.0);
        assert_eq!(s.target(&market(25, 90.0)).abs(), 70.0);
    }

    #[test]
    fn a_lever_sees_its_own_size_and_sends_m_times_as_much() {
        struct Rider;
        impl Strategy for Rider {
            fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
                // It asks for 10 more than it holds, in market orders.
                assert_eq!(market.position.size, 20.0);
                orders.buy(7.0, 101.0);
                orders.target_position(40.0);
                orders.cancel_all();
                Ok(())
            }
        }
        let mut lever = Lever {
            inner: Boxed(Box::new(Rider)),
            m: 3.0,
        };
        let mut held = market(0, 0.0);
        held.position.size = 60.0;
        let mut orders = Orders::new();
        lever.step(&held, &mut orders).unwrap();
        let sent: Vec<Intent> = orders.into_intents();
        // 7 × 3 = 21 shares, in slices of at most 10.
        let sizes: Vec<f64> = sent
            .iter()
            .filter_map(|i| match i {
                Intent::Market { size, .. } => Some(*size),
                _ => None,
            })
            .collect();
        assert_eq!(sizes, [10.0, 10.0, 1.0]);
        assert!(sent.iter().any(
            |i| matches!(i, Intent::Target { position, .. } if *position == 120.0)
        ));
        assert!(sent.iter().any(|i| matches!(i, Intent::CancelAll)));
    }

    #[test]
    fn a_breacher_is_chosen_at_its_share_and_goes_in_the_first_half() {
        let chosen = (0..7200)
            .filter(|s| Breacher::chooses(*s, 0, 3.0 / 72.0))
            .count();
        assert!((200..400).contains(&chosen), "{chosen} of 7200");
        assert!(!Breacher::chooses(1, 0, 0.0));
        for seed in 0..200 {
            let at = Breacher::step_of(seed, 0, 120);
            assert!((30..60).contains(&at), "{at}");
        }
    }

    #[test]
    fn a_crash_comes_when_the_pnl_is_under_the_fraction() {
        assert!(!behind(-100.0, 600.0, 0.3));
        assert!(behind(-180.0, 600.0, 0.3));
        assert!(!behind(-1e9, 0.0, 0.3));
    }

    #[test]
    fn the_honest_mixture_follows_the_reference_at_its_blend() {
        let follows = |blend: f64| {
            (0..4000u64)
                .filter(|s| draw(*s, 0, TAG_BLEND, 0) < blend)
                .count() as f64
                / 4000.0
        };
        assert!((follows(0.25) - 0.25).abs() < 0.03);
        assert_eq!(follows(1.0), 1.0);
        assert_eq!(follows(0.0), 0.0);
    }
}
