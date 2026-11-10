//! Market maker for SPCX on Phoenix.
//!
//! Three things, in order of how much they earn:
//! - reads every taker order off the chain, and when the flow turns one-sided
//!   (the informed burst ahead of a jump) takes the position cap that way;
//! - quotes one tick inside the competing makers, one side only when holding
//!   inventory, so the position unwinds earning the spread;
//! - tracks its own equity pessimistically and shrinks, then stops, as the
//!   drawdown limit nears.

use std::collections::VecDeque;

use mtx_kit::prelude::*;
use mtx_kit::protocols::phoenix::eternal::phoenix_rise::{core::MarketOrderTicket, math::Side};
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;
use tracing::{info, warn};

/// Slots of taker flow the burst detector looks back over.
const WINDOW: u64 = 24;
/// A burst is a net imbalance of at least this many orders...
const MIN_NET: f64 = 5.0;
/// ...and this many standard deviations of the session's noise flow.
const Z_ENTER: f64 = 2.2;
/// Slots a burst's position is held for, waiting for the jump.
const HOLD: u64 = 28;
/// A step's move in the oracle that is a jump, not the walk.
const JUMP: f64 = 0.01;
/// Closest a quote goes to the oracle, in ticks.
const MIN_EDGE: i64 = 2;
/// Size of each quote, and the inventory past which only one side quotes.
const QUOTE: f64 = 3.0;
const SOFT: f64 = 6.0;
/// Drawdown (USD, own estimate) the position is sized against, and at which
/// the bot flattens and stops.
const DD_BUDGET: f64 = 205.0;
const DD_HALT: f64 = 170.0;
/// The worst single move the position is sized for.
const WORST_MOVE: f64 = 0.043;

struct Taker {
    slot: u64,
    sign: f64,
}

#[derive(Default)]
struct Bot {
    me: String,
    program: String,
    disc: Vec<u8>,
    side_at: usize,
    last_sig: Option<String>,
    flow: VecDeque<Taker>,
    seen: u64,
    start_slot: u64,
    tick: f64,
    comp_bid: i64,
    comp_ask: i64,
    seen_bid: VecDeque<i64>,
    seen_ask: VecDeque<i64>,
    half0: i64,
    my_bid: Option<(i64, f64)>,
    my_ask: Option<(i64, f64)>,
    prev_oracle: f64,
    prev_pos: f64,
    cash: f64,
    peak: f64,
    pending: f64,
    pending_px: f64,
    sig_dir: f64,
    sig_until: u64,
    halted: bool,
    started: bool,
}

impl Bot {
    /// New taker orders since the last step, oldest first.
    fn read_flow(&mut self, trader: &mtx_kit::Trader) -> BotResult<Vec<Taker>> {
        let mut opts = json!({"limit": 80, "commitment": "confirmed"});
        if let Some(sig) = &self.last_sig {
            opts["until"] = json!(sig);
        }
        let sigs: Value = trader
            .rpc
            .send(RpcRequest::GetSignaturesForAddress, json!([self.program, opts]))?;
        let sigs = sigs.as_array().cloned().unwrap_or_default();
        let first = self.last_sig.is_none();
        if let Some(newest) = sigs.first().and_then(|s| s["signature"].as_str()) {
            self.last_sig = Some(newest.to_string());
        }
        let mut out = Vec::new();
        if first {
            return Ok(out);
        }
        for s in sigs.iter().rev() {
            if !s["err"].is_null() {
                continue;
            }
            let Some(sig) = s["signature"].as_str() else { continue };
            let tx: Value = match trader.rpc.send(
                RpcRequest::GetTransaction,
                json!([sig, {"encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed"}]),
            ) {
                Ok(tx) => tx,
                Err(_) => continue,
            };
            let msg = &tx["transaction"]["message"];
            let keys = &msg["accountKeys"];
            if keys[0].as_str() == Some(self.me.as_str()) {
                continue;
            }
            for ix in msg["instructions"].as_array().into_iter().flatten() {
                let pid = ix["programIdIndex"].as_u64().unwrap_or(u64::MAX) as usize;
                if keys[pid].as_str() != Some(self.program.as_str()) {
                    continue;
                }
                let data = bs58::decode(ix["data"].as_str().unwrap_or(""))
                    .into_vec()
                    .unwrap_or_default();
                if data.len() <= self.side_at || !data.starts_with(&self.disc) {
                    continue;
                }
                let sign = if data[self.side_at] == 0 { 1.0 } else { -1.0 };
                out.push(Taker {
                    slot: tx["slot"].as_u64().unwrap_or(0),
                    sign,
                });
            }
        }
        Ok(out)
    }
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        let t = &session.trader;
        self.me = t.authority().to_string();
        self.start_slot = session.hello.start_slot;
        let market = t.phoenix.market(&t.env.symbol)?;
        self.tick = t.phoenix.ticks_to_usd(market, 1);
        // What a market order looks like on the wire: build one each way
        // (never sent) and keep the program, the discriminator and where the
        // side is.
        let mut datas = Vec::new();
        for side in [Side::Bid, Side::Ask] {
            let ticket = MarketOrderTicket::builder()
                .authority(t.authority())
                .trader_account(t.account)
                .symbol(t.env.symbol.as_str())
                .side(side)
                .num_base_lots(1000)
                .client_order_id(7)
                .price(100.0)
                .build()?;
            let ixs = t.phoenix.tx().place_market_order(ticket).await?;
            let ix = ixs
                .iter()
                .max_by_key(|ix| ix.data.len())
                .ok_or("no market order instruction")?;
            self.program = ix.program_id.to_string();
            datas.push(ix.data.clone());
        }
        self.disc = datas[0][..8].to_vec();
        self.side_at = datas[0]
            .iter()
            .zip(datas[1].iter())
            .position(|(a, b)| a != b)
            .unwrap_or(9);
        info!(program = self.program, side_at = self.side_at, tick = self.tick, "started");
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let Some(t) = orders.trader() else { return Ok(()) };
        let o = market.oracle;
        let ot = (o / self.tick).round() as i64;
        let pos = market.position.size;
        let cap = market.cap;

        // Own equity, priced against us: taker fills at their limit plus a
        // fee, passive fills at the oracle they were quoted around.
        let mut dpos = 0.0;
        if self.started {
            dpos = pos - self.prev_pos;
            let mut rest = dpos;
            if self.pending != 0.0 && rest * self.pending > 0.0 {
                let q = rest.abs().min(self.pending.abs()) * rest.signum();
                self.cash -= q * self.pending_px + q.abs() * self.pending_px * 0.0006;
                rest -= q;
            }
            self.cash -= rest * self.prev_oracle;
            if (o / self.prev_oracle - 1.0).abs() > JUMP {
                // The jump the burst announced (or one nothing did): either
                // way the flow before it says nothing about what's next.
                self.flow.clear();
                self.sig_dir = 0.0;
            }
        }
        self.pending = 0.0;
        self.started = true;
        self.prev_oracle = o;
        self.prev_pos = pos;
        let equity = self.cash + pos * o;
        self.peak = self.peak.max(equity);
        let dd = self.peak - equity;

        match self.read_flow(t) {
            Ok(new) => {
                self.seen += new.len() as u64;
                self.flow.extend(new);
            }
            Err(error) => warn!(%error, "couldn't read the flow"),
        }
        while self.flow.front().is_some_and(|f| f.slot + WINDOW < market.slot) {
            self.flow.pop_front();
        }

        // The competing quotes: the best bid and offer, when they aren't ours.
        let bbo = t.inner.bbo().await?;
        let (bb, ba) = (bbo.best_bid_ticks as i64, bbo.best_ask_ticks as i64);
        // A median of the last few sightings: one stale quote after a move
        // doesn't pin ours to the minimum edge.
        if !self.my_bid.is_some_and(|(p, _)| p >= bb) && bb > 0 {
            self.comp_bid = median(&mut self.seen_bid, (ot - bb).max(1));
        }
        if !self.my_ask.is_some_and(|(p, _)| p <= ba) && ba > 0 {
            self.comp_ask = median(&mut self.seen_ask, (ba - ot).max(1));
        }
        if self.half0 == 0 {
            self.half0 = self.comp_bid.max(self.comp_ask).clamp(8, 60);
        }

        // The burst detector.
        let elapsed = (market.slot.saturating_sub(self.start_slot)) as f64;
        let rate = (self.seen as f64 + 0.32 * 40.0) / (elapsed + 40.0);
        let net: f64 = self.flow.iter().map(|f| f.sign).sum();
        let z = net.abs() / (rate * WINDOW as f64).max(1.0).sqrt();
        if net.abs() >= MIN_NET && z >= Z_ENTER {
            self.sig_dir = net.signum();
            self.sig_until = market.slot + HOLD;
        }
        if self.sig_dir != 0.0 && market.slot > self.sig_until {
            self.sig_dir = 0.0;
        }

        let allowed = ((DD_BUDGET - dd) / (o * WORST_MOVE)).clamp(0.0, cap);
        if dd > DD_HALT {
            self.halted = true;
        }
        let slip = (self.half0 as f64 * 1.6 + 6.0) * self.tick;
        let resting = self.my_bid.is_some() || self.my_ask.is_some();

        info!(
            k = market.k, o, pos, eq = equity, dd, net, z, sig = self.sig_dir,
            cb = self.comp_bid, ca = self.comp_ask, allowed, "step"
        );

        // Past what the drawdown left allows (after a loss): cut down to it now,
        // rather than wait for the quotes to unwind it.
        let over = pos.abs() > allowed + 0.3;
        if self.halted || self.sig_dir != 0.0 || over {
            let target = if self.halted {
                0.0
            } else if self.sig_dir != 0.0 {
                self.sig_dir * allowed
            } else {
                pos.signum() * allowed
            };
            let diff = target - pos;
            if resting {
                // Cancel here, not on the sheet: a resting quote that outlived
                // a failed cancel could fill on top of the position taken.
                if let Err(error) = t.cancel_all() {
                    warn!(%error, "cancel failed; waiting a step");
                    return Ok(());
                }
                self.my_bid = None;
                self.my_ask = None;
            }
            if diff.abs() >= 0.3 {
                let px = if diff > 0.0 { o + slip } else { o - slip };
                let mut left = diff.abs();
                while left > 0.05 {
                    let q = left.min(8.0);
                    if diff > 0.0 {
                        orders.buy(q, px);
                    } else {
                        orders.sell(q, px);
                    }
                    left -= q;
                }
                self.pending = diff;
                let cost = (self.half0 as f64 * 1.15 + 3.0) * self.tick;
                self.pending_px = if diff > 0.0 { o + cost } else { o - cost };
            }
            return Ok(());
        }

        // Making: one tick inside the competition, never closer than
        // MIN_EDGE; only the reducing side when holding inventory.
        let bid_px = (ot - self.comp_bid + 1).min(ot - MIN_EDGE);
        let ask_px = (ot + self.comp_ask - 1).max(ot + MIN_EDGE);
        let lot = |x: f64| (x * 1000.0).floor() / 1000.0;
        let mut bid_sz = lot(QUOTE.min(allowed - pos).max(0.0));
        let mut ask_sz = lot(QUOTE.min(allowed + pos).max(0.0));
        if pos > SOFT {
            bid_sz = 0.0;
            ask_sz = lot(pos.min(4.0));
        } else if pos < -SOFT {
            ask_sz = 0.0;
            bid_sz = lot((-pos).min(4.0));
        }
        let want_bid = (bid_sz >= 0.05).then_some((bid_px, bid_sz));
        let want_ask = (ask_sz >= 0.05).then_some((ask_px, ask_sz));
        if dpos.abs() > 1e-9 || want_bid != self.my_bid || want_ask != self.my_ask {
            if resting {
                if let Err(error) = t.cancel_all() {
                    warn!(%error, "cancel failed; waiting a step");
                    return Ok(());
                }
                self.my_bid = None;
                self.my_ask = None;
            }
            if let Some((p, s)) = want_bid {
                orders.bid((p as f64 + 0.25) * self.tick, s);
            }
            if let Some((p, s)) = want_ask {
                orders.ask((p as f64 - 0.25) * self.tick, s);
            }
            self.my_bid = want_bid;
            self.my_ask = want_ask;
        }
        Ok(())
    }
}

fn median(seen: &mut VecDeque<i64>, fresh: i64) -> i64 {
    seen.push_back(fresh);
    if seen.len() > 5 {
        seen.pop_front();
    }
    let mut sorted: Vec<i64> = seen.iter().copied().collect();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn main() -> BotResult {
    mtx_kit::run(Bot::default())
}
