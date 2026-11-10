//! A market maker for Phoenix's SPCX perp.
//!
//! Quotes both sides inside the competing makers, skewed against inventory,
//! and watches its own fills for one-sided bursts of taker flow: a burst is
//! what informed takers look like ahead of a jump, so the bot leans into it
//! until the jump lands or the burst times out.

use std::collections::VecDeque;

use mtx_kit::phoenix_rise::core::{LimitOrderTicket, MarketOrderTicket};
use mtx_kit::prelude::*;
use mtx_kit::{Instruction, Trader};

const TICK: f64 = 0.01;
/// Never hold or rest past this, whatever the mandate's cap says.
const CAP_MARGIN: f64 = 0.1;

// Quoting.
const QUOTE_FRAC: f64 = 0.6; // of the competitor half-spread
const QUOTE_SIZE: f64 = 3.0;
const NORMAL_CAP: f64 = 6.0;
const SKEW: f64 = 0.8;

// Burst detection.
const WINDOW_SLOTS: u64 = 12;
const MIN_COUNT: f64 = 6.0;
const Z: f64 = 3.0;
const HOLD_SLOTS: u64 = 32;
/// After a lean, slots allowed for working the inventory off with quotes.
const UNWIND_SLOTS: u64 = 150;
const COOL_SLOTS: u64 = 6;
const SIGNAL_POS: f64 = 19.0;
const KEEP_AFTER: f64 = 5.0;
const JUMP_FRAC: f64 = 0.01;
const BACKOFF: f64 = 3.0;
const VOL_T: f64 = 4.5;

#[derive(Default, Clone, Copy, PartialEq, Debug)]
struct Resting {
    bid_ticks: i64,
    bid_rem: f64,
    ask_ticks: i64,
    ask_rem: f64,
    known: bool,
}

#[derive(Clone, Copy)]
struct Signal {
    dir: f64,
    slot: u64,
    oracle: f64,
}

#[derive(Default)]
struct Maker {
    symbol: String,
    half_ticks: f64,
    next_id: u128,
    last_pos: Option<f64>,
    last_oracle: f64,
    resting: Resting,
    flow: VecDeque<(u64, f64)>,
    total_count: f64,
    first_slot: u64,
    signal: Option<Signal>,
    cool_until: u64,
    unwind_until: u64,
    equity: f64,
    peak: f64,
    tape: Option<Tape>,
    last_sig: String,
    tape_ok: bool,
    me: String,
    vol: VecDeque<(u64, f64)>,
}

/// How to recognise a taker's market order in a block, learnt from the
/// bot's own order builder: the program, the data's length, and where the
/// side sits in it.
struct Tape {
    program: String,
    len: usize,
    side_off: usize,
    bid_byte: u8,
    ask_byte: u8,
    /// The instruction's discriminant.
    disc: Vec<u8>,
}

impl Maker {
    fn id(&mut self) -> u128 {
        self.next_id += 1;
        self.next_id
    }

    async fn limit_ix(
        &mut self,
        t: &Trader,
        side: Side,
        ticks: i64,
        size: f64,
    ) -> BotResult<Vec<Instruction>> {
        let ticket = LimitOrderTicket::builder()
            .authority(t.authority())
            .trader_account(t.account)
            .symbol(self.symbol.as_str())
            .side(side)
            .price(ticks as f64 / 100.0)
            .num_base_lots(t.lots(size)?)
            .client_order_id(self.id())
            .build()?;
        Ok(t.phoenix.tx().place_limit_order(ticket).await?)
    }

    async fn market_ix(
        &mut self,
        t: &Trader,
        side: Side,
        ticks: i64,
        size: f64,
    ) -> BotResult<Vec<Instruction>> {
        let ticket = MarketOrderTicket::builder()
            .authority(t.authority())
            .trader_account(t.account)
            .symbol(self.symbol.as_str())
            .side(side)
            .price(ticks as f64 / 100.0)
            .num_base_lots(t.lots(size)?)
            .client_order_id(self.id())
            .build()?;
        Ok(t.phoenix.tx().place_market_order(ticket).await?)
    }

    /// The takers' market orders in the blocks since the last turn: how
    /// many bought and how many sold.
    fn read_tape(&mut self, t: &Trader, slot: u64) -> (u32, u32) {
        use solana_rpc_client_api::request::RpcRequest as R;
        let Some(tape) = &self.tape else {
            return (0, 0);
        };
        let sigs = match t.rpc.send::<serde_json::Value>(
            R::GetSignaturesForAddress,
            serde_json::json!([tape.program, {"limit": 30, "commitment": "confirmed"}]),
        ) {
            Ok(v) => v,
            Err(_) => {
                self.tape_ok = false;
                return (0, 0);
            }
        };
        let Some(sigs) = sigs.as_array() else {
            self.tape_ok = false;
            return (0, 0);
        };
        self.tape_ok = true;
        let first_turn = self.last_sig.is_empty();
        let mut fresh: Vec<&str> = Vec::new();
        for entry in sigs {
            let Some(sig) = entry["signature"].as_str() else {
                continue;
            };
            if sig == self.last_sig {
                break;
            }
            if entry["slot"].as_u64().is_some_and(|s| s + 8 < slot) {
                break;
            }
            if entry["err"].is_null() {
                fresh.push(sig);
            }
        }
        if let Some(newest) = sigs.first().and_then(|e| e["signature"].as_str()) {
            self.last_sig = newest.to_string();
        }
        if first_turn {
            return (0, 0);
        }
        let (mut buys, mut sells) = (0, 0);
        for sig in fresh {
            let Ok(tx) = t.rpc.send::<serde_json::Value>(
                R::GetTransaction,
                serde_json::json!([sig, {"encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed"}]),
            ) else {
                continue;
            };
            let msg = &tx["transaction"]["message"];
            let Some(keys) = msg["accountKeys"].as_array() else {
                continue;
            };
            if keys.first().and_then(|k| k.as_str()) == Some(self.me.as_str()) {
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else {
                continue;
            };
            for ix in ixs {
                let program = ix["programIdIndex"]
                    .as_u64()
                    .and_then(|i| keys.get(i as usize))
                    .and_then(|k| k.as_str());
                if program != Some(tape.program.as_str()) {
                    continue;
                }
                let Some(data) = ix["data"].as_str().and_then(|d| bs58::decode(d).into_vec().ok()) else {
                    continue;
                };
                if data.len() != tape.len || data[..8] != tape.disc[..] {
                    continue;
                }
                if data[tape.side_off] == tape.bid_byte {
                    buys += 1;
                } else if data[tape.side_off] == tape.ask_byte {
                    sells += 1;
                }
            }
        }
        (buys, sells)
    }

    fn cancel_ix(&self, t: &Trader) -> BotResult<Vec<Instruction>> {
        Ok(t.phoenix
            .tx()
            .build_cancel_all_orders(t.authority(), t.account, &self.symbol)?)
    }

    /// Cancel everything and trade toward `target` at market, then read the
    /// position back. Returns the position after.
    async fn trade_to(
        &mut self,
        t: &Trader,
        pos: f64,
        target: f64,
        oracle: f64,
        cap: f64,
    ) -> BotResult<f64> {
        let target = target.clamp(-cap, cap);
        let diff = target - pos;
        if diff.abs() < 0.05 {
            return Ok(pos);
        }
        let slip = (self.half_ticks + 10.0) * TICK;
        let (side, ticks) = if diff > 0.0 {
            (Side::Bid, ((oracle + slip) / TICK).floor() as i64)
        } else {
            (Side::Ask, ((oracle - slip) / TICK).ceil() as i64)
        };
        let mut ixs = self.cancel_ix(t)?;
        ixs.extend(self.market_ix(t, side, ticks, diff.abs()).await?);
        self.resting = Resting::default();
        let sent = mtx_kit::send(&t.rpc, &t.keypair, ixs, "lean");
        if let Err(e) = &sent {
            eprintln!("lean failed: {e}");
        }
        let after = t.position().await?.size;
        if sent.is_ok() {
            // Cancelled for sure.
            self.resting.known = true;
        }
        // What crossing cost: the spread and the taker fee.
        let cost_bps = self.half_ticks * TICK / oracle * 1e4 + 3.5;
        self.equity -= (after - pos).abs() * oracle * cost_bps / 1e4;
        Ok(after)
    }
}

impl Strategy for Maker {
    async fn start(&mut self, session: &Session) -> BotResult {
        self.symbol = session.env.symbol.clone();
        let bbo = session.trader.inner.bbo().await?;
        let half = match (bbo.best_bid_ticks(), bbo.best_ask_ticks()) {
            (Some(b), Some(a)) if a > b => (a - b) as f64 / 2.0,
            _ => 20.0,
        };
        // Sane bounds: 8 to 30 bp of a ~143 USD price is 11 to 43 ticks.
        self.half_ticks = half.clamp(8.0, 60.0);
        eprintln!("start half_ticks={}", self.half_ticks);
        let t = &session.trader;
        self.me = t.authority().to_string();
        self.next_id = 0;
        let mb = self.market_ix(t, Side::Bid, 14000, 1.0).await?;
        self.next_id = 0;
        let ma = self.market_ix(t, Side::Ask, 14000, 1.0).await?;
        self.next_id = 0;
        let lb = self.limit_ix(t, Side::Bid, 14000, 1.0).await?;
        self.next_id = 1000;
        if let (Some(mb), Some(ma), Some(lb)) = (mb.last(), ma.last(), lb.last()) {
            let side_off = mb.data.iter().zip(&ma.data).position(|(a, b)| a != b);
            if let Some(side_off) = side_off.filter(|_| mb.data.len() == ma.data.len()) {
                let tag = if lb.data.len() == mb.data.len() {
                    mb.data
                        .iter()
                        .zip(&lb.data)
                        .position(|(a, b)| a != b)
                        .map(|i| (i, mb.data[i]))
                } else {
                    None
                };
                eprintln!(
                    "tape program={} len={} limit_len={} side_off={} bid={} ask={} tag={:?} mb={:?} lb={:?}",
                    mb.program_id, mb.data.len(), lb.data.len(), side_off, mb.data[side_off], ma.data[side_off], tag, mb.data, lb.data
                );
self.tape = Some(Tape {
                    program: mb.program_id.to_string(),
                    len: mb.data.len(),
                    side_off,
                    bid_byte: mb.data[side_off],
                    ask_byte: ma.data[side_off],
                    disc: mb.data[..8].to_vec(),
                });
            }
        }
        Ok(())
    }

    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let Some(t) = orders.trader() else {
            return Ok(());
        };
        let oracle = m.oracle;
        let cap = (m.cap - CAP_MARGIN).max(0.0);
        let mut pos = m.position.size;
        if self.last_pos.is_none() {
            self.first_slot = m.slot;
            self.last_oracle = oracle;
        }

        // Equity at the oracle, and the drawdown guard built on it.
        let prev = self.last_pos.unwrap_or(pos);
        self.equity += prev * (oracle - self.last_oracle);
        self.peak = self.peak.max(self.equity);
        let dd = self.peak - self.equity;
        let (sig_cap, normal_cap) = if dd > 120.0 {
            (0.0, 2.0)
        } else if dd > 60.0 {
            (10.0, 4.0)
        } else {
            (SIGNAL_POS, NORMAL_CAP)
        };

        // Fills since the last turn are the takers' flow: they bought what
        // we sold.
        let dpos = pos - prev;
        if dpos.abs() > 1e-6 {
            if dpos > 0.0 {
                self.resting.bid_rem -= dpos;
            } else {
                self.resting.ask_rem += dpos;
            }
        }
        if dpos.abs() > 1e-6 {
            self.vol.push_back((m.slot, -dpos));
        }
        while self.vol.front().is_some_and(|&(s, _)| s + WINDOW_SLOTS <= m.slot) {
            self.vol.pop_front();
        }
        let netv: f64 = self.vol.iter().map(|&(_, v)| v).sum();
        let (buys, sells) = self.read_tape(t, m.slot);
        for _ in 0..buys {
            self.flow.push_back((m.slot, 1.0));
        }
        for _ in 0..sells {
            self.flow.push_back((m.slot, -1.0));
        }
        self.total_count += (buys + sells) as f64;
        while self
            .flow
            .front()
            .is_some_and(|&(s, _)| s + WINDOW_SLOTS <= m.slot)
        {
            self.flow.pop_front();
        }
        let net: f64 = self.flow.iter().map(|&(_, n)| n).sum();
        let elapsed = (m.slot - self.first_slot).max(20) as f64;
        let rate = self.total_count.max(3.0) / elapsed;
        let threshold = MIN_COUNT.max(Z * (rate * WINDOW_SLOTS as f64).sqrt());

        // A burst fires on the tape's order count; on fills alone if the
        // tape can't be read.
        let fire = if self.tape_ok {
            if net.abs() >= threshold { net.signum() } else { 0.0 }
        } else if netv.abs() >= VOL_T {
            netv.signum()
        } else {
            0.0
        };

        // Leave a lean when the jump lands or the burst times out.
        let mut reduce_to: Option<f64> = None;
        if let Some(sig) = self.signal {
            let moved = (oracle - sig.oracle) / sig.oracle;
            if moved.abs() > JUMP_FRAC || m.slot >= sig.slot + HOLD_SLOTS {
                eprintln!("k={} leave lean dir={} moved={:.4}", m.k, sig.dir, moved);
                self.signal = None;
                self.cool_until = m.slot + COOL_SLOTS;
                self.flow.clear();
                self.vol.clear();
                if dd > 60.0 {
                    reduce_to = Some(KEEP_AFTER.min(normal_cap));
                } else {
                    self.unwind_until = m.slot + UNWIND_SLOTS;
                }
            }
        } else if (oracle - self.last_oracle).abs() / oracle > JUMP_FRAC {
            // A jump we didn't lean into: its burst, if any, is spent.
            self.flow.clear();
            self.vol.clear();
        } else if fire != 0.0 && m.slot >= self.cool_until && sig_cap > 0.0 {
            eprintln!("k={} lean dir={} net={} thr={:.1} netv={:.2}", m.k, fire, net, threshold, netv);
            self.signal = Some(Signal {
                dir: fire,
                slot: m.slot,
                oracle,
            });
        }

        if let Some(sig) = self.signal {
            let target = sig.dir * sig_cap.min(cap);
            if (target - pos) * sig.dir > 0.5 {
                pos = self.trade_to(t, pos, target, oracle, cap).await?;
            }
        } else if let Some(keep) = reduce_to {
            if pos.abs() > keep + 0.5 {
                pos = self.trade_to(t, pos, keep * pos.signum(), oracle, cap).await?;
            }
        } else if pos.abs() > normal_cap + 3.0 && (m.slot >= self.unwind_until || dd > 60.0) {
            pos = self
                .trade_to(t, pos, normal_cap * pos.signum(), oracle, cap)
                .await?;
        }

        // Quotes.
        let q = (QUOTE_FRAC * self.half_ticks).max(3.0);
        let (mut bid_d, mut ask_d, mut bid_sz, mut ask_sz);
        if let Some(sig) = self.signal {
            // Only the side that adds to the lean, close in.
            let d = (0.4 * self.half_ticks).max(3.0);
            bid_d = d;
            ask_d = d;
            bid_sz = if sig.dir > 0.0 { QUOTE_SIZE } else { 0.0 };
            ask_sz = if sig.dir < 0.0 { QUOTE_SIZE } else { 0.0 };
        } else {
            let s = (pos / NORMAL_CAP).clamp(-1.25, 1.25) * SKEW;
            bid_d = q * (1.0 + s);
            ask_d = q * (1.0 - s);
            bid_sz = QUOTE_SIZE.min(normal_cap - pos);
            ask_sz = QUOTE_SIZE.min(normal_cap + pos);
            // One-sided flow may know something: stop feeding it.
            if net >= BACKOFF {
                ask_sz = 0.0;
            } else if net <= -BACKOFF {
                bid_sz = 0.0;
            }
        }
        bid_d = bid_d.max(2.0);
        ask_d = ask_d.max(2.0);
        bid_sz = bid_sz.min(cap - pos).max(0.0);
        ask_sz = ask_sz.min(cap + pos).max(0.0);
        if bid_sz < 0.05 {
            bid_sz = 0.0;
        }
        if ask_sz < 0.05 {
            ask_sz = 0.0;
        }
        let bid_ticks = (oracle / TICK - bid_d).floor() as i64;
        let ask_ticks = (oracle / TICK + ask_d).ceil() as i64;

        let r = self.resting;
        let side_ok = |rem: f64, want: f64, same_price: bool| {
            if want == 0.0 {
                rem <= 1e-6
            } else {
                same_price && rem >= 0.5 * want && rem <= want + 1e-6
            }
        };
        let keep = r.known
            && side_ok(r.bid_rem, bid_sz, r.bid_ticks == bid_ticks)
            && side_ok(r.ask_rem, ask_sz, r.ask_ticks == ask_ticks);
        if !keep {
            let mut ixs = self.cancel_ix(t)?;
            if bid_sz > 0.0 {
                ixs.extend(self.limit_ix(t, Side::Bid, bid_ticks, bid_sz).await?);
            }
            if ask_sz > 0.0 {
                ixs.extend(self.limit_ix(t, Side::Ask, ask_ticks, ask_sz).await?);
            }
            match mtx_kit::send(&t.rpc, &t.keypair, ixs, "quote") {
                Ok(_) => {
                    self.resting = Resting {
                        bid_ticks,
                        bid_rem: bid_sz,
                        ask_ticks,
                        ask_rem: ask_sz,
                        known: true,
                    };
                }
                Err(e) => {
                    eprintln!("k={} quote failed: {e}", m.k);
                    self.resting.known = false;
                }
            }
        }

        eprintln!(
            "k={} slot={} or={:.3} pos={:.3} dpos={:.3} net={} netv={:.2} eq={:.2} dd={:.2} tape={}/{} bid={} ask={} sz={:.2}/{:.2} sent={} budget={:?}",
            m.k, m.slot, oracle, pos, dpos, net, netv, self.equity, dd, buys, sells, bid_ticks, ask_ticks, bid_sz, ask_sz, !keep, m.budget_left
        );
        self.last_pos = Some(pos);
        self.last_oracle = oracle;
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}
