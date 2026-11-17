//! Reference: learn who pushes the book through round levels, then ride
//! and fade a stop hunter's runs and follow an informed breaker's.
//!
//! Every Phoenix trader account is on chain, so the bot lists them once
//! (`getProgramAccounts`) and reads each one's TSLA position every step. A
//! stop hunter's push and an informed breaker's sweep look the same: one
//! large clip (2.4 to 4.8 TSLA) through a level from an account that came
//! into the session flat. What tells them apart is who sent it, and that the
//! bot learns: once a push's run has played out, the oracle has either
//! followed it (a breaker) or not (a hunter), and the account keeps that
//! name for the session.
//!
//! - **A known hunter's push.** It rides the push into the book's run with
//!   the cap, turns once the book is through the level and fades the run
//!   with the cap, and covers when the book comes back to the level.
//! - **A known breaker's sweep.** It follows with the cap at once and holds
//!   until the oracle has moved and stalled.
//! - **A push from an account it doesn't know yet** it leaves alone: it
//!   could be either.
//!
//! It isn't told who anyone is.

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
/// How far the oracle must have followed a push, in basis points, once its
/// run has had `SETTLE_SLOTS` to play out, for its account to be a breaker.
const FOLLOWED_BPS: f64 = 20.0;
const SETTLE_SLOTS: u64 = 90;
/// How far the book must run past where it was at a push to be through.
const THROUGH: f64 = 0.0008;
/// Slots a push waits for the book to run.
const PUSH_WAIT_SLOTS: u64 = 12;

/// What an account turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Hunter,
    Breaker,
}

/// A push seen: who, which way, when, and the mid and oracle then.
#[derive(Debug, Clone, Copy)]
struct Push {
    account: usize,
    dir: f64,
    at: u64,
    mid: f64,
    oracle: f64,
}

/// The strategy.
#[derive(Default)]
pub struct Reference {
    cap: f64,
    step: f64,
    watch: Watch,
    /// Accounts that held TSLA before the session: not its traders.
    old: Vec<bool>,
    /// What each account turned out to be.
    roles: BTreeMap<usize, Role>,
    /// Pushes whose runs haven't played out yet.
    pending: Vec<Push>,
    /// A known hunter's push being ridden.
    riding: Option<Push>,
    trade: Option<Trade>,
}

impl Reference {
    /// Each push's run that has played out names its account.
    fn learn(&mut self, slot: u64, oracle: f64) {
        let (done, open): (Vec<Push>, Vec<Push>) = self
            .pending
            .iter()
            .partition(|p| slot >= p.at + SETTLE_SLOTS);
        self.pending = open;
        for p in done {
            let followed = p.dir * (oracle / p.oracle - 1.0) * 10_000.0 >= FOLLOWED_BPS;
            let role = if followed { Role::Breaker } else { Role::Hunter };
            if self.roles.insert(p.account, role) != Some(role) {
                log(&format!(
                    "slot {slot}: account {} is a {role:?}",
                    self.watch.accounts[p.account]
                ));
            }
        }
    }

    /// This step's pushes: a large clip from one of the session's own
    /// accounts that was flat before it.
    fn pushes(&self, slot: u64, mid: f64, oracle: f64) -> Vec<Push> {
        let now = self.watch.now();
        self.watch
            .last_changes()
            .into_iter()
            .enumerate()
            .filter(|(i, c)| {
                c.abs() >= PUSH_UNITS && !self.old[*i] && (now[*i] - c).abs() < 0.01
            })
            .map(|(account, c)| Push {
                account,
                dir: c.signum(),
                at: slot,
                mid,
                oracle,
            })
            .collect()
    }
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.cap;
        self.step = LEVEL_STEP_USD;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let own = venue.trader_account(&key(&session.authority)?).to_string();
        let accounts = Watch::every_trader(&own)?;
        log(&format!("watching {} trader accounts", accounts.len()));
        self.watch = Watch::new(&venue, &session.symbol, accounts, 20)?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let slot = market.slot;
        orders.cancel_all();
        self.watch.read(slot)?;
        if self.old.is_empty() {
            self.old = self.watch.first.iter().map(|p| p.abs() > 1e-9).collect();
        }
        let oracle = market.oracle;
        self.learn(slot, oracle);
        let Some(mid) = mid(market) else {
            return Ok(());
        };

        for push in self.pushes(slot, mid, oracle) {
            self.pending.push(push);
            if self.trade.is_some() || self.riding.is_some() {
                continue;
            }
            match self.roles.get(&push.account) {
                Some(Role::Hunter) => {
                    log(&format!("slot {slot}: ride a known hunter's push {:+}", push.dir));
                    self.riding = Some(push);
                }
                Some(Role::Breaker) => {
                    log(&format!("slot {slot}: follow a known breaker's sweep {:+}", push.dir));
                    let stop = mid * (1.0 - push.dir * 0.006);
                    self.trade = Some(Trade::new(Bet::Follow, push.dir, stop, slot, oracle));
                }
                None => {}
            }
        }
        if let Some(push) = self.riding {
            if push.dir * (mid / push.mid - 1.0) >= THROUGH {
                let level = if push.dir > 0.0 {
                    (push.mid / self.step).ceil() * self.step
                } else {
                    (push.mid / self.step).floor() * self.step
                };
                self.trade = Some(Trade::new(Bet::Fade, push.dir, level, slot, oracle));
                self.riding = None;
            } else if slot > push.at + PUSH_WAIT_SLOTS {
                self.riding = None;
            }
        }
        if let Some(trade) = &mut self.trade {
            if trade.done(slot, mid, oracle) {
                self.trade = None;
            }
        }
        let target = match (&self.trade, self.riding, market.last) {
            (_, _, true) => 0.0,
            (Some(trade), _, _) => trade.target(self.cap),
            (None, Some(push), _) => push.dir * self.cap,
            _ => 0.0,
        };
        go_to(target, market, orders);
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Reference);
