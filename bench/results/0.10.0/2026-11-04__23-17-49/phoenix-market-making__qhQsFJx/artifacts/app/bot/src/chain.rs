//! Everything the bot reads from and sends to the chain, over standard RPC
//! methods only.

use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use mtx_botkit::{
    Keypair, Pubkey, RpcClient, Signer, Trader,
    luce::{
        chain::{InstructionGroup, TransactionPlan},
        protocols::phoenix::{COMPUTE_UNIT_LIMIT, PhoenixExchange},
    },
    phoenix_rise::{
        accounts::orderbook::Orderbook,
        core::MarketOrderTicket,
        ix::PhoenixInstruction,
        math::Side,
    },
    rpc_client,
};
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;
use std::sync::Arc;

use crate::Plan;

const PHOENIX: &str = "EtrnLzgbS7nMMy5fbD42kXiUzGg8XQzJ972Xtk1cjWih";

/// A taker's market order, as sent.
#[derive(Debug, Clone, Copy)]
pub struct TakerOrder {
    pub slot: u64,
    /// +1 buy, -1 sell.
    pub side: i8,
    pub size: f64,
    pub limit_ticks: u64,
}

/// The account's position and value at the oracle.
#[derive(Debug, Clone, Copy)]
pub struct Account {
    pub position: f64,
    pub value: f64,
}

/// One of our resting orders.
#[derive(Debug, Clone, Copy)]
pub struct MyOrder {
    pub price: u64,
    pub remaining: u64,
}

/// The FIFO book, split into ours and everyone else's.
#[derive(Debug, Clone, Default)]
pub struct Book {
    pub best_other_bid: Option<u64>,
    pub best_other_ask: Option<u64>,
    pub my_bids: Vec<MyOrder>,
    pub my_asks: Vec<MyOrder>,
}

pub struct Chain {
    rpc: RpcClient,
    rpc_url: String,
    exchange: Arc<PhoenixExchange>,
    keypair: Keypair,
    authority: Pubkey,
    authority_str: String,
    account: Pubkey,
    symbol: String,
    asset_id: u32,
    market: Pubkey,
    pub tick_usd: f64,
    pub lot: f64,
    last_sig: Option<String>,
    market_order_disc: [u8; 8],
    my_trader_id: Option<u32>,
    /// Orders we placed and haven't recognised in the book yet: (side, price, lots).
    pending: Vec<(i8, u64, u64)>,
    order_id: u128,
}

impl Chain {
    pub fn new(trader: &Trader) -> Result<Self> {
        let cfg = trader.phoenix.market(&trader.env.symbol)?;
        let market = Pubkey::from_str(&cfg.market_pubkey)?;
        let tick_usd = trader.phoenix.ticks_to_usd(cfg, 1);
        let lot = 10f64.powi(-(cfg.base_lots_decimals as i32));
        Ok(Self {
            rpc: rpc_client(&trader.env.rpc_url),
            rpc_url: trader.env.rpc_url.clone(),
            exchange: trader.phoenix.exchange().clone(),
            keypair: trader.keypair.insecure_clone(),
            authority: trader.authority(),
            authority_str: trader.authority().to_string(),
            account: trader.account,
            symbol: trader.env.symbol.clone(),
            asset_id: cfg.asset_id,
            market,
            tick_usd,
            lot,
            last_sig: None,
            market_order_disc: PhoenixInstruction::PlaceMarketOrder.discriminant(),
            my_trader_id: None,
            pending: Vec::new(),
            order_id: 1,
        })
    }

    /// Market orders sent by others since the last call, oldest first.
    pub fn new_taker_orders(&mut self) -> Result<Vec<TakerOrder>> {
        let mut cfg = json!({"limit": 1000, "commitment": "confirmed"});
        match &self.last_sig {
            Some(s) => cfg["until"] = json!(s),
            None => cfg["limit"] = json!(1),
        }
        let sigs: Value = self.rpc.send(
            RpcRequest::GetSignaturesForAddress,
            json!([self.market.to_string(), cfg]),
        )?;
        let sigs = sigs.as_array().cloned().unwrap_or_default();
        let first_call = self.last_sig.is_none();
        if let Some(newest) = sigs.first() {
            if let Some(s) = newest["signature"].as_str() {
                self.last_sig = Some(s.to_string());
            }
        }
        if first_call {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in sigs.iter().rev() {
            let Some(sig) = entry["signature"].as_str() else { continue };
            let slot = entry["slot"].as_u64().unwrap_or(0);
            let tx: Value = match self.rpc.send(
                RpcRequest::GetTransaction,
                json!([sig, {"encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed"}]),
            ) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("getTransaction {sig}: {e}");
                    continue;
                }
            };
            let msg = &tx["transaction"]["message"];
            let keys = msg["accountKeys"].as_array().cloned().unwrap_or_default();
            let signer = keys.first().and_then(|k| k.as_str()).unwrap_or("");
            if signer == self.authority_str {
                continue;
            }
            for ix in msg["instructions"].as_array().cloned().unwrap_or_default() {
                let idx = ix["programIdIndex"].as_u64().unwrap_or(u64::MAX) as usize;
                if keys.get(idx).and_then(|k| k.as_str()) != Some(PHOENIX) {
                    continue;
                }
                let data = bs58::decode(ix["data"].as_str().unwrap_or(""))
                    .into_vec()
                    .unwrap_or_default();
                if let Some(o) = self.decode_market_order(&data, slot) {
                    out.push(o);
                }
            }
        }
        Ok(out)
    }

    fn decode_market_order(&self, data: &[u8], slot: u64) -> Option<TakerOrder> {
        // discriminant, packet kind (2 = immediate or cancel), side,
        // Option<u64> price in ticks, u64 base lots.
        if data.len() < 8 + 1 + 1 + 1 + 8 || data[..8] != self.market_order_disc || data[8] != 2 {
            return None;
        }
        let side = if data[9] == 0 { 1 } else { -1 };
        let mut at = 10;
        let limit_ticks = if data[at] == 1 {
            let v = u64::from_le_bytes(data[at + 1..at + 9].try_into().ok()?);
            at += 9;
            v
        } else {
            at += 1;
            0
        };
        let lots = u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?);
        Some(TakerOrder {
            slot,
            side,
            size: lots as f64 * self.lot,
            limit_ticks,
        })
    }

    pub async fn account(&self, oracle: f64) -> Result<Account> {
        let hawkeye = self.exchange.hawkeye(&self.rpc_url, self.authority);
        let asset = hawkeye
            .view_margin_for_asset(self.authority, 0, 0, self.asset_id)
            .await
            .map_err(|e| anyhow!("view_margin_for_asset: {e}"))?
            .value;
        let margin = hawkeye
            .view_margin(self.authority, 0, 0)
            .await
            .map_err(|e| anyhow!("view_margin: {e}"))?
            .value;
        let position = asset.base_lots as f64 * self.lot;
        let mark = asset.mark_price_ticks as f64 * self.tick_usd;
        let value = margin.collateral_quote_lots as f64 / 1e6
            + asset.unrealized_pnl_quote_lots as f64 / 1e6
            + position * (oracle - mark);
        Ok(Account { position, value })
    }

    pub fn book(&mut self) -> Result<Book> {
        let acct = self.rpc.get_account(&self.market).context("orderbook")?;
        // The zero-copy view wants an aligned buffer.
        let mut aligned = vec![0u64; acct.data.len().div_ceil(8)];
        let bytes: &mut [u8] = bytemuck_cast(&mut aligned);
        bytes[..acct.data.len()].copy_from_slice(&acct.data);
        let bytes = &bytes[..acct.data.len()];
        let ob = Orderbook::try_from_account_bytes(bytes).map_err(|e| anyhow!("orderbook: {e:?}"))?;

        let mut book = Book::default();
        // Learn our trader id from an order we placed.
        if self.my_trader_id.is_none() && !self.pending.is_empty() {
            for (side, it) in [(1i8, ob.bid_orders()), (-1i8, ob.ask_orders())] {
                for e in it {
                    let o = e.order();
                    let price = e.price_in_ticks().as_inner();
                    let init = o.initial_trade_size().as_inner();
                    if self
                        .pending
                        .iter()
                        .any(|&(s, p, l)| s == side && p == price && l == init)
                    {
                        self.my_trader_id = o.trader_position_id().trader_id().as_u32_checked();
                        eprintln!("my trader id {:?}", self.my_trader_id);
                        break;
                    }
                }
                if self.my_trader_id.is_some() {
                    break;
                }
            }
        }
        let me = self.my_trader_id;
        for (side, it) in [(1i8, ob.bid_orders()), (-1i8, ob.ask_orders())] {
            for e in it {
                let o = e.order();
                let price = e.price_in_ticks().as_inner();
                let remaining = o.num_base_lots_remaining().as_inner();
                if remaining == 0 {
                    continue;
                }
                let tid = o.trader_position_id().trader_id().as_u32_checked();
                if me.is_some() && tid == me {
                    let mine = MyOrder { price, remaining };
                    if side > 0 {
                        book.my_bids.push(mine);
                    } else {
                        book.my_asks.push(mine);
                    }
                } else if side > 0 {
                    if book.best_other_bid.is_none() {
                        book.best_other_bid = Some(price);
                    }
                } else if book.best_other_ask.is_none() {
                    book.best_other_ask = Some(price);
                }
            }
        }
        Ok(book)
    }

    fn next_id(&mut self) -> u128 {
        self.order_id += 1;
        self.order_id
    }

    /// Send the plan as one transaction: cancels, the take, then quotes.
    pub async fn execute(&mut self, plan: &Plan, book: &Book, lot: f64) -> Result<()> {
        let exchange = self.exchange.clone();
        let tx = exchange.tx();
        let mut ixs = Vec::new();
        let mut bids = Vec::new();
        let mut asks = Vec::new();
        let keep = |want: Option<(u64, u64)>, have: &[MyOrder]| -> bool {
            match want {
                None => have.is_empty(),
                Some((px, lots)) => {
                    have.len() == 1
                        && have[0].price == px
                        && have[0].remaining * 2 >= lots
                        && have[0].remaining <= lots
                }
            }
        };
        // Until we know which orders are ours, cancel everything each time.
        let known = self.my_trader_id.is_some();
        let keep_bid = known && keep(plan.bid, &book.my_bids);
        let keep_ask = known && keep(plan.ask, &book.my_asks);
        if !keep_bid && (!book.my_bids.is_empty() || !known) {
            ixs.extend(tx.build_cancel_up_to(self.authority, self.account, &self.symbol, Side::Bid, None, None)?);
        }
        if !keep_ask && (!book.my_asks.is_empty() || !known) {
            ixs.extend(tx.build_cancel_up_to(self.authority, self.account, &self.symbol, Side::Ask, None, None)?);
        }
        if let Some((side, qty, limit)) = plan.take {
            let lots = (qty / lot).floor() as u64;
            if lots > 0 {
                let ticket = MarketOrderTicket::builder()
                    .authority(self.authority)
                    .trader_account(self.account)
                    .symbol(&self.symbol)
                    .side(if side > 0 { Side::Bid } else { Side::Ask })
                    .price(limit)
                    .num_base_lots(lots)
                    .client_order_id(self.next_id())
                    .build()?;
                ixs.extend(tx.place_market_order(ticket).await?);
            }
        }
        let mut placed = Vec::new();
        if !keep_bid {
            if let Some((px, lots)) = plan.bid {
                bids.push(((px as f64 + 0.25) * self.tick_usd, lots));
                placed.push((1i8, px, lots));
            }
        }
        if !keep_ask {
            if let Some((px, lots)) = plan.ask {
                asks.push(((px as f64 - 0.25) * self.tick_usd, lots));
                placed.push((-1i8, px, lots));
            }
        }
        if !bids.is_empty() || !asks.is_empty() {
            ixs.extend(tx.build_multi_limit_order(
                self.authority,
                self.account,
                &self.symbol,
                &bids,
                &asks,
                true,
            )?);
        }
        if ixs.is_empty() {
            return Ok(());
        }
        let n_cancel_only = plan.take.is_none() && bids.is_empty() && asks.is_empty();
        match self.send(ixs) {
            Ok(()) => {
                if self.my_trader_id.is_none() {
                    self.pending.extend(placed);
                }
                Ok(())
            }
            Err(e) if !n_cancel_only => {
                eprintln!("send failed: {e:#}; cancelling instead");
                let mut ixs = tx.build_cancel_all_orders(self.authority, self.account, &self.symbol)?;
                if ixs.is_empty() {
                    return Err(e);
                }
                ixs.truncate(ixs.len());
                self.send(ixs)
            }
            Err(e) => Err(e),
        }
    }

    fn send(&self, ixs: Vec<solana_transaction::Instruction>) -> Result<()> {
        let plan = TransactionPlan::from_group(&InstructionGroup::new("quote", ixs), COMPUTE_UNIT_LIMIT);
        let blockhash = self.rpc.get_latest_blockhash()?;
        let mut tx = plan.transaction(&self.authority, blockhash);
        tx.sign(&[&self.keypair], blockhash);
        let sim = self.rpc.simulate_transaction(&tx)?.value;
        if let Some(err) = sim.err {
            let logs = sim.logs.unwrap_or_default();
            let tail = logs[logs.len().saturating_sub(6)..].join(" | ");
            return Err(anyhow!("simulation: {err} {tail}"));
        }
        self.rpc.send_and_confirm_transaction(&tx)?;
        Ok(())
    }
}

fn bytemuck_cast(v: &mut [u64]) -> &mut [u8] {
    // SAFETY: u64 has no padding and any byte pattern is a valid u8.
    unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, v.len() * 8) }
}

#[allow(dead_code)]
fn _signer(k: &Keypair) -> Pubkey {
    k.pubkey()
}
