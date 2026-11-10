//! A market maker for SPCX that reads the takers' flow from the chain's
//! blocks: it quotes one tick inside the competing makers, skewed against
//! its inventory, and when the flow turns one-sided the way it does ahead
//! of a jump, it stops quoting that side and takes the position instead.

use std::collections::VecDeque;

use mtx_kit::phoenix_rise::ix::PhoenixInstruction;
use mtx_kit::Trader;
use mtx_kit::prelude::*;
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;

/// The burst detector: net taker orders (buys minus sells) over `WINDOW`
/// slots reaching `TRIGGER` starts a lean, held for `HOLD` slots.
const WINDOW: u64 = 12;
const TRIGGER: i64 = 6;
const HOLD: u64 = 30;
/// A step's oracle move this large is a jump: the lean is over.
const JUMP: f64 = 0.008;
/// How far past the oracle a lean's market orders may fill.
const TAKE_SLIP: f64 = 0.0050;
/// Quoting: the closest a quote comes to the reservation price, the skew
/// at a full position, and the sizes that add to and reduce the position.
const MIN_HALF: f64 = 0.00025;
const MAX_HALF: f64 = 0.0032;
const SKEW: f64 = 0.0003;
const QUOTE_SIZE: f64 = 3.0;
const REDUCE_SIZE: f64 = 5.0;
/// The position the bot allows itself, under the mandate's cap.
const CAP_MARGIN: f64 = 0.1;
/// Drawdown guard: what a worst jump costs per unit, and the room kept.
const WORST_JUMP: f64 = 0.045;
const DRAWDOWN_MARGIN: f64 = 40.0;

const DEBUG: bool = true;

#[derive(Default)]
struct Maker {
    me: String,
    tick: f64,
    lots_per_unit: f64,
    last_slot: u64,
    dumped: u32,
    /// Taker orders seen: (slot, +1 buy / -1 sell).
    flow: VecDeque<(u64, i64)>,
    lean: i64,
    lean_until: u64,
    last_oracle: f64,
    last_position: f64,
    /// The account's value since the start, at the oracle, and its peak.
    value: f64,
    peak: f64,
    /// The quotes resting, if any: (bid, ask) in ticks, 0 for none.
    resting: Option<(i64, i64)>,
    resting_position: f64,
}

impl Maker {
    /// The taker orders in the blocks since the last step.
    fn read_flow(&mut self, trader: &Trader, slot: u64) {
        // A slot's block is readable once the next step has produced its own.
        let slot = slot - 1;
        let from = if self.last_slot == 0 { slot.saturating_sub(3) } else { self.last_slot + 1 };
        let market_order = PhoenixInstruction::PlaceMarketOrder.discriminant();
        for s in from..=slot {
            let block: Value = match trader.rpc.send(
                RpcRequest::GetBlock,
                json!([s, {"encoding": "json", "transactionDetails": "full", "rewards": false, "maxSupportedTransactionVersion": 0, "commitment": "confirmed"}]),
            ) {
                Ok(block) => block,
                Err(error) => {
                    if DEBUG {
                        eprintln!("getBlock {s} failed: {error}");
                    }
                    continue;
                }
            };
            if DEBUG && self.dumped < 0 {
                self.dumped += 1;
                eprintln!("block {s}: {}", block.to_string().chars().take(3000).collect::<String>());
            }
            let Some(txs) = block.get("transactions").and_then(Value::as_array) else { continue };
            for tx in txs {
                let message = &tx["transaction"]["message"];
                if message["accountKeys"][0].as_str() == Some(self.me.as_str()) {
                    continue;
                }
                let Some(ixs) = message["instructions"].as_array() else { continue };
                for ix in ixs {
                    let Some(data) = ix["data"].as_str().and_then(|d| bs58::decode(d).into_vec().ok()) else { continue };
                    if data.len() < 10 || data[..8] != market_order {
                        continue;
                    }
                    let sign = if data[9] == 0 { 1 } else { -1 };
                    self.flow.push_back((s, sign));
                    if DEBUG {
                        eprintln!("flow slot={s} sign={sign} failed={}", !tx["meta"]["err"].is_null());
                    }
                }
            }
        }
        self.last_slot = slot;
        while self.flow.front().is_some_and(|&(s, _)| s + WINDOW <= slot + 1) {
            self.flow.pop_front();
        }
    }

    /// A price on the tick grid, as the decimal it prints as.
    fn usd(&self, ticks: i64) -> f64 {
        (ticks as f64 * self.tick * 1e6).round() / 1e6
    }

    fn ticks(&self, usd: f64) -> i64 {
        (usd / self.tick).round() as i64
    }
}

impl Strategy for Maker {
    async fn start(&mut self, session: &Session) -> BotResult {
        let trader = &session.trader;
        self.me = trader.authority().to_string();
        let market = trader.phoenix.market(&trader.env.symbol)?;
        self.tick = trader.phoenix.ticks_to_usd(market, 1);
        self.lots_per_unit = trader.phoenix.base_units_to_lots(market, 1.0) as f64;
        if DEBUG {
            eprintln!("start me={} tick={} lots={}", self.me, self.tick, self.lots_per_unit);
        }
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let Some(trader) = orders.trader() else { return Ok(()) };
        let oracle = market.oracle;
        let position = market.position.size;
        let cap = market.cap - CAP_MARGIN;

        // The account's value at the oracle, and how far it is below its peak.
        if self.last_oracle > 0.0 {
            self.value += self.last_position * (oracle - self.last_oracle);
        }
        self.peak = self.peak.max(self.value);
        let drawdown = self.peak - self.value;
        let jumped = self.last_oracle > 0.0 && (oracle / self.last_oracle - 1.0).abs() >= JUMP;
        if DEBUG && self.last_oracle > 0.0 && (position - self.last_position).abs() > 1e-9 {
            eprintln!("fill k={} slot={} pos {} -> {} oracle {} -> {}", market.k, market.slot, self.last_position, position, self.last_oracle, oracle);
        }
        self.last_oracle = oracle;
        self.last_position = position;

        self.read_flow(trader, market.slot);
        if jumped {
            self.lean = 0;
            self.flow.clear();
        }
        let imbalance: i64 = self.flow.iter().map(|&(_, sign)| sign).sum();
        if imbalance.abs() >= TRIGGER {
            self.lean = imbalance.signum();
            self.lean_until = market.slot + HOLD;
        } else if self.lean != 0 && market.slot > self.lean_until {
            self.lean = 0;
        }

        // The most the bot may hold and still survive a worst jump against it.
        let allowed = ((market.max_drawdown - drawdown - DRAWDOWN_MARGIN) / (oracle * WORST_JUMP)).clamp(0.0, cap);
        let mut expected = position;
        let mut sheet: Vec<(Side, f64, f64)> = Vec::new();
        if self.lean != 0 {
            let missing = self.lean as f64 * allowed - position;
            if missing * self.lean as f64 >= 0.5 {
                let side = if missing > 0.0 { Side::Bid } else { Side::Ask };
                sheet.push((side, missing.abs(), oracle * (1.0 + self.lean as f64 * TAKE_SLIP)));
                expected += missing;
            }
        }
        if expected.abs() > allowed + 0.5 {
            let excess = expected.abs() - allowed;
            let (side, limit) = if expected > 0.0 { (Side::Ask, oracle * (1.0 - TAKE_SLIP)) } else { (Side::Bid, oracle * (1.0 + TAKE_SLIP)) };
            sheet.push((side, excess, limit));
            expected -= excess * expected.signum();
        }
        for &(_, size, _) in &sheet {
            self.value -= size * oracle * 0.003;
        }

        // The quotes: one tick inside the best other quote, no closer than
        // MIN_HALF to a reservation price skewed against the position.
        let bbo = trader.inner.bbo().await?;
        let best_bid = bbo.best_bid_ticks().map(|t| t as i64).unwrap_or(0);
        let best_ask = bbo.best_ask_ticks().map(|t| t as i64).unwrap_or(0);
        let (my_bid, my_ask) = self.resting.unwrap_or((0, 0));
        let reservation = oracle * (1.0 - SKEW * expected / market.cap);
        let bid_cap = (reservation * (1.0 - MIN_HALF)).min(oracle - self.tick);
        let ask_floor = (reservation * (1.0 + MIN_HALF)).max(oracle + self.tick);
        let mut bid = if my_bid != 0 && best_bid == my_bid { my_bid } else if best_bid != 0 { best_bid + 1 } else { self.ticks(oracle * (1.0 - MAX_HALF)) };
        let mut ask = if my_ask != 0 && best_ask == my_ask { my_ask } else if best_ask != 0 { best_ask - 1 } else { self.ticks(oracle * (1.0 + MAX_HALF)) };
        bid = bid.clamp((oracle * (1.0 - MAX_HALF) / self.tick).floor() as i64, (bid_cap / self.tick).floor() as i64);
        ask = ask.clamp((ask_floor / self.tick).ceil() as i64, (oracle * (1.0 + MAX_HALF) / self.tick).ceil() as i64);
        let room_buy = (allowed - expected).max(0.0);
        let room_sell = (allowed + expected).max(0.0);
        let mut bid_size = if expected < -0.5 { REDUCE_SIZE.min(-expected).max(QUOTE_SIZE) } else { QUOTE_SIZE }.min(room_buy);
        let mut ask_size = if expected > 0.5 { REDUCE_SIZE.min(expected).max(QUOTE_SIZE) } else { QUOTE_SIZE }.min(room_sell);
        // Leaning: don't sell to the buyers who know, or buy from the sellers.
        if self.lean > 0 || allowed < 1.0 {
            ask_size = 0.0;
        }
        if self.lean < 0 || allowed < 1.0 {
            bid_size = 0.0;
        }
        let bid_ticks = if bid_size >= 0.1 { bid } else { 0 };
        let ask_ticks = if ask_size >= 0.1 { ask } else { 0 };
        let wanted = (bid_ticks, ask_ticks);
        let unchanged = self.resting == Some(wanted) && (self.resting_position - position).abs() < 1e-9 && sheet.is_empty();
        if DEBUG {
            eprintln!(
                "step k={} slot={} oracle={:.3} mark={:.3} pos={:.3} imb={} lean={} bbo={}/{} want={:?} keep={} value={:.2} dd={:.2} budget={:?}",
                market.k, market.slot, oracle, market.mark, position, imbalance, self.lean, best_bid, best_ask, wanted, unchanged, self.value, drawdown, market.budget_left
            );
        }
        if unchanged {
            return Ok(());
        }
        let nothing_resting = matches!(self.resting, None | Some((0, 0)));
        if !nothing_resting || self.resting.is_none() {
            orders.cancel_all();
        }
        for (side, size, limit) in sheet {
            match side {
                Side::Bid => orders.buy(size, limit),
                Side::Ask => orders.sell(size, limit),
            };
        }
        if bid_ticks != 0 {
            orders.bid(self.usd(bid_ticks), (bid_size * self.lots_per_unit).floor() / self.lots_per_unit);
        }
        if ask_ticks != 0 {
            orders.ask(self.usd(ask_ticks), (ask_size * self.lots_per_unit).floor() / self.lots_per_unit);
        }
        self.resting = Some(wanted);
        self.resting_position = expected;
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}
