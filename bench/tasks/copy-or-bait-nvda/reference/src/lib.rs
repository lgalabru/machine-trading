//! Copy a public wallet's runs of trading, and stop before it is bait.
//!
//! The leader's wallet is public (`MTX_PUBLIC_WALLETS`). Each step the bot
//! reads the leader's new transactions (`getSignaturesForAddress` on the
//! wallet, then `getTransaction`: the side and size of each order that
//! takes) and groups its takes into runs: a run starts with a take on the
//! other side, or `run_slots` after the last run's start. A copier takes a
//! run's side at the cap, once, and holds it `hold_slots`; it never flips
//! while it holds.
//!
//! On a bait day the owner moves its views elsewhere once one account has
//! followed enough of its runs within a window after each start, and the
//! wallet goes on trading at the same pace on coin-flip views. The rungs
//! ([`Mode`]) differ in how they copy:
//!
//! - **reference**: copies every run early, and after each run it can
//!   score, updates the odds that the wallet has turned since its own
//!   copying began: right 0.85 of the time on genuine views, 0.5 after the
//!   turn, which it expects soon after its own third or fourth followed run.
//!   Once those odds pass `turn_odds`, it stops copying.
//! - **informed** (told the bait rule as `MTX_REGIME`, never the day):
//!   follows at most `matches - 1` of any `episodes` runs inside the window,
//!   and copies the rest just after the window closes, so it never sets the
//!   bait itself.
//! - **informed-day** (a diagnostic, told the whole draw): copies every run
//!   early on a genuine day, and as the informed bot does on a bait day.
//! - **copy-all**, **copy-1in8**, **copy-late**, **copy-small**,
//!   **safe-blind**, **twap**, **nop**: the ladder's rungs that don't read
//!   the leader's record (see [`Mode`]).

use std::collections::{HashSet, VecDeque};

use mtx_kit::{
    log,
    prelude::*,
    rpc,
    venue::{
        PhoenixVenue,
        phoenix_rise::{
            ix::{PhoenixInstruction, order_packet::OrderPacket},
            types::exchange::ExchangeMarketConfig,
        },
    },
};
use serde_json::{Value, json};

/// Which rung the machinery runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Nop,
    Reference,
    /// Told the bait rule.
    Informed,
    /// Told the session's whole draw, the day included: a diagnostic only.
    InformedDay,
    /// Copies every run early.
    CopyAll,
    /// Copies one run in each eight early, skips the rest.
    CopyOneInEight,
    /// Copies every run once the widest window the instruction states is
    /// over.
    CopyLate,
    /// Copies every run early, at a size under the narrowest share the
    /// instruction states.
    CopySmall,
    /// Copies early as many runs as the smallest stated rule allows, and
    /// every other run once the widest window is over.
    SafeBlind,
    /// Buys toward the cap evenly over the session.
    Twap,
}

impl Mode {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "nop" => Self::Nop,
            "reference" => Self::Reference,
            "informed" => Self::Informed,
            "informed-day" => Self::InformedDay,
            "copy-all" => Self::CopyAll,
            "copy-1in8" => Self::CopyOneInEight,
            "copy-late" => Self::CopyLate,
            "copy-small" => Self::CopySmall,
            "safe-blind" => Self::SafeBlind,
            "twap" => Self::Twap,
            _ => return None,
        })
    }

    /// The rung a build was made for (`MTX_COB_RUNG`), the reference if
    /// none.
    pub fn built() -> Self {
        option_env!("MTX_COB_RUNG")
            .and_then(Self::parse)
            .unwrap_or(Self::Reference)
    }
}

/// The ranges the instruction states.
pub const WIDEST_WINDOW: u64 = 40;
pub const NARROWEST_SHARE: f64 = 0.6;
pub const FEWEST_MATCHES: u32 = 3;
pub const EPISODES: u32 = 8;

/// The rung's knobs.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// A run ends this long after it starts, for runs the bot counts.
    pub run_slots: u64,
    /// How long a copy is held from its entry.
    pub hold_slots: u64,
    /// Slots a late copy waits after a run's start.
    pub late_slots: u64,
    /// Slots of margin around the leader's window.
    pub margin: u64,
    /// The reference: a run's markout, over `mark_slots`, counts as right
    /// past `cut_bps` its way.
    pub mark_slots: u64,
    pub cut_bps: f64,
    /// The reference stops copying once the odds the wallet has turned
    /// pass this.
    pub turn_odds: f64,
    /// The reference's prior that a day is a bait day.
    pub bait_prior: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            run_slots: 100,
            hold_slots: 100,
            late_slots: WIDEST_WINDOW + 4,
            margin: 4,
            mark_slots: 100,
            cut_bps: 0.0,
            turn_odds: 0.6,
            bait_prior: 0.5,
        }
    }
}

/// The bait rule, as the informed bot is told it.
#[derive(Debug, Clone, Copy)]
struct Rule {
    follow_slots: u64,
    matches: u32,
    episodes: u32,
    horizon_slots: u64,
}

impl Rule {
    fn of(value: &Value) -> Option<Self> {
        Some(Self {
            follow_slots: value["follow_slots"].as_u64()?,
            matches: u32::try_from(value["matches"].as_u64()?).ok()?,
            episodes: u32::try_from(value["episodes"].as_u64()?).ok()?,
            horizon_slots: value["horizon_slots"].as_u64()?,
        })
    }
}

/// One of the leader's runs, as the bot reads it.
#[derive(Debug, Clone)]
struct Run {
    start: u64,
    /// 1 bought, -1 sold.
    way: f64,
    /// Its first take's size, base units.
    first: f64,
    /// The mid at its start.
    mid: f64,
    /// Whether the bot traded its way inside its window.
    followed: bool,
    /// Whether the bot copied it (early or late).
    copied: bool,
    /// Whether its markout is scored yet.
    scored: bool,
}

struct Setup {
    venue: PhoenixVenue,
    market: ExchangeMarketConfig,
    leader: String,
    start_slot: u64,
    length: u64,
    cap: f64,
}

pub struct Copier {
    pub mode: Mode,
    pub params: Params,
    setup: Option<Setup>,
    rule: Option<Rule>,
    /// Told a genuine day (`informed-day` only).
    genuine: Option<bool>,
    seen: HashSet<String>,
    runs: VecDeque<Run>,
    /// Every run's outcome once scored: its index and whether it was right.
    outcomes: Vec<(usize, bool)>,
    /// Runs read so far.
    count: usize,
    /// Indexes of the runs it followed inside their window.
    followed: Vec<usize>,
    /// The position it holds and until when; a late copy waiting.
    target: f64,
    hold_until: u64,
    late: Option<(u64, f64)>,
    stopped: bool,
    copies: u64,
}

impl Default for Copier {
    fn default() -> Self {
        Self::new(Mode::built())
    }
}

impl Copier {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            params: Params::default(),
            setup: None,
            rule: None,
            genuine: None,
            seen: HashSet::new(),
            runs: VecDeque::new(),
            outcomes: Vec::new(),
            count: 0,
            followed: Vec::new(),
            target: 0.0,
            hold_until: 0,
            late: None,
            stopped: false,
            copies: 0,
        }
    }

    fn window(&self) -> u64 {
        self.rule.map_or(WIDEST_WINDOW, |r| r.follow_slots)
    }

    fn run_slots(&self) -> u64 {
        self.rule.map_or(self.params.run_slots, |r| r.horizon_slots)
    }

    /// How many runs it may follow inside their window among any
    /// `episodes` in a row, if it guards against setting the bait.
    fn budget(&self) -> Option<(u32, u32)> {
        match self.mode {
            Mode::Informed => self.rule.map(|r| (r.matches - 1, r.episodes)),
            Mode::InformedDay if self.genuine == Some(false) => {
                self.rule.map(|r| (r.matches - 1, r.episodes))
            }
            Mode::SafeBlind => Some((FEWEST_MATCHES - 1, u32::MAX)),
            Mode::CopyOneInEight => Some((1, EPISODES)),
            _ => None,
        }
    }

    /// Whether it may follow one more run inside its window now.
    fn may_follow(&self) -> bool {
        let Some((budget, episodes)) = self.budget() else {
            return true;
        };
        let from = self.count.saturating_sub(episodes as usize);
        let recent = self.followed.iter().filter(|i| **i >= from).count() as u32;
        recent < budget
    }

    /// The open window, if any, of a run whose side trading `way` would
    /// follow: the run's index.
    fn window_hit(&self, now: u64, way: f64) -> Option<usize> {
        let window = self.window() + self.params.margin;
        let first = self.count - self.runs.len();
        self.runs
            .iter()
            .enumerate()
            .rev()
            .find(|(_, r)| r.way == way && now + 1 >= r.start && now < r.start + window)
            .map(|(i, _)| first + i)
    }

    /// Read the leader's new takes into runs.
    fn read(&mut self, now: u64, mid: f64) {
        let Some(setup) = self.setup.as_ref() else {
            return;
        };
        let takes = leader_takes(&setup.venue, &setup.market, &setup.leader, &mut self.seen);
        let start = setup.start_slot;
        let run_slots = self.run_slots();
        for (slot, signed) in takes {
            let at = slot.saturating_sub(start).min(now);
            let way = signed.signum();
            let fresh = self
                .runs
                .back()
                .is_none_or(|r| r.way != way || at >= r.start + run_slots);
            if fresh {
                self.runs.push_back(Run {
                    start: at,
                    way,
                    first: signed.abs(),
                    mid,
                    followed: false,
                    copied: false,
                    scored: false,
                });
                self.count += 1;
                self.on_run(now);
            }
        }
        while self.runs.len() > 32 {
            self.runs.pop_front();
        }
    }

    /// A run has just started: copy it now, later, or not at all.
    fn on_run(&mut self, now: u64) {
        let Some(run) = self.runs.back().cloned() else {
            return;
        };
        let cap = self.setup.as_ref().map_or(0.0, |s| s.cap);
        if self.stopped || self.mode == Mode::Nop || self.mode == Mode::Twap {
            return;
        }
        if self.target != 0.0 {
            // Already holding its way: hold on through this run too, with no
            // new trade. Never flip.
            if self.target.signum() == run.way {
                self.hold_until = now + self.hold();
                if let Some(last) = self.runs.back_mut() {
                    last.copied = true;
                }
            }
            return;
        }
        let late_at = run.start + self.window() + self.params.margin + 1;
        match self.mode {
            Mode::Nop | Mode::Twap => {}
            Mode::CopyAll | Mode::Reference => self.enter(now, run.way * cap, false),
            Mode::InformedDay if self.genuine == Some(true) => {
                self.enter(now, run.way * cap, false)
            }
            Mode::CopySmall => {
                let size = 0.9 * NARROWEST_SHARE * run.first;
                self.enter(now, run.way * size, false);
            }
            Mode::CopyLate => {
                self.late = Some((run.start + self.params.late_slots, run.way * cap));
            }
            Mode::CopyOneInEight => {
                if self.may_follow() {
                    self.enter(now, run.way * cap, true);
                }
            }
            Mode::Informed | Mode::InformedDay | Mode::SafeBlind => {
                if self.may_follow() {
                    self.enter(now, run.way * cap, true);
                } else {
                    self.late = Some((late_at, run.way * cap));
                }
            }
        }
    }

    /// Take `target` now and hold it.
    fn enter(&mut self, now: u64, target: f64, counted: bool) {
        if target == 0.0 {
            return;
        }
        let index = self.count - 1;
        if let Some(run) = self.runs.back_mut() {
            run.copied = true;
            run.followed = true;
        }
        if counted || self.budget().is_none() {
            self.followed.push(index);
        }
        self.target = target;
        self.hold_until = now + self.hold();
        self.late = None;
        self.copies += 1;
    }

    fn hold(&self) -> u64 {
        self.rule.map_or(self.params.hold_slots, |r| r.horizon_slots)
    }

    /// Score the runs whose markouts are in, for the reference's odds.
    fn score(&mut self, now: u64, mid: f64) {
        let mark = self.params.mark_slots;
        let cut = self.params.cut_bps;
        let first = self.count - self.runs.len();
        for (i, run) in self.runs.iter_mut().enumerate() {
            if !run.scored && now >= run.start + mark {
                run.scored = true;
                let markout = run.way * (mid / run.mid - 1.0) * 1e4;
                self.outcomes.push((first + i, markout > cut));
            }
        }
    }

    /// The odds the wallet has turned by now: a change point after which
    /// its runs are right half the time, expected right after its own
    /// third or fourth followed run, against a genuine day's 0.85 all
    /// along. A turn with no scored run after it is as likely as before.
    fn turn_odds(&self) -> f64 {
        let (right, turned) = (0.85f64, 0.5f64);
        let ll = |r: bool, p: f64| if r { p.ln() } else { (1.0 - p).ln() };
        let turns: Vec<usize> = [FEWEST_MATCHES as usize, FEWEST_MATCHES as usize + 1]
            .iter()
            .filter_map(|k| self.followed.get(k - 1).map(|i| i + 1))
            .collect();
        if turns.is_empty() {
            return 0.0;
        }
        let prior = self.params.bait_prior;
        let genuine: f64 = self.outcomes.iter().map(|(_, r)| ll(*r, right)).sum();
        let mut terms = vec![((1.0 - prior).ln() + genuine, false)];
        for tau in &turns {
            let l: f64 = self
                .outcomes
                .iter()
                .map(|(i, r)| ll(*r, if i >= tau { turned } else { right }))
                .sum();
            terms.push(((prior / turns.len() as f64).ln() + l, *tau <= self.count));
        }
        let top = terms.iter().map(|(l, _)| *l).fold(f64::NEG_INFINITY, f64::max);
        let total: f64 = terms.iter().map(|(l, _)| (l - top).exp()).sum();
        let gone: f64 = terms
            .iter()
            .filter(|(_, t)| *t)
            .map(|(l, _)| (l - top).exp())
            .sum();
        gone / total
    }

    fn twap(&mut self, now: u64) {
        let Some(setup) = self.setup.as_ref() else {
            return;
        };
        self.target = setup.cap * (now as f64 / setup.length as f64).min(1.0);
    }
}

impl Strategy for Copier {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?.clone();
        let public: Value = session
            .var("MTX_PUBLIC_WALLETS")
            .and_then(|v| serde_json::from_str(v).ok())
            .ok_or("no public wallets: MTX_PUBLIC_WALLETS")?;
        let leader = public["informed"][0]
            .as_str()
            .ok_or("MTX_PUBLIC_WALLETS names no leader")?
            .to_string();
        let regime: Option<Value> = session
            .var("MTX_REGIME")
            .and_then(|r| serde_json::from_str(r).ok());
        match self.mode {
            Mode::Informed => {
                self.rule = regime.as_ref().and_then(|r| Rule::of(&r["rule"]));
                if self.rule.is_none() {
                    return Err("the informed bot needs MTX_REGIME's rule".into());
                }
            }
            Mode::InformedDay => {
                let r = regime.ok_or("informed-day needs the session's draw")?;
                let mut rule = r["bait"].clone();
                rule["horizon_slots"] = r["horizon_slots"].clone();
                self.rule = Rule::of(&rule);
                self.genuine = r["day"].as_str().map(|d| d == "genuine");
                if self.rule.is_none() || self.genuine.is_none() {
                    return Err("informed-day needs the bait rule and the day".into());
                }
            }
            _ => {}
        }
        let length = session.end_slot.saturating_sub(session.start_slot).max(1);
        log(&format!(
            "copier: {:?}, leader {leader}, rule {:?}, genuine {:?}",
            self.mode, self.rule, self.genuine
        ));
        self.setup = Some(Setup {
            venue,
            market,
            leader,
            start_slot: session.start_slot,
            length,
            cap: session.cap,
        });
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let Some(setup) = self.setup.as_ref() else {
            return Ok(());
        };
        let now = market.slot.saturating_sub(setup.start_slot);
        let mid = match (market.best_bid, market.best_ask) {
            (Some(b), Some(a)) => (a + b) / 2.0,
            _ => market.mark,
        };
        match self.mode {
            Mode::Nop => return Ok(()),
            Mode::Twap => self.twap(now),
            _ => {
                self.read(now, mid);
                if self.mode == Mode::Reference {
                    self.score(now, mid);
                    if !self.stopped && self.turn_odds() > self.params.turn_odds {
                        self.stopped = true;
                        log(&format!(
                            "copier: the wallet looks turned at {now}, after {} runs",
                            self.count
                        ));
                    }
                }
                if let Some((at, target)) = self.late
                    && now >= at
                {
                    self.late = None;
                    if !self.stopped && self.target == 0.0 && now < at + 20 {
                        self.target = target;
                        self.hold_until = now + self.hold();
                        self.copies += 1;
                        if let Some(run) = self.runs.back_mut() {
                            run.copied = true;
                        }
                    }
                }
                if self.target != 0.0 && now >= self.hold_until {
                    // Flat again, unless that sale follows a run inside its
                    // window with no budget left: then wait.
                    let way = -self.target.signum();
                    let hit = self.window_hit(now, way);
                    match hit {
                        Some(index) if self.budget().is_some() => {
                            if self.may_follow() && !self.followed.contains(&index) {
                                self.followed.push(index);
                                self.target = 0.0;
                            }
                        }
                        _ => self.target = 0.0,
                    }
                }
            }
        }
        if market.last || market.slots_left <= 4 {
            self.target = 0.0;
        }
        if (self.target - market.position.size).abs() >= 0.01 {
            orders.target_position_with(
                self.target,
                Execution {
                    slippage_bps: 50.0,
                    min_trade: 0.01,
                    ..Execution::default()
                },
            );
        }
        Ok(())
    }

    fn end(&mut self, _: Option<&Market>) {
        log(&format!(
            "copier: {} runs read, {} copies, {} followed inside a window, stopped {}",
            self.count,
            self.copies,
            self.followed.len(),
            self.stopped
        ));
    }
}

/// The leader's takes not seen before: their slot and side times size in
/// base units, oldest first. Post-only orders rest and are left out.
fn leader_takes(
    venue: &PhoenixVenue,
    market: &ExchangeMarketConfig,
    leader: &str,
    seen: &mut HashSet<String>,
) -> Vec<(u64, f64)> {
    let call = |method: &str, params: Value| -> Option<Value> {
        serde_json::from_str(&rpc(method, &params.to_string()).ok()?).ok()
    };
    let Some(sigs) = call(
        "getSignaturesForAddress",
        json!([leader, { "limit": 20, "commitment": "confirmed" }]),
    ) else {
        return Vec::new();
    };
    let mut fresh: Vec<(u64, String)> = sigs
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["err"].is_null())
        .filter_map(|s| Some((s["slot"].as_u64()?, s["signature"].as_str()?.to_string())))
        .filter(|(_, sig)| !seen.contains(sig))
        .collect();
    fresh.sort_by_key(|(slot, _)| *slot);
    let mut out = Vec::new();
    for (slot, sig) in fresh {
        seen.insert(sig.clone());
        let Some(tx) = call(
            "getTransaction",
            json!([sig, { "encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed" }]),
        ) else {
            continue;
        };
        out.extend(
            takes_in(&tx, venue, market, leader)
                .into_iter()
                .map(|signed| (slot, signed)),
        );
    }
    out
}

/// The takes `signer` sends in one transaction (`getTransaction`, json):
/// side times size.
fn takes_in(
    tx: &Value,
    venue: &PhoenixVenue,
    market: &ExchangeMarketConfig,
    signer: &str,
) -> Vec<f64> {
    let mut out = Vec::new();
    if !tx["meta"]["err"].is_null() {
        return out;
    }
    let message = &tx["transaction"]["message"];
    let keys: Vec<&str> = message["accountKeys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if keys.first() != Some(&signer) {
        return out;
    }
    let market_key = market.market_pubkey.to_string();
    for ix in message["instructions"].as_array().into_iter().flatten() {
        let on_market = ix["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .any(|i| keys.get(i as usize) == Some(&market_key.as_str()));
        if !on_market {
            continue;
        }
        let data = ix["data"]
            .as_str()
            .and_then(|d| bs58::decode(d).into_vec().ok())
            .unwrap_or_default();
        let Some(kind) = data
            .get(..8)
            .and_then(|tag| PhoenixInstruction::from_discriminant(tag.try_into().ok()?))
        else {
            continue;
        };
        if !matches!(
            kind,
            PhoenixInstruction::PlaceLimitOrder | PhoenixInstruction::PlaceMarketOrder
        ) {
            continue;
        }
        let Ok(packet) = <OrderPacket as borsh::BorshDeserialize>::deserialize(&mut &data[8..])
        else {
            continue;
        };
        let Ok(value) = serde_json::to_value(&packet) else {
            continue;
        };
        let Some((variant, fields)) = value["kind"].as_object().and_then(|o| o.iter().next())
        else {
            continue;
        };
        if variant.to_ascii_lowercase().contains("post") {
            continue;
        }
        let side = match fields["side"].as_str() {
            Some(s) if s.eq_ignore_ascii_case("bid") => 1.0,
            Some(_) => -1.0,
            None => continue,
        };
        let Some(lots) = fields["num_base_lots"].as_i64() else {
            continue;
        };
        let size = venue.lots_to_base_units(market, lots);
        if size > 0.0 {
            out.push(side * size);
        }
    }
    out
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Copier);
