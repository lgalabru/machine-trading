//! Reference: find which trade-size class carries information today, then
//! follow that class's net flow into the moves.
//!
//! Each step it reads every Phoenix order sent to the market since the last
//! one (`getSignaturesForAddress` on the market at `confirmed`, then
//! `getTransaction`: the side and size of each order that takes) and sorts
//! each take into a class by its size: small under `size_cut`, large at or
//! over it. The session is cut into blocks of `block_slots`. A block whose
//! class net flow is `min_block_net` takes or more one way is a vote for
//! that way, scored by the mid's move over the next block. Votes in one
//! block are one observation, so a burst that runs for many takes counts
//! once: a trade-by-trade test would take a run of correlated takes for
//! many independent ones. Once one class's mean score is `t_cut` standard
//! errors above zero and `t_margin` above the other's, with `min_blocks`
//! scored blocks in each, it commits to that class for the session.
//!
//! The position is the cap the way the committed class's net flow over the
//! last `look_slots` points, when it is `threshold` takes or more, held
//! `hold_slots`.
//!
//! The same machinery runs the ladder's rungs ([`Mode`]): the informed bot,
//! told the class, follows it from the first take; the rungs that don't read
//! the classes follow one class, all flow, or the price's move.

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
    Reference,
    /// Told the session's informed class (`MTX_REGIME`).
    Informed,
    FollowSmall,
    FollowLarge,
    FollowAll,
    /// Follows all net flow at a fifth of the cap, on a low threshold: the
    /// cheapest way to do something.
    SmallFollowAll,
    Momentum,
}

impl Mode {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "reference" => Self::Reference,
            "informed" => Self::Informed,
            "follow-small" => Self::FollowSmall,
            "follow-large" => Self::FollowLarge,
            "follow-all" => Self::FollowAll,
            "small-follow-all" => Self::SmallFollowAll,
            "momentum" => Self::Momentum,
            _ => return None,
        })
    }
}

/// The rung's knobs.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Flow counted toward a signal, in slots back.
    pub look_slots: u64,
    /// How long a position is held after a signal.
    pub hold_slots: u64,
    /// Net takes a signal needs.
    pub threshold: f64,
    /// The size, in base units, from which a take is large.
    pub size_cut: f64,
    /// The share of the cap a position takes.
    pub size_share: f64,
    /// The price move over `look_slots` that momentum follows, in bps.
    pub momentum_bps: f64,
    /// The class test: block length, the net takes a block needs to vote,
    /// and the commitment rule.
    pub block_slots: u64,
    pub min_block_net: f64,
    pub t_cut: f64,
    pub t_margin: f64,
    pub min_blocks: u64,
}

impl Params {
    pub fn of(mode: Mode) -> Self {
        let base = Params {
            look_slots: 20,
            hold_slots: 150,
            threshold: 4.0,
            size_cut: 0.08,
            size_share: 1.0,
            momentum_bps: 15.0,
            block_slots: 75,
            min_block_net: 3.0,
            t_cut: 1.5,
            t_margin: 0.5,
            min_blocks: 4,
        };
        match mode {
            Mode::Reference | Mode::Informed => base,
            Mode::FollowSmall | Mode::FollowLarge => Params {
                look_slots: 40,
                threshold: 6.0,
                ..base
            },
            Mode::FollowAll => Params {
                look_slots: 40,
                threshold: 8.0,
                ..base
            },
            Mode::SmallFollowAll => Params {
                look_slots: 40,
                threshold: 2.0,
                hold_slots: 80,
                size_share: 0.2,
                ..base
            },
            Mode::Momentum => Params {
                look_slots: 40,
                hold_slots: 150,
                momentum_bps: 25.0,
                ..base
            },
        }
    }

    /// Overrides from `var(name)` (`LOOK`, `HOLD`, `THRESHOLD`, `SIZE_CUT`,
    /// `SIZE_SHARE`, `MOMENTUM`, `BLOCK`, `MIN_BLOCK_NET`, `T_CUT`,
    /// `T_MARGIN`, `MIN_BLOCKS`), for calibration.
    pub fn knobs(&mut self, var: impl Fn(&str) -> Option<String>) {
        let f = |name: &str| var(name).and_then(|v| v.parse::<f64>().ok());
        let u = |name: &str| var(name).and_then(|v| v.parse::<u64>().ok());
        if let Some(v) = u("LOOK") {
            self.look_slots = v;
        }
        if let Some(v) = u("HOLD") {
            self.hold_slots = v;
        }
        if let Some(v) = f("THRESHOLD") {
            self.threshold = v;
        }
        if let Some(v) = f("SIZE_CUT") {
            self.size_cut = v;
        }
        if let Some(v) = f("SIZE_SHARE") {
            self.size_share = v;
        }
        if let Some(v) = f("MOMENTUM") {
            self.momentum_bps = v;
        }
        if let Some(v) = u("BLOCK") {
            self.block_slots = v;
        }
        if let Some(v) = f("MIN_BLOCK_NET") {
            self.min_block_net = v;
        }
        if let Some(v) = f("T_CUT") {
            self.t_cut = v;
        }
        if let Some(v) = f("T_MARGIN") {
            self.t_margin = v;
        }
        if let Some(v) = u("MIN_BLOCKS") {
            self.min_blocks = v;
        }
    }
}

/// A size class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Small,
    Large,
}

#[derive(Clone, Copy)]
struct Take {
    at: u64,
    /// Side: +1 a buy, -1 a sell.
    side: f64,
    class: Class,
}

/// One class's block test: each block's net takes, and the scored votes.
#[derive(Default)]
struct Test {
    sum: f64,
    sum_sq: f64,
    n: u64,
}

impl Test {
    fn add(&mut self, score: f64) {
        self.sum += score;
        self.sum_sq += score * score;
        self.n += 1;
    }

    fn t(&self) -> f64 {
        if self.n < 2 {
            return 0.0;
        }
        let n = self.n as f64;
        let mean = self.sum / n;
        let var = (self.sum_sq / n - mean * mean).max(100.0);
        mean / (var / n).sqrt()
    }
}

struct Setup {
    venue: PhoenixVenue,
    market: ExchangeMarketConfig,
    orderbook: String,
    me: String,
    start_slot: u64,
    cap: f64,
}

pub struct Flow {
    pub mode: Mode,
    pub params: Params,
    setup: Option<Setup>,
    seen: HashSet<String>,
    mids: Vec<(u64, f64)>,
    recent: VecDeque<Take>,
    /// Net takes by block, per class.
    blocks: Vec<[f64; 2]>,
    scored: usize,
    tests: [Test; 2],
    class: Option<Class>,
    committed_at: Option<u64>,
    dir: f64,
    hold_until: u64,
    entries: u64,
    takes: [u64; 2],
}

impl Default for Flow {
    fn default() -> Self {
        Self::new(Mode::Reference)
    }
}

impl Flow {
    pub fn new(mode: Mode) -> Self {
        let class = match mode {
            Mode::FollowSmall => Some(Class::Small),
            Mode::FollowLarge => Some(Class::Large),
            _ => None,
        };
        Self {
            mode,
            params: Params::of(mode),
            setup: None,
            seen: HashSet::new(),
            mids: Vec::new(),
            recent: VecDeque::new(),
            blocks: Vec::new(),
            scored: 0,
            tests: [Test::default(), Test::default()],
            class,
            committed_at: None,
            dir: 0.0,
            hold_until: 0,
            entries: 0,
            takes: [0, 0],
        }
    }

    fn mid_at(&self, at: u64) -> Option<f64> {
        let i = self.mids.partition_point(|(t, _)| *t <= at);
        self.mids.get(i.saturating_sub(1)).map(|(_, m)| *m)
    }

    /// The signal's direction now, if it fires.
    fn signal(&self, now: u64) -> Option<f64> {
        let p = self.params;
        let window = self.recent.iter().filter(|t| t.at + p.look_slots > now);
        let net: f64 = match self.mode {
            Mode::Momentum => {
                let then = self.mid_at(now.saturating_sub(p.look_slots))?;
                let moved = (self.mid_at(now)? / then - 1.0) * 1e4;
                return (moved.abs() >= p.momentum_bps).then(|| moved.signum());
            }
            Mode::FollowAll | Mode::SmallFollowAll => window.map(|t| t.side).sum(),
            Mode::Reference | Mode::Informed | Mode::FollowSmall | Mode::FollowLarge => {
                let class = self.class?;
                window.filter(|t| t.class == class).map(|t| t.side).sum()
            }
        };
        (net.abs() >= p.threshold.max(1.0)).then(|| net.signum())
    }

    fn read_tape(&mut self, now: u64) {
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
        for (slot, wallet, signed) in orders {
            if wallet == me {
                continue;
            }
            let at = slot.saturating_sub(start).min(now);
            let class = if signed.abs() >= p.size_cut {
                Class::Large
            } else {
                Class::Small
            };
            let side = signed.signum();
            self.takes[class as usize] += 1;
            let block = (at / p.block_slots) as usize;
            if self.blocks.len() <= block {
                self.blocks.resize(block + 1, [0.0, 0.0]);
            }
            self.blocks[block][class as usize] += side;
            self.recent.push_back(Take { at, side, class });
        }
        while self
            .recent
            .front()
            .is_some_and(|t| t.at + p.look_slots < now)
        {
            self.recent.pop_front();
        }
    }

    /// Scores every block whose next block has ended, and commits to a class
    /// once the test settles it.
    fn test_classes(&mut self, now: u64) {
        let p = self.params;
        let b = p.block_slots;
        while self.scored < self.blocks.len() && (self.scored as u64 + 2) * b <= now {
            let k = self.scored as u64;
            let (Some(from), Some(to)) = (self.mid_at((k + 1) * b), self.mid_at((k + 2) * b)) else {
                break;
            };
            let moved = (to / from - 1.0) * 1e4;
            for c in 0..2 {
                let net = self.blocks[self.scored][c];
                if net.abs() >= p.min_block_net {
                    self.tests[c].add(net.signum() * moved);
                }
            }
            self.scored += 1;
        }
        if self.class.is_some() || self.mode != Mode::Reference {
            return;
        }
        let (ts, tl) = (self.tests[0].t(), self.tests[1].t());
        let enough = self.tests.iter().all(|t| t.n >= p.min_blocks);
        if !enough {
            return;
        }
        if ts > p.t_cut && ts - tl > p.t_margin {
            self.class = Some(Class::Small);
        } else if tl > p.t_cut && tl - ts > p.t_margin {
            self.class = Some(Class::Large);
        }
        if self.class.is_some() {
            self.committed_at = Some(now);
            log(&format!(
                "flow: committed to {:?} at slot {now} (t small {ts:.2}, large {tl:.2})",
                self.class
            ));
        }
    }
}

impl Strategy for Flow {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?.clone();
        let orderbook = market.market_pubkey.to_string();
        if self.mode == Mode::Informed {
            let regime: Option<Value> = session
                .var("MTX_REGIME")
                .and_then(|r| serde_json::from_str(r).ok());
            self.class = match regime.as_ref().and_then(|r| r["informed_class"].as_str()) {
                Some("small") => Some(Class::Small),
                Some("large") => Some(Class::Large),
                _ => return Err("the informed bot needs MTX_REGIME's informed_class".into()),
            };
        }
        log(&format!("flow: {:?}, class {:?}", self.mode, self.class));
        self.setup = Some(Setup {
            venue,
            market,
            orderbook,
            me: session.authority.clone(),
            start_slot: session.start_slot,
            cap: session.cap,
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
        self.mids.push((now, mid));
        if self.mode != Mode::Momentum {
            self.read_tape(now);
            self.test_classes(now);
        }
        if now >= self.hold_until {
            match self.signal(now) {
                Some(dir) => {
                    if dir != self.dir {
                        self.entries += 1;
                    }
                    self.dir = dir;
                    self.hold_until = now + self.params.hold_slots;
                }
                None => self.dir = 0.0,
            }
        }
        if market.last || market.slots_left <= 4 {
            self.dir = 0.0;
        }
        let target = self.dir * cap * self.params.size_share;
        if (target - market.position.size).abs() >= 0.01 {
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
        log(&format!(
            "flow: {:?}; takes small {}, large {}; class {:?} from {:?}; t small {:.2} ({} blocks), large {:.2} ({} blocks); {} entries",
            self.mode,
            self.takes[0],
            self.takes[1],
            self.class,
            self.committed_at,
            self.tests[0].t(),
            self.tests[0].n,
            self.tests[1].t(),
            self.tests[1].n,
            self.entries
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
mtx_kit::export!(Flow);
