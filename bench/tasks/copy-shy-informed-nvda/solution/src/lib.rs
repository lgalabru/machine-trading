//! Reference: find the fund whose takes the price rewards, and copy it
//! small enough that it keeps trading.
//!
//! Each step it reads every Phoenix order sent to the market since the last
//! (`getSignaturesForAddress` on the market at `confirmed`, then
//! `getTransaction`: the signer, side and size of each order that takes),
//! and keeps a record per wallet: each take is marked out `MARK_SLOTS`
//! later against the book's mid, older takes decayed at a half-life of a
//! quarter of the session. A take made after the book already moved its
//! way by `chase_bps` is a chase, and a wallet whose takes are mostly
//! chases is dropped.
//!
//! The fund hides once one account's flow its way, over its last fills,
//! comes to a share of them (the shy rule, whose range the instruction
//! states). So the reference copies each take of a credible wallet at
//! `share` of its size, under the rule's lowest share, building a position
//! the way their net flow over about a horizon points, never flipping, and
//! holding it `hold_slots` past the last take it copied.
//!
//! The same machinery runs the ladder's rungs ([`Mode`]): the informed bot,
//! told who the fund is and its exact rule, copies the fund alone just under
//! its share; the full-size copier, told who the fund is, takes the cap its
//! way at its first take; the rungs that don't read wallets follow the
//! market's net flow, follow the price's move, or work the cap one way over
//! the session.

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
    /// Never trades.
    Nop,
    Reference,
    /// Told the fund's and the decoys' wallets and the shy rule
    /// (`MTX_REGIME`).
    Informed,
    /// Told the fund's wallet; copies it at the cap.
    FullCopy,
    FollowFlow,
    Momentum,
    TwapLong,
    TwapShort,
}

impl Mode {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "nop" => Self::Nop,
            "reference" => Self::Reference,
            "informed" => Self::Informed,
            "full-copy" => Self::FullCopy,
            "follow-flow" => Self::FollowFlow,
            "momentum" => Self::Momentum,
            "twap-long" => Self::TwapLong,
            "twap-short" => Self::TwapShort,
            _ => return None,
        })
    }

    fn told(self) -> bool {
        matches!(self, Self::Informed | Self::FullCopy)
    }
}

/// The rung's knobs.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Flow counted toward a signal, in slots back (the flow and momentum
    /// rungs).
    pub look_slots: u64,
    /// How long a position is held past the last signal.
    pub hold_slots: u64,
    /// The net flow, in base units, the flow rung needs.
    pub threshold: f64,
    pub mark_slots: u64,
    pub chase_bps: f64,
    /// A credible wallet: at least this much decayed weight of marked takes,
    /// a mean markout of at least `min_score_bps`, and under half chases.
    pub min_weight: f64,
    pub min_score_bps: f64,
    pub half_life_share: f64,
    pub momentum_bps: f64,
    /// A copier's size, as a share of each take it copies: for the
    /// informed bot, a share of the fund's rule's.
    pub share: f64,
    /// Slots a copier waits after a take before copying it.
    pub lag_slots: u64,
    /// The copied wallets' net flow a copier reads, in slots back: about
    /// one horizon of the fund's.
    pub flow_slots: u64,
}

impl Params {
    pub fn of(mode: Mode) -> Self {
        let base = Params {
            look_slots: 40,
            hold_slots: 200,
            threshold: 0.0,
            mark_slots: 200,
            chase_bps: 8.0,
            min_weight: 3.0,
            min_score_bps: 5.0,
            half_life_share: 0.25,
            momentum_bps: 8.0,
            share: 0.24,
            lag_slots: 0,
            flow_slots: 240,
        };
        match mode {
            Mode::Informed => Params {
                share: 0.6,
                ..base
            },
            Mode::FullCopy => Params {
                share: f64::INFINITY,
                ..base
            },
            Mode::FollowFlow => Params {
                look_slots: 40,
                hold_slots: 40,
                threshold: 4.0,
                ..base
            },
            Mode::Momentum => Params {
                look_slots: 4,
                hold_slots: 80,
                ..base
            },
            _ => base,
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
    length: u64,
    cap: f64,
    half_life: f64,
    fund: HashSet<String>,
    /// The fund's share, when told.
    copy_share: Option<f64>,
}

pub struct Copier {
    pub mode: Mode,
    pub params: Params,
    setup: Option<Setup>,
    seen: HashSet<String>,
    mids: VecDeque<(u64, f64)>,
    recent: VecDeque<Take>,
    pending: VecDeque<Take>,
    /// Takes to copy once their lag has passed.
    queued: VecDeque<Take>,
    /// The copied wallets' takes over the last `flow_slots`: slot and side
    /// times size.
    flow: VecDeque<(u64, f64)>,
    records: HashMap<String, Record>,
    /// The position it builds, and the slot it holds it until.
    target: f64,
    hold_until: u64,
    copies: u64,
}

impl Default for Copier {
    fn default() -> Self {
        Self::new(Mode::Reference)
    }
}

impl Copier {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            params: Params::of(mode),
            setup: None,
            seen: HashSet::new(),
            mids: VecDeque::new(),
            recent: VecDeque::new(),
            pending: VecDeque::new(),
            queued: VecDeque::new(),
            flow: VecDeque::new(),
            records: HashMap::new(),
            target: 0.0,
            hold_until: 0,
            copies: 0,
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

    /// Whether a copier copies `wallet`'s takes.
    fn copies_wallet(&self, wallet: &str) -> bool {
        match self.mode {
            Mode::Informed | Mode::FullCopy => {
                self.setup.as_ref().is_some_and(|s| s.fund.contains(wallet))
            }
            Mode::Reference => self.credible(wallet),
            _ => false,
        }
    }

    /// What it copies of a take: its share of the take's size.
    fn copy_size(&self, signed: f64) -> f64 {
        let share = match (self.mode, self.setup.as_ref().and_then(|s| s.copy_share)) {
            (Mode::Informed, Some(rule)) => self.params.share * rule,
            _ => self.params.share,
        };
        signed * share
    }

    /// A copier's target. It reads the copied wallets' net flow over the
    /// last `flow_slots`: a take adds its share the take's way only when
    /// that net flow points the same way and the position is flat or already
    /// that way. It never flips: a take against the position is read, not
    /// copied, and the position goes flat `hold_slots` after the last take
    /// it copied.
    fn copy(&mut self, now: u64, cap: f64) {
        while self
            .queued
            .front()
            .is_some_and(|t| t.at + self.params.lag_slots <= now)
        {
            let take = self.queued.pop_front().expect("checked");
            self.flow.push_back((take.at, take.signed));
            while self
                .flow
                .front()
                .is_some_and(|(at, _)| at + self.params.flow_slots < take.at)
            {
                self.flow.pop_front();
            }
            let net: f64 = self.flow.iter().map(|(_, signed)| signed).sum();
            let way = take.signed.signum();
            if net * way <= 0.0 || self.target * way < 0.0 {
                continue;
            }
            let add = self.copy_size(take.signed);
            self.target = if add.is_infinite() {
                cap * way
            } else {
                (self.target + add).clamp(-cap, cap)
            };
            self.hold_until = now + self.params.hold_slots;
            self.copies += 1;
        }
        if now > self.hold_until {
            self.target = 0.0;
        }
    }

    /// A non-reading rung's target.
    fn blind(&mut self, now: u64, cap: f64) -> Option<()> {
        let p = self.params;
        let length = self.setup.as_ref()?.length;
        let dir = match self.mode {
            Mode::TwapLong | Mode::TwapShort => {
                let way = if self.mode == Mode::TwapLong {
                    1.0
                } else {
                    -1.0
                };
                self.target = way * cap * (now as f64 / (length as f64 / 2.0)).min(1.0);
                return Some(());
            }
            Mode::Momentum => {
                let (then, mid) = (
                    self.mid_at(now.saturating_sub(p.look_slots))?,
                    self.mid_at(now)?,
                );
                let moved = (mid / then - 1.0) * 1e4;
                (moved.abs() >= p.momentum_bps).then(|| moved.signum())
            }
            Mode::FollowFlow => {
                let net: f64 = self
                    .recent
                    .iter()
                    .filter(|t| t.at + p.look_slots > now)
                    .map(|t| t.signed)
                    .sum();
                (net.abs() > p.threshold.max(1e-9)).then(|| net.signum())
            }
            _ => None,
        };
        if let Some(dir) = dir {
            self.target = dir * cap;
            self.hold_until = now + p.hold_slots;
        }
        if now > self.hold_until {
            self.target = 0.0;
        }
        Some(())
    }

    fn read_tape(&mut self, now: u64, mid: f64) {
        let Some(setup) = self.setup.as_ref() else {
            return;
        };
        let orders = new_orders(
            &setup.venue,
            &setup.orderbook,
            &setup.market,
            &mut self.seen,
        );
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
            if self.copies_wallet(&take.wallet) {
                self.queued.push_back(take.clone());
            }
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

impl Strategy for Copier {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?.clone();
        let orderbook = market.market_pubkey.to_string();
        let regime: Option<Value> = session
            .var("MTX_REGIME")
            .and_then(|r| serde_json::from_str(r).ok());
        let fund: HashSet<String> = regime
            .as_ref()
            .and_then(|r| r["wallets"]["informed"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|w| w.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let copy_share = regime
            .as_ref()
            .and_then(|r| r["shy"]["copy_share"].as_f64());
        if self.mode.told() && fund.is_empty() {
            return Err("a told rung needs MTX_REGIME's informed wallets".into());
        }
        let length = session.end_slot.saturating_sub(session.start_slot).max(1);
        log(&format!(
            "copier: {:?}, {} fund wallets told, share {:?}",
            self.mode,
            fund.len(),
            copy_share
        ));
        self.setup = Some(Setup {
            venue,
            market,
            orderbook,
            me: session.authority.clone(),
            start_slot: session.start_slot,
            length,
            cap: session.cap,
            half_life: (length as f64 * self.params.half_life_share).max(1.0),
            fund,
            copy_share,
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
        if !matches!(self.mode, Mode::Momentum | Mode::TwapLong | Mode::TwapShort) {
            self.read_tape(now, mid);
            self.mark_out(now, mid);
        }
        match self.mode {
            Mode::Reference | Mode::Informed | Mode::FullCopy => self.copy(now, cap),
            _ => {
                self.blind(now, cap);
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
        let found = credible.iter().filter(|w| setup.fund.contains(*w)).count();
        log(&format!(
            "copier: {} wallets seen, {} credible at the end ({} of them the told fund); {} copies",
            self.records.len(),
            credible.len(),
            found,
            self.copies
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

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Copier);
