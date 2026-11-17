//! Reference: follow the wallets whose takes the price has rewarded lately,
//! and stop following them when their record turns.
//!
//! Each step it reads every Phoenix order sent to the market since the last
//! one (`getSignaturesForAddress` on the market at `confirmed`, then
//! `getTransaction`: the signer, side and size of each order that takes),
//! and keeps a record per wallet: each take is marked out `MARK_SLOTS`
//! later against the book's mid, and the record is the mean markout with
//! older takes decayed at a half-life of a quarter of the session. A take
//! made after the book already moved its way by `CHASE_BPS` is a chase, and
//! a wallet whose takes are mostly chases is dropped.
//!
//! The position is the cap the way the credible wallets' net flow over the
//! last `LOOK_SLOTS` points, held `HOLD_SLOTS` past the last time it did.
//!
//! The same machinery runs the ladder's rungs ([`Mode`]): the informed bot,
//! told which wallets are informed and which are decoys, follows the
//! informed alone from their first take; the rungs that don't read wallets
//! follow or fade the market's net flow, or follow the price's move; the
//! naive reader follows the busiest wallet.

use std::collections::{HashMap, HashSet, VecDeque};

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
    Reference,
    /// Told the informed and decoy wallets (`MTX_REGIME`).
    Informed,
    FollowFlow,
    FadeFlow,
    Momentum,
    MostActive,
}

impl Mode {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "reference" => Self::Reference,
            "informed" => Self::Informed,
            "follow-flow" => Self::FollowFlow,
            "fade-flow" => Self::FadeFlow,
            "momentum" => Self::Momentum,
            "most-active" => Self::MostActive,
            _ => return None,
        })
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
    /// The price move over `look_slots` that momentum follows, in bps.
    pub momentum_bps: f64,
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
                min_score_bps: 20.0,
                ..base
            },
            Mode::MostActive => Params {
                look_slots: 10,
                hold_slots: 20,
                threshold: 0.0,
                ..base
            },
            Mode::FollowFlow => Params {
                look_slots: 40,
                hold_slots: 40,
                threshold: 4.0,
                ..base
            },
            Mode::FadeFlow => Params {
                look_slots: 10,
                hold_slots: 10,
                threshold: 6.0,
                ..base
            },
            Mode::Momentum => Params {
                look_slots: 4,
                hold_slots: 80,
                momentum_bps: 8.0,
                ..base
            },
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
    market: ExchangeMarketConfig,
    orderbook: String,
    me: String,
    start_slot: u64,
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
    mids: VecDeque<(u64, f64)>,
    recent: VecDeque<Take>,
    pending: VecDeque<Take>,
    records: HashMap<String, Record>,
    dir: f64,
    hold_until: u64,
    trades: u64,
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
            mids: VecDeque::new(),
            recent: VecDeque::new(),
            pending: VecDeque::new(),
            records: HashMap::new(),
            dir: 0.0,
            hold_until: 0,
            trades: 0,
        }
    }

    fn mid_at(&self, at: u64) -> Option<f64> {
        self.mids
            .iter()
            .rev()
            .find(|(t, _)| *t <= at)
            .or(self.mids.front())
            .map(|(_, m)| *m)
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
        let window = self
            .recent
            .iter()
            .filter(|t| t.at + p.look_slots > now);
        let net: f64 = match self.mode {
            Mode::Momentum => {
                let (then, mid) = (self.mid_at(now.saturating_sub(p.look_slots))?, self.mid_at(now)?);
                let moved = (mid / then - 1.0) * 1e4;
                return (moved.abs() >= p.momentum_bps).then(|| moved.signum());
            }
            Mode::FollowFlow => window.map(|t| t.signed).sum(),
            Mode::FadeFlow => -window.map(|t| t.signed).sum::<f64>(),
            Mode::Informed => window
                .filter(|t| setup.informed.contains(&t.wallet))
                .map(|t| t.signed)
                .sum(),
            Mode::Reference => window
                .filter(|t| self.credible(&t.wallet))
                .map(|t| t.signed)
                .sum(),
            Mode::MostActive => {
                let busiest = self
                    .records
                    .iter()
                    .max_by(|a, b| a.1.takes.cmp(&b.1.takes).then(b.0.cmp(a.0)))
                    .map(|(w, _)| w.clone());
                let Some(busiest) = busiest else {
                    return None;
                };
                window
                    .filter(|t| t.wallet == busiest)
                    .map(|t| t.signed)
                    .sum()
            }
        };
        (net.abs() > p.threshold.max(1e-9)).then(|| net.signum())
    }

    fn read_tape(&mut self, now: u64, mid: f64) {
        let Some(setup) = self.setup.as_ref() else {
            return;
        };
        let orders = new_orders(&setup.venue, &setup.orderbook, &setup.market, &mut self.seen);
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
}

impl Strategy for Flow {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?.clone();
        let orderbook = market.market_pubkey.to_string();
        let regime: Option<Value> = session
            .var("MTX_REGIME")
            .and_then(|r| serde_json::from_str(r).ok());
        let told = |kind: &str| -> HashSet<String> {
            regime
                .as_ref()
                .and_then(|r| r["wallets"][kind].as_array())
                .map(|a| a.iter().filter_map(|w| w.as_str().map(String::from)).collect())
                .unwrap_or_default()
        };
        let (informed, decoys) = (told("informed"), told("decoy"));
        if self.mode == Mode::Informed && informed.is_empty() {
            return Err("the informed bot needs MTX_REGIME's informed wallets".into());
        }
        let length = session.end_slot.saturating_sub(session.start_slot).max(1);
        log(&format!(
            "flow: {:?}, {} informed and {} decoy wallets told",
            self.mode,
            informed.len(),
            decoys.len()
        ));
        self.setup = Some(Setup {
            venue,
            market,
            orderbook,
            me: session.authority.clone(),
            start_slot: session.start_slot,
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
        let (start, cap) = (setup.start_slot, setup.cap);
        let now = market.slot.saturating_sub(start);
        let mid = match (market.best_bid, market.best_ask) {
            (Some(b), Some(a)) => (a + b) / 2.0,
            _ => market.mark,
        };
        self.mids.push_back((now, mid));
        while self.mids.len() > 400 {
            self.mids.pop_front();
        }
        if self.mode != Mode::Momentum {
            self.read_tape(now, mid);
            self.mark_out(now, mid);
        }
        if let Some(dir) = self.signal(now) {
            if dir != self.dir {
                self.trades += 1;
            }
            self.dir = dir;
            self.hold_until = now + self.params.hold_slots;
        }
        if now > self.hold_until {
            self.dir = 0.0;
        }
        if market.last || market.slots_left <= 4 {
            self.dir = 0.0;
        }
        let target = self.dir * cap;
        if (target - market.position.size).abs() >= 0.5 {
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
            "flow: {} wallets seen, {} credible at the end ({} told informed, {} told decoys); {} entries",
            self.records.len(),
            credible.len(),
            hit(&setup.informed),
            hit(&setup.decoys),
            self.trades
        ));
    }
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
        let Some((variant, fields)) = value["kind"].as_object().and_then(|o| o.iter().next()) else {
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

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Flow);
