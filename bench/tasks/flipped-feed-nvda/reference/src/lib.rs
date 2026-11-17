//! Reference: trade NVDA's open off the house model's calls, once the
//! market has shown which of the feed's two gateways reports them the right
//! way round.
//!
//! The gateways' wallets come in the session's vars (`MTX_FEED`). Each call
//! is a memo one of them signs: `{"call", "side", "horizon_slots",
//! "confidence"}`. One gateway reports the side the model would take; the
//! other, the opposite. Which is which is not said.
//!
//! - **Reading.** Every step it lists each gateway's signatures and takes in
//!   the calls it hasn't seen, with the index where it saw them. Once a
//!   call's horizon has passed it marks it out: the index's move since,
//!   signed by the call's side.
//! - **Weighing.** A call that marked out well says its gateway is the
//!   model's own; one that marked out badly says it is the other. Calls
//!   within [`BLOCK_GAP_SLOTS`] of each other ride the same move and the
//!   same right or wrong view, so a burst counts once: by its majority. It
//!   adds each burst's say to a log-odds, at a hit rate it assumes
//!   ([`ASSUMED_HIT`]), and commits once the odds reach [`COMMIT_ODDS`].
//! - **Trading.** Until then it stays flat. After, it follows each new call
//!   from the gateway it trusts and fades each from the other, at full size,
//!   until the call's horizon has passed.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`).

use std::collections::BTreeSet;

use mtx_kit::{FEED_VAR, log, prelude::*, rpc};

/// The size it trades on a call: the mandate's cap.
pub const SIZE: f64 = 4.0;
/// How far from the mark a market order may fill, in basis points.
pub const SLIPPAGE_BPS: f64 = 15.0;
/// Calls nearer than this, in slots, are one burst.
pub const BLOCK_GAP_SLOTS: u64 = 240;
/// The hit rate it weighs a burst at, and the odds it commits at.
pub const ASSUMED_HIT: f64 = 0.75;
pub const COMMIT_ODDS: f64 = 0.95;

/// One call, as the bot saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    /// Which of the feed's wallets sent it, as `MTX_FEED` lists them.
    pub gateway: usize,
    pub id: u64,
    /// `1` for a buy, `-1` for a sell.
    pub sign: f64,
    pub horizon: u64,
    pub confidence: f64,
    /// The slot it was seen at, and the index there.
    pub slot: u64,
    pub index_at: f64,
    /// The index's move over its horizon, in basis points, once known.
    pub markout: Option<f64>,
}

impl Seen {
    /// Whether the market went the call's way: `1`, `-1`, or `0` for no
    /// move or no markout yet.
    pub fn verdict(&self) -> f64 {
        match self.markout {
            Some(m) if m != 0.0 => (self.sign * m).signum(),
            _ => 0.0,
        }
    }
}

/// The feed's gateways and every call seen on them.
#[derive(Debug, Clone, Default)]
pub struct Feed {
    pub wallets: Vec<String>,
    signatures: BTreeSet<String>,
    pub calls: Vec<Seen>,
}

impl Feed {
    pub fn from_session(session: &Session) -> BotResult<Self> {
        let wallets: Vec<String> = session
            .var(FEED_VAR)
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or("no MTX_FEED: the session names no feed")?;
        Ok(Self {
            wallets,
            ..Self::default()
        })
    }

    /// Takes in the gateways' new calls, oldest first, and marks out those
    /// whose horizon has passed. Returns the new calls' indices.
    pub fn poll(&mut self, market: &Market) -> Vec<usize> {
        let mut new = Vec::new();
        for (gateway, wallet) in self.wallets.iter().enumerate() {
            let params = serde_json::json!([wallet, { "limit": 20, "commitment": "confirmed" }]);
            let Ok(answer) = rpc("getSignaturesForAddress", &params.to_string()) else {
                continue;
            };
            let Ok(serde_json::Value::Array(list)) = serde_json::from_str(&answer) else {
                continue;
            };
            for entry in list.iter().rev() {
                let Some(signature) = entry["signature"].as_str() else {
                    continue;
                };
                if self.signatures.contains(signature) {
                    continue;
                }
                self.signatures.insert(signature.to_string());
                if let Some(call) = entry["memo"].as_str().and_then(|m| parse(m, gateway, market)) {
                    new.push(call);
                }
            }
        }
        new.sort_by_key(|c| c.id);
        let from = self.calls.len();
        self.calls.extend(new);
        for call in &mut self.calls {
            if call.markout.is_none() && market.slot >= call.slot + call.horizon {
                call.markout = Some((market.index / call.index_at - 1.0) * 10_000.0);
            }
        }
        (from..self.calls.len()).collect()
    }
}

/// A call from its memo, as `getSignaturesForAddress` lists it: `[61]
/// {"call":3,...}`.
fn parse(memo: &str, gateway: usize, market: &Market) -> Option<Seen> {
    let call: serde_json::Value = serde_json::from_str(&memo[memo.find('{')?..]).ok()?;
    let sign = match call["side"].as_str()? {
        "buy" => 1.0,
        "sell" => -1.0,
        _ => return None,
    };
    Some(Seen {
        gateway,
        id: call["call"].as_u64()?,
        sign,
        horizon: call["horizon_slots"].as_u64()?,
        confidence: call["confidence"].as_f64().unwrap_or(0.5),
        slot: market.slot,
        index_at: market.index,
        markout: None,
    })
}

/// What it holds on the calls it acted on: a direction until a slot.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hold {
    pub direction: f64,
    pub until: u64,
}

impl Hold {
    /// Be `direction` (1 long, -1 short) until `until`: a call the same way
    /// as the held one extends it, one the other way replaces it.
    pub fn take(&mut self, direction: f64, until: u64) {
        if direction == self.direction {
            self.until = self.until.max(until);
        } else {
            *self = Hold { direction, until };
        }
    }

    /// The position it wants at `slot`.
    pub fn target(&self, slot: u64, size: f64) -> f64 {
        if slot < self.until {
            self.direction * size
        } else {
            0.0
        }
    }
}

/// Trades toward `target` with market orders.
pub fn trade_to(market: &Market, orders: &mut Orders, target: f64) {
    if (target - market.position.size).abs() < 0.01 {
        return;
    }
    orders.target_position_with(
        target,
        Execution {
            max_per_step: f64::INFINITY,
            slice: SIZE,
            slippage_bps: SLIPPAGE_BPS,
            min_trade: 0.01,
        },
    );
}

/// The direction a call says to trade, given which gateway reports the
/// model's own side.
pub fn direction(call: &Seen, taker_side: usize) -> f64 {
    if call.gateway == taker_side {
        call.sign
    } else {
        -call.sign
    }
}

/// Weighs the calls' markouts burst by burst, for which gateway is the
/// model's own.
#[derive(Debug, Clone, Default)]
pub struct Weigher {
    /// Each burst's calls, by index into the feed's, and whether it has
    /// been weighed.
    bursts: Vec<(Vec<usize>, bool)>,
    /// The log-odds that gateway 0 is the model's own.
    pub log_odds: f64,
    /// The gateway it decided reports the model's own side.
    pub taker_side: Option<usize>,
}

impl Weigher {
    /// Takes in a step's new calls and weighs every burst that is over and
    /// marked out.
    pub fn update(&mut self, feed: &Feed, new: &[usize], slot: u64) {
        for &i in new {
            let at = feed.calls[i].slot;
            match self.bursts.last_mut() {
                Some((members, false))
                    if members
                        .last()
                        .is_some_and(|&j| at <= feed.calls[j].slot + BLOCK_GAP_SLOTS) =>
                {
                    members.push(i)
                }
                _ => self.bursts.push((vec![i], false)),
            }
        }
        if self.taker_side.is_some() {
            return;
        }
        let step = (ASSUMED_HIT / (1.0 - ASSUMED_HIT)).ln();
        let limit = (COMMIT_ODDS / (1.0 - COMMIT_ODDS)).ln();
        for (members, weighed) in &mut self.bursts {
            let last = members.last().map_or(0, |&j| feed.calls[j].slot);
            let marked = members.iter().all(|&j| feed.calls[j].markout.is_some());
            if *weighed || !marked || slot <= last + BLOCK_GAP_SLOTS {
                continue;
            }
            *weighed = true;
            // A call that marked out well says its gateway is the model's
            // own: gateway 0's for, gateway 1's against.
            let say: f64 = members
                .iter()
                .map(|&j| {
                    let call = &feed.calls[j];
                    let of_zero = if call.gateway == 0 { 1.0 } else { -1.0 };
                    of_zero * call.verdict()
                })
                .sum();
            self.log_odds += say.signum() * step;
            if self.log_odds.abs() > limit {
                let taker_side = usize::from(self.log_odds < 0.0);
                log(&format!(
                    "slot {slot}: gateway {taker_side} reports the model's side (log-odds {:.2})",
                    self.log_odds
                ));
                self.taker_side = Some(taker_side);
                return;
            }
        }
    }
}

/// The strategy.
#[derive(Default)]
pub struct Reference {
    feed: Feed,
    weigher: Weigher,
    hold: Hold,
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.feed = Feed::from_session(session)?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let new = self.feed.poll(market);
        let decided = self.weigher.taker_side;
        self.weigher.update(&self.feed, &new, market.slot);
        if let Some(taker_side) = decided {
            for &i in &new {
                let call = &self.feed.calls[i];
                self.hold
                    .take(direction(call, taker_side), call.slot + call.horizon);
            }
        }
        trade_to(market, orders, self.hold.target(market.slot, SIZE));
        Ok(())
    }
}

#[cfg(all(target_arch = "wasm32", feature = "export"))]
mtx_kit::export!(Reference);
