//! Reference: make markets while the tape is noise, and lean with the flow
//! once the tape shows it is informed.
//!
//! Every Phoenix trader account is on chain, so the bot lists them once
//! (`getProgramAccounts`) and reads each one's TSLA position every step.
//!
//! - **The tape.** Each account's change in position at a step is a trade.
//!   An account that trades at most of the steps anyone does is a maker,
//!   on the other side of the takes; every other account's change is a
//!   take.
//! - **The read.** Each step's takes are marked out against the oracle 60
//!   slots later, in basis points, the takes' way, volume-weighted. Once 30
//!   steps are marked, a mean markout over 4 bp says the morning is toxic:
//!   the takers know where the price goes.
//! - **Quiet.** Rest a bid at the best bid and an ask at the best ask, one
//!   TSLA each, post-only; past 4 TSLA only the side that reduces.
//! - **Toxic.** The takers' net buying over the last 20 slots past 1.5
//!   TSLA either way sets the side, with the cap, held 40 slots after the
//!   last signal.
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

/// How far past the price a taking order may fill, as a share.
pub const TAKE_REACH: f64 = 0.006;
/// Slots a flow is summed over, the net, in TSLA, that sets a side, and how
/// long a side holds after its last signal.
pub const FLOW_SLOTS: u64 = 20;
pub const FLOW_UNITS: f64 = 1.5;
pub const HOLD_SLOTS: u64 = 40;
/// A quote's size, and the inventory past which only the side that reduces
/// it is quoted.
pub const QUOTE_SIZE: f64 = 1.0;
pub const QUOTE_MAX: f64 = 4.0;
/// Slots a take is marked out over, the marked steps a read needs, and the
/// mean markout, in basis points, past which the takers are informed.
const MARKOUT_SLOTS: u64 = 60;
const MIN_STEPS: u32 = 30;
const TOXIC_BPS: f64 = 4.0;
/// The share of trading steps past which an account is a maker, and the
/// trading steps before anyone is called one.
const MAKER_SHARE: f64 = 0.3;
const MAKER_MIN_STEPS: u32 = 20;
/// Slots before the end the desk goes flat.
const CLOSE_SLOTS: u64 = 12;

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

/// Rest a bid at the best bid and an ask at the best ask, post-only, of
/// [`QUOTE_SIZE`] each; past [`QUOTE_MAX`] of inventory, only the side that
/// reduces it.
pub fn make(market: &Market, orders: &mut Orders) {
    let (Some(bid), Some(ask)) = (market.best_bid, market.best_ask) else {
        return;
    };
    let held = market.position.size;
    if held < QUOTE_MAX {
        orders.bid(bid, QUOTE_SIZE.min(market.cap - held)).post_only();
    }
    if held > -QUOTE_MAX {
        orders.ask(ask, QUOTE_SIZE.min(market.cap + held)).post_only();
    }
}

/// Whether the session is close enough to its end to go flat.
pub fn closing(market: &Market) -> bool {
    market.last || market.slots_left <= CLOSE_SLOTS
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

/// Who takes and who makes, from how often each account trades.
#[derive(Debug, Clone, Default)]
pub struct Takers {
    /// Steps at which anyone traded, and at which each account did.
    steps: u32,
    traded: Vec<u32>,
}

impl Takers {
    pub fn new(accounts: usize) -> Self {
        Self {
            steps: 0,
            traded: vec![0; accounts],
        }
    }

    pub fn update(&mut self, changes: &[f64]) {
        if changes.iter().all(|c| c.abs() < 1e-9) {
            return;
        }
        self.steps += 1;
        for (n, c) in self.traded.iter_mut().zip(changes) {
            if c.abs() >= 1e-9 {
                *n += 1;
            }
        }
    }

    /// `1.0` for each account that takes, `0.0` for each that makes (or
    /// before any trading has been seen).
    pub fn weights(&self) -> Vec<f64> {
        if self.steps < MAKER_MIN_STEPS {
            return vec![0.0; self.traded.len()];
        }
        let steps = f64::from(self.steps);
        self.traded
            .iter()
            .map(|&n| {
                if f64::from(n) > MAKER_SHARE * steps {
                    0.0
                } else {
                    1.0
                }
            })
            .collect()
    }
}

/// The takers' net buying in `changes`, by `weights`.
pub fn net(changes: &[f64], weights: &[f64]) -> f64 {
    changes.iter().zip(weights).map(|(c, w)| c * w).sum()
}

/// The side a net flow calls for, past `units`.
pub fn flow_side(net: f64, units: f64) -> Option<f64> {
    (net.abs() >= units).then(|| net.signum())
}

/// The market's takes, marked out against the oracle.
#[derive(Debug, Clone, Default)]
pub struct Markouts {
    /// Steps with takes: when, the oracle then, and the takes.
    pending: VecDeque<(u64, f64, Vec<f64>)>,
    /// Marked steps, and their volume-weighted markouts' sum and sum of
    /// squares, in basis points.
    steps: u32,
    sum: f64,
    sq: f64,
}

impl Markouts {
    /// The step's takes (signed sizes), and the oracle: takes to mark out,
    /// and the takes old enough marked out.
    pub fn update(&mut self, at: u64, oracle: f64, takes: Vec<f64>) {
        while self
            .pending
            .front()
            .is_some_and(|(then, ..)| then + MARKOUT_SLOTS <= at)
        {
            let (_, was, takes) = self.pending.pop_front().expect("a take is pending");
            let moved = (oracle / was - 1.0) * 10_000.0;
            let volume: f64 = takes.iter().map(|t| t.abs()).sum();
            let m = takes.iter().map(|t| t * moved).sum::<f64>() / volume;
            self.steps += 1;
            self.sum += m;
            self.sq += m * m;
        }
        if takes.iter().any(|t| t.abs() > 1e-9) {
            self.pending.push_back((at, oracle, takes));
        }
    }

    /// The mean markout so far, once enough steps are marked.
    pub fn mean(&self) -> Option<f64> {
        (self.steps >= MIN_STEPS).then(|| self.sum / f64::from(self.steps))
    }

    /// Whether the takers are informed: `None` until enough is marked.
    pub fn toxic(&self) -> Option<bool> {
        self.mean().map(|m| m > TOXIC_BPS)
    }
}

/// The side the bot leans: set by a signal, held [`HOLD_SLOTS`] after it.
#[derive(Debug, Clone, Default)]
pub struct Lean {
    pub dir: f64,
    until: u64,
}

impl Lean {
    pub fn signal(&mut self, dir: f64, at: u64) {
        if dir != self.dir {
            log(&format!("slot {at}: lean {dir:+}"));
        }
        self.dir = dir;
        self.until = at + HOLD_SLOTS;
    }

    pub fn manage(&mut self, at: u64) {
        if self.dir != 0.0 && at > self.until {
            log(&format!("slot {at}: lean over"));
            self.dir = 0.0;
        }
    }
}

/// The whole market's tape, as any reader of the chain has it: every
/// account's position each step, who takes, and the takes' markouts.
#[derive(Debug, Clone, Default)]
pub struct Tape {
    pub watch: Watch,
    pub takers: Takers,
    pub markouts: Markouts,
    weights: Vec<f64>,
}

impl Tape {
    pub fn start(session: &Session) -> BotResult<Self> {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let own = venue.trader_account(&key(&session.authority)?).to_string();
        let accounts = Watch::every_trader(&own)?;
        log(&format!("{} trader accounts", accounts.len()));
        let n = accounts.len();
        Ok(Self {
            watch: Watch::new(&venue, &session.symbol, accounts, FLOW_SLOTS + 20)?,
            takers: Takers::new(n),
            markouts: Markouts::default(),
            weights: vec![0.0; n],
        })
    }

    pub fn read(&mut self, at: u64, oracle: f64) -> BotResult {
        self.watch.read(at)?;
        let last = self.watch.last_changes();
        self.takers.update(&last);
        self.weights = self.takers.weights();
        let takes: Vec<f64> = last.iter().zip(&self.weights).map(|(c, w)| c * w).collect();
        self.markouts.update(at, oracle, takes);
        Ok(())
    }

    /// The takers' net buying over the last [`FLOW_SLOTS`].
    pub fn net(&self) -> f64 {
        net(&self.watch.changes(FLOW_SLOTS), &self.weights)
    }
}

/// What the desk does at a step, by what it reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Policy {
    /// Read the tape: make while it is noise, follow once it is toxic.
    Read,
    /// Never trade.
    Flat,
    /// Make all session.
    Make,
    /// Follow the net flow past `units` all session.
    Follow(f64),
    /// Follow the net flow past `units`, and make between.
    Switch(f64),
}

/// The desk: a tape, a lean, and a policy.
pub struct Desk {
    pub policy: Policy,
    start_slot: u64,
    tape: Tape,
    lean: Lean,
    said: Option<bool>,
    counts: BTreeMap<&'static str, u64>,
}

impl Desk {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            start_slot: 0,
            tape: Tape::default(),
            lean: Lean::default(),
            said: None,
            counts: BTreeMap::new(),
        }
    }
}

impl Default for Desk {
    fn default() -> Self {
        Self::new(Policy::Read)
    }
}

impl Strategy for Desk {
    fn start(&mut self, session: &Session) -> BotResult {
        self.start_slot = session.start_slot;
        if self.policy != Policy::Flat {
            self.tape = Tape::start(session)?;
        }
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let at = market.slot.saturating_sub(self.start_slot);
        orders.cancel_all();
        if self.policy == Policy::Flat {
            return Ok(());
        }
        self.tape.read(at, market.oracle)?;
        let toxic = self.tape.markouts.toxic();
        if toxic != self.said {
            log(&format!(
                "slot {at}: mean markout {:?} bp, toxic {toxic:?}",
                self.tape.markouts.mean()
            ));
            self.said = toxic;
        }
        let follow = match self.policy {
            Policy::Read => (toxic == Some(true)).then_some(FLOW_UNITS),
            Policy::Follow(units) | Policy::Switch(units) => Some(units),
            Policy::Make | Policy::Flat => None,
        };
        if let Some(units) = follow
            && let Some(dir) = flow_side(self.tape.net(), units)
        {
            self.lean.signal(dir, at);
        }
        self.lean.manage(at);
        if closing(market) {
            go_to(0.0, market, orders);
            return Ok(());
        }
        let quotes = match self.policy {
            Policy::Read => toxic != Some(true),
            Policy::Make | Policy::Switch(_) => true,
            Policy::Follow(_) | Policy::Flat => false,
        };
        if self.lean.dir != 0.0 {
            *self.counts.entry("lean_steps").or_default() += 1;
            go_to(self.lean.dir * market.cap, market, orders);
        } else if quotes {
            *self.counts.entry("quote_steps").or_default() += 1;
            if market.position.size.abs() > market.cap * 0.75 {
                go_to(0.0, market, orders);
            } else {
                make(market, orders);
            }
        } else {
            go_to(0.0, market, orders);
        }
        if market.last {
            log(&format!("counts {:?}", self.counts));
        }
        Ok(())
    }
}

/// The reference strategy.
pub type Reference = Desk;

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Reference);
