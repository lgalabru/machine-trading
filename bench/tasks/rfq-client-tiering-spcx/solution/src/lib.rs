//! The RFQ desk: answers SPCX requests for quotes at each client's reach,
//! and declines the clients it has found informed.
//!
//! A client takes a quote no worse than its value by its reach:
//! `tolerance_bps` for one trade size (0.021 SPCX), times the square root
//! of the size in trade sizes. An uninformed client values SPCX at the
//! level, which Phoenix's index shows to the cent; an informed one at where
//! the level is going, and asks the way it is going.
//!
//! - **Reach.** The family's tolerance is 0.2 to 0.5 bp; the desk narrows
//!   it from its own answers: a declined quote was past the reach, a taken
//!   one (from a client not flagged) within it.
//! - **Hit ratio on wide quotes.** Each client's first request is answered
//!   for one trade size, just past the widest reach the family allows. No uninformed client takes that; an informed one does when its
//!   view is worth more than the gap. A client that takes one is flagged.
//! - **Markouts.** Every request of a client, answered or not, is marked
//!   against the index a minute later, signed by its side; a client whose
//!   record says informed is flagged too.
//! - **Flow.** A flagged client's request says which way SPCX is going: for
//!   a minute after it, the desk doesn't sell to a buyer (or buy from a
//!   seller) on its side.
//! - **Inventory.** No answer takes the desk's SPCX more than 1 SPCX from
//!   where it started.

use std::collections::{HashMap, HashSet};

use mtx_kit::{LineOptions, Rfq, prelude::*};

/// One trade size, in SPCX: a request's size over it is its size in trade
/// sizes.
pub const TRADE_SIZE: f64 = 0.021;
/// The family's tolerance, bp for one trade size.
pub const TOLERANCE: (f64, f64) = (0.2, 0.5);
/// How far inside the reach a quote sits: the index is the level to a cent.
pub const EPS_BP: f64 = 0.4;
/// The most the desk's SPCX may move from its start, in SPCX.
pub const SOFT: f64 = 1.0;

/// A request as the desk saw it.
#[derive(Debug, Clone)]
pub struct Ask {
    pub slot: u64,
    pub client: String,
    /// +1: the client buys.
    pub sign: f64,
    /// Its size, in trade sizes.
    pub n: f64,
    pub index: f64,
}

/// What became of an answer.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub slot: u64,
    pub client: String,
    pub filled: bool,
    pub tag: Tag,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tag {
    /// A quote at the desk's reach estimate, for this tolerance.
    Reach(f64),
    Probe,
    Other,
}

pub enum Answer {
    Decline,
    /// `bp` past the index, for `size` SPCX (the whole request without).
    Quote { bp: f64, size: Option<f64>, tag: Tag },
}

/// The desk's view at a request.
pub struct View<'a> {
    pub slot: u64,
    pub index: f64,
    /// The index at every step so far, by slot.
    pub history: &'a [(u64, f64)],
    /// Every request so far, this one last.
    pub requests: &'a [Ask],
    pub outcomes: &'a [Outcome],
}

impl View<'_> {
    /// The index at the first step at or after `slot`, if one has come.
    pub fn index_at(&self, slot: u64) -> Option<f64> {
        let i = self.history.partition_point(|(s, _)| *s < slot);
        self.history.get(i).map(|(_, v)| *v)
    }

    /// The index's sd per slot over the last `window` slots.
    pub fn sd_per_slot(&self, window: u64) -> Option<f64> {
        let from = self.slot.saturating_sub(window);
        let i = self.history.partition_point(|(s, _)| *s < from);
        let tail = &self.history[i..];
        if tail.len() < 20 {
            return None;
        }
        let r: Vec<f64> = tail
            .windows(2)
            .map(|w| (w[1].1 / w[0].1).ln() / ((w[1].0 - w[0].0).max(1) as f64).sqrt())
            .collect();
        let mean = r.iter().sum::<f64>() / r.len() as f64;
        let var = r.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / r.len() as f64;
        Some(var.sqrt())
    }
}

pub trait Policy {
    fn start(&mut self, session: &Session) {
        let _ = session;
    }
    fn answer(&mut self, view: &View<'_>, ask: &Ask) -> Answer;
}

/// The client a request comes from: `c3` of `c3-17`.
pub fn client_of(id: &str) -> String {
    id.split('-').next().unwrap_or(id).to_string()
}

/// A reach, in bp, for `n` trade sizes at `tolerance`.
pub fn reach_bp(tolerance: f64, n: f64) -> f64 {
    tolerance * n.max(0.0).sqrt()
}

/// What was answered at the last step and is still waiting on its client.
struct Pending {
    slot: u64,
    client: String,
    /// The desk's change in SPCX if it fills.
    base: f64,
    price: f64,
    size: f64,
    tag: Tag,
}

/// The run loop around a policy: the index's history, the requests, what
/// became of each answer, and the inventory.
pub struct Desk<P> {
    pub policy: P,
    history: Vec<(u64, f64)>,
    requests: Vec<Ask>,
    outcomes: Vec<Outcome>,
    seen: HashSet<String>,
    pending: Vec<Pending>,
    inventory: f64,
}

impl<P: Policy> Desk<P> {
    pub fn new(policy: P) -> Self {
        Self {
            policy,
            history: Vec::new(),
            requests: Vec::new(),
            outcomes: Vec::new(),
            seen: HashSet::new(),
            pending: Vec::new(),
            inventory: 0.0,
        }
    }

    /// Settle last step's answers against this step's fills.
    fn settle(&mut self, market: &Market) {
        let mut fills: Vec<_> = market
            .fills
            .iter()
            .filter(|f| f.venue.starts_with("rfq:"))
            .collect();
        for p in std::mem::take(&mut self.pending) {
            let hit = fills.iter().position(|f| {
                (f.price - p.price).abs() <= 1e-6 * p.price && (f.size - p.size).abs() <= 1e-9
            });
            let filled = hit.is_some();
            if let Some(i) = hit {
                fills.swap_remove(i);
                self.inventory += p.base;
            }
            self.outcomes.push(Outcome {
                slot: p.slot,
                client: p.client,
                filled,
                tag: p.tag,
            });
        }
    }
}

impl<P: Policy> Strategy for Desk<P> {
    fn start(&mut self, session: &Session) -> BotResult {
        self.policy.start(session);
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.settle(market);
        if market.index <= 0.0 {
            return Ok(());
        }
        self.history.push((market.slot, market.index));
        let mut exposure = 0.0;
        let open: Vec<&Rfq> = market
            .rfqs
            .iter()
            .filter(|r| !self.seen.contains(&r.id))
            .collect();
        for rfq in open {
            self.seen.insert(rfq.id.clone());
            let Some(wants) = rfq.side else { continue };
            let sign = if wants == Side::Bid { 1.0 } else { -1.0 };
            let ask = Ask {
                slot: market.slot,
                client: client_of(&rfq.id),
                sign,
                n: rfq.size / TRADE_SIZE,
                index: market.index,
            };
            self.requests.push(ask.clone());
            let view = View {
                slot: market.slot,
                index: market.index,
                history: &self.history,
                requests: &self.requests,
                outcomes: &self.outcomes,
            };
            let Answer::Quote { bp, size, tag } = self.policy.answer(&view, &ask) else {
                continue;
            };
            let size = size.map_or(rfq.size, |s| s.min(rfq.size));
            let base = -sign * size;
            let after = self.inventory + exposure + base;
            if after.abs() > SOFT && after.abs() > (self.inventory + exposure).abs() {
                continue;
            }
            exposure += base;
            let price = market.index * (1.0 + sign * bp / 1e4);
            let (bid, ask_px) = if sign > 0.0 {
                (None, Some(price))
            } else {
                (Some(price), None)
            };
            orders.push(
                Intent::Respond {
                    id: rfq.id.clone(),
                    bid,
                    ask: ask_px,
                    size: Some(size),
                },
                LineOptions {
                    venue: Some(rfq.venue.clone()),
                    ..LineOptions::default()
                },
            );
            self.pending.push(Pending {
                slot: market.slot,
                client: ask.client,
                base,
                price,
                size,
                tag,
            });
        }
        Ok(())
    }
}

/// The reference: reach learned from its own answers, a probe past the
/// widest reach for each client's first requests, request markouts, and
/// its flagged clients' flow.
pub struct Reference {
    lo: f64,
    hi: f64,
    absorbed: usize,
    probes: HashMap<String, u32>,
    flagged: HashSet<String>,
}

/// The reference's settings.
const PROBES: u32 = 1;
const PROBE_BP: f64 = 0.5;
const MARKOUT_SLOTS: u64 = 60;
const PRIOR: f64 = 0.3;
const FLAG: f64 = 0.7;
const INFORMED_MARKOUT: f64 = 0.8;
const SMALL_N: f64 = 4.0;
const NEED: usize = 2;
const FOLLOW_SLOTS: u64 = 60;

impl Default for Reference {
    fn default() -> Self {
        Self {
            lo: TOLERANCE.0,
            hi: TOLERANCE.1,
            absorbed: 0,
            probes: HashMap::new(),
            flagged: HashSet::new(),
        }
    }
}

impl Reference {
    fn absorb(&mut self, outcomes: &[Outcome]) {
        for o in &outcomes[self.absorbed..] {
            match o.tag {
                Tag::Probe if o.filled => {
                    self.flagged.insert(o.client.clone());
                }
                Tag::Reach(t) if !o.filled => {
                    self.hi = self.hi.min(t);
                    if self.lo >= self.hi {
                        self.lo = TOLERANCE.0;
                    }
                }
                Tag::Reach(t) if !self.flagged.contains(&o.client) => {
                    self.lo = self.lo.max(t.min(self.hi));
                }
                _ => {}
            }
        }
        self.absorbed = outcomes.len();
    }

    /// The odds that `client` is informed, from its requests' markouts, and
    /// how many are on record.
    fn posterior(&self, view: &View<'_>, client: &str) -> (f64, usize) {
        let sd = view.sd_per_slot(400).unwrap_or(7.8e-5);
        let sd_m = (sd * (MARKOUT_SLOTS as f64).sqrt()).max(1e-6);
        let mu = INFORMED_MARKOUT * sd_m;
        let mut llr = (PRIOR / (1.0 - PRIOR)).ln();
        let mut count = 0;
        for r in view.requests.iter().filter(|r| r.client == client) {
            if r.slot + MARKOUT_SLOTS > view.slot {
                continue;
            }
            let Some(later) = view.index_at(r.slot + MARKOUT_SLOTS) else {
                continue;
            };
            let x = (r.sign * (later - r.index) / r.index).clamp(-3.0 * sd_m, 3.0 * sd_m);
            llr += (x * mu - mu * mu / 2.0) / (sd_m * sd_m);
            count += 1;
        }
        (1.0 / (1.0 + (-llr).exp()), count)
    }
}

impl Policy for Reference {
    fn answer(&mut self, view: &View<'_>, ask: &Ask) -> Answer {
        self.absorb(view.outcomes);
        if self.flagged.contains(&ask.client) {
            return Answer::Decline;
        }
        let probes = self.probes.entry(ask.client.clone()).or_insert(0);
        if *probes < PROBES {
            *probes += 1;
            return Answer::Quote {
                bp: TOLERANCE.1 + PROBE_BP,
                size: Some(TRADE_SIZE),
                tag: Tag::Probe,
            };
        }
        let (odds, count) = self.posterior(view, &ask.client);
        if odds >= FLAG {
            return Answer::Decline;
        }
        let follow = view
            .requests
            .iter()
            .rev()
            .skip(1)
            .take_while(|r| r.slot + FOLLOW_SLOTS > view.slot)
            .find(|r| self.flagged.contains(&r.client));
        if follow.is_some_and(|r| r.sign == ask.sign) {
            return Answer::Decline;
        }
        let t = if self.hi - self.lo > 0.06 {
            0.5 * (self.lo + self.hi)
        } else {
            self.lo
        };
        let size = (count < NEED).then_some(SMALL_N * TRADE_SIZE);
        let n = size.map_or(ask.n, |s| ask.n.min(s / TRADE_SIZE));
        Answer::Quote {
            bp: reach_bp(t, n) - EPS_BP,
            size,
            tag: Tag::Reach(t),
        }
    }
}

/// The latent state a told bot reads from `MTX_REGIME`.
#[derive(Debug, Clone, Default)]
pub struct Told {
    pub informed: HashSet<String>,
    pub lead_slots: u64,
    pub tolerance_bps: f64,
}

impl Told {
    pub fn read(session: &Session) -> Self {
        let Some(regime) = session.var("MTX_REGIME") else {
            return Self::default();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(regime) else {
            return Self::default();
        };
        let informed = v["wallets"]["rfq-informed"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        Self {
            informed,
            lead_slots: v["lead_slots"].as_u64().unwrap_or(0),
            tolerance_bps: v["tolerance_bps"].as_f64().unwrap_or(TOLERANCE.0),
        }
    }
}

impl<P: Policy + Default> Default for Desk<P> {
    fn default() -> Self {
        Self::new(P::default())
    }
}

/// The task's reference: the desk with the reference's policy.
pub type Bot = Desk<Reference>;

#[cfg(all(target_arch = "wasm32", feature = "export"))]
mtx_kit::export!(Bot);
