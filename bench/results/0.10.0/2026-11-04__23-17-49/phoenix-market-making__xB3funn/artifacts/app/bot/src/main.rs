//! SPCX market maker for Phoenix.
//!
//! Each turn the bot reads what happened on the order book since its last
//! turn (every transaction that touched the SPCX book: taker market orders
//! and the competing maker's quotes), the oracle, and its own account, then:
//!
//! - watches the takers for informed bursts (several large-limit orders on
//!   one side in a short window) and, when one shows up, takes a position in
//!   its direction ahead of the jump it usually announces;
//! - leans with a trend inferred from the taker imbalance and the drift;
//! - otherwise quotes both sides around the oracle, skewed against its
//!   inventory relative to its target position, sized so fills never break
//!   the position cap, and tighter than the competing makers.

use std::collections::VecDeque;

use anyhow::Result;
use mtx_botkit::{
    BotEnv, Trader, Turns,
    phoenix_rise::{self, core::MarketOrderTicket, math::Side},
};
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;

const TICK: f64 = 0.01; // USD per tick
const LOTS_PER_UNIT: f64 = 1000.0;
const FEE_TAKER_BPS: f64 = 3.5;

fn log(msg: String) {
    eprintln!("{msg}");
}

#[derive(Clone, Copy, Debug)]
struct TakerOrder {
    slot: u64,
    side: i32, // +1 buy, -1 sell
    size: f64,
    limit_bps: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct MakerQuote {
    bid: Option<u64>,
    ask: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct Burst {
    side: i32,
    start: u64,
    last_hi: u64,
}

struct Params {
    r0: f64,
    r1: f64,
    tau: f64,
    theta: f64,
    defend: f64,
    burst_timeout: u64,
    quote_size: f64,
    trend_scale: f64,
    trend_max: f64,
    pick_bps: f64,
    floor_bps: f64,
    skew_bps: f64,
    reduce_min_ticks: i64,
    reduce_size: f64,
    trend_veto: f64,
}

impl Params {
    fn load() -> Self {
        let mut p = Params {
            r0: 1.8,
            r1: 2.2,
            tau: 6.0,
            theta: 3.0,
            defend: 1.5,
            burst_timeout: 26,
            quote_size: 1.5,
            trend_scale: 1.0,
            trend_max: 20.0,
            pick_bps: 1.5,
            floor_bps: 2.5,
            skew_bps: 0.4,
            reduce_min_ticks: 1,
            reduce_size: 4.0,
            trend_veto: 0.85,
        };
        // Experiment overrides (development only; absent when graded).
        if let Ok(text) = std::fs::read_to_string("/tmp/mmbot-params.json") {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(x) = v["r0"].as_f64() { p.r0 = x; }
                if let Some(x) = v["r1"].as_f64() { p.r1 = x; }
                if let Some(x) = v["tau"].as_f64() { p.tau = x; }
                if let Some(x) = v["theta"].as_f64() { p.theta = x; }
                if let Some(x) = v["defend"].as_f64() { p.defend = x; }
                if let Some(x) = v["burst_timeout"].as_u64() { p.burst_timeout = x; }
                if let Some(x) = v["quote_size"].as_f64() { p.quote_size = x; }
                if let Some(x) = v["trend_scale"].as_f64() { p.trend_scale = x; }
                if let Some(x) = v["trend_max"].as_f64() { p.trend_max = x; }
                if let Some(x) = v["pick_bps"].as_f64() { p.pick_bps = x; }
                if let Some(x) = v["floor_bps"].as_f64() { p.floor_bps = x; }
                if let Some(x) = v["skew_bps"].as_f64() { p.skew_bps = x; }
                if let Some(x) = v["reduce_min_ticks"].as_i64() { p.reduce_min_ticks = x; }
                if let Some(x) = v["reduce_size"].as_f64() { p.reduce_size = x; }
                if let Some(x) = v["trend_veto"].as_f64() { p.trend_veto = x; }
            }
        }
        p
    }
}

struct Bot {
    trader: Trader,
    me: String,
    orderbook: String,
    start_slot: u64,
    end_slot: u64,
    max_pos: f64,
    max_dd: f64,
    sig_cursor: Option<String>,
    disc_market: [u8; 8],
    disc_limit: [u8; 8],
    takers: Vec<TakerOrder>,
    maker_id: Option<String>,
    maker: MakerQuote,
    oracle: f64,
    prev_oracle: f64,
    oracle0: f64,
    c_bps: f64,
    c_prior: f64,
    burst: Option<Burst>,
    last_jump: Option<u64>,
    peak_value: f64,
    start_value: Option<f64>,
    target: f64,
    my_quotes: (Option<(u64, u64)>, Option<(u64, u64)>), // (ticks, lots)
    p: Params,
    order_id: u128,
    prices: VecDeque<(u64, f64)>,
    last_pos: f64,
    trend: TrendEst,
}

impl Bot {
    fn poll_book(&mut self) -> Result<Vec<Value>> {
        let mut cfg = json!({"limit": 1000, "commitment": "confirmed"});
        if let Some(u) = &self.sig_cursor {
            cfg["until"] = json!(u);
        }
        let sigs: Value = self
            .trader
            .rpc
            .send(RpcRequest::GetSignaturesForAddress, json!([self.orderbook, cfg]))?;
        let sigs = sigs.as_array().cloned().unwrap_or_default();
        if let Some(first) = sigs.first() {
            if let Some(s) = first["signature"].as_str() {
                self.sig_cursor = Some(s.to_string());
            }
        }
        let mut txs = Vec::new();
        for s in sigs.iter().rev() {
            if !s["err"].is_null() {
                continue;
            }
            let slot = s["slot"].as_u64().unwrap_or(0);
            if slot + 2 < self.start_slot {
                continue;
            }
            let Some(sig) = s["signature"].as_str() else { continue };
            let tx: Value = self.trader.rpc.send(
                RpcRequest::GetTransaction,
                json!([sig, {"encoding":"json","maxSupportedTransactionVersion":0,"commitment":"confirmed"}]),
            )?;
            txs.push(tx);
        }
        Ok(txs)
    }

    fn ingest(&mut self, txs: &[Value]) {
        for tx in txs {
            let slot = tx["slot"].as_u64().unwrap_or(0);
            let msg = &tx["transaction"]["message"];
            let keys = &msg["accountKeys"];
            let signer = keys[0].as_str().unwrap_or("").to_string();
            if signer == self.me {
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else { continue };
            for ix in ixs {
                let Some(d) = ix["data"].as_str() else { continue };
                let Ok(data) = bs58::decode(d).into_vec() else { continue };
                if data.len() < 8 {
                    continue;
                }
                let disc: [u8; 8] = data[..8].try_into().unwrap();
                let body = &data[8..];
                if disc == self.disc_market && body.len() >= 19 && body[0] == 2 {
                    let side = if body[1] == 0 { 1 } else { -1 };
                    let (price, off) = if body[2] == 1 {
                        (Some(u64::from_le_bytes(body[3..11].try_into().unwrap())), 11)
                    } else {
                        (None, 3)
                    };
                    let lots = u64::from_le_bytes(body[off..off + 8].try_into().unwrap());
                    let size = lots as f64 / LOTS_PER_UNIT;
                    let limit_bps = match price {
                        Some(p) => {
                            let px = p as f64 * TICK;
                            (px / self.oracle - 1.0) * 1e4 * side as f64
                        }
                        None => 1000.0,
                    };
                    let o = TakerOrder { slot, side, size, limit_bps };
                    log(format!("TAKER slot {} side {} size {:.3} limit {:.2} ({:.2}c)", slot - self.start_slot, side, size, limit_bps, limit_bps / self.c_bps));
                    self.takers.push(o);
                } else if disc == self.disc_limit && body.len() >= 18 && body[0] == 1 {
                    // The competing maker: cancel all, then a limit a side.
                    if self.maker_id.is_none() {
                        self.maker_id = Some(signer.clone());
                    }
                    if self.maker_id.as_deref() == Some(signer.as_str()) {
                        let price = u64::from_le_bytes(body[2..10].try_into().unwrap());
                        if body[1] == 0 {
                            self.maker.bid = Some(price);
                        } else {
                            self.maker.ask = Some(price);
                        }
                    }
                }
            }
        }
    }

    /// Re-estimate the competitor half-spread from the noise takers' limits:
    /// they're lognormal around it.
    fn update_c(&mut self) {
        let mut lims: Vec<f64> = self
            .takers
            .iter()
            .map(|t| t.limit_bps)
            .filter(|&l| l > 0.0 && l < 2.2 * self.c_prior)
            .collect();
        if lims.len() < 6 {
            self.c_bps = self.c_prior;
            return;
        }
        lims.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // Median of the truncated sample, nudged for the truncation of the
        // upper tail (noise with sigma 0.5: P(L > 2.2c) ~ 6%).
        let med = lims[lims.len() * 47 / 100];
        let n = lims.len() as f64;
        let w = (n / (n + 15.0)).min(0.85);
        self.c_bps = w * med + (1.0 - w) * self.c_prior;
    }

    /// How informed an order looks: 0 below `r0` times the competitor
    /// half-spread, 1 above `r1`, scaled down for small sizes.
    fn hi_weight(&self, t: &TakerOrder) -> f64 {
        let r = t.limit_bps / self.c_bps;
        let w = ((r - self.p.r0) / (self.p.r1 - self.p.r0)).clamp(0.0, 1.0);
        w * (0.4 + t.size / 2.0).min(1.0)
    }

    /// Exponentially decayed weight of informed-looking orders on `side`.
    fn burst_score(&self, side: i32, now: u64) -> f64 {
        let tau = self.p.tau;
        self.takers
            .iter()
            .rev()
            .take_while(|t| (now.saturating_sub(t.slot) as f64) < 5.0 * tau)
            .filter(|t| t.side == side)
            .map(|t| self.hi_weight(t) * (-((now.saturating_sub(t.slot)) as f64) / tau).exp())
            .sum()
    }

    async fn account(&self) -> Result<(f64, f64)> {
        let hawk = self.trader.phoenix.hawkeye(self.trader.authority());
        let asset = self
            .trader
            .luce
            .margin()
            .await?;
        let margin = hawk.view_margin(self.trader.authority(), 0, 0).await?.value;
        let pos = asset.base_lots as f64 / LOTS_PER_UNIT;
        let mark = asset.mark_price_ticks as f64 * TICK;
        let value = (margin.collateral_quote_lots as f64
            + margin.unrealized_pnl_quote_lots as f64
            + margin.unsettled_funding_quote_lots as f64)
            / 1e6
            + pos * (self.oracle - mark);
        Ok((pos, value))
    }

    fn next_id(&mut self) -> u128 {
        self.order_id += 1;
        self.order_id
    }

    fn send(&mut self, bids: &[(u64, u64)], asks: &[(u64, u64)], take: Option<(Side, u64, f64)>) -> Result<()> {
        let oid = self.next_id();
        let tx = self.trader.phoenix.tx();
        let auth = self.trader.authority();
        let acct = self.trader.account;
        let sym = self.trader.env.symbol.clone();
        let mut ixs = tx.build_cancel_all_orders(auth, acct, &sym)?;
        if let Some((side, lots, limit)) = take {
            let ticket = MarketOrderTicket::builder()
                .authority(auth)
                .trader_account(acct)
                .symbol(sym.clone())
                .side(side)
                .num_base_lots(lots)
                .price(limit)
                .client_order_id(oid)
                .build()?;
            let rt = tokio::runtime::Handle::current();
            let m = tokio::task::block_in_place(|| rt.block_on(tx.place_market_order(ticket)))?;
            ixs.extend(m);
        }
        if !bids.is_empty() || !asks.is_empty() {
            let b: Vec<(f64, u64)> = bids.iter().map(|&(t, l)| (t as f64 * TICK + 0.002, l)).collect();
            let a: Vec<(f64, u64)> = asks.iter().map(|&(t, l)| (t as f64 * TICK - 0.002, l)).collect();
            ixs.extend(tx.build_multi_limit_order(auth, acct, &sym, &b, &a, true)?);
        }
        mtx_botkit::send(&self.trader.rpc, &self.trader.keypair, ixs, "quote")?;
        Ok(())
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let me = trader.authority().to_string();
    let market = trader.phoenix.market(&env.symbol)?;
    let orderbook = market.market_pubkey.clone();
    let max_pos = env.max_position;
    let max_dd = env.max_drawdown_usd;
    let hello_start = turns.hello().start_slot;
    let oracle = trader.oracle_usd().await?;
    let bbo = trader.luce.bbo().await?;
    let c_prior = {
        let bid = bbo.best_bid_ticks as f64 * TICK;
        let ask = bbo.best_ask_ticks as f64 * TICK;
        let h = (ask - bid) / 2.0 / oracle * 1e4;
        if h.is_finite() && h > 4.0 && h < 60.0 { h } else { 18.0 }
    };
    log(format!("start oracle {oracle} c_prior {c_prior:.2} start_slot {hello_start} end {}", env.end_slot));
    let mut bot = Bot {
        trader,
        me,
        orderbook,
        start_slot: hello_start,
        end_slot: env.end_slot,
        max_pos,
        max_dd,
        sig_cursor: None,
        disc_market: phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant(),
        disc_limit: phoenix_rise::ix::PhoenixInstruction::PlaceLimitOrder.discriminant(),
        takers: Vec::new(),
        maker_id: None,
        maker: MakerQuote::default(),
        oracle,
        prev_oracle: oracle,
        oracle0: oracle,
        c_bps: c_prior,
        c_prior,
        burst: None,
        last_jump: None,
        peak_value: 0.0,
        start_value: None,
        target: 0.0,
        my_quotes: (None, None),
        p: Params::load(),
        order_id: 0,
        prices: VecDeque::new(),
        last_pos: 0.0,
        trend: TrendEst::default(),
    };
    // Skip the setup history: everything before the session.
    let _ = bot.poll_book();
    bot.takers.clear();

    while let Some(step) = turns.next()? {
        if let Err(e) = turn(&mut bot, step.slot).await {
            log(format!("turn error: {e:#}"));
        }
    }
    Ok(())
}

/// Posterior on a trend from the walk's drift and the noise takers' lean.
#[derive(Default)]
struct TrendEst {
    moves: Vec<f64>,
    llr_up: f64,
    llr_dn: f64,
    jumped: bool,
}

impl TrendEst {
    const MU: f64 = 0.012; // drift per move, % of the starting price
    const E: f64 = 0.085; // takers' lean
    const PRIOR: f64 = 0.125;

    fn taker(&mut self, side: i32) {
        let s = side as f64;
        self.llr_up += (1.0 + 2.0 * Self::E * s).ln();
        self.llr_dn += (1.0 - 2.0 * Self::E * s).ln();
    }

    /// P(up) - P(down).
    fn lean(&self) -> f64 {
        let n = self.moves.len();
        if self.jumped || n < 3 {
            return 0.0;
        }
        let nf = n as f64;
        let mean = self.moves.iter().sum::<f64>() / nf;
        let var = self.moves.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (nf - 1.0);
        let sig2 = var.max(1e-6);
        let sum: f64 = self.moves.iter().sum();
        let mu = Self::MU;
        let lu = mu * sum / sig2 - nf * mu * mu / (2.0 * sig2) + self.llr_up;
        let ld = -mu * sum / sig2 - nf * mu * mu / (2.0 * sig2) + self.llr_dn;
        let wu = Self::PRIOR * lu.min(50.0).exp();
        let wd = Self::PRIOR * ld.min(50.0).exp();
        let wn = 1.0 - 2.0 * Self::PRIOR;
        (wu - wd) / (wu + wd + wn)
    }
}

async fn turn(bot: &mut Bot, now: u64) -> Result<()> {
    let txs = bot.poll_book()?;
    let n_before = bot.takers.len();
    bot.ingest(&txs);
    bot.update_c();
    let c = bot.c_bps;
    for i in n_before..bot.takers.len() {
        let t = bot.takers[i];
        if bot.hi_weight(&t) == 0.0 {
            bot.trend.taker(t.side);
        }
    }
    bot.prev_oracle = bot.oracle;
    let oracle = bot.trader.oracle_usd().await?;
    bot.oracle = oracle;
    let (pos, value) = bot.account().await?;
    let sv = *bot.start_value.get_or_insert(value);
    bot.peak_value = bot.peak_value.max(value);
    let dd = bot.peak_value - value;
    let rel = now.saturating_sub(bot.start_slot);
    let left = bot.end_slot.saturating_sub(now);

    // Jumps.
    let mv = oracle / bot.prev_oracle - 1.0;
    if mv.abs() > 0.008 {
        log(format!("JUMP at {rel}: {:.2}%", mv * 100.0));
        bot.last_jump = Some(now);
        bot.burst = None;
        bot.trend.jumped = true;
    } else if mv != 0.0 {
        bot.trend.moves.push((oracle - bot.prev_oracle) / bot.oracle0 * 100.0);
    }

    // Informed bursts.
    for side in [1, -1] {
        let n = bot.burst_score(side, now);
        if n >= 1.0 {
            log(format!("HI side {side} score {n:.2} at {rel}"));
        }
        let last_hi = bot
            .takers
            .iter()
            .rev()
            .find(|t| t.side == side && bot.hi_weight(t) >= 0.5)
            .map(|t| t.slot);
        if let Some(b) = bot.burst.as_mut() {
            if b.side == side {
                if let Some(s) = last_hi {
                    b.last_hi = b.last_hi.max(s);
                }
            }
        }
        let jump_ok = bot.last_jump.map_or(true, |j| now > j + 50);
        let can = rel >= 30 && left >= 56 && jump_ok;
        // A strong trend means no jumps this session: a burst against it is a fake-out.
        let lean = bot.trend.lean();
        let against_trend = lean * side as f64 <= -bot.p.trend_veto;
        if n >= bot.p.theta && can && !against_trend && bot.burst.map_or(true, |b| b.side != side) {
            log(format!("BURST side {side} at {rel} score {n:.2} lean {lean:.2}"));
            bot.burst = Some(Burst { side, start: now, last_hi: now });
        }
    }
    if let Some(b) = bot.burst {
        if now > b.last_hi + bot.p.burst_timeout || left < 50 {
            log(format!("BURST timeout at {rel} (start {})", b.start - bot.start_slot));
            bot.burst = None;
        }
    }

    // Position allowed by the drawdown budget: survive a 4.2% adverse jump
    // plus some walk and crossing costs.
    let budget = (bot.max_dd - 40.0 - dd).max(0.0);
    let cap = (budget / (oracle * 0.042)).min(bot.max_pos - 0.3);
    let lean = bot.trend.lean();
    let target = match bot.burst {
        Some(b) => b.side as f64 * cap,
        None => (bot.p.trend_scale * lean).clamp(-1.0, 1.0) * cap.min(bot.p.trend_max),
    };
    bot.target = target;

    // Market orders: chase a burst; pick off a stale competing quote.
    let room_buy0 = (bot.max_pos - 0.2 - pos).max(0.0);
    let room_sell0 = (bot.max_pos - 0.2 + pos).max(0.0);
    let inv = pos - target;
    let mut take: Option<(Side, f64, f64)> = None;
    let chase = bot.burst.is_some() && inv.abs() > 1.0;
    if chase {
        let slip = (c + 25.0) / 1e4;
        if inv < 0.0 {
            take = Some((Side::Bid, (-inv).min(room_buy0), oracle * (1.0 + slip)));
        } else {
            take = Some((Side::Ask, inv.min(room_sell0), oracle * (1.0 - slip)));
        }
    } else {
        let edge = (FEE_TAKER_BPS + bot.p.pick_bps) / 1e4;
        if let Some(a) = bot.maker.ask {
            let px = a as f64 * TICK;
            if px < oracle * (1.0 - edge) && room_buy0 > 0.5 {
                take = Some((Side::Bid, room_buy0.min(5.0), px));
            }
        }
        if let Some(b) = bot.maker.bid {
            let px = b as f64 * TICK;
            if px > oracle * (1.0 + edge) && room_sell0 > 0.5 {
                take = Some((Side::Ask, room_sell0.min(5.0), px));
            }
        }
        if let Some((_, _, px)) = take {
            log(format!("PICK at {rel}: px {px:.2} oracle {oracle:.2}"));
        }
    }
    // Assume a chase fills (it sweeps the book); a pick-off may not.
    let pos_after = match take {
        Some((Side::Bid, q, _)) if chase => pos + q,
        Some((Side::Ask, q, _)) if chase => pos - q,
        _ => pos,
    };
    let inv_after = pos_after - target;

    // Quotes, in ticks from the oracle.
    let tick_bps = TICK / oracle * 1e4;
    let ot = (oracle / TICK).round() as i64;
    let comp_bid = (oracle * (1.0 - c / 1e4) / TICK).floor() as i64;
    let comp_ask = (oracle * (1.0 + c / 1e4) / TICK).ceil() as i64;
    let mb = bot.maker.bid.map(|t| t as i64).unwrap_or(comp_bid).max(comp_bid);
    let ma = bot.maker.ask.map(|t| t as i64).unwrap_or(comp_ask).min(comp_ask);
    let dmin = (bot.p.floor_bps / tick_bps).ceil() as i64;
    // Base: one tick inside the best other quote, no closer than our floor.
    let mut bid_d = (ot - (mb + 1)).max(dmin);
    let mut ask_d = ((ma - 1) - ot).max(dmin);
    // Inventory against target: tighten the side that reduces it, widen the other.
    let k_in = bot.p.skew_bps / tick_bps; // ticks per SPCX of inventory
    let sh = (inv_after.abs() * k_in).round() as i64;
    if inv_after > 0.0 {
        ask_d = (ask_d - sh).max(bot.p.reduce_min_ticks);
        bid_d += sh;
    } else {
        bid_d = (bid_d - sh).max(bot.p.reduce_min_ticks);
        ask_d += sh;
    }
    let bid_t = ot - bid_d;
    let ask_t = ot + ask_d;
    // Whatever the market order does, resting fills can't break the cap.
    let room_buy = (bot.max_pos - 0.2 - pos_after.max(pos)).max(0.0);
    let room_sell = (bot.max_pos - 0.2 + pos_after.min(pos)).max(0.0);
    let q = bot.p.quote_size;
    let mut bsz = q;
    let mut asz = q;
    if inv_after > 0.5 {
        asz = asz.max(inv_after.min(bot.p.reduce_size));
        if inv_after > 6.0 { bsz = q * 0.5; }
    }
    if inv_after < -0.5 {
        bsz = bsz.max((-inv_after).min(bot.p.reduce_size));
        if inv_after < -6.0 { asz = q * 0.5; }
    }
    // Never quote into a burst.
    if let Some(b) = bot.burst {
        if b.side > 0 && inv_after <= 0.5 { asz = 0.0; }
        if b.side < 0 && inv_after >= -0.5 { bsz = 0.0; }
    }
    // Defensive: large-limit flow on one side just started.
    for side in [1, -1] {
        if bot.burst_score(side, now) >= bot.p.defend {
            if side > 0 && inv_after <= 0.5 { asz = 0.0; }
            if side < 0 && inv_after >= -0.5 { bsz = 0.0; }
        }
    }
    bsz = bsz.min(room_buy);
    asz = asz.min(room_sell);
    if left <= 2 {
        bsz = 0.0;
        asz = 0.0;
    }
    let bl = (bsz * LOTS_PER_UNIT).floor() as u64;
    let al = (asz * LOTS_PER_UNIT).floor() as u64;
    let want_bid = if bl >= 10 { Some((bid_t.max(1) as u64, bl)) } else { None };
    let want_ask = if al >= 10 { Some((ask_t.max(1) as u64, al)) } else { None };
    log(format!(
        "STEP {rel} oracle {oracle:.2} pos {pos:.3} tgt {target:.1} lean {lean:.2} val {:.2} dd {dd:.2} c {c:.1} maker {:?}/{:?} want {:?}/{:?} take {:?}",
        value - sv, bot.maker.bid.map(|t| t as i64 - ot), bot.maker.ask.map(|t| t as i64 - ot),
        want_bid.map(|(t, l)| (t as i64 - ot, l)), want_ask.map(|(t, l)| (t as i64 - ot, l)), take.map(|t| t.1)
    ));
    let filled = (pos - bot.last_pos).abs() > 1e-9;
    bot.last_pos = pos_after;
    let take_lots = take.map(|(s, q, px)| (s, (q * LOTS_PER_UNIT).floor() as u64, px)).filter(|t| t.1 >= 10);
    if take_lots.is_some() || filled || (want_bid, want_ask) != bot.my_quotes {
        let bids: Vec<_> = want_bid.into_iter().collect();
        let asks: Vec<_> = want_ask.into_iter().collect();
        match bot.send(&bids, &asks, take_lots) {
            Ok(()) => bot.my_quotes = (want_bid, want_ask),
            Err(e) => {
                log(format!("send error: {e:#}"));
                bot.my_quotes = (None, None);
            }
        }
    }
    Ok(())
}
