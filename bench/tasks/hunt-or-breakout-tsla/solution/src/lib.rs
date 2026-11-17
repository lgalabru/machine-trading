//! Reference: when the book runs through a round level, read who ran it,
//! then fade a stop hunt back to the level or follow a real break.
//!
//! Every Phoenix trader account is on chain, so the bot lists them once
//! (`getProgramAccounts`) and reads each one's TSLA position every step. It
//! reads only the takers that came into this session: an account that held
//! TSLA before the session started is the snapshot's, and one whose
//! position moves against the book step after step is providing liquidity,
//! not leading anything. A run through a level is then classified from
//! three things:
//!
//! - **The leader.** One account that took a large clip in a single step
//!   just before the run is a pusher. A hunter then goes quiet until it
//!   trades back; an informed trader who happened to take a large clip
//!   keeps trading the run's way, so the bot watches the pusher for a few
//!   slots. An account that pushed a run earlier in the session and then
//!   traded straight back out of it is a known hunter: a run it leads is
//!   faded at once, whoever else joins.
//! - **The crowd.** Two or more accounts that built a position the run's
//!   way over the half-minute before it (before anyone chasing the run
//!   could) are informed flow.
//! - **The oracle.** A hunter can push the book but not the oracle. When
//!   nobody pushed and no crowd led, or a pusher and a crowd both did, the
//!   bot waits up to 30 slots: an oracle that follows means a break; one
//!   that stays put while a pusher's book is still through the level means
//!   a hunt. A run nobody led that the oracle doesn't follow (the walk
//!   drifting through a level) isn't traded.
//!
//! A hunt is faded with the cap and covered once the book comes back to the
//! level (or the hold has clearly passed); a break is followed with the cap
//! and held until the oracle has moved and stopped, with a stop if the book
//! falls back through the level.
//!
//! It isn't told who anyone is: it learns the hunter from what it does, and
//! the informed accounts only as a crowd.

use std::collections::{BTreeMap, VecDeque};

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

/// The grid of round levels, in USD, when the session doesn't say.
pub const LEVEL_STEP_USD: f64 = 2.0;
/// How far past the price a taking order may fill, as a share.
pub const TAKE_REACH: f64 = 0.006;
/// Slots a trade is held at most.
pub const MAX_HOLD_SLOTS: u64 = 220;

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

/// Runs of the book through the levels of a grid.
#[derive(Debug, Clone, Default)]
pub struct Crosses {
    pub step: f64,
    last: Option<f64>,
}

impl Crosses {
    pub fn new(step: f64) -> Self {
        Self { step, last: None }
    }

    /// The level the mid ran through since the last look, and which way
    /// (`1.0` up), if it ran through one.
    pub fn update(&mut self, mid: f64) -> Option<(f64, f64)> {
        let last = self.last.replace(mid)?;
        let (a, b) = ((last / self.step).floor(), (mid / self.step).floor());
        if a == b {
            return None;
        }
        Some(if b > a {
            (b * self.step, 1.0)
        } else {
            ((b + 1.0) * self.step, -1.0)
        })
    }
}

/// A number in `MTX_REGIME`, by key.
pub fn told_number(told: &serde_json::Value, key: &str) -> Option<f64> {
    told.get(key)?.as_f64()
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
    /// Each account's position at the first read.
    pub first: Vec<f64>,
}

impl Watch {
    pub fn new(venue: &PhoenixVenue, symbol: &str, accounts: Vec<String>, keep_slots: u64) -> BotResult<Self> {
        let market = venue.market(symbol)?;
        Ok(Self {
            accounts,
            asset_id: u64::from(market.asset_id),
            lot: venue.lots_to_base_units(market, 1),
            history: VecDeque::new(),
            keep_slots,
            first: Vec::new(),
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
        if self.first.is_empty() {
            self.first = now.clone();
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

    /// Each account's change from `from` slots ago to `to` slots ago
    /// (`from` > `to`), at the reads nearest each.
    pub fn changes_between(&self, from: u64, to: u64) -> Vec<f64> {
        let Some((slot, now)) = self.history.back() else {
            return Vec::new();
        };
        let at = |back: u64| {
            self.history
                .iter()
                .find(|(s, _)| s + back >= *slot)
                .map_or(now, |(_, p)| p)
        };
        let (then, later) = (at(from), at(to));
        later.iter().zip(then).map(|(a, b)| a - b).collect()
    }

    /// Each account's position at the last read.
    pub fn now(&self) -> &[f64] {
        self.history.back().map_or(&[], |(_, p)| p.as_slice())
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

/// What a trade is betting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bet {
    /// The run comes back: short an up-run, cover at the level.
    Fade,
    /// The run keeps going: long an up-run, out once the oracle settles.
    Follow,
}

/// An open trade on one run through a level.
#[derive(Debug, Clone, Copy)]
pub struct Trade {
    pub bet: Bet,
    /// The run's way (`1.0` up) and its level.
    pub dir: f64,
    pub level: f64,
    pub opened: u64,
    /// The oracle when it opened, and its furthest the run's way since.
    pub oracle: f64,
    pub best: f64,
    pub best_at: u64,
}

impl Trade {
    pub fn new(bet: Bet, dir: f64, level: f64, slot: u64, oracle: f64) -> Self {
        Self {
            bet,
            dir,
            level,
            opened: slot,
            oracle,
            best: oracle,
            best_at: slot,
        }
    }

    /// The position it wants, at a cap of `cap`.
    pub fn target(&self, cap: f64) -> f64 {
        match self.bet {
            Bet::Fade => -self.dir * cap,
            Bet::Follow => self.dir * cap,
        }
    }

    /// Whether it is done at this step: a fade once the book is back at the
    /// level; a follow once the oracle has moved its way and stalled, or the
    /// book is back through the level; either after `MAX_HOLD_SLOTS`.
    pub fn done(&mut self, slot: u64, mid: f64, oracle: f64) -> bool {
        if self.dir * (oracle - self.best) > 0.0 {
            (self.best, self.best_at) = (oracle, slot);
        }
        if slot >= self.opened + MAX_HOLD_SLOTS {
            return true;
        }
        let through = self.dir * (mid - self.level);
        match self.bet {
            Bet::Fade => through <= 0.0,
            Bet::Follow => {
                let moved = self.dir * (self.best / self.oracle - 1.0);
                through < 0.0 || (moved > 0.002 && slot >= self.best_at + 16)
            }
        }
    }
}

/// Above this, a single step's take is a push, in base units: under the
/// smallest push the instruction states (2.4 TSLA), above all but the
/// informed traders' largest clips.
const PUSH_UNITS: f64 = 2.2;
/// A crowd: accounts that each built at least this the run's way over
/// `CROWD_SLOTS`, in base units.
const CROWD_UNITS: f64 = 0.4;
const CROWD_SLOTS: u64 = 80;
/// The crowd is read up to this many slots before the run, before chasers
/// can have reacted to it.
const CROWD_BEFORE_SLOTS: u64 = 4;
/// How long it waits on the oracle, and how far it must move.
const ORACLE_SLOTS: u64 = 30;
const ORACLE_BPS: f64 = 6.0;
/// Slots it watches a pusher after the run: a hunter goes quiet until it
/// trades back; an informed trader who took a large clip keeps trading.
const QUIET_SLOTS: u64 = 14;

/// A run it has seen and not yet classified.
#[derive(Debug, Clone, Copy)]
struct Pending {
    dir: f64,
    level: f64,
    at: u64,
    oracle: f64,
    /// Who led it, read as it ran, and the pusher's position then.
    pusher: Option<usize>,
    crowd: usize,
    pushed_to: f64,
}

/// The strategy.
#[derive(Default)]
pub struct Reference {
    cap: f64,
    crosses: Crosses,
    watch: Watch,
    /// Pushes seen, by account: the slot and the way.
    pushes: BTreeMap<usize, (u64, f64)>,
    /// Accounts seen pushing a run and then trading back out.
    hunters: Vec<usize>,
    /// Pushes that led a run, by account: the slot, awaiting a trade back.
    led: BTreeMap<usize, (u64, f64)>,
    /// Steps each account's position moved with the book, and against it.
    with_book: Vec<u32>,
    against_book: Vec<u32>,
    last_mid: Option<f64>,
    pending: Option<Pending>,
    trade: Option<Trade>,
    /// The last level traded, and when: a run back through it soon after
    /// is the same run.
    last_level: Option<(f64, u64)>,
}

impl Reference {
    /// The account that pushed the run's way in the last few slots, if one
    /// did.
    fn pusher(&self, dir: f64, slot: u64) -> Option<usize> {
        self.pushes
            .iter()
            .filter(|(i, (at, way))| *way == dir && at + 12 >= slot && self.taker(**i))
            .map(|(i, _)| *i)
            .next()
    }

    /// Whether an account is one of the session's takers: new this session
    /// (no TSLA before it), and not one whose position keeps moving against
    /// the book, as a maker's does.
    fn taker(&self, i: usize) -> bool {
        let new = self.watch.first.get(i).is_some_and(|p| p.abs() < 1e-9);
        let (with, against) = (self.with_book[i], self.against_book[i]);
        new && !(against >= 4 && against > 3 * with)
    }

    /// Who led a run through a level the mid just crossed `dir`.
    fn leaders(&self, dir: f64, slot: u64) -> (Option<usize>, usize) {
        let pusher = self.pusher(dir, slot);
        let crowd = self
            .watch
            .changes_between(CROWD_SLOTS, CROWD_BEFORE_SLOTS)
            .iter()
            .enumerate()
            .filter(|(i, c)| Some(*i) != pusher && self.taker(*i) && dir * **c >= CROWD_UNITS)
            .count();
        (pusher, crowd)
    }

    fn classify(&mut self, run: Pending, slot: u64, oracle: f64, mid: f64) -> Option<Bet> {
        let moved = run.dir * (oracle / run.oracle - 1.0) * 10_000.0;
        let Some(p) = run.pusher else {
            // No one pushed: a break if a crowd led it or the oracle follows.
            return (run.crowd >= 2 || moved >= ORACLE_BPS).then_some(Bet::Follow);
        };
        if self.hunters.contains(&p) {
            return Some(Bet::Fade);
        }
        // Did the pusher keep trading the run's way?
        let kept = self
            .watch
            .now()
            .get(p)
            .is_some_and(|now| run.dir * (now - run.pushed_to) >= 0.1);
        if kept {
            return Some(Bet::Follow);
        }
        if slot < run.at + QUIET_SLOTS {
            return None;
        }
        if run.crowd < 2 {
            return (run.dir * (mid - run.level) > 0.0).then_some(Bet::Fade);
        }
        if moved >= ORACLE_BPS {
            return Some(Bet::Follow);
        }
        if slot >= run.at + ORACLE_SLOTS {
            return (run.dir * (mid - run.level) > 0.0).then_some(Bet::Fade);
        }
        None
    }

    fn learn(&mut self, slot: u64, mid: Option<f64>) {
        let changes = self.watch.last_changes();
        if self.with_book.len() != changes.len() {
            self.with_book = vec![0; changes.len()];
            self.against_book = vec![0; changes.len()];
        }
        let moved = match (self.last_mid, mid) {
            (Some(a), Some(b)) => b - a,
            _ => 0.0,
        };
        if mid.is_some() {
            self.last_mid = mid;
        }
        for (i, c) in changes.into_iter().enumerate() {
            if c.abs() >= 0.05 && moved != 0.0 {
                if c.signum() == moved.signum() {
                    self.with_book[i] += 1;
                } else {
                    self.against_book[i] += 1;
                }
            }
            if c.abs() < PUSH_UNITS {
                continue;
            }
            let way = c.signum();
            if let Some((at, led)) = self.led.get(&i).copied() {
                if led == -way && at + 120 >= slot {
                    if !self.hunters.contains(&i) {
                        log(&format!(
                            "slot {slot}: account {} pushed a run and came back",
                            self.watch.accounts[i]
                        ));
                        self.hunters.push(i);
                    }
                    self.led.remove(&i);
                    continue;
                }
            }
            self.pushes.insert(i, (slot, way));
        }
    }
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.cap;
        let step = session
            .var("MTX_LEVEL_STEP_USD")
            .and_then(|v| v.parse().ok())
            .unwrap_or(LEVEL_STEP_USD);
        self.crosses = Crosses::new(step);
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let own = venue.trader_account(&key(&session.authority)?).to_string();
        let accounts = Watch::every_trader(&own)?;
        log(&format!("watching {} trader accounts", accounts.len()));
        self.watch = Watch::new(&venue, &session.symbol, accounts, CROWD_SLOTS + 20)?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let slot = market.slot;
        orders.cancel_all();
        self.watch.read(slot)?;
        let now = mid(market);
        self.learn(slot, now);
        let Some(mid) = now else {
            return Ok(());
        };
        let oracle = market.oracle;
        let cross = self.crosses.update(mid);

        if let Some(trade) = &mut self.trade {
            if trade.done(slot, mid, oracle) || market.last {
                self.trade = None;
            }
        } else if let Some((level, dir)) = cross {
            let again = self
                .last_level
                .is_some_and(|(l, at)| (l - level).abs() < 1e-6 && at + 300 > slot);
            if !again && self.pending.is_none() {
                let (pusher, crowd) = self.leaders(dir, slot);
                let pushed_to = pusher
                    .and_then(|p| self.watch.now().get(p).copied())
                    .unwrap_or(0.0);
                if let Some(p) = pusher {
                    self.led.insert(p, (slot, dir));
                }
                self.pending = Some(Pending {
                    dir,
                    level,
                    at: slot,
                    oracle,
                    pusher,
                    crowd,
                    pushed_to,
                });
            }
        }
        if let Some(run) = self.pending {
            match self.classify(run, slot, oracle, mid) {
                Some(bet) => {
                    log(&format!("slot {slot}: {bet:?} the run {:+} through {}", run.dir, run.level));
                    self.trade = Some(Trade::new(bet, run.dir, run.level, slot, run.oracle));
                    self.last_level = Some((run.level, slot));
                    self.pending = None;
                }
                None if slot >= run.at + ORACLE_SLOTS => {
                    self.pending = None
                }
                None => {}
            }
        }
        let target = match (&self.trade, market.last) {
            (Some(trade), false) => trade.target(self.cap),
            _ => 0.0,
        };
        go_to(target, market, orders);
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Reference);
