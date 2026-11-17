//! Reference: read who trades NVDA ahead of its moves, and trade QQQ on it.
//!
//! The account may trade QQQ only; NVDA's tape is where the information is.
//! Each step the bot reads every Phoenix order sent to NVDA's market since
//! the last one (`getSignaturesForAddress` on the market, then
//! `getTransaction`: the signer, side and size of each order that takes)
//! and both markets' oracles (the perp asset map). It keeps a record per
//! NVDA wallet: each take is marked out `mark_slots` later against NVDA's
//! oracle, and the record is the mean markout with older takes decayed at a
//! half-life of a quarter of the session. A take made after NVDA already
//! moved its way by `chase_bps` is a chase, and a wallet whose takes are
//! mostly chases is dropped.
//!
//! It estimates what share of NVDA's moves QQQ makes (`beta`) from the two
//! oracles' returns, and trades QQQ only while that share of a typical
//! NVDA move clears QQQ's round trip: the cap the way the credible wallets'
//! net NVDA flow over the last `look_slots` points, held `hold_slots` past
//! the last time it did.
//!
//! The same machinery runs the ladder's rungs ([`Mode`]): the informed bot,
//! told which NVDA wallets are informed and which are decoys, follows the
//! informed alone from their first take; the rungs that don't read wallets
//! follow NVDA's or QQQ's net taker flow, or NVDA's price move, into QQQ,
//! or buy QQQ evenly through the session.

use std::collections::{HashMap, HashSet, VecDeque};

use mtx_kit::{
    log,
    prelude::*,
    rpc, rpc_account,
    venue::{
        PhoenixVenue,
        phoenix_rise::{
            accounts::perp_asset_map::PerpAssetMap,
            ix::{PhoenixInstruction, order_packet::OrderPacket},
            types::exchange::ExchangeMarketConfig,
        },
    },
};
use serde_json::{Value, json};

/// Which rung the machinery runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reference,
    /// Told the NVDA wallets that are informed and decoys (`MTX_REGIME`).
    Informed,
    /// QQQ the way NVDA's net taker flow went.
    FollowNvdaFlow,
    /// QQQ the way QQQ's own net taker flow went.
    FollowQqqFlow,
    /// QQQ the way NVDA's price moved.
    Momentum,
    /// QQQ bought evenly through the session to the cap, and sold at the end.
    Twap,
}

impl Mode {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "reference" => Self::Reference,
            "informed" => Self::Informed,
            "follow-nvda-flow" => Self::FollowNvdaFlow,
            "follow-qqq-flow" => Self::FollowQqqFlow,
            "momentum" => Self::Momentum,
            "twap" => Self::Twap,
            _ => return None,
        })
    }

    fn reads_nvda_tape(self) -> bool {
        matches!(
            self,
            Self::Reference | Self::Informed | Self::FollowNvdaFlow
        )
    }
}

/// The rung's knobs.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Flow counted toward a signal, in slots back.
    pub look_slots: u64,
    /// How long a position is held past the last signal.
    pub hold_slots: u64,
    /// The net flow, in base units, a signal needs.
    pub threshold: f64,
    /// A take's markout horizon, and the move before a take that makes it a
    /// chase, in bps.
    pub mark_slots: u64,
    pub chase_bps: f64,
    /// A credible wallet: at least this much decayed weight of marked takes,
    /// a mean markout of at least `min_score_bps`, and under half chases.
    pub min_weight: f64,
    pub min_score_bps: f64,
    /// The record's half-life, as a share of the session.
    pub half_life_share: f64,
    /// The NVDA move over `look_slots` that momentum follows, in bps.
    pub momentum_bps: f64,
    /// QQQ's share of NVDA's moves: the returns' lag in slots, the prior and
    /// its weight in squared-bps, and the typical NVDA move it is applied
    /// to, in bps. A rung trades only while `beta * move_bps` clears
    /// `cost_bps` (QQQ's round trip: two taker fees and the spread). A
    /// `cost_bps` of 0 trades every signal.
    pub beta_lag: u64,
    pub beta_prior: f64,
    pub beta_prior_weight: f64,
    pub move_bps: f64,
    pub cost_bps: f64,
    /// The position taken on a signal, as a share of the cap.
    pub size: f64,
}

impl Params {
    pub fn of(mode: Mode) -> Self {
        let base = Params {
            look_slots: 40,
            hold_slots: 40,
            threshold: 0.5,
            mark_slots: 40,
            chase_bps: 8.0,
            min_weight: 2.0,
            min_score_bps: 5.0,
            half_life_share: 0.25,
            momentum_bps: 8.0,
            beta_lag: 10,
            beta_prior: 0.5,
            beta_prior_weight: 400.0,
            move_bps: 40.0,
            cost_bps: 0.0,
            size: 1.0,
        };
        match mode {
            Mode::Informed => Params {
                look_slots: 10,
                hold_slots: 40,
                threshold: 0.0,
                ..base
            },
            Mode::Reference => Params {
                look_slots: 10,
                hold_slots: 40,
                threshold: 0.0,
                min_score_bps: 40.0,
                min_weight: 3.0,
                ..base
            },
            Mode::FollowNvdaFlow => Params {
                look_slots: 40,
                hold_slots: 40,
                threshold: 4.0,
                ..base
            },
            Mode::FollowQqqFlow => Params {
                look_slots: 40,
                hold_slots: 40,
                threshold: 0.2,
                ..base
            },
            Mode::Momentum => Params {
                look_slots: 4,
                hold_slots: 80,
                momentum_bps: 8.0,
                ..base
            },
            Mode::Twap => base,
        }
    }
}

#[derive(Clone)]
struct Take {
    at: u64,
    wallet: String,
    /// Side times size, base units.
    signed: f64,
    mid: f64,
}

#[derive(Default)]
struct Record {
    weight: f64,
    sum_bps: f64,
    at: u64,
    takes: u64,
    chased: u64,
}

impl Record {
    fn decay_to(&mut self, now: u64, half_life: f64) {
        let k = 0.5f64.powf((now.saturating_sub(self.at)) as f64 / half_life);
        self.weight *= k;
        self.sum_bps *= k;
        self.at = now;
    }
}

struct Setup {
    venue: PhoenixVenue,
    /// The market read (NVDA) and the market traded (QQQ).
    read: ExchangeMarketConfig,
    traded: ExchangeMarketConfig,
    read_book: String,
    traded_book: String,
    asset_map: String,
    me: String,
    start_slot: u64,
    length: u64,
    cap: f64,
    half_life: f64,
    informed: HashSet<String>,
    decoys: HashSet<String>,
}

pub struct Flow {
    pub mode: Mode,
    pub params: Params,
    setup: Option<Setup>,
    seen: HashSet<String>,
    seen_traded: HashSet<String>,
    /// NVDA's and QQQ's oracles by slot into the session.
    mids: VecDeque<(u64, f64)>,
    traded_mids: VecDeque<(u64, f64)>,
    recent: VecDeque<Take>,
    traded_recent: VecDeque<(u64, f64)>,
    pending: VecDeque<Take>,
    records: HashMap<String, Record>,
    /// Sums for QQQ's share of NVDA's returns.
    sxy: f64,
    sxx: f64,
    dir: f64,
    hold_until: u64,
    trades: u64,
    skipped: u64,
}

impl Default for Flow {
    fn default() -> Self {
        Self::new(Mode::Reference)
    }
}

impl Flow {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            params: Params::of(mode),
            setup: None,
            seen: HashSet::new(),
            seen_traded: HashSet::new(),
            mids: VecDeque::new(),
            traded_mids: VecDeque::new(),
            recent: VecDeque::new(),
            traded_recent: VecDeque::new(),
            pending: VecDeque::new(),
            records: HashMap::new(),
            sxy: 0.0,
            sxx: 0.0,
            dir: 0.0,
            hold_until: 0,
            trades: 0,
            skipped: 0,
        }
    }

    fn at(series: &VecDeque<(u64, f64)>, at: u64) -> Option<f64> {
        series
            .iter()
            .rev()
            .find(|(t, _)| *t <= at)
            .or(series.front())
            .map(|(_, m)| *m)
    }

    fn mid_at(&self, at: u64) -> Option<f64> {
        Self::at(&self.mids, at)
    }

    /// QQQ's estimated share of NVDA's moves.
    pub fn beta(&self) -> f64 {
        let p = self.params;
        (self.sxy + p.beta_prior * p.beta_prior_weight) / (self.sxx + p.beta_prior_weight)
    }

    fn credible(&self, wallet: &str) -> bool {
        self.records.get(wallet).is_some_and(|r| {
            r.weight >= self.params.min_weight
                && r.sum_bps / r.weight >= self.params.min_score_bps
                && 2 * r.chased < r.takes
        })
    }

    /// The signal's direction now, if it fires.
    fn signal(&self, now: u64) -> Option<f64> {
        let p = self.params;
        let setup = self.setup.as_ref()?;
        let window = self.recent.iter().filter(|t| t.at + p.look_slots > now);
        let net: f64 = match self.mode {
            Mode::Twap => return None,
            Mode::Momentum => {
                let (then, mid) = (
                    self.mid_at(now.saturating_sub(p.look_slots))?,
                    self.mid_at(now)?,
                );
                let moved = (mid / then - 1.0) * 1e4;
                return (moved.abs() >= p.momentum_bps).then(|| moved.signum());
            }
            Mode::FollowNvdaFlow => window.map(|t| t.signed).sum(),
            Mode::FollowQqqFlow => self
                .traded_recent
                .iter()
                .filter(|(at, _)| at + p.look_slots > now)
                .map(|(_, s)| s)
                .sum(),
            Mode::Informed => window
                .filter(|t| setup.informed.contains(&t.wallet))
                .map(|t| t.signed)
                .sum(),
            Mode::Reference => window
                .filter(|t| self.credible(&t.wallet))
                .map(|t| t.signed)
                .sum(),
        };
        (net.abs() > p.threshold.max(1e-9)).then(|| net.signum())
    }

    fn read_tape(&mut self, now: u64, mid: f64) {
        let Some(setup) = self.setup.as_ref() else {
            return;
        };
        let orders = new_orders(&setup.venue, &setup.read_book, &setup.read, &mut self.seen);
        let start = setup.start_slot;
        let me = setup.me.clone();
        let p = self.params;
        let before = self.mid_at(now.saturating_sub(p.look_slots)).unwrap_or(mid);
        let moved = (mid / before - 1.0) * 1e4;
        for (slot, wallet, signed) in orders {
            if wallet == me {
                continue;
            }
            let at = slot.saturating_sub(start).min(now);
            let chased = moved * signed.signum() > p.chase_bps;
            let record = self.records.entry(wallet.clone()).or_default();
            record.takes += 1;
            record.chased += u64::from(chased);
            let take = Take {
                at,
                wallet,
                signed,
                mid,
            };
            self.pending.push_back(take.clone());
            self.recent.push_back(take);
        }
        while self
            .recent
            .front()
            .is_some_and(|t| t.at + p.look_slots.max(p.mark_slots) < now)
        {
            self.recent.pop_front();
        }
    }

    fn read_traded_tape(&mut self, now: u64) {
        let Some(setup) = self.setup.as_ref() else {
            return;
        };
        let orders = new_orders(
            &setup.venue,
            &setup.traded_book,
            &setup.traded,
            &mut self.seen_traded,
        );
        let (start, me) = (setup.start_slot, setup.me.clone());
        for (slot, wallet, signed) in orders {
            if wallet != me {
                self.traded_recent
                    .push_back((slot.saturating_sub(start).min(now), signed));
            }
        }
        let look = self.params.look_slots;
        while self
            .traded_recent
            .front()
            .is_some_and(|(at, _)| at + look < now)
        {
            self.traded_recent.pop_front();
        }
    }

    fn mark_out(&mut self, now: u64, mid: f64) {
        let half_life = self.setup.as_ref().map_or(400.0, |s| s.half_life);
        while self
            .pending
            .front()
            .is_some_and(|t| t.at + self.params.mark_slots <= now)
        {
            let take = self.pending.pop_front().expect("checked");
            let markout = take.signed.signum() * (mid / take.mid - 1.0) * 1e4;
            let record = self.records.entry(take.wallet).or_default();
            record.decay_to(now, half_life);
            record.weight += 1.0;
            record.sum_bps += markout;
        }
    }

    /// Adds the latest pair of returns, `beta_lag` slots long, to the sums.
    fn learn_beta(&mut self, now: u64) {
        let lag = self.params.beta_lag;
        if now < lag || !now.is_multiple_of(lag) {
            return;
        }
        let pair = |series: &VecDeque<(u64, f64)>| -> Option<f64> {
            Some((Self::at(series, now)? / Self::at(series, now - lag)?).ln() * 1e4)
        };
        if let (Some(x), Some(y)) = (pair(&self.mids), pair(&self.traded_mids)) {
            self.sxx += x * x;
            self.sxy += x * y;
        }
    }
}

impl Strategy for Flow {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let traded = venue.market(&session.symbol)?.clone();
        let read_symbol = session
            .mandate
            .universe
            .iter()
            .map(|a| &a.instrument)
            .find(|i| i.symbol != session.symbol)
            .map(|i| i.symbol.clone())
            .ok_or("the mandate names no market to read")?;
        let read = venue.market(&read_symbol)?.clone();
        let regime: Option<Value> = session
            .var("MTX_REGIME")
            .and_then(|r| serde_json::from_str(r).ok());
        let told = |kind: &str| -> HashSet<String> {
            regime
                .as_ref()
                .and_then(|r| r["wallets"][kind].as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|w| w.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let (informed, decoys) = (told("informed"), told("decoy"));
        if self.mode == Mode::Informed && informed.is_empty() {
            return Err("the informed bot needs MTX_REGIME's informed wallets".into());
        }
        let length = session.end_slot.saturating_sub(session.start_slot).max(1);
        log(&format!(
            "flow: {:?} on {} reading {}, {} informed and {} decoy wallets told",
            self.mode,
            session.symbol,
            read_symbol,
            informed.len(),
            decoys.len()
        ));
        self.setup = Some(Setup {
            read_book: read.market_pubkey.to_string(),
            traded_book: traded.market_pubkey.to_string(),
            asset_map: venue.perp_asset_map()?.to_string(),
            venue,
            read,
            traded,
            me: session.authority.clone(),
            start_slot: session.start_slot,
            length,
            cap: session.cap,
            half_life: (length as f64 * self.params.half_life_share).max(1.0),
            informed,
            decoys,
        });
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let Some(setup) = self.setup.as_ref() else {
            return Ok(());
        };
        let (start, cap, length) = (setup.start_slot, setup.cap, setup.length);
        let now = market.slot.saturating_sub(start);
        let Some((mid, traded_mid)) = oracles(setup) else {
            return Ok(());
        };
        self.mids.push_back((now, mid));
        self.traded_mids.push_back((now, traded_mid));
        while self.mids.len() > 400 {
            self.mids.pop_front();
            self.traded_mids.pop_front();
        }
        self.learn_beta(now);
        if self.mode.reads_nvda_tape() {
            self.read_tape(now, mid);
            self.mark_out(now, mid);
        }
        if self.mode == Mode::FollowQqqFlow {
            self.read_traded_tape(now);
        }
        if let Some(dir) = self.signal(now) {
            let p = self.params;
            if p.cost_bps > 0.0 && self.beta() * p.move_bps < p.cost_bps {
                self.skipped += 1;
            } else {
                if dir != self.dir {
                    self.trades += 1;
                }
                self.dir = dir;
                self.hold_until = now + p.hold_slots;
            }
        }
        if now > self.hold_until {
            self.dir = 0.0;
        }
        let mut target = self.dir * cap * self.params.size;
        if self.mode == Mode::Twap {
            target = (cap * now as f64 / length as f64 / 0.9).min(cap);
        }
        if market.last || market.slots_left <= 4 {
            target = 0.0;
        }
        if (target - market.position.size).abs() >= 0.05 * cap {
            orders.target_position_with(
                target,
                Execution {
                    slippage_bps: 50.0,
                    ..Execution::default()
                },
            );
        }
        Ok(())
    }

    fn end(&mut self, _: Option<&Market>) {
        let Some(setup) = self.setup.as_ref() else {
            return;
        };
        let credible: Vec<_> = self
            .records
            .keys()
            .filter(|w| self.credible(w))
            .cloned()
            .collect();
        let hit = |set: &HashSet<String>| credible.iter().filter(|w| set.contains(*w)).count();
        log(&format!(
            "flow: {} wallets seen, {} credible at the end ({} told informed, {} told decoys); {} entries, {} skipped; beta {:.3}",
            self.records.len(),
            credible.len(),
            hit(&setup.informed),
            hit(&setup.decoys),
            self.trades,
            self.skipped,
            self.beta()
        ));
    }
}

/// The read market's and the traded market's oracles, in USD, from one read
/// of the perp asset map: the median of each market's exchange reports.
fn oracles(setup: &Setup) -> Option<(f64, f64)> {
    let data = rpc_account(&setup.asset_map).ok()??;
    let map = PerpAssetMap::try_from_account_bytes(&data).ok()?;
    let oracle = |market: &ExchangeMarketConfig| -> Option<f64> {
        let entry = map.find_by_symbol(&market.symbol).ok()??;
        let price = &entry.metadata.oracle_price().mark_price;
        let reported = |t: u64| t > 0 && t != u64::MAX;
        let mut ticks: Vec<u64> = price
            .spot_price_component
            .last_exchange_spot_price
            .iter()
            .map(|p| p.ticks.as_inner())
            .filter(|&t| reported(t))
            .collect();
        if ticks.is_empty() {
            ticks = price
                .perp_price_component
                .last_exchange_perp_price
                .iter()
                .map(|p| p.ticks.as_inner())
                .filter(|&t| reported(t))
                .collect();
        }
        ticks.sort_unstable();
        let median = *ticks.get(ticks.len() / 2)?;
        Some(setup.venue.ticks_to_usd(market, median))
    };
    Some((oracle(&setup.read)?, oracle(&setup.traded)?))
}

/// The takes on `orderbook` not seen before: their slot, the first signer
/// and side times size in base units, oldest first. Post-only orders rest
/// and are left out.
fn new_orders(
    venue: &PhoenixVenue,
    orderbook: &str,
    market: &ExchangeMarketConfig,
    seen: &mut HashSet<String>,
) -> Vec<(u64, String, f64)> {
    let call = |method: &str, params: Value| -> Option<Value> {
        serde_json::from_str(&rpc(method, &params.to_string()).ok()?).ok()
    };
    let Some(sigs) = call(
        "getSignaturesForAddress",
        json!([orderbook, { "limit": 100, "commitment": "confirmed" }]),
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
            takes_in(&tx, venue, market)
                .into_iter()
                .map(|(signer, signed)| (slot, signer, signed)),
        );
    }
    out
}

/// The takes in one transaction (`getTransaction`, json): its first signer,
/// and side times size.
fn takes_in(tx: &Value, venue: &PhoenixVenue, market: &ExchangeMarketConfig) -> Vec<(String, f64)> {
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
    let Some(signer) = keys.first() else {
        return out;
    };
    for ix in message["instructions"].as_array().into_iter().flatten() {
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
            out.push((signer.to_string(), side * size));
        }
    }
    out
}

/// The process bot: the rung `MTX_NAIVE` names (the reference by default),
/// or `mode`, with `MTX_FLOW_*` overriding its knobs for calibration.
///
/// A process on `mtx_kit::run` trades `MTX_SYMBOL`'s market, the session's
/// primary (NVDA, whose tape the library's wallets trade). The desk trades
/// QQQ: the process takes QQQ as its market, and QQQ's cap from the
/// mandate, before the run loop reads them.
#[cfg(not(target_arch = "wasm32"))]
pub fn main_with(mode: Option<Mode>) -> BotResult {
    trade("QQQ")?;
    let mode = mode
        .or_else(|| {
            std::env::var("MTX_NAIVE")
                .ok()
                .and_then(|m| Mode::parse(&m))
        })
        .unwrap_or(Mode::Reference);
    let mut flow = Flow::new(mode);
    let knob = |name: &str| std::env::var(format!("MTX_FLOW_{name}")).ok();
    let mut p = flow.params;
    let set = |v: &mut f64, name: &str| {
        if let Some(x) = knob(name).and_then(|x| x.parse().ok()) {
            *v = x;
        }
    };
    set(&mut p.threshold, "THRESHOLD");
    set(&mut p.min_score_bps, "MIN_SCORE");
    set(&mut p.min_weight, "MIN_WEIGHT");
    set(&mut p.momentum_bps, "MOMENTUM");
    set(&mut p.cost_bps, "COST");
    set(&mut p.move_bps, "MOVE");
    set(&mut p.size, "SIZE");
    if let Some(v) = knob("LOOK").and_then(|v| v.parse().ok()) {
        p.look_slots = v;
    }
    if let Some(v) = knob("HOLD").and_then(|v| v.parse().ok()) {
        p.hold_slots = v;
    }
    flow.params = p;
    mtx_kit::run(flow)
}

/// Points the run loop at `symbol`: `MTX_SYMBOL`, and `MTX_MAX_POSITION`
/// from the mandate's cap on it.
#[cfg(not(target_arch = "wasm32"))]
fn trade(symbol: &str) -> BotResult {
    let path = std::env::var("MTX_MANDATE").map_err(|_| "no MTX_MANDATE")?;
    let mandate: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let cap = mandate["universe"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| {
            a["instrument"]
                .as_str()
                .is_some_and(|i| i.ends_with(&format!(":{symbol}")))
        })
        .and_then(|a| a["max_position"].as_f64())
        .ok_or_else(|| format!("the mandate has no cap on {symbol}"))?;
    // SAFETY: set before the run loop starts any thread.
    unsafe {
        std::env::set_var("MTX_SYMBOL", symbol);
        std::env::set_var("MTX_MAX_POSITION", cap.to_string());
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Flow);
