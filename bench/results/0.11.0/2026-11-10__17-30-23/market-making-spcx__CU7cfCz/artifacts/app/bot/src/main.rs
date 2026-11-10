//! SPCX market maker: quotes inside the competing makers around the oracle,
//! watches the takers' order flow in the blocks, and leans into one-sided
//! bursts (the informed flow ahead of a jump).

use std::collections::VecDeque;

use mtx_kit::prelude::*;
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;

/// Borsh tag of an immediate-or-cancel order packet: a taker's market order.
const IOC_TAG: u8 = 2;

struct Bot {
    init: bool,
    me: String,
    tick: f64,
    taker_fee: f64,
    /// The competitor half-spread, as a fraction of the price.
    c: f64,
    last_block: u64,
    /// Taker orders seen in the blocks: (slot, +1 buy / -1 sell, size).
    flow: VecDeque<(u64, i32, f64)>,
    lots_per_unit: f64,
    my_bid: Option<f64>,
    my_ask: Option<f64>,
    hb: f64,
    ha: f64,
    sig_dir: i32,
    sig_slot: u64,
    last_oracle: f64,
    last_pos: f64,
    pnl: f64,
    peak: f64,
    cost: f64,
    dbg: u32,
    expect_pos: f64,
    sent_market: bool,
    sym: bool,
    tox_dir: i32,
    tox_slot: u64,
}

impl Default for Bot {
    fn default() -> Self {
        Bot {
            init: false,
            me: String::new(),
            tick: 0.01,
            taker_fee: 0.0,
            c: 0.0015,
            last_block: 0,
            flow: VecDeque::new(),
            lots_per_unit: 1000.0,
            my_bid: None,
            my_ask: None,
            hb: 0.0,
            ha: 0.0,
            sig_dir: 0,
            sig_slot: 0,
            last_oracle: 0.0,
            last_pos: 0.0,
            pnl: 0.0,
            peak: 0.0,
            cost: 0.0,
            dbg: 0,
            expect_pos: 0.0,
            sent_market: false,
            sym: false,
            tox_dir: 0,
            tox_slot: 0,
        }
    }
}

impl Bot {
    /// Read the takers' market orders out of a block.
    fn read_block(&mut self, trader: &mtx_kit::Trader, slot: u64) {
        let params = json!([slot, {"encoding": "json", "transactionDetails": "full",
            "maxSupportedTransactionVersion": 0, "rewards": false}]);
        let block: Value = match trader.rpc.send(RpcRequest::GetBlock, params) {
            Ok(b) => b,
            Err(e) => {
                if self.dbg < 6 { self.dbg += 1; eprintln!("BLOCKERR {slot} {e}"); }
                return;
            }
        };
        if self.dbg < 6 {
            self.dbg += 1;
            let t = block.to_string();
            eprintln!("BLOCK {slot} {}", &t[..t.len().min(6000)]);
        }
        let Some(txs) = block.get("transactions").and_then(|t| t.as_array()) else {
            return;
        };
        for tx in txs {
            let msg = &tx["transaction"]["message"];
            if msg["accountKeys"][0].as_str() == Some(self.me.as_str()) {
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else {
                continue;
            };
            for ix in ixs {
                let Some(data) = ix["data"].as_str() else {
                    continue;
                };
                let Ok(d) = bs58::decode(data).into_vec() else {
                    continue;
                };
                if d.len() < 27 || d[8] != IOC_TAG || d[9] > 1 || d[10] > 1 {
                    continue;
                }
                let off = if d[10] == 1 { 19 } else { 11 };
                if d.len() < off + 8 {
                    continue;
                }
                let lots = u64::from_le_bytes(d[off..off + 8].try_into().unwrap());
                let size = lots as f64 / self.lots_per_unit;
                if !(0.05..=10.0).contains(&size) {
                    continue;
                }
                let dir = if d[9] == 0 { 1 } else { -1 };
                self.flow.push_back((slot, dir, size));
            }
        }
    }

    fn net(&self, slot: u64, window: u64) -> i32 {
        self.flow
            .iter()
            .filter(|(s, _, _)| s + window > slot)
            .map(|(_, d, _)| d)
            .sum()
    }
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        let t = &session.trader;
        self.me = t.authority().to_string();
        let m = t.phoenix.market(&t.env.symbol)?;
        self.tick = t.phoenix.ticks_to_usd(m, 1);
        self.lots_per_unit = t.phoenix.base_units_to_lots(m, 1.0) as f64;
        self.taker_fee = m.taker_fee;
        eprintln!(
            "START tick={} lots={} taker_fee={} maker_fee={}",
            self.tick, self.lots_per_unit, m.taker_fee, m.maker_fee
        );
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let trader = orders.trader().ok_or("no trader")?;
        let slot = market.slot;
        let oracle = market.oracle;
        let pos = market.position.size;
        let cap = market.cap - 0.05;
        let m = trader.phoenix.market(&trader.env.symbol)?;
        let bbo = trader.inner.bbo().await?;
        let best_bid = bbo.best_bid_ticks().map(|t| trader.phoenix.ticks_to_usd(m, t));
        let best_ask = bbo.best_ask_ticks().map(|t| trader.phoenix.ticks_to_usd(m, t));

        if !self.init {
            self.init = true;
            if let (Some(b), Some(a)) = (best_bid, best_ask) {
                let c = (a - b) / 2.0 / oracle;
                if c > 0.0003 && c < 0.005 {
                    self.c = c;
                }
            }
            self.hb = 0.75 * self.c * oracle;
            self.ha = self.hb;
            self.last_block = slot.saturating_sub(2);
            self.last_oracle = oracle;
            self.last_pos = pos;
            eprintln!("INIT slot={slot} oracle={oracle} bb={best_bid:?} ba={best_ask:?} c={}", self.c);
        }

        // Our PnL at the oracle, less what we paid to cross.
        self.pnl += self.last_pos * (oracle - self.last_oracle) - self.cost;
        self.cost = 0.0;
        self.peak = self.peak.max(self.pnl);
        let dd = self.peak - self.pnl;
        let ret = oracle / self.last_oracle - 1.0;
        self.last_oracle = oracle;
        self.last_pos = pos;

        // The takers' flow, as our own fills show it: a taker who buys
        // lifts our ask, and our position falls.
        let before = self.flow.len();
        let delta = pos - self.expect_pos;
        if !self.sent_market && self.sym && delta.abs() >= 0.08 {
            self.flow.push_back((slot, if delta < 0.0 { 1 } else { -1 }, delta.abs()));
        }
        self.sent_market = false;
        while self.flow.front().is_some_and(|(s, _, _)| s + 64 < slot) {
            self.flow.pop_front();
        }
        let seen = self.flow.len() - before.min(self.flow.len());

        // A jump: whatever the burst said has happened.
        if ret.abs() > 0.01 {
            eprintln!("JUMP slot={slot} ret={ret:.4} pos={pos} sig={}", self.sig_dir);
            self.sig_dir = 0;
            self.tox_dir = 0;
            self.flow.clear();
        }
        let (n24, n12) = (self.net(slot, 24), self.net(slot, 12));
        let dir = if n24.abs() >= 5 {
            n24.signum()
        } else if n12.abs() >= 4 {
            n12.signum()
        } else {
            0
        };
        if dir != 0 {
            if self.sig_dir != dir {
                eprintln!("SIGNAL slot={slot} dir={dir} n24={n24} n12={n12} pos={pos}");
            }
            self.sig_dir = dir;
            self.sig_slot = slot;
        } else if self.sig_dir != 0 && slot > self.sig_slot + 40 {
            eprintln!("SIGNAL expired slot={slot} pos={pos}");
            self.sig_dir = 0;
        }

        // A few one-sided fills: stop quoting the side they hit.
        if n24.abs() >= 3 {
            self.tox_dir = n24.signum();
            self.tox_slot = slot;
        } else if self.tox_dir != 0 && slot > self.tox_slot + 30 {
            self.tox_dir = 0;
        }
        let inv_cap: f64 = 6.0;
        let sig_pos: f64 = 8.0;

        // How much we may hold, so one more adverse jump can't breach the drawdown limit.
        let max_pos = ((market.max_drawdown * 0.8 - dd) / (oracle * 0.045)).clamp(0.0, cap);

        orders.cancel_all();
        let mut p = pos;
        let fee = self.taker_fee.max(0.0);

        // Stale quotes the price has moved through: take them.
        let edge = oracle * (fee + 0.0001);
        let mine_a = self.my_ask.is_some_and(|a| best_ask.is_some_and(|b| b >= a - 1e-9));
        let mine_b = self.my_bid.is_some_and(|a| best_bid.is_some_and(|b| b <= a + 1e-9));
        if let Some(a) = best_ask
            && !mine_a
            && a < oracle - edge
            && self.sig_dir >= 0
        {
            let size = (max_pos - p).min(5.0);
            if size >= 0.1 {
                orders.buy(size, oracle - edge);
                eprintln!("SNIPE buy slot={slot} ask={a} oracle={oracle} size={size}");
                p += size;
            }
        }
        if let Some(b) = best_bid
            && !mine_b
            && b > oracle + edge
            && self.sig_dir <= 0
        {
            let size = (max_pos + p).min(5.0);
            if size >= 0.1 {
                orders.sell(size, oracle + edge);
                eprintln!("SNIPE sell slot={slot} bid={b} oracle={oracle} size={size}");
                p -= size;
            }
        }

        // A burst: be on its side of the jump.
        if self.sig_dir != 0 {
            let target = self.sig_dir as f64 * max_pos.min(sig_pos);
            let diff = target - p;
            let slip = self.c + 0.004;
            if diff > 0.2 && self.sig_dir > 0 {
                orders.buy(diff, oracle * (1.0 + slip));
                self.cost += diff * oracle * (self.c + fee);
                p += diff;
            } else if diff < -0.2 && self.sig_dir < 0 {
                orders.sell(-diff, oracle * (1.0 - slip));
                self.cost += -diff * oracle * (self.c + fee);
                p += diff;
            }
        } else if p.abs() > max_pos.min(sig_pos) + 2.5 {
            // Over what the drawdown allows: cut.
            let diff = p.abs() - max_pos.min(sig_pos);
            if p > 0.0 {
                orders.sell(diff, oracle * (1.0 - self.c - 0.004));
                p -= diff;
            } else {
                orders.buy(diff, oracle * (1.0 + self.c + 0.004));
                p += diff;
            }
            self.cost += diff * oracle * (self.c + fee);
        }

        self.sent_market = (p - pos).abs() > 1e-9;
        self.expect_pos = pos;
        // Quotes: one tick inside whoever is better than us, no wider than
        // three quarters of the competitor, no tighter than the floor.
        let hmin = (oracle * 0.00015).max(2.0 * self.tick);
        let hmax = 0.75 * self.c * oracle;
        if !mine_b && let Some(b) = best_bid {
            self.hb = oracle - b - self.tick;
        }
        if !mine_a && let Some(a) = best_ask {
            self.ha = a - oracle - self.tick;
        }
        self.hb = self.hb.clamp(hmin, hmax);
        self.ha = self.ha.clamp(hmin, hmax);
        let inv = (p / inv_cap).clamp(-1.5, 1.5);
        let mut hb = self.hb;
        let mut ha = self.ha;
        // Lean against inventory: the side that reduces it stays tight, the
        // other one backs off.
        if inv > 0.5 {
            hb += inv * self.c * oracle;
        } else if inv < -0.5 {
            ha += -inv * self.c * oracle;
        }
        let q: f64 = 4.0;
        let lim = max_pos.min(inv_cap);
        let bid_size = if self.sig_dir < 0 || self.tox_dir < 0 { 0.0 } else { q.min(lim - p) };
        let ask_size = if self.sig_dir > 0 || self.tox_dir > 0 { 0.0 } else { q.min(lim + p) };
        self.sym = self.sig_dir == 0 && self.tox_dir == 0 && inv.abs() <= 0.5 && bid_size >= 1.0 && ask_size >= 1.0;
        let bid = ((oracle - hb) / self.tick).floor() * self.tick;
        let ask = ((oracle + ha) / self.tick).ceil() * self.tick;
        self.my_bid = None;
        self.my_ask = None;
        if bid_size >= 0.1 {
            orders.bid(bid, (bid_size * 100.0).floor() / 100.0);
            self.my_bid = Some(bid);
        }
        if ask_size >= 0.1 {
            orders.ask(ask, (ask_size * 100.0).floor() / 100.0);
            self.my_ask = Some(ask);
        }
        eprintln!(
            "STEP k={} slot={slot} o={oracle:.2} pos={pos:.2} bb={:.2} ba={:.2} q={bid:.2}/{ask:.2} seen={seen} n24={n24} sig={} pnl={:.1} dd={dd:.1} left={:?}",
            market.k,
            best_bid.unwrap_or(0.0),
            best_ask.unwrap_or(0.0),
            self.sig_dir,
            self.pnl,
            market.budget_left
        );
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::default())
}
