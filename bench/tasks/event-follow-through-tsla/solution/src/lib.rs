//! Reference: stay flat through the headline; once the jump has landed,
//! mark out every wallet's takes against the oracle, find the fund whose
//! trades keep leading it (and the one whose trades keep losing), and take
//! the leader's side before the verdict.
//!
//! Every Phoenix trader account is on chain, so the bot lists them once
//! (`getProgramAccounts`) and reads each one's TSLA position every step.
//!
//! - **The headline.** The oracle moving more than 0.5% in 30 slots, from
//!   slot 280 on, is the jump; twelve slots without a move of 8 bp is the
//!   jump done.
//! - **The record.** From then on, each account's change in position at a
//!   step is a take, marked out against the oracle 30 slots later, in basis
//!   points, the take's way. An account with six or more takes whose mean
//!   markout is two standard errors above zero leads the price; one two
//!   below lags it.
//! - **The trade.** The leaders' net buying over the last 30 slots, less
//!   the laggards', past 1.2 TSLA either way sets the side, with the cap,
//!   once it has read the same side three steps running. An account stays
//!   read until its markouts fall under one standard error, and once in,
//!   the bot doesn't turn round.
//!   A move in the oracle of 0.15% within 20 slots is the verdict: six
//!   slots after it the bot closes and is done for the session.
//!
//! It isn't told who anyone is.

use std::collections::VecDeque;

use mtx_kit::{
    log,
    prelude::*,
    rpc_accounts,
    venue::{
        PhoenixVenue, key,
        phoenix_rise::{
            accounts::{PhoenixAccount, trader::Trader},
            ix::PHOENIX_PROGRAM_ID,
        },
    },
};

/// The session slot the headline's window opens at.
pub const EVENT_FROM: u64 = 300;
/// How far past the price a taking order may fill, as a share.
pub const TAKE_REACH: f64 = 0.006;
/// Slots a flow is summed over, and the net, in TSLA, that sets a side.
pub const FLOW_SLOTS: u64 = 30;
pub const FLOW_UNITS: f64 = 1.2;
/// Slots after the verdict's first move the bot closes at.
pub const AFTER_VERDICT_SLOTS: u64 = 6;

/// Slots a take is marked out over, the takes an account needs, and the
/// standard errors its mean markout must clear.
const MARKOUT_SLOTS: u64 = 30;
const MIN_TAKES: u32 = 6;
const MIN_T: f64 = 2.0;
/// An account read as leading or lagging stays so until its mean markout
/// falls back under this many standard errors.
const KEEP_T: f64 = 1.0;
/// Steps running the same side must read before the bot enters.
const CONFIRM_STEPS: u32 = 3;

/// The mid of the book, if it has both sides.
pub fn mid(market: &Market) -> Option<f64> {
    Some((market.best_bid? + market.best_ask?) / 2.0)
}

/// Trade toward `target` (base units, positive long) at once, crossing the
/// book, never further than [`TAKE_REACH`] past the touch.
pub fn go_to(target: f64, market: &Market, orders: &mut Orders) {
    let diff = target - market.position.size;
    let size = (diff.abs() * 1000.0).floor() / 1000.0;
    if size < 0.001 {
        return;
    }
    if diff > 0.0 {
        if let Some(ask) = market.best_ask {
            orders.buy(size, ask * (1.0 + TAKE_REACH));
        }
    } else if let Some(bid) = market.best_bid {
        orders.sell(size, bid * (1.0 - TAKE_REACH));
    }
}

/// Phoenix trader accounts whose position in one market the bot reads
/// every step.
#[derive(Debug, Clone, Default)]
pub struct Watch {
    pub accounts: Vec<String>,
    asset_id: u64,
    lot: f64,
    /// Each account's position at each step read, newest last.
    history: VecDeque<(u64, Vec<f64>)>,
    keep_slots: u64,
}

impl Watch {
    pub fn new(
        venue: &PhoenixVenue,
        symbol: &str,
        accounts: Vec<String>,
        keep_slots: u64,
    ) -> BotResult<Self> {
        let market = venue.market(symbol)?;
        Ok(Self {
            accounts,
            asset_id: u64::from(market.asset_id),
            lot: venue.lots_to_base_units(market, 1),
            history: VecDeque::new(),
            keep_slots,
        })
    }

    /// The trader accounts of these authorities (base58).
    pub fn of_authorities(venue: &PhoenixVenue, wallets: &[String]) -> BotResult<Vec<String>> {
        wallets
            .iter()
            .map(|w| Ok(venue.trader_account(&key(w)?).to_string()))
            .collect()
    }

    /// Every Phoenix trader account on chain, but `skip`.
    pub fn every_trader(skip: &str) -> BotResult<Vec<String>> {
        let tag = bs58::encode(PhoenixAccount::Trader.discriminant()).into_string();
        let params = serde_json::json!([
            PHOENIX_PROGRAM_ID.to_string(),
            {
                "encoding": "base64",
                "dataSlice": { "offset": 0, "length": 0 },
                "filters": [{ "memcmp": { "offset": 0, "bytes": tag } }],
            }
        ]);
        let answer = rpc("getProgramAccounts", &params.to_string())?;
        let answer: serde_json::Value = serde_json::from_str(&answer).map_err(|e| e.to_string())?;
        let list = answer
            .as_array()
            .or_else(|| answer["value"].as_array())
            .ok_or("getProgramAccounts answered no list")?;
        let mut out: Vec<String> = list
            .iter()
            .filter_map(|o| o["pubkey"].as_str().map(str::to_string))
            .filter(|a| a != skip)
            .collect();
        out.sort();
        Ok(out)
    }

    fn position(&self, data: &[u8]) -> f64 {
        let Ok(trader) = Trader::try_from_account_bytes(data) else {
            return 0.0;
        };
        trader
            .positions()
            .find(|(asset, _)| *asset == self.asset_id)
            .map_or(0.0, |(_, p)| {
                p.to_position().base_lot_position.as_inner() as f64 * self.lot
            })
    }

    /// Read every account's position at `slot`.
    pub fn read(&mut self, slot: u64) -> BotResult {
        let mut now = Vec::with_capacity(self.accounts.len());
        for chunk in self.accounts.chunks(100) {
            let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
            for data in rpc_accounts(&refs)? {
                now.push(data.map_or(0.0, |d| self.position(&d)));
            }
        }
        self.history.push_back((slot, now));
        while self
            .history
            .front()
            .is_some_and(|(s, _)| s + self.keep_slots < slot)
        {
            self.history.pop_front();
        }
        Ok(())
    }

    /// Each account's position now less its position `back` slots ago (the
    /// oldest read kept, if that's later).
    pub fn changes(&self, back: u64) -> Vec<f64> {
        let Some((slot, now)) = self.history.back() else {
            return Vec::new();
        };
        let then = self
            .history
            .iter()
            .find(|(s, _)| s + back >= *slot)
            .map_or(now, |(_, p)| p);
        now.iter().zip(then).map(|(a, b)| a - b).collect()
    }

    /// Each account's change over the last read.
    pub fn last_changes(&self) -> Vec<f64> {
        let n = self.history.len();
        if n < 2 {
            return vec![0.0; self.accounts.len()];
        }
        let (now, then) = (&self.history[n - 1].1, &self.history[n - 2].1);
        now.iter().zip(then).map(|(a, b)| a - b).collect()
    }
}

/// Where the session stands around its headline, from the oracle alone.
#[derive(Debug, Clone, Default)]
pub struct Headline {
    history: VecDeque<(u64, f64)>,
    /// When the jump was seen, and its way (`1.0` up).
    pub jumped: Option<(u64, f64)>,
    /// When the jump was done, and the oracle then.
    pub settled: Option<(u64, f64)>,
    /// When the verdict was seen, and its way.
    pub verdict: Option<(u64, f64)>,
}

impl Headline {
    /// The oracle `back` slots before the latest read (the oldest kept, if
    /// that's later).
    fn back(&self, back: u64) -> f64 {
        let (at, now) = *self.history.back().expect("read first");
        self.history
            .iter()
            .find(|(s, _)| s + back >= at)
            .map_or(now, |(_, o)| *o)
    }

    /// The slots a flow is summed over at `at`: [`FLOW_SLOTS`], or fewer
    /// since the jump was done (none before), so the jump's own flow never
    /// counts.
    pub fn flow_slots(&self, at: u64) -> u64 {
        self.settled
            .map_or(0, |(settled, _)| FLOW_SLOTS.min(at.saturating_sub(settled)))
    }

    pub fn update(&mut self, at: u64, oracle: f64) {
        self.history.push_back((at, oracle));
        while self.history.front().is_some_and(|(s, _)| s + 64 < at) {
            self.history.pop_front();
        }
        match (self.jumped, self.settled, self.verdict) {
            (None, ..) => {
                let moved = oracle / self.back(30) - 1.0;
                if at + 20 >= EVENT_FROM && moved.abs() > 0.005 {
                    self.jumped = Some((at, moved.signum()));
                    log(&format!(
                        "slot {at}: the headline jumped {:+.2}%",
                        moved * 100.0
                    ));
                }
            }
            (Some((jumped, _)), None, _) => {
                let recent = self.history.iter().filter(|(s, _)| s + 12 >= at);
                let (lo, hi) = recent.fold((f64::MAX, f64::MIN), |(lo, hi), (_, o)| {
                    (lo.min(*o), hi.max(*o))
                });
                if at >= jumped + 12 && hi / lo - 1.0 < 0.0008 {
                    self.settled = Some((at, oracle));
                    log(&format!("slot {at}: the jump is done at {oracle:.2}"));
                }
            }
            (Some(_), Some((settled, _)), None) => {
                let moved = oracle / self.back((at - settled).min(20)) - 1.0;
                if moved.abs() > 0.0015 {
                    self.verdict = Some((at, moved.signum()));
                    log(&format!(
                        "slot {at}: the verdict moved {:+.2}%",
                        moved * 100.0
                    ));
                }
            }
            _ => {}
        }
    }
}

/// The side the bot holds: set by a signal until the verdict, closed six
/// slots after it, and then done.
#[derive(Debug, Clone, Default)]
pub struct Hold {
    pub dir: f64,
    pub done: bool,
}

impl Hold {
    pub fn want(&mut self, dir: f64, at: u64) {
        if !self.done && dir != self.dir {
            log(&format!("slot {at}: side {dir:+}"));
            self.dir = dir;
        }
    }

    /// Closes once the verdict is six slots old.
    pub fn manage(&mut self, at: u64, headline: &Headline) {
        if let Some((v, _)) = headline.verdict
            && at >= v + AFTER_VERDICT_SLOTS
            && !self.done
        {
            log(&format!("slot {at}: out after the verdict"));
            self.dir = 0.0;
            self.done = true;
        }
    }

    pub fn target(&self, market: &Market) -> f64 {
        if market.last {
            0.0
        } else {
            self.dir * market.cap
        }
    }
}

/// The side a weighted flow calls for, past [`FLOW_UNITS`].
pub fn flow_side(net: f64) -> Option<f64> {
    (net.abs() >= FLOW_UNITS).then(|| net.signum())
}

/// One step's takes: when, the oracle then, and each account's change.
type Takes = (u64, f64, Vec<(usize, f64)>);

/// Each account's takes, marked out against the oracle.
#[derive(Debug, Clone, Default)]
pub struct Markouts {
    pending: VecDeque<Takes>,
    /// Takes, and their markouts' sum and sum of squares, in basis points.
    stats: Vec<(u32, f64, f64)>,
    /// Each account's reading: `1.0` leads, `-1.0` lags, `0.0` neither.
    read: Vec<f64>,
}

impl Markouts {
    pub fn new(accounts: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            stats: vec![(0, 0.0, 0.0); accounts],
            read: vec![0.0; accounts],
        }
    }

    /// The step's changes in position, and the oracle: takes to mark out,
    /// and the takes old enough marked out.
    pub fn update(&mut self, at: u64, oracle: f64, changes: &[f64]) {
        while self
            .pending
            .front()
            .is_some_and(|(then, ..)| then + MARKOUT_SLOTS <= at)
        {
            let (_, was, takes) = self.pending.pop_front().expect("a take is pending");
            let moved = (oracle / was - 1.0) * 10_000.0;
            for (i, change) in takes {
                let m = change.signum() * moved;
                let s = &mut self.stats[i];
                *s = (s.0 + 1, s.1 + m, s.2 + m * m);
            }
        }
        let takes: Vec<(usize, f64)> = changes
            .iter()
            .enumerate()
            .filter(|(_, c)| c.abs() > 1e-9)
            .map(|(i, c)| (i, *c))
            .collect();
        if !takes.is_empty() {
            self.pending.push_back((at, oracle, takes));
        }
    }

    /// `1.0` for an account that leads the price, `-1.0` for one that lags
    /// it, `0.0` for the rest. An account is read at [`MIN_T`] standard
    /// errors and kept until it falls under [`KEEP_T`].
    pub fn weights(&mut self) -> Vec<f64> {
        for (read, &(n, sum, sq)) in self.read.iter_mut().zip(&self.stats) {
            if n < MIN_TAKES {
                continue;
            }
            let n = f64::from(n);
            let mean = sum / n;
            let sd = ((sq / n - mean * mean).max(0.0) * n / (n - 1.0))
                .sqrt()
                .max(1.0);
            let t = mean / (sd / n.sqrt());
            *read = if t >= MIN_T {
                1.0
            } else if t <= -MIN_T {
                -1.0
            } else if *read * t >= KEEP_T {
                *read
            } else {
                0.0
            };
        }
        self.read.clone()
    }
}

/// The strategy.
#[derive(Default)]
pub struct Reference {
    start_slot: u64,
    watch: Watch,
    headline: Headline,
    markouts: Markouts,
    hold: Hold,
    known: usize,
    /// The side the flow has read, and for how many steps running.
    reading: (f64, u32),
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.start_slot = session.start_slot;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let own = venue.trader_account(&key(&session.authority)?).to_string();
        let accounts = Watch::every_trader(&own)?;
        self.markouts = Markouts::new(accounts.len());
        self.watch = Watch::new(&venue, &session.symbol, accounts, FLOW_SLOTS + 20)?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let at = market.slot.saturating_sub(self.start_slot);
        orders.cancel_all();
        self.watch.read(at)?;
        self.headline.update(at, market.oracle);
        if self.headline.settled.is_some() && self.headline.verdict.is_none() {
            self.markouts
                .update(at, market.oracle, &self.watch.last_changes());
            let weights = self.markouts.weights();
            let known = weights.iter().filter(|w| **w != 0.0).count();
            if known != self.known {
                log(&format!("slot {at}: {known} accounts read"));
                self.known = known;
            }
            if known > 0 {
                let net: f64 = self
                    .watch
                    .changes(self.headline.flow_slots(at))
                    .iter()
                    .zip(&weights)
                    .map(|(c, w)| c * w)
                    .sum();
                let side = flow_side(net).unwrap_or(0.0);
                self.reading = if side != 0.0 && side == self.reading.0 {
                    (side, self.reading.1 + 1)
                } else {
                    (side, u32::from(side != 0.0))
                };
                // Enter once the reading holds; never turn round after.
                if self.hold.dir == 0.0 && self.reading.1 >= CONFIRM_STEPS {
                    self.hold.want(side, at);
                }
            }
        }
        self.hold.manage(at, &self.headline);
        if mid(market).is_some() {
            go_to(self.hold.target(market), market, orders);
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Reference);
