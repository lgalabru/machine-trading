//! SPCX market maker: quotes inside the competitor with inventory skew,
//! reads taker market orders from each step's blocks, detects one-sided
//! bursts (informed flow) and leans into the jump, and leans with a drift.

use std::collections::VecDeque;

use mtx_kit::prelude::*;
use mtx_kit::{Pubkey, Signer};
use solana_transaction_status_client_types::{
    EncodedTransaction, UiMessage,
};
use tracing::{info, warn};

const TICK: f64 = 0.01;
const PHOENIX: &str = "EtrnLzgbS7nMMy5fbD42kXiUzGg8XQzJ972Xtk1cjWih";
const MARKET_ORDER_DISC: [u8; 8] = [0x5a, 0x76, 0xc0, 0xfc, 0xc0, 0x63, 0x27, 0x91];

// Detector parameters (tuned offline on generated scenarios).
const READ_BLOCKS: bool = false;
const WINDOW_SLOTS: u64 = 8;
const DEFENSE_FILLS: f64 = 1.8;
const DEFENSE_STEPS: i64 = 12;
const TRIGGER: i64 = 6;
const HOLD_STEPS: i64 = 20;

#[derive(Clone, Copy, Debug)]
struct TakerOrder {
    slot: u64,
    buy: bool,
    size: f64,
}

#[derive(Default)]
struct Bot {
    me: Option<Pubkey>,
    comp_bps: f64,
    flow: VecDeque<TakerOrder>,
    oracles: VecDeque<f64>,
    lean: f64,
    lean_steps: i64,
    lean_dir: f64,
    last_oracle: f64,
    jump_cool: i64,
    last_slot_read: u64,
    blocks_ok: bool,
    block_errors: u32,
    flatten: bool,
    last_pos: Option<f64>,
    fills: VecDeque<f64>,
    defense_steps: i64,
    defense_dir: f64,
}

impl Bot {
    fn walk_sigma(&self) -> f64 {
        let v: Vec<f64> = self.oracles.iter().copied().collect();
        let mut s = 0.0;
        let mut n = 0.0;
        for w in v.windows(2) {
            let r = (w[1] - w[0]) / w[0];
            if r.abs() < 0.006 {
                s += r * r;
                n += 1.0;
            }
        }
        if n < 10.0 { 0.0005 } else { (s / n).sqrt().max(0.00005) }
    }

    /// Read the taker market orders in `slot`'s block.
    fn read_block(&mut self, rpc: &mtx_kit::RpcClient, slot: u64) -> Result<Vec<TakerOrder>, String> {
        let params = serde_json::json!([slot, {"encoding":"json","transactionDetails":"full","rewards":false,"maxSupportedTransactionVersion":0}]);
        let raw: serde_json::Value = rpc
            .send(solana_rpc_client_api::request::RpcRequest::GetBlock, params)
            .map_err(|e| e.to_string())?;
        if raw.is_null() {
            return Err(format!("null block (slot {slot})"));
        }
        let block: solana_transaction_status_client_types::UiConfirmedBlock =
            serde_json::from_value(raw).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        let me = self.me.map(|p| p.to_string());
        for tx in block.transactions.unwrap_or_default() {
            if let Some(meta) = &tx.meta {
                if meta.err.is_some() {
                    continue;
                }
            }
            let EncodedTransaction::Json(ui) = &tx.transaction else { continue };
            let UiMessage::Raw(msg) = &ui.message else { continue };
            if let (Some(me), Some(first)) = (&me, msg.account_keys.first()) {
                if first == me {
                    continue;
                }
            }
            for ix in &msg.instructions {
                let Some(pid) = msg.account_keys.get(ix.program_id_index as usize) else { continue };
                if pid != PHOENIX {
                    continue;
                }
                let Ok(data) = bs58::decode(&ix.data).into_vec() else { continue };
                if data.len() < 27 || data[0..8] != MARKET_ORDER_DISC {
                    continue;
                }
                // OrderPacketKind::ImmediateOrCancel { side: u8, price: Option<u64>, num_base_lots: u64, .. }
                if data[8] != 2 {
                    continue;
                }
                let buy = data[9] == 0;
                let off = if data[10] == 1 { 19 } else { 11 };
                if data.len() < off + 8 {
                    continue;
                }
                let lots = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
                out.push(TakerOrder { slot, buy, size: lots as f64 / 1000.0 });
            }
        }
        Ok(out)
    }
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        info!(symbol = session.env.symbol, steps = session.hello.steps, cap = session.env.max_position, "started");
        self.me = Some(session.trader.keypair.pubkey());
        self.comp_bps = 15.0;
        if let (Ok(bbo), Ok(oracle)) = (session.trader.inner.bbo().await, session.trader.oracle_usd().await) {
            let bid = bbo.best_bid_ticks().map(|t| t as f64 * TICK);
            let ask = bbo.best_ask_ticks().map(|t| t as f64 * TICK);
            if let (Some(b), Some(a)) = (bid, ask) {
                let half = ((oracle - b).max(a - oracle)) / oracle * 1e4;
                if half > 1.0 && half < 60.0 {
                    self.comp_bps = half;
                }
                info!(bid = b, ask = a, oracle, comp_bps = self.comp_bps, "book at start");
            }
        }
        self.last_slot_read = session.hello.start_slot.saturating_sub(1);
        Ok(())
    }

    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let px = m.oracle;
        let q = m.position.size;
        let cap = m.cap;

        // Taker flow from the new blocks.
        if let (true, Some(trader)) = (READ_BLOCKS, orders.trader()) {
            if m.k % 10 == 0 || m.k < 3 {
                let chain_slot = trader.rpc.get_slot().map_err(|e| e.to_string());
                info!(step = m.k, step_slot = m.slot, ?chain_slot, "slots");
            }
            let from = (self.last_slot_read + 1).max(m.slot.saturating_sub(3));
            for s in from..=m.slot {
                match self.read_block(&trader.rpc, s) {
                    Ok(list) => {
                        self.blocks_ok = true;
                        for o in list {
                            self.flow.push_back(o);
                        }
                    }
                    Err(e) => {
                        self.block_errors += 1;
                        if self.block_errors <= 5 || self.block_errors % 50 == 0 {
                            warn!(slot = s, error = %e, "block read failed");
                        }
                    }
                }
            }
            self.last_slot_read = m.slot;
        }
        while let Some(f) = self.flow.front() {
            if f.slot + 60 < m.slot { self.flow.pop_front(); } else { break; }
        }
        let (mut buys, mut sells) = (0i64, 0i64);
        for o in &self.flow {
            if o.slot + WINDOW_SLOTS > m.slot {
                if o.buy { buys += 1 } else { sells += 1 }
            }
        }
        let diff = buys - sells;

        // Our own fills: position change per step (+ = takers sold to us).
        let dq = self.last_pos.map(|lp| q - lp).unwrap_or(0.0);
        self.last_pos = Some(q);
        self.fills.push_back(dq);
        if self.fills.len() > 4 { self.fills.pop_front(); }
        let net_fills: f64 = self.fills.iter().sum();
        if self.defense_steps > 0 { self.defense_steps -= 1; }
        if self.lean_steps == 0 && net_fills.abs() >= DEFENSE_FILLS && net_fills.signum() == q.signum() && q.abs() >= 1.5 {
            // One-sided flow pushed us into inventory: maybe informed. Back off and flatten.
            self.defense_dir = net_fills.signum();
            if self.defense_steps == 0 {
                info!(step = m.k, net_fills, q, "one-sided fills: defensive flatten");
            }
            self.defense_steps = DEFENSE_STEPS;
            self.fills.clear();
        }

        // Oracle history and jumps.
        let jump = if self.last_oracle > 0.0 { (px - self.last_oracle) / self.last_oracle } else { 0.0 };
        self.last_oracle = px;
        self.oracles.push_back(px);
        if self.oracles.len() > 120 { self.oracles.pop_front(); }
        let recent_jump = jump.abs() > 0.008;
        if recent_jump { self.jump_cool = 8; }
        if self.jump_cool > 0 { self.jump_cool -= 1; }

        // Lean management.
        if self.lean_steps > 0 {
            self.lean_steps -= 1;
            if recent_jump && jump.signum() == self.lean_dir {
                info!(step = m.k, jump, "jump in leaned direction: flattening");
                self.lean = 0.0;
                self.lean_steps = 0;
                self.flow.clear();
                self.flatten = true;
            } else if recent_jump {
                info!(step = m.k, jump, "jump against lean: flattening");
                self.lean = 0.0;
                self.lean_steps = 0;
                self.flow.clear();
                self.flatten = true;
            } else if self.lean_steps == 0 {
                info!(step = m.k, "lean expired");
                self.lean = 0.0;
                self.flow.clear();
                self.flatten = true;
            } else if (diff >= TRIGGER && self.lean_dir < 0.0) || (diff <= -TRIGGER && self.lean_dir > 0.0) {
                // Flow turned against the lean: give up.
                info!(step = m.k, diff, "flow reversed: flattening");
                self.lean = 0.0;
                self.lean_steps = 0;
                self.flow.clear();
                self.flatten = true;
            } else if (diff >= TRIGGER && self.lean_dir > 0.0) || (diff <= -TRIGGER && self.lean_dir < 0.0) {
                // Burst continues: extend the hold.
                self.lean_steps = HOLD_STEPS;
            }
        } else if self.jump_cool == 0 && m.steps_left > 3 {
            if diff >= TRIGGER {
                self.lean_dir = 1.0;
                self.lean = cap;
                self.lean_steps = HOLD_STEPS;
                info!(step = m.k, buys, sells, "burst detected: leaning long");
            } else if diff <= -TRIGGER {
                self.lean_dir = -1.0;
                self.lean = -cap;
                self.lean_steps = HOLD_STEPS;
                info!(step = m.k, buys, sells, "burst detected: leaning short");
            }
        }

        // Trend signal: cumulative oracle move over the window, excluding jumps.
        let mut trend_target = 0.0;
        if self.oracles.len() >= 60 {
            let sig = self.walk_sigma();
            let v: Vec<f64> = self.oracles.iter().copied().collect();
            let mut cum = 0.0;
            for w in v.windows(2) {
                let r = (w[1] - w[0]) / w[0];
                if r.abs() < 0.006 { cum += r; }
            }
            let z = cum / (sig * ((v.len() - 1) as f64).sqrt());
            if z.abs() > 3.0 {
                trend_target = z.signum() * ((z.abs() - 3.0) / 2.0).min(1.0) * cap * 0.5;
            }
        }

        // Drawdown guard.
        let dd_room = m.drawdown_room();
        let risk_scale = if dd_room < 70.0 { 0.0 } else if dd_room < 130.0 { 0.5 } else { 1.0 };
        let target = if self.lean_steps > 0 { self.lean } else { trend_target } * risk_scale;

        // Quotes.
        let h_bps = (self.comp_bps * 0.6).max(2.0);
        let h = (px * h_bps / 1e4).max(2.0 * TICK);
        let inv = q - target;
        let skew = h * (inv / 5.0).clamp(-1.5, 1.5);
        let bid = (px - h - skew).min(px - TICK);
        let ask = (px + h - skew).max(px + TICK);
        let base_size = (self.comp_bps / 10.0).clamp(1.0, 3.0) * if risk_scale == 0.0 { 0.5 } else { 1.0 };
        let mut bid_size = base_size;
        let mut ask_size = base_size;
        if inv > 3.0 { bid_size *= 0.5; }
        if inv > 6.0 { bid_size = 0.0; }
        if inv < -3.0 { ask_size *= 0.5; }
        if inv < -6.0 { ask_size = 0.0; }
        if self.defense_steps > 0 {
            if self.defense_dir > 0.0 { bid_size = 0.0 } else { ask_size = 0.0 }
        }
        // While leaning, don't quote the side that would undo the lean.
        if self.lean_steps > 0 {
            if self.lean_dir > 0.0 { ask_size = 0.0 } else { bid_size = 0.0 }
        }

        // Cancel first so market orders don't self-trade with our quotes.
        orders.cancel_all();
        if m.steps_left <= 1 {
            return Ok(());
        }

        let diff_pos = target - q;
        if self.lean_steps > 0 && diff_pos.abs() >= 1.0 {
            orders.target_position_with(target, Execution { slice: 10.0, slippage_bps: 100.0, min_trade: 0.5, max_per_step: 20.0 });
        } else if self.flatten {
            self.flatten = false;
            // Lean over: trade to the target now.
            if diff_pos.abs() >= 0.5 { orders.target_position_with(target, Execution { slice: 10.0, slippage_bps: 100.0, min_trade: 0.5, max_per_step: 20.0 }); }
        } else if self.defense_steps > 0 && self.lean_steps == 0 && (q - target) * self.defense_dir >= 1.0 {
            orders.target_position_with(target, Execution { slice: 10.0, slippage_bps: 100.0, min_trade: 0.5, max_per_step: 20.0 });
        } else if self.lean_steps == 0 && trend_target != 0.0 && diff_pos.abs() >= 3.0 && m.steps_left > 20 {
            orders.target_position_with(target, Execution { slice: 10.0, slippage_bps: 100.0, min_trade: 0.5, max_per_step: 3.0 });
        }
        if bid_size > 0.0 { orders.bid(bid, bid_size); }
        if ask_size > 0.0 { orders.ask(ask, ask_size); }

        if m.k % 10 == 0 || self.lean_steps > 0 {
            info!(step = m.k, px, q, buys, sells, net_fills, defense = self.defense_steps, target, trend_target, pnl = m.pnl, dd = m.drawdown, flow = self.flow.len(), blocks_ok = self.blocks_ok, "step");
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::default())
}
