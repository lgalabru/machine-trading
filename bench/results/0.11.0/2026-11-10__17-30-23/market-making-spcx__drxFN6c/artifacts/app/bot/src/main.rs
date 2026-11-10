//! A market maker for Phoenix's SPCX perpetual.
//!
//! Every step it quotes one tick inside the best competing quotes, skewed
//! against its inventory, and watches the takers' flow. A one-sided burst of
//! taker orders is what the informed flow before a jump looks like: on one,
//! it pulls its quotes and takes a position with the burst, holds it until
//! the jump lands or the burst goes stale, and then goes flat again.

use std::collections::VecDeque;

use mtx_kit::{
    phoenix_rise::core::{LimitOrderTicket, MarketOrderTicket},
    prelude::*,
    Instruction, Trader,
};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_rpc_client_api::config::{RpcBlockConfig, RpcTransactionConfig};
use solana_transaction_status_client_types::{TransactionDetails, UiTransactionEncoding};
use tracing::{info, warn};

/// Steps of taker flow the burst detector sums, and the net flow that trips it.
const WINDOW: usize = 4;
const TRIP: f64 = 6.0;
/// The net flow at which the side the flow is hitting stops quoting.
const WARY: f64 = 3.0;
/// Steps a burst position is held, waiting for its jump.
const HOLD_STEPS: u64 = 16;
/// A one-step oracle move this large is a jump.
const JUMP: f64 = 0.01;
/// The position taken with a burst, and the smaller one once the session has
/// drawn down.
const BURST_SIZE: f64 = 19.0;
const BURST_SIZE_CAUTIOUS: f64 = 8.0;
/// Market making: the size quoted a side, the inventory past which a side
/// stops quoting, and the skew in ticks per unit of inventory.
const QUOTE_SIZE: f64 = 2.0;
const INVENTORY_LIMIT: f64 = 6.0;
const SKEW_TICKS: f64 = 0.5;
const MIN_OFFSET_TICKS: f64 = 2.0;
/// How far through the oracle a taking order may fill.
const TAKE_SLIPPAGE: f64 = 0.006;
/// Drawdown (USD) past which burst positions shrink, and past which the bot
/// goes flat and stops.
const CAUTIOUS_DRAWDOWN: f64 = 90.0;
const STOP_DRAWDOWN: f64 = 165.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Making,
    Holding { dir: f64, since: u64 },
    Stopped,
}

struct Maker {
    mode: Mode,
    tick: f64,
    flows: VecDeque<f64>,
    /// The position after the bot's own orders last turn: what it holds now,
    /// less this, is what the takers did.
    reference: Option<f64>,
    last_oracle: Option<f64>,
    /// The bot's resting quotes, in ticks.
    my_bid: Option<u64>,
    my_ask: Option<u64>,
    /// The competing quotes' distance from the oracle, in ticks.
    their_bid_offset: f64,
    their_ask_offset: f64,
    cooldown_until: u64,
    /// The account's PnL at the oracle, its peak, estimated from the position.
    pnl: f64,
    peak: f64,
    order_id: u128,
    /// How a taker's market order reads in a block: the program, the
    /// instruction's leading bytes, and where its side is.
    program: Option<mtx_kit::Pubkey>,
    prefix: Vec<u8>,
    side_at: usize,
    buy_byte: u8,
    sell_byte: u8,
    last_slot: Option<u64>,
    seen: (u64, u64),
    diag: u32,
    blocks_work: bool,
    by_signature: bool,
    last_signature: Option<mtx_kit::Signature>,
    mine: std::collections::HashSet<String>,
}

impl Default for Maker {
    fn default() -> Self {
        Maker {
            mode: Mode::Making,
            tick: 0.01,
            flows: VecDeque::new(),
            reference: None,
            last_oracle: None,
            my_bid: None,
            my_ask: None,
            their_bid_offset: 20.0,
            their_ask_offset: 20.0,
            cooldown_until: 0,
            pnl: 0.0,
            peak: 0.0,
            order_id: 1,
            program: None,
            prefix: Vec::new(),
            side_at: 0,
            buy_byte: 0,
            sell_byte: 1,
            last_slot: None,
            seen: (0, 0),
            diag: 0,
            blocks_work: false,
            by_signature: true,
            last_signature: None,
            mine: std::collections::HashSet::new(),
        }
    }
}

impl Maker {
    fn next_id(&mut self) -> u128 {
        self.order_id += 1;
        self.order_id
    }

    fn price(&self, ticks: f64) -> f64 {
        (ticks * self.tick * 1e6).round() / 1e6
    }

    async fn limit(
        &mut self,
        trader: &Trader,
        side: Side,
        ticks: f64,
        size: f64,
    ) -> BotResult<Vec<Instruction>> {
        let ticket = LimitOrderTicket::builder()
            .authority(trader.authority())
            .trader_account(trader.account)
            .symbol(trader.env.symbol.as_str())
            .side(side)
            .price(self.price(ticks))
            .num_base_lots(trader.lots(size)?)
            .client_order_id(self.next_id())
            .build()?;
        Ok(trader.phoenix.tx().place_limit_order(ticket).await?)
    }

    async fn take(
        &mut self,
        trader: &Trader,
        side: Side,
        ticks: f64,
        size: f64,
    ) -> BotResult<Vec<Instruction>> {
        let ticket = MarketOrderTicket::builder()
            .authority(trader.authority())
            .trader_account(trader.account)
            .symbol(trader.env.symbol.as_str())
            .side(side)
            .num_base_lots(trader.lots(size)?)
            .client_order_id(self.next_id())
            .price(self.price(ticks))
            .build()?;
        Ok(trader.phoenix.tx().place_market_order(ticket).await?)
    }

    /// Instructions that trade toward `target` now, against the book.
    async fn trade_toward(
        &mut self,
        trader: &Trader,
        market: &Market,
        target: f64,
    ) -> BotResult<Vec<Instruction>> {
        let position = market.position.size;
        let target = target.clamp(-market.cap + 0.5, market.cap - 0.5);
        let missing = ((target - position) * 100.0).floor() / 100.0;
        if missing.abs() < 0.05 {
            return Ok(Vec::new());
        }
        let oracle_ticks = market.oracle / self.tick;
        if missing > 0.0 {
            let limit = (oracle_ticks * (1.0 + TAKE_SLIPPAGE)).floor();
            self.take(trader, Side::Bid, limit, missing).await
        } else {
            let limit = (oracle_ticks * (1.0 - TAKE_SLIPPAGE)).ceil();
            self.take(trader, Side::Ask, limit, -missing).await
        }
    }
}

impl Maker {
    /// Learn how a market order reads on the chain, from the bot's own.
    async fn learn_orders(&mut self, trader: &Trader) -> BotResult {
        let buy = self.take(trader, Side::Bid, 15000.0, 1.0).await?;
        let sell = self.take(trader, Side::Ask, 15000.0, 1.0).await?;
        let resting = self.limit(trader, Side::Bid, 15000.0, 1.0).await?;
        let (Some(buy), Some(sell), Some(resting)) = (buy.last(), sell.last(), resting.last())
        else {
            return Err("no order instructions".into());
        };
        let Some(at) = (0..buy.data.len().min(sell.data.len())).find(|&i| buy.data[i] != sell.data[i])
        else {
            return Err("market orders don't differ by side".into());
        };
        self.prefix = buy.data[..at.min(8)].to_vec();
        self.side_at = at;
        self.buy_byte = buy.data[at];
        self.sell_byte = sell.data[at];
        self.program = Some(buy.program_id);
        info!(
            ixs = buy.data.len(),
            at,
            buy = ?&buy.data[..buy.data.len().min(24)],
            sell = ?&sell.data[..sell.data.len().min(24)],
            resting = ?&resting.data[..resting.data.len().min(24)],
            same_program = resting.program_id == buy.program_id,
            "learned how orders read"
        );
        if resting.data.starts_with(&self.prefix) && resting.program_id == buy.program_id {
            warn!("limit and market orders read alike");
        }
        Ok(())
    }

    /// The same, where blocks can't be read: from the program's latest
    /// transactions, less the bot's own.
    fn taker_orders_by_signature(&mut self, trader: &Trader, program: mtx_kit::Pubkey) -> (u64, u64) {
        let first_call = self.last_signature.is_none();
        let config = GetConfirmedSignaturesForAddress2Config {
            before: None,
            until: self.last_signature,
            limit: Some(if first_call { 1 } else { 30 }),
            commitment: Some(CommitmentConfig::confirmed()),
        };
        let signatures = match trader.rpc.get_signatures_for_address_with_config(&program, config) {
            Ok(signatures) => signatures,
            Err(error) => {
                if self.diag < 24 {
                    self.diag += 1;
                    warn!(%error, "no signatures");
                }
                return (0, 0);
            }
        };
        if self.diag < 24 {
            self.diag += 1;
            info!(signatures = signatures.len(), "signatures");
        }
        let Some(newest) = signatures.first().and_then(|s| s.signature.parse().ok()) else {
            return (0, 0);
        };
        self.last_signature = Some(newest);
        if first_call {
            return (0, 0);
        }
        let (mut buys, mut sells) = (0, 0);
        for entry in &signatures {
            if self.mine.contains(&entry.signature) || entry.err.is_some() {
                continue;
            }
            let Ok(signature) = entry.signature.parse::<mtx_kit::Signature>() else {
                continue;
            };
            let config = RpcTransactionConfig {
                encoding: Some(UiTransactionEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            };
            let tx = match trader.rpc.get_transaction_with_config(&signature, config) {
                Ok(tx) => tx,
                Err(error) => {
                    if self.diag < 24 {
                        self.diag += 1;
                        warn!(%error, "no transaction");
                    }
                    continue;
                }
            };
            let Some(tx) = tx.transaction.transaction.decode() else {
                continue;
            };
            let keys = tx.message.static_account_keys();
            for ix in tx.message.instructions() {
                if keys.get(ix.program_id_index as usize) != Some(&program)
                    || ix.data.len() <= self.side_at
                    || !ix.data.starts_with(&self.prefix)
                {
                    continue;
                }
                if ix.data[self.side_at] == self.buy_byte {
                    buys += 1;
                } else if ix.data[self.side_at] == self.sell_byte {
                    sells += 1;
                }
            }
        }
        self.seen.0 += buys;
        self.seen.1 += sells;
        (buys, sells)
    }

    /// The takers' market orders in the blocks since the last step: buys and sells.
    fn taker_orders(&mut self, trader: &Trader, slot: u64) -> (u64, u64) {
        let Some(program) = self.program else {
            return (0, 0);
        };
        let me = trader.authority();
        if self.by_signature {
            return self.taker_orders_by_signature(trader, program);
        }
        let first = self.last_slot.map_or(slot.saturating_sub(1), |last| last + 1);
        self.last_slot = Some(slot);
        let (mut buys, mut sells) = (0, 0);
        for slot in first.max(slot.saturating_sub(7))..=slot {
            let config = RpcBlockConfig {
                encoding: Some(UiTransactionEncoding::Base64),
                transaction_details: Some(TransactionDetails::Full),
                rewards: Some(false),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            };
            let block = match trader.rpc.get_block_with_config(slot, config) {
                Ok(block) => block,
                Err(error) => {
                    if self.diag < 12 {
                        self.diag += 1;
                        warn!(slot, %error, "no block");
                    }
                    if !self.blocks_work {
                        self.by_signature = true;
                    }
                    continue;
                }
            };
            self.blocks_work = true;
            let txs = block.transactions.unwrap_or_default();
            if self.diag < 12 {
                self.diag += 1;
                info!(slot, txs = txs.len(), "block");
            }
            for tx in txs {
                let Some(tx) = tx.transaction.decode() else {
                    if self.diag < 12 {
                        warn!(slot, "undecoded transaction");
                    }
                    continue;
                };
                let keys = tx.message.static_account_keys();
                if self.diag < 12 {
                    for ix in tx.message.instructions() {
                        info!(
                            slot,
                            payer = ?keys.first(),
                            program = ?keys.get(ix.program_id_index as usize),
                            data = ?&ix.data[..ix.data.len().min(12)],
                            "instruction"
                        );
                    }
                }
                if keys.first() == Some(&me) {
                    continue;
                }
                for ix in tx.message.instructions() {
                    if keys.get(ix.program_id_index as usize) != Some(&program)
                        || ix.data.len() <= self.side_at
                        || !ix.data.starts_with(&self.prefix)
                    {
                        continue;
                    }
                    if ix.data[self.side_at] == self.buy_byte {
                        buys += 1;
                    } else if ix.data[self.side_at] == self.sell_byte {
                        sells += 1;
                    }
                }
            }
        }
        self.seen.0 += buys;
        self.seen.1 += sells;
        (buys, sells)
    }
}

impl Strategy for Maker {
    async fn start(&mut self, session: &Session) -> BotResult {
        let trader = &session.trader;
        let config = trader.phoenix.market(&trader.env.symbol)?;
        self.tick = trader.phoenix.ticks_to_usd(config, 1);
        if let Err(error) = self.learn_orders(trader).await {
            warn!(%error, "couldn't learn how orders read");
        }
        info!(tick = self.tick, steps = session.hello.steps, "started");
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let Some(trader) = orders.trader() else {
            return Ok(());
        };
        let position = market.position.size;
        let oracle = market.oracle;
        let oracle_ticks = oracle / self.tick;

        // The account's PnL at the oracle, from the position held over the move.
        let moved = self.last_oracle.map_or(0.0, |last| oracle - last);
        let jumped = self
            .last_oracle
            .is_some_and(|last| (oracle / last - 1.0).abs() > JUMP);
        self.last_oracle = Some(oracle);
        // Fills happen before the move lands, so the position now held it.
        self.pnl += position * moved;
        self.peak = self.peak.max(self.pnl);
        let drawdown = self.peak - self.pnl;

        // The takers' net flow this step: what moved the position.
        let (buys, sells) = self.taker_orders(trader, market.slot);
        let flow = buys as f64 - sells as f64;
        self.flows.push_back(flow);
        while self.flows.len() > WINDOW {
            self.flows.pop_front();
        }
        let signal: f64 = self.flows.iter().sum();

        // The competing quotes: the best ones, where they aren't the bot's own.
        let bbo = trader.inner.bbo().await?;
        let (best_bid, best_ask) = (bbo.best_bid_ticks(), bbo.best_ask_ticks());
        if let Some(bid) = best_bid
            && Some(bid) != self.my_bid
        {
            self.their_bid_offset = oracle_ticks - bid as f64;
        }
        if let Some(ask) = best_ask
            && Some(ask) != self.my_ask
        {
            self.their_ask_offset = ask as f64 - oracle_ticks;
        }

        // Mode changes.
        if drawdown > STOP_DRAWDOWN && self.mode != Mode::Stopped {
            warn!(drawdown, "drawdown: going flat and stopping");
            self.mode = Mode::Stopped;
        }
        match self.mode {
            Mode::Holding { dir, since } => {
                if jumped || market.k >= since + HOLD_STEPS {
                    info!(step = market.k, jumped, dir, "leaving the burst position");
                    self.mode = Mode::Making;
                    if jumped {
                        self.cooldown_until = market.k + 5;
                        self.flows.clear();
                    }
                }
            }
            Mode::Making => {
                if signal.abs() >= TRIP && market.k >= self.cooldown_until && market.steps_left > 3
                {
                    let dir = signal.signum();
                    info!(step = market.k, signal, dir, "burst: taking a position");
                    self.mode = Mode::Holding {
                        dir,
                        since: market.k,
                    };
                }
            }
            Mode::Stopped => {}
        }

        let cancel =
            trader
                .phoenix
                .tx()
                .build_cancel_all_orders(trader.authority(), trader.account, &trader.env.symbol)?;
        let had_quotes = self.my_bid.is_some() || self.my_ask.is_some();
        let mut ixs: Vec<Instruction> = Vec::new();
        let mut took = false;
        let (mut new_bid, mut new_ask) = (None, None);
        match self.mode {
            Mode::Holding { dir, .. } => {
                let size = if drawdown > CAUTIOUS_DRAWDOWN {
                    BURST_SIZE_CAUTIOUS
                } else {
                    BURST_SIZE
                };
                let take = self.trade_toward(trader, market, dir * size).await?;
                took = !take.is_empty();
                ixs.extend(take);
            }
            Mode::Stopped => {
                let take = self.trade_toward(trader, market, 0.0).await?;
                took = !take.is_empty();
                ixs.extend(take);
            }
            Mode::Making => {
                // Past the inventory limit (after a burst position, say),
                // trade back inside it at once.
                if position.abs() > INVENTORY_LIMIT + 1.0 {
                    let take = self.trade_toward(trader, market, 0.0).await?;
                    took = !take.is_empty();
                    ixs.extend(take);
                }
                let after = if took { 0.0 } else { position };
                let skew = after * SKEW_TICKS;
                let bid_offset = (self.their_bid_offset - 1.0 + skew).max(if skew < 0.0 {
                    1.0
                } else {
                    MIN_OFFSET_TICKS
                });
                let ask_offset = (self.their_ask_offset - 1.0 - skew).max(if skew > 0.0 {
                    1.0
                } else {
                    MIN_OFFSET_TICKS
                });
                let mut bid = (oracle_ticks - bid_offset).floor();
                let mut ask = (oracle_ticks + ask_offset).ceil();
                // Never cross the competing quotes: that would be taking.
                if let Some(best) = best_ask
                    && Some(best) != self.my_ask
                {
                    bid = bid.min(best as f64 - 1.0);
                }
                if let Some(best) = best_bid
                    && Some(best) != self.my_bid
                {
                    ask = ask.max(best as f64 + 1.0);
                }
                if ask <= bid {
                    ask = bid + 1.0;
                }
                if after < INVENTORY_LIMIT && market.steps_left > 0 && signal > -WARY {
                    let size = QUOTE_SIZE.min(market.cap - 1.0 - after);
                    ixs.extend(self.limit(trader, Side::Bid, bid, size).await?);
                    new_bid = Some(bid as u64);
                }
                if after > -INVENTORY_LIMIT && market.steps_left > 0 && signal < WARY {
                    let size = QUOTE_SIZE.min(market.cap - 1.0 + after);
                    ixs.extend(self.limit(trader, Side::Ask, ask, size).await?);
                    new_ask = Some(ask as u64);
                }
            }
        }

        if ixs.is_empty() && !had_quotes {
            self.reference = Some(position);
            return Ok(());
        }
        let mut all = cancel.clone();
        all.extend(ixs);
        match mtx_kit::send(&trader.rpc, &trader.keypair, all, "requote") {
            Ok(signature) => {
                self.mine.insert(signature.to_string());
                self.my_bid = new_bid;
                self.my_ask = new_ask;
            }
            Err(error) => {
                warn!(step = market.k, %error, "requote failed; cancelling");
                if let Err(error) = mtx_kit::send(&trader.rpc, &trader.keypair, cancel, "cancel") {
                    warn!(step = market.k, %error, "cancel failed");
                }
                self.my_bid = None;
                self.my_ask = None;
                took = true;
            }
        }
        self.reference = Some(if took {
            trader.position().await.map_or(position, |p| p.size)
        } else {
            position
        });
        if market.k % 20 == 0 {
            info!(
                step = market.k,
                oracle,
                position,
                pnl = self.pnl,
                drawdown,
                signal,
                bid_off = self.their_bid_offset,
                ask_off = self.their_ask_offset,
                budget_ms = market.budget_left.as_millis() as u64,
                buys = self.seen.0,
                sells = self.seen.1,
                "step"
            );
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}
