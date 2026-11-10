//! SPCX market maker that reads taker flow from the chain.
//!
//! Every step it scans the new blocks for takers' IOC orders. A one-sided
//! burst of them is the informed flow ahead of a jump: it takes a position
//! that way and exits after the jump (or a timeout). Otherwise it quotes
//! inside the competing makers, skewed against its inventory.

use std::collections::VecDeque;

use mtx_kit::prelude::*;
use mtx_kit::RpcClient;
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;
use tracing::{info, warn};

fn envf(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

struct Params {
    window: u64,        // slots of flow the burst detector looks at
    burst_net: f64,     // net one-sided orders to trigger
    burst_z: f64,       // net / sqrt(count) to trigger
    bet: f64,           // position taken on a burst
    hold: u64,          // slots to hold after the last confirming flow
    jump_pct: f64,      // oracle move that counts as the jump
    quote_frac: f64,    // our half-spread as a fraction of the competitor's
    quote_min_bps: f64, // never quote closer than this
    quote_size: f64,
    skew_bps: f64,      // fair shift per SPCX of inventory
    mm_limit: f64,      // inventory beyond which we stop adding on that side
    trend_z: f64,
    trend_lean: f64,
    cool: u64,
}

impl Params {
    fn load() -> Self {
        Params {
            window: envf("P_WINDOW", 12.0) as u64,
            burst_net: envf("P_BURST_NET", 8.0),
            burst_z: envf("P_BURST_Z", 2.0),
            bet: envf("P_BET", 18.0),
            hold: envf("P_HOLD", 40.0) as u64,
            jump_pct: envf("P_JUMP", 0.009),
            quote_frac: envf("P_QFRAC", 0.5),
            quote_min_bps: envf("P_QMIN", 2.0),
            quote_size: envf("P_QSIZE", 1.5),
            skew_bps: envf("P_SKEW", 1.0),
            mm_limit: envf("P_MMLIM", 8.0),
            trend_z: envf("P_TREND_Z", 2.5),
            trend_lean: envf("P_TREND_LEAN", 6.0),
            cool: envf("P_COOL", 30.0) as u64,
        }
    }
}

#[derive(Clone, Copy)]
struct Bet {
    dir: f64,
    entry_oracle: f64,
    last_confirm: u64,
}

struct Bot {
    p: Params,
    me: String,
    watch: String,
    last_sig: Option<String>,
    mine: std::collections::HashSet<String>,
    flow: VecDeque<(u64, f64)>, // (slot, +1 buy / -1 sell)
    all_net: f64,
    all_n: f64,
    comp_half: f64, // competitor half-spread, fraction
    bet: Option<Bet>,
    cooldown_until: u64,
    flatten: bool,
    hist: VecDeque<(u64, f64)>,
}

impl Bot {
    fn new() -> Self {
        Bot {
            p: Params::load(),
            me: String::new(),
            watch: String::new(),
            last_sig: None,
            mine: Default::default(),
            flow: VecDeque::new(),
            all_net: 0.0,
            all_n: 0.0,
            comp_half: 0.0015,
            bet: None,
            cooldown_until: 0,
            flatten: false,
            hist: VecDeque::new(),
        }
    }

    fn scan(&mut self, rpc: &RpcClient, slot: u64) {
        let mut params = json!({"limit": 200, "commitment": "confirmed"});
        if let Some(u) = &self.last_sig {
            params["until"] = json!(u);
        }
        let sigs: Value = match rpc.send(
            RpcRequest::GetSignaturesForAddress,
            json!([self.watch, params]),
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(%e, "getSignaturesForAddress failed");
                return;
            }
        };
        let Some(list) = sigs.as_array() else { return };
        if let Some(first) = list.first().and_then(|x| x["signature"].as_str()) {
            let first_time = self.last_sig.is_none();
            self.last_sig = Some(first.to_string());
            if first_time {
                return;
            }
        }
        for item in list.iter().rev() {
            if !item["err"].is_null() {
                continue;
            }
            let Some(sig) = item["signature"].as_str() else { continue };
            if self.mine.contains(sig) {
                continue;
            }
            let tx: Value = match rpc.send(
                RpcRequest::GetTransaction,
                json!([sig, {"encoding": "json", "maxSupportedTransactionVersion": 0,
                    "commitment": "confirmed"}]),
            ) {
                Ok(t) => t,
                Err(e) => {
                    warn!(%e, "getTransaction failed");
                    continue;
                }
            };
            let msg = &tx["transaction"]["message"];
            let Some(keys) = msg["accountKeys"].as_array() else { continue };
            if keys.first().and_then(|k| k.as_str()) == Some(self.me.as_str()) {
                self.mine.insert(sig.to_string());
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else { continue };
            for ix in ixs {
                let Some(data) = ix["data"].as_str() else { continue };
                let Ok(bytes) = bs58::decode(data).into_vec() else { continue };
                if bytes.len() >= 20 && bytes[8] == 2 && bytes[9] <= 1 {
                    let side = if bytes[9] == 0 { 1.0 } else { -1.0 };
                    self.flow.push_back((slot, side));
                    self.all_net += side;
                    self.all_n += 1.0;
                }
            }
        }
    }
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        self.me = session.trader.authority().to_string();
        self.watch = session.trader.phoenix.perp_asset_map()?.to_string();
        if let Ok(bbo) = session.trader.inner.bbo().await {
            if let (Some(b), Some(a)) = (bbo.best_bid_ticks(), bbo.best_ask_ticks()) {
                if a > b && b > 0 {
                    let half = (a - b) as f64 / (a + b) as f64;
                    if half > 0.0003 && half < 0.01 {
                        self.comp_half = half;
                    }
                }
            }
        }
        info!(comp_half = self.comp_half, "started");
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let trader = orders.trader();
        if let Some(t) = trader {
            self.scan(&t.rpc, market.slot);
        }
        let p = &self.p;
        let slot = market.slot;
        while let Some(&(s, _)) = self.flow.front() {
            if s + p.window < slot { self.flow.pop_front(); } else { break; }
        }
        let n = self.flow.len() as f64;
        let net: f64 = self.flow.iter().map(|x| x.1).sum();
        let z = if n > 0.0 { net / n.sqrt() } else { 0.0 };
        let oracle = market.oracle;
        let pos = market.position.size;

        // A big move just happened: the flow before it is spent.
        self.hist.push_back((slot, oracle));
        while self.hist.len() > 1 && self.hist[0].0 + 24 < slot { self.hist.pop_front(); }
        let old = self.hist[0].1;
        if (oracle / old - 1.0).abs() > p.jump_pct && self.bet.is_none() && slot >= self.cooldown_until {
            info!(slot, "move seen; cooling down");
            self.cooldown_until = slot + p.cool;
            self.flow.clear();
            self.hist.clear();
            self.hist.push_back((slot, oracle));
        }

        // Burst detection.
        if net.abs() >= p.burst_net && z.abs() >= p.burst_z && slot >= self.cooldown_until {
            let dir = net.signum();
            match &mut self.bet {
                Some(b) if b.dir == dir => b.last_confirm = slot,
                _ => {
                    info!(slot, net, n, dir, oracle, "burst: betting");
                    self.bet = Some(Bet { dir, entry_oracle: oracle, last_confirm: slot });
                }
            }
        }
        if let Some(b) = self.bet {
            let moved = (oracle / b.entry_oracle - 1.0) * b.dir;
            let done = moved > p.jump_pct || moved < -p.jump_pct || slot > b.last_confirm + p.hold;
            if done {
                info!(slot, moved, pnl = market.pnl, "bet over");
                self.bet = None;
                self.cooldown_until = slot + p.cool;
                self.flatten = true;
                self.flow.clear();
            }
        }

        let slip = (self.comp_half * 1.6).max(0.0015);
        if let Some(b) = self.bet {
            orders.cancel_all();
            let target = b.dir * p.bet;
            let diff = target - pos;
            if diff > 0.05 {
                orders.buy(diff, oracle * (1.0 + slip));
            } else if diff < -0.05 {
                orders.sell(-diff, oracle * (1.0 - slip));
            }
            return Ok(());
        }

        if self.flatten {
            if pos.abs() < 1.0 { self.flatten = false; } else {
                orders.cancel_all();
                if pos > 0.0 { orders.sell(pos, oracle * (1.0 - slip)); } else { orders.buy(-pos, oracle * (1.0 + slip)); }
                return Ok(());
            }
        }
        // Flatten leftovers from a bet beyond the market-making range.
        let tz = if self.all_n > 20.0 { self.all_net / self.all_n.sqrt() } else { 0.0 };
        let lean = if tz.abs() > p.trend_z { tz.signum() * p.trend_lean } else { 0.0 };
        let excess = pos - lean;
        if excess.abs() > p.mm_limit + 2.0 {
            orders.cancel_all();
            let d = excess - excess.signum() * p.mm_limit;
            if d > 0.0 { orders.sell(d, oracle * (1.0 - slip)); } else { orders.buy(-d, oracle * (1.0 + slip)); }
            return Ok(());
        }

        let half = (self.comp_half * p.quote_frac).max(p.quote_min_bps * 1e-4);
        let fair = oracle * (1.0 - excess * p.skew_bps * 1e-4);
        let bid = fair * (1.0 - half);
        let ask = fair * (1.0 + half);
        let bid = bid.min(oracle * (1.0 - p.quote_min_bps * 1e-4));
        let ask = ask.max(oracle * (1.0 + p.quote_min_bps * 1e-4));
        orders.cancel_all();
        if excess < p.mm_limit {
            orders.bid(bid, p.quote_size);
        }
        if excess > -p.mm_limit {
            orders.ask(ask, p.quote_size);
        }
        if market.k % 20 == 0 {
            info!(slot, oracle, pos, pnl = market.pnl, net, n, tz, "step");
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::new())
}
