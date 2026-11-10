//! SPCX market-making bot for Phoenix.
//!
//! Each turn: read the oracle, the new fills from the order book's trade
//! buffer, the position and equity, and the FIFO book. Detect informed bursts
//! (large one-sided aggressive flow) and trends; lean into them by crossing;
//! unwind afterwards; otherwise quote both sides one tick inside the
//! competitor, skewed by inventory. Sizes are capped so the position can never
//! exceed the mandate.

use std::collections::{BTreeSet, VecDeque};
use std::str::FromStr;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use mtx_botkit::{BotEnv, Pubkey, Side, Trader, Turns, phoenix_rise};
use phoenix_rise::accounts::orderbook::{ORDERBOOK_HEADER_LEN, Orderbook, OrderbookHeader};
use phoenix_rise::ix::multi_limit_order::MultiLimitOrderParams;
use phoenix_rise::ix::order_packet::CondensedOrder;
use solana_account_decoder_client_types::{UiAccountEncoding, UiDataSliceConfig};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------- parameters

fn envf(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

#[derive(Debug, Clone)]
struct Params {
    quote_size: f64,
    s_min_ticks: i64,
    s_cap_ticks: i64,
    burst_window: u64,
    burst_big: f64,
    burst_n: usize,
    burst_vol: f64,
    burst_other_ratio: f64,
    dd_reduce: f64,
    lean: f64,
    burst_expire: u64,
    jump_bps: f64,
    trend_min_steps: usize,
    trend_on: f64,
    trend_off: f64,
    trend_max: f64,
    trend_chunk: f64,
    limit_extra_bps: f64,
    dd_safe: f64,
    pos_cap: f64,
    pull_big: f64,
    pull_slots: u64,
    edge_start: u64,
    edge_end: u64,
}

impl Params {
    fn load(env: &BotEnv) -> Self {
        Self {
            quote_size: envf("BOT_QUOTE_SIZE", 0.8),
            s_min_ticks: envf("BOT_S_MIN_TICKS", 2.0) as i64,
            s_cap_ticks: envf("BOT_S_CAP_TICKS", 40.0) as i64,
            burst_window: envf("BOT_BURST_WINDOW", 12.0) as u64,
            burst_big: envf("BOT_BURST_BIG", 1.0),
            burst_n: envf("BOT_BURST_N", 4.0) as usize,
            burst_vol: envf("BOT_BURST_VOL", 8.0),
            burst_other_ratio: envf("BOT_BURST_OTHER_RATIO", 0.5),
            dd_reduce: envf("BOT_DD_REDUCE", 100.0),
            lean: envf("BOT_LEAN", 19.0),
            burst_expire: envf("BOT_BURST_EXPIRE", 36.0) as u64,
            jump_bps: envf("BOT_JUMP_BPS", 90.0),
            trend_min_steps: envf("BOT_TREND_MIN_STEPS", 30.0) as usize,
            trend_on: envf("BOT_TREND_ON", 2.5),
            trend_off: envf("BOT_TREND_OFF", 1.5),
            trend_max: envf("BOT_TREND_MAX", 12.0),
            trend_chunk: envf("BOT_TREND_CHUNK", 4.0),
            limit_extra_bps: envf("BOT_LIMIT_EXTRA_BPS", 12.0),
            dd_safe: envf("BOT_DD_SAFE", 150.0),
            pos_cap: envf("BOT_POS_CAP", (env.max_position - 0.5).max(1.0)),
            pull_big: envf("BOT_PULL_BIG", 1.2),
            pull_slots: envf("BOT_PULL_SLOTS", 6.0) as u64,
            edge_start: envf("BOT_EDGE_START", 34.0) as u64,
            edge_end: envf("BOT_EDGE_END", 56.0) as u64,
        }
    }
}

// ---------------------------------------------------------------- chain reads

fn aligned_copy(data: &[u8]) -> Vec<u64> {
    let mut v = vec![0u64; data.len().div_ceil(8)];
    bytemuck::cast_slice_mut::<u64, u8>(&mut v)[..data.len()].copy_from_slice(data);
    v
}

#[derive(Debug, Clone, Copy)]
struct Fill {
    slot: u64,
    seq: u64,
    price: f64,
    /// Signed by the taker: positive = taker bought, negative = taker sold.
    size: f64,
    mine: bool,
    maker: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
struct BookOrder {
    ticks: i64,
    size: f64,
    trader: u32,
}

#[derive(Debug, Default, Clone)]
struct Book {
    bids: Vec<BookOrder>,
    asks: Vec<BookOrder>,
}

struct Chain {
    trader: Trader,
    market_pk: Pubkey,
    tick_usd: f64,
    lot_size: f64,
    perp_asset_map: Pubkey,
    spline_pk: Pubkey,
    gti: Vec<Pubkey>,
    atb: Vec<Pubkey>,
    symbol: String,
    my_account_bytes: [u8; 32],
    last_trade_seq: u64,
    order_seq: std::cell::Cell<u128>,
    asset_id: u64,
    takers: Vec<Pubkey>,
    taker_pos: Vec<Option<i64>>,
    taker_deposit: Vec<u64>,
    /// The competing maker's account among the session traders.
    comp_account: Option<Pubkey>,
    start_slot: u64,
    /// Innermost active spline offsets from their mid, in ticks (bid, ask).
    spline_inner: Option<(i64, i64)>,
}

impl Chain {
    fn new(trader: Trader, env: &BotEnv) -> Result<Self> {
        let cfg = trader.phoenix.market(&env.symbol)?.clone();
        let tick_usd = trader.phoenix.ticks_to_usd(&cfg, 1);
        let lot_size = 10f64.powi(-(cfg.base_lots_decimals as i32));
        let keys = trader.phoenix.metadata().keys().clone();
        let pk = |s: &str| Pubkey::from_str(s).map_err(|e| anyhow!("bad pubkey {s}: {e}"));
        Ok(Self {
            market_pk: pk(&cfg.market_pubkey)?,
            spline_pk: pk(&cfg.spline_pubkey)?,
            perp_asset_map: pk(&keys.perp_asset_map)?,
            gti: keys.global_trader_index.iter().map(|s| pk(s)).collect::<Result<_>>()?,
            atb: keys.active_trader_buffer.iter().map(|s| pk(s)).collect::<Result<_>>()?,
            symbol: env.symbol.clone(),
            my_account_bytes: trader.account.to_bytes(),
            tick_usd,
            lot_size,
            last_trade_seq: 0,
            order_seq: std::cell::Cell::new(1000),
            asset_id: cfg.asset_id as u64,
            takers: vec![],
            taker_pos: vec![],
            taker_deposit: vec![],
            comp_account: None,
            start_slot: 0,
            spline_inner: None,
            trader,
        })
    }

    fn account_data(&self, pk: &Pubkey, slice: Option<(usize, usize)>) -> Result<Vec<u8>> {
        let acc = self
            .trader
            .rpc
            .get_ui_account_with_config(
                pk,
                RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    data_slice: slice.map(|(offset, length)| UiDataSliceConfig { offset, length }),
                    commitment: Some(CommitmentConfig::processed()),
                    min_context_slot: None,
                },
            )?
            .value
            .ok_or_else(|| anyhow!("account {pk} missing"))?;
        acc.data.decode().ok_or_else(|| anyhow!("account {pk} data undecodable"))
    }

    /// New fills since the last call, oldest first.
    fn new_fills(&mut self) -> Result<Vec<Fill>> {
        let data = self.account_data(&self.market_pk.clone(), Some((0, ORDERBOOK_HEADER_LEN)))?;
        if data.len() < ORDERBOOK_HEADER_LEN {
            return Err(anyhow!("short orderbook header"));
        }
        let buf = aligned_copy(&data);
        let bytes: &[u8] = bytemuck::cast_slice(&buf);
        let header: &OrderbookHeader = bytemuck::from_bytes(&bytes[..ORDERBOOK_HEADER_LEN]);
        let mut fills: Vec<Fill> = header
            .trade_list()
            .iter()
            .filter(|e| e.trade_sequence_number > self.last_trade_seq)
            .map(|e| {
                let qty = e.maker_fill.quantity.as_inner() as f64 * self.lot_size;
                Fill {
                    slot: e.slot,
                    seq: e.trade_sequence_number,
                    price: e.maker_fill.price.as_inner() as f64 * self.tick_usd,
                    size: -qty,
                    mine: e.maker == self.my_account_bytes,
                    maker: e.maker,
                }
            })
            .collect();
        fills.sort_by_key(|f| f.seq);
        if let Some(last) = fills.last() {
            self.last_trade_seq = last.seq;
        }
        Ok(fills)
    }

    /// Prime the trade cursor so that fills from before the session are ignored.
    fn prime_trades(&mut self) -> Result<()> {
        let data = self.account_data(&self.market_pk.clone(), Some((0, ORDERBOOK_HEADER_LEN)))?;
        let buf = aligned_copy(&data);
        let bytes: &[u8] = bytemuck::cast_slice(&buf);
        let header: &OrderbookHeader = bytemuck::from_bytes(&bytes[..ORDERBOOK_HEADER_LEN]);
        self.last_trade_seq = header
            .trade_list()
            .iter()
            .map(|e| e.trade_sequence_number)
            .max()
            .unwrap_or(0);
        Ok(())
    }

    fn book(&self) -> Result<Book> {
        let data = self.account_data(&self.market_pk, None)?;
        let buf = aligned_copy(&data);
        let bytes: &[u8] = bytemuck::cast_slice(&buf);
        let book = Orderbook::try_from_account_bytes(&bytes[..data.len()])
            .map_err(|e| anyhow!("book decode: {e}"))?;
        let conv = |e: phoenix_rise::accounts::orderbook::OrderbookEntryRef<'_>| BookOrder {
            ticks: e.price_in_ticks().as_inner() as i64,
            size: e.num_base_lots_remaining().as_inner() as f64 * self.lot_size,
            trader: e.order().trader_position_id().trader_id().to_u32_or_sentinel(),
        };
        Ok(Book {
            bids: book.bid_orders().take(12).map(conv).collect(),
            asks: book.ask_orders().take(12).map(conv).collect(),
        })
    }

    /// Find the session's taker accounts: Phoenix trader accounts funded
    /// during the session's setup (everything but ours). Their position
    /// changes are the taker flow, including fills the FIFO trade buffer
    /// never records (those against the splines).
    fn discover_takers(&mut self) -> Result<()> {
        use phoenix_rise::accounts::PhoenixAccount;
        use phoenix_rise::accounts::trader::Trader as TraderAccount;
        let program_id = *phoenix_rise::ix::PHOENIX_PROGRAM_ID;
        let disc = PhoenixAccount::Trader.discriminant().to_vec();
        let accounts = self.trader.rpc.get_program_ui_accounts_with_config(
            &program_id,
            RpcProgramAccountsConfig {
                filters: Some(vec![RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, disc))]),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    data_slice: None,
                    commitment: Some(CommitmentConfig::processed()),
                    min_context_slot: None,
                },
                with_context: None,
                sort_results: None,
            },
        )?;
        let mut takers = vec![];
        let mut deposits = vec![];
        let mut total = 0usize;
        for (pk, acc) in &accounts {
            total += 1;
            let Some(data) = acc.data.decode() else { continue };
            let buf = aligned_copy(&data);
            let bytes: &[u8] = bytemuck::cast_slice(&buf);
            let Ok(t) = TraderAccount::try_from_account_bytes(&bytes[..data.len()]) else { continue };
            let deposit = t.header().last_deposit_slot();
            if deposit + 600 >= self.start_slot && *pk != self.trader.account {
                takers.push(*pk);
                deposits.push(deposit);
                info!(%pk, deposit, len = data.len(), "session trader");
            }
        }
        info!(total, takers = takers.len(), "trader accounts");
        if takers != self.takers {
            self.takers = takers;
            self.taker_pos = vec![None; self.takers.len()];
            self.taker_deposit = deposits;
            // The competing maker is funded last in the session's setup; a
            // fill naming it as maker confirms or corrects this.
            if let Some((i, _)) = self.taker_deposit.iter().enumerate().max_by_key(|(_, d)| **d) {
                self.comp_account = Some(self.takers[i]);
                info!(comp = %self.takers[i], "competitor guess");
            }
        }
        Ok(())
    }

    /// The innermost offsets at which Phoenix's own makers (the splines)
    /// quote, in ticks from their mid: nobody fills beyond them, so our
    /// quotes shouldn't sit there either.
    fn read_spline_inner(&mut self) -> Result<()> {
        use phoenix_rise::accounts::spline_collection::SplineCollection;
        let data = self.account_data(&self.spline_pk, None)?;
        let buf = aligned_copy(&data);
        let bytes: &[u8] = bytemuck::cast_slice(&buf);
        let coll = SplineCollection::try_from_account_bytes(&bytes[..data.len()]).map_err(|e| anyhow!("spline decode: {e}"))?;
        let mut bid = i64::MAX;
        let mut ask = i64::MAX;
        for sp in coll.iter().flatten() {
            if !sp.is_active() || sp.mid_price().as_inner() == 0 {
                continue;
            }
            for r in sp.active_bid_regions() {
                if r.total_size().as_inner() >= 500 && r.density().as_inner() > 0 {
                    bid = bid.min(r.start_offset().as_inner() as i64);
                }
            }
            for r in sp.active_ask_regions() {
                if r.total_size().as_inner() >= 500 && r.density().as_inner() > 0 {
                    ask = ask.min(r.start_offset().as_inner() as i64);
                }
            }
        }
        if bid != i64::MAX && ask != i64::MAX {
            if self.spline_inner != Some((bid, ask)) {
                info!(bid, ask, "spline inner offsets");
            }
            self.spline_inner = Some((bid, ask));
        }
        Ok(())
    }

    /// Position changes of the session's other traders since the last call,
    /// in base units, one entry per account that moved.
    fn taker_flow(&mut self) -> Result<Vec<f64>> {
        use phoenix_rise::accounts::trader::Trader as TraderAccount;
        if self.takers.is_empty() {
            return Ok(vec![]);
        }
        let accounts = self.trader.rpc.get_multiple_accounts(&self.takers)?;
        let mut out = vec![];
        for (i, acc) in accounts.iter().enumerate() {
            let Some(acc) = acc else { continue };
            let buf = aligned_copy(&acc.data);
            let bytes: &[u8] = bytemuck::cast_slice(&buf);
            let Ok(t) = TraderAccount::try_from_account_bytes(&bytes[..acc.data.len()]) else { continue };
            let pos = t
                .positions()
                .find(|(asset, _)| *asset == self.asset_id)
                .map(|(_, p)| p.base_lot_position().as_inner())
                .unwrap_or(0);
            if let Some(prev) = self.taker_pos[i]
                && pos != prev
                && Some(self.takers[i]) != self.comp_account
            {
                out.push((pos - prev) as f64 * self.lot_size);
            }
            self.taker_pos[i] = Some(pos);
        }
        Ok(out)
    }

    async fn oracle(&self) -> Result<f64> {
        Ok(self.trader.oracle_usd().await?)
    }

    async fn position(&self) -> Result<f64> {
        Ok(self.trader.position().await?.size)
    }

    /// Equity in USD at Phoenix's mark: collateral plus unrealized PnL.
    async fn equity(&self) -> Result<(f64, bool)> {
        let m = self
            .trader
            .phoenix
            .hawkeye(self.trader.authority())
            .view_margin(self.trader.authority(), 0, 0)
            .await
            .map_err(|e| anyhow!("view_margin: {e}"))?
            .value;
        Ok((
            (m.collateral_quote_lots + m.unrealized_pnl_quote_lots) as f64 / 1e6,
            m.is_liquidatable != 0,
        ))
    }

    fn lots(&self, size: f64) -> u64 {
        (size / self.lot_size).round() as u64
    }

    /// Cancel everything and place the given quotes, in one transaction.
    fn requote(&self, bids: &[(i64, f64)], asks: &[(i64, f64)]) -> Result<()> {
        let tx = self.trader.phoenix.tx();
        let mut ixs = tx.build_cancel_all_orders(self.trader.authority(), self.trader.account, &self.symbol)?;
        let to_orders = |levels: &[(i64, f64)]| -> Vec<CondensedOrder> {
            levels
                .iter()
                .filter(|(t, s)| *t > 0 && self.lots(*s) > 0)
                .map(|(t, s)| CondensedOrder {
                    price_in_ticks: *t as u64,
                    size_in_base_lots: self.lots(*s),
                    last_valid_slot: None,
                })
                .collect()
        };
        let b = to_orders(bids);
        let a = to_orders(asks);
        if !b.is_empty() || !a.is_empty() {
            let params = MultiLimitOrderParams::builder()
                .trader(self.trader.authority())
                .trader_account(self.trader.account)
                .perp_asset_map(self.perp_asset_map)
                .orderbook(self.market_pk)
                .spline_collection(self.spline_pk)
                .global_trader_index(self.gti.clone())
                .active_trader_buffer(self.atb.clone())
                .bids(b)
                .asks(a)
                .slide(true)
                .symbol(self.symbol.clone())
                .build()?;
            ixs.extend(tx.build_multi_limit_order_with_params(params)?);
        }
        mtx_botkit::send(&self.trader.rpc, &self.trader.keypair, ixs, "requote")?;
        Ok(())
    }

    /// Cancel resting orders and take `size` at no worse than `limit_usd`,
    /// in one transaction, so our own quotes never block the order.
    async fn market(&self, side: Side, size: f64, limit_usd: f64) -> Result<()> {
        use phoenix_rise::core::MarketOrderTicket;
        use phoenix_rise::ix::types::SelfTradeBehavior;
        let tx = self.trader.phoenix.tx();
        let mut ixs = tx.build_cancel_all_orders(self.trader.authority(), self.trader.account, &self.symbol)?;
        let limit_ticks = match side {
            Side::Bid => (limit_usd / self.tick_usd).floor(),
            Side::Ask => (limit_usd / self.tick_usd).ceil(),
        };
        let ticket = MarketOrderTicket::builder()
            .authority(self.trader.authority())
            .trader_account(self.trader.account)
            .symbol(self.symbol.clone())
            .side(side)
            .price(limit_ticks * self.tick_usd)
            .num_base_lots(self.lots(size))
            .self_trade_behavior(SelfTradeBehavior::CancelProvide)
            .client_order_id(self.next_order_id())
            .build()?;
        ixs.extend(tx.place_market_order(ticket).await?);
        mtx_botkit::send(&self.trader.rpc, &self.trader.keypair, ixs, "take")?;
        Ok(())
    }

    fn next_order_id(&self) -> u128 {
        self.order_seq.set(self.order_seq.get() + 1);
        self.order_seq.get()
    }
}

// ---------------------------------------------------------------- signals

#[derive(Debug, Clone, Copy)]
struct Order {
    slot: u64,
    /// +1 buy, -1 sell (taker side).
    side: i32,
    size: f64,
    /// Distance from the oracle, in bps, of the worst fill price.
    bps: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Quote,
    /// Leaning into a suspected informed burst.
    Burst { side: i32, start: u64 },
}

struct Signals {
    p: Params,
    start_slot: u64,
    end_slot: u64,
    oracle: f64,
    last_oracle: f64,
    step_moves: Vec<f64>,
    orders: VecDeque<Order>,
    n_buy: f64,
    n_sell: f64,
    v_buy: f64,
    v_sell: f64,
    mode: Mode,
    trend_target: f64,
    comp_bps: f64,
    last_jump_slot: u64,
    jumps: Vec<(u64, f64)>,
    pull_bid_until: u64,
    pull_ask_until: u64,
    /// Orders before this slot can't start a burst: a jump just happened
    /// (the next is at least 60 slots away) or a burst just ended.
    ignore_before: u64,
}

impl Signals {
    fn new(p: Params, start_slot: u64, end_slot: u64, oracle: f64) -> Self {
        Self {
            p,
            start_slot,
            end_slot,
            oracle,
            last_oracle: oracle,
            step_moves: vec![],
            orders: VecDeque::new(),
            n_buy: 0.0,
            n_sell: 0.0,
            v_buy: 0.0,
            v_sell: 0.0,
            mode: Mode::Quote,
            trend_target: 0.0,
            comp_bps: 20.0,
            last_jump_slot: 0,
            jumps: vec![],
            pull_bid_until: 0,
            pull_ask_until: 0,
            ignore_before: 0,
        }
    }

    fn on_price(&mut self, oracle: f64, slot: u64) -> Option<f64> {
        self.last_oracle = self.oracle;
        self.oracle = oracle;
        let bps = (oracle / self.last_oracle - 1.0) * 1e4;
        if bps.abs() >= self.p.jump_bps {
            self.last_jump_slot = slot;
            self.jumps.push((slot, bps));
            self.ignore_before = self.ignore_before.max(slot + 30);
            Some(bps)
        } else {
            self.step_moves.push(bps);
            None
        }
    }

    /// Fills against the competitor tell where it quotes.
    fn note_quotes(&mut self, fills: &[Fill]) {
        for f in fills.iter().filter(|f| !f.mine) {
            let bps = ((f.price / self.oracle - 1.0) * 1e4).abs();
            if bps > 5.0 && bps < 80.0 {
                self.comp_bps = self.comp_bps.max(bps.min(35.0));
            }
        }
    }

    fn on_fills(&mut self, fills: &[Fill], slot: u64) {
        // Aggregate fills by (slot, side) into taker orders.
        for f in fills {
            let side = if f.size > 0.0 { 1 } else { -1 };
            let bps = ((f.price / self.oracle - 1.0) * 1e4).abs();
            if let Some(last) = self.orders.back_mut()
                && last.slot == f.slot
                && last.side == side
            {
                last.size += f.size.abs();
                last.bps = last.bps.max(bps);
            } else {
                self.orders.push_back(Order { slot: f.slot, side, size: f.size.abs(), bps });
            }
            if side > 0 {
                self.n_buy += 1.0;
                self.v_buy += f.size.abs();
            } else {
                self.n_sell += 1.0;
                self.v_sell += f.size.abs();
            }
        }
        while let Some(o) = self.orders.front() {
            if o.slot + 80 < slot {
                self.orders.pop_front();
            } else {
                break;
            }
        }
        // A single large aggressive order: pull the quote it would hit next.
        for o in self.orders.iter().rev() {
            if o.slot + self.p.pull_slots < slot {
                break;
            }
            if o.size >= self.p.pull_big {
                if o.side > 0 {
                    self.pull_ask_until = self.pull_ask_until.max(o.slot + self.p.pull_slots);
                } else {
                    self.pull_bid_until = self.pull_bid_until.max(o.slot + self.p.pull_slots);
                }
            }
        }
    }

    fn rel(&self, slot: u64) -> u64 {
        slot.saturating_sub(self.start_slot)
    }

    fn jumps_possible(&self, slot: u64) -> bool {
        let rel = self.rel(slot);
        rel >= self.p.edge_start && slot + self.p.edge_end <= self.end_slot
    }

    /// Burst detection over the trailing window: (side, first slot, volume, count).
    fn burst_candidate(&self, slot: u64, since: u64) -> Option<(i32, u64, f64, usize)> {
        let mut best = None;
        for side in [1i32, -1] {
            let mut n = 0usize;
            let mut v = 0.0;
            let mut other = 0.0;
            let mut first = u64::MAX;
            for o in self.orders.iter() {
                if o.slot + self.p.burst_window < slot || o.slot < since {
                    continue;
                }
                if o.side != side {
                    other += o.size;
                    continue;
                }
                if o.size >= self.p.burst_big {
                    n += 1;
                    v += o.size;
                    first = first.min(o.slot);
                }
            }
            if n >= self.p.burst_n && v >= self.p.burst_vol && other <= self.p.burst_other_ratio * v {
                let cand = (side, first, v, n);
                if best.is_none_or(|(_, _, bv, _)| v > bv) {
                    best = Some(cand);
                }
            }
        }
        best
    }

    fn update_trend(&mut self) {
        let n = self.step_moves.len();
        if n < self.p.trend_min_steps {
            return;
        }
        let mean = self.step_moves.iter().sum::<f64>() / n as f64;
        let var = self.step_moves.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
        let sd = var.sqrt().max(0.3);
        let t = mean * (n as f64).sqrt() / sd;
        let nf = self.n_buy + self.n_sell;
        let zf = if nf > 0.0 { (self.n_buy - self.n_sell) / nf.sqrt() } else { 0.0 };
        let score = t + 0.8 * zf;
        let want = if score.abs() >= self.p.trend_on {
            score.signum() * (4.0 * (score.abs() - 1.6)).clamp(0.0, self.p.trend_max)
        } else if self.trend_target != 0.0 && score.abs() >= self.p.trend_off {
            self.trend_target.signum() * (4.0 * (score.abs() - 1.6)).clamp(2.0, self.p.trend_max)
        } else {
            0.0
        };
        // Move the target gradually to avoid churn.
        let step = 2.0;
        if (want - self.trend_target).abs() <= step {
            self.trend_target = want;
        } else {
            self.trend_target += step * (want - self.trend_target).signum();
        }
        self.trend_target = (self.trend_target * 2.0).round() / 2.0;
        if n % 25 == 0 {
            info!(n, mean = format!("{mean:.3}"), sd = format!("{sd:.3}"), t = format!("{t:.2}"), zf = format!("{zf:.2}"), score = format!("{score:.2}"), trend_target = self.trend_target, "trend");
        }
    }
}

// ---------------------------------------------------------------- bot

struct Bot {
    chain: Chain,
    sig: Signals,
    p: Params,
    my_id: Option<u32>,
    known_ids: BTreeSet<u32>,
    last_quotes: Option<(Vec<(i64, f64)>, Vec<(i64, f64)>)>,
    fills_since_quote: bool,
    peak_equity: f64,
    safe_mode: bool,
    equity: f64,
    oracle_ticks: i64,
}

impl Bot {
    fn learn_my_id(&mut self, book: &Book) {
        if self.my_id.is_some() {
            return;
        }
        let all = book.bids.iter().chain(book.asks.iter());
        if self.last_quotes.is_none() {
            for o in all {
                self.known_ids.insert(o.trader);
            }
            return;
        }
        let (lb, la) = self.last_quotes.as_ref().unwrap();
        for o in all {
            if self.known_ids.contains(&o.trader) {
                continue;
            }
            let at_mine = lb.iter().chain(la.iter()).any(|(t, sz)| *t == o.ticks && (o.size - *sz).abs() < 1e-6);
            if at_mine {
                self.my_id = Some(o.trader);
                info!(id = o.trader, "learned my trader id");
                return;
            }
        }
    }

    fn is_mine(&self, o: &BookOrder, side_bid: bool) -> bool {
        if let Some(id) = self.my_id {
            return o.trader == id;
        }
        match &self.last_quotes {
            Some((lb, la)) => {
                let mine = if side_bid { lb } else { la };
                mine.iter().any(|(t, sz)| *t == o.ticks && (o.size - *sz).abs() < 1e-6)
            }
            None => false,
        }
    }

    fn comp_best(&self, book: &Book) -> (Option<BookOrder>, Option<BookOrder>) {
        let cb = book.bids.iter().find(|o| !self.is_mine(o, true)).copied();
        let ca = book.asks.iter().find(|o| !self.is_mine(o, false)).copied();
        (cb, ca)
    }

    async fn turn(&mut self, step: mtx_botkit::Step) -> Result<()> {
        let t0 = Instant::now();
        let slot = step.slot;
        if step.k % 25 == 24 {
            if let Err(e) = self.chain.read_spline_inner() {
                warn!(err = %e, "spline read failed");
            }
        }
        let oracle = self.chain.oracle().await?;
        let fills = self.chain.new_fills()?;
        // A fill naming a session trader as maker identifies the competitor.
        for f in fills.iter().filter(|f| !f.mine) {
            let maker = Pubkey::new_from_array(f.maker);
            if self.chain.takers.contains(&maker) && self.chain.comp_account != Some(maker) {
                info!(comp = %maker, "competitor identified");
                self.chain.comp_account = Some(maker);
            }
        }
        // Taker orders come from the taker accounts' position changes: exact,
        // per account, and including fills against the splines, which the
        // trade buffer never records. Without tracked accounts, fall back to
        // the trade buffer.
        let deltas = self.chain.taker_flow().unwrap_or_else(|e| {
            warn!(err = %e, "taker flow failed");
            vec![]
        });
        let mut orders: Vec<Fill> = if self.chain.takers.is_empty() {
            fills.clone()
        } else {
            deltas
                .iter()
                .map(|d| Fill { slot, seq: 0, price: oracle * (1.0 + d.signum() * self.sig.comp_bps / 1e4), size: *d, mine: false, maker: [0; 32] })
                .collect()
        };
        orders.retain(|o| o.size.abs() >= 0.001);
        if !deltas.is_empty() {
            info!(slot, ?deltas, "taker flow");
        }
        let pos = self.chain.position().await?;
        let (equity, liquidatable) = self.chain.equity().await.unwrap_or((self.equity, false));
        let book = self.chain.book().unwrap_or_default();
        let t_read = t0.elapsed().as_millis();

        self.equity = equity;
        self.peak_equity = self.peak_equity.max(equity);
        let drawdown = self.peak_equity - equity;
        if drawdown > self.p.dd_safe && !self.safe_mode {
            warn!(drawdown, "entering safe mode");
            self.safe_mode = true;
        }
        self.oracle_ticks = (oracle / self.chain.tick_usd).round() as i64;
        self.learn_my_id(&book);
        let (cb, ca) = self.comp_best(&book);

        // ---- signals
        let jump = self.sig.on_price(oracle, slot);
        if fills.iter().any(|f| f.mine) {
            self.fills_since_quote = true;
        }
        for f in &fills {
            info!(slot = f.slot, px = f.price, size = f.size, mine = f.mine, bps = format!("{:.1}", (f.price / oracle - 1.0) * 1e4), "fill");
        }
        self.sig.note_quotes(&fills);
        self.sig.on_fills(&orders, slot);
        self.sig.update_trend();
        if let Some(bps) = jump {
            info!(slot, bps = format!("{bps:.0}"), mode = ?self.sig.mode, pos, "jump");
        }

        // ---- mode transitions
        let mut mode = self.sig.mode;
        match mode {
            Mode::Burst { start, side } => {
                // A fresh qualifying burst on the same side (e.g. the real
                // informed flow after a fake-out) restarts the clock.
                let mut start = start;
                if let Some((s2, first, v, n)) = self.sig.burst_candidate(slot, start + 1)
                    && s2 == side
                    && first > start
                {
                    info!(first_rel = self.sig.rel(first), v, n, "burst renewed");
                    start = first;
                    mode = Mode::Burst { side, start };
                }
                let deadline = start + self.p.burst_expire;
                if jump.is_some() {
                    info!("burst resolved by jump");
                    mode = Mode::Quote;
                    self.sig.ignore_before = self.sig.ignore_before.max(slot + 1);
                } else if slot > deadline {
                    info!("burst expired without a jump");
                    mode = Mode::Quote;
                    self.sig.ignore_before = self.sig.ignore_before.max(slot + 1);
                }
            }
            Mode::Quote => {}
        }
        if !matches!(mode, Mode::Burst { .. }) && !self.safe_mode && self.sig.jumps_possible(slot) {
            let since = self.sig.ignore_before;
            if let Some((side, first, v, n)) = self.sig.burst_candidate(slot, since) {
                info!(side, first_rel = self.sig.rel(first), v, n, pos, "burst detected");
                mode = Mode::Burst { side, start: first };
            }
        }
        self.sig.mode = mode;

        // ---- target position
        let cap = self.p.pos_cap;
        let target = if self.safe_mode {
            0.0
        } else {
            match mode {
                Mode::Burst { side, .. } => {
                    let lean = if drawdown > self.p.dd_reduce { self.p.lean * 0.45 } else { self.p.lean };
                    side as f64 * lean.min(cap)
                }
                Mode::Quote => self.sig.trend_target.clamp(-cap, cap),
            }
        };

        // ---- market orders: lean into a burst, follow a trend target, or cut
        // risk. Leftover inventory after a burst is worked out passively.
        let allowed = if self.safe_mode {
            0.0
        } else if drawdown > self.p.dd_reduce {
            self.p.lean * 0.45
        } else {
            cap
        };
        let mut pos_now = pos;
        let mut order: Option<(Side, f64)> = None;
        match mode {
            Mode::Burst { side, .. } => {
                let need = target - pos_now;
                if need * side as f64 > 0.5 {
                    order = Some((if need > 0.0 { Side::Bid } else { Side::Ask }, need.abs()));
                }
            }
            Mode::Quote => {
                if pos_now.abs() > allowed + 0.3 {
                    // Risk cut: all of the excess at once.
                    let excess = pos_now.abs() - allowed;
                    order = Some((if pos_now > 0.0 { Side::Ask } else { Side::Bid }, excess));
                } else if target != 0.0 && (target - pos_now).abs() >= 2.0 {
                    let diff = target - pos_now;
                    order = Some((if diff > 0.0 { Side::Bid } else { Side::Ask }, diff.abs().min(self.p.trend_chunk)));
                }
            }
        }
        let mut t_mkt = 0u128;
        if let Some((side, size)) = order {
            let buying = matches!(side, Side::Bid);
            let room = if buying { cap - pos_now } else { cap + pos_now };
            let size = size.min(room).max(0.0);
            let size = (size / self.chain.lot_size).floor() * self.chain.lot_size;
            if size >= 0.1 {
                let spline_bps = self.chain.spline_inner.map(|(b, a)| (if buying { a } else { b }) as f64 * self.chain.tick_usd / oracle * 1e4).unwrap_or(0.0);
                let extra = (self.sig.comp_bps.max(spline_bps) + self.p.limit_extra_bps) / 1e4;
                let limit = if buying { oracle * (1.0 + extra) } else { oracle * (1.0 - extra) };
                let tm = Instant::now();
                match self.chain.market(side, size, limit).await {
                    Ok(()) => {
                        let after = self.chain.position().await.unwrap_or(pos_now + if buying { size } else { -size });
                        info!(?side, size, limit = format!("{limit:.2}"), before = pos_now, after, ?mode, "market order");
                        pos_now = after;
                        self.last_quotes = None;
                    }
                    Err(e) => {
                        warn!(?side, size, limit = format!("{limit:.3}"), err = ?e, "market order failed");
                        self.last_quotes = None;
                    }
                }
                t_mkt = tm.elapsed().as_millis();
            }
        }

        // ---- quotes
        let (bids, asks) = self.quotes(pos_now, target, mode, cb, ca, slot);
        let want = (bids.clone(), asks.clone());
        let must = self.last_quotes.as_ref() != Some(&want) || self.fills_since_quote;
        let mut t_q = 0u128;
        if must {
            let tq = Instant::now();
            match self.chain.requote(&bids, &asks) {
                Ok(()) => {
                    self.last_quotes = Some(want);
                    self.fills_since_quote = false;
                }
                Err(e) => {
                    warn!(err = %e, ?bids, ?asks, "requote failed");
                    self.last_quotes = None;
                }
            }
            t_q = tq.elapsed().as_millis();
        }

        info!(
            k = step.k,
            rel = self.sig.rel(slot),
            oracle,
            pos = pos_now,
            target,
            eq = format!("{equity:.2}"),
            dd = format!("{drawdown:.1}"),
            ?mode,
            cb = ?cb.map(|o| (o.ticks - self.oracle_ticks, o.size)),
            ca = ?ca.map(|o| (o.ticks - self.oracle_ticks, o.size)),
            bids = ?bids.iter().map(|(t, s)| (t - self.oracle_ticks, *s)).collect::<Vec<_>>(),
            asks = ?asks.iter().map(|(t, s)| (t - self.oracle_ticks, *s)).collect::<Vec<_>>(),
            requoted = must,
            ms = t0.elapsed().as_millis() as u64,
            ms_read = t_read as u64,
            ms_mkt = t_mkt as u64,
            ms_q = t_q as u64,
            liq = liquidatable,
            "turn"
        );
        Ok(())
    }

    /// Quotes as (ticks, size) per side.
    fn quotes(
        &self,
        pos: f64,
        target: f64,
        mode: Mode,
        cb: Option<BookOrder>,
        ca: Option<BookOrder>,
        slot: u64,
    ) -> (Vec<(i64, f64)>, Vec<(i64, f64)>) {
        let p = &self.p;
        let ot = self.oracle_ticks;
        let comp_ticks = (self.sig.comp_bps / 1e4 * self.sig.oracle / self.chain.tick_usd).round() as i64;
        let (spline_bid, spline_ask) = self.chain.spline_inner.unwrap_or((p.s_cap_ticks, p.s_cap_ticks));
        let dist = |o: Option<BookOrder>, sign: i64| -> i64 {
            let cap = if sign < 0 { spline_bid } else { spline_ask }.clamp(2, p.s_cap_ticks);
            match o {
                Some(o) => ((o.ticks - ot) * sign).clamp(1, cap),
                None => cap,
            }
        };
        // One tick inside the competitor, never tighter than s_min.
        let mut s_bid = (dist(cb, -1) - 1).max(p.s_min_ticks);
        let mut s_ask = (dist(ca, 1) - 1).max(p.s_min_ticks);
        let mut q_bid = p.quote_size;
        let mut q_ask = p.quote_size;

        // Inventory skew relative to the target.
        let e = pos - target;
        if e > 0.5 {
            s_ask = (s_ask - (e * 0.5).round() as i64).max(1);
            s_bid += (e * 1.5).round() as i64;
            q_ask = (p.quote_size + e * 0.3).min(3.0);
            q_bid = (p.quote_size - e * 0.1).max(0.0);
        } else if e < -0.5 {
            let e = -e;
            s_bid = (s_bid - (e * 0.5).round() as i64).max(1);
            s_ask += (e * 1.5).round() as i64;
            q_bid = (p.quote_size + e * 0.3).min(3.0);
            q_ask = (p.quote_size - e * 0.1).max(0.0);
        }

        match mode {
            Mode::Burst { side, .. } => {
                // Hold the lean: don't offer against the expected move.
                if side > 0 {
                    q_ask = 0.0;
                    q_bid = if p.pos_cap - pos >= 0.6 { p.quote_size } else { 0.0 };
                    s_bid = (dist(cb, -1) - 1).max(p.s_min_ticks);
                } else {
                    q_bid = 0.0;
                    q_ask = if p.pos_cap + pos >= 0.6 { p.quote_size } else { 0.0 };
                    s_ask = (dist(ca, 1) - 1).max(p.s_min_ticks);
                }
            }
            Mode::Quote => {
                if e.abs() > 3.0 {
                    // Working leftover inventory out: reduce at the oracle,
                    // in size, and don't add.
                    if e > 0.0 {
                        s_ask = 1;
                        q_ask = e.min(4.0);
                        q_bid = 0.0;
                    } else {
                        s_bid = 1;
                        q_bid = (-e).min(4.0);
                        q_ask = 0.0;
                    }
                }
                if e.abs() <= 3.0 {
                    if target > 0.0 {
                        // Leaning long with the trend: sell only at a wide price.
                        s_ask = s_ask.max(dist(ca, 1) - 1).max(comp_ticks - 1);
                    } else if target < 0.0 {
                        s_bid = s_bid.max(dist(cb, -1) - 1).max(comp_ticks - 1);
                    }
                }
            }
        }

        // Pull a side a large aggressive order is likely to hit again.
        if slot <= self.sig.pull_ask_until && e <= 0.5 {
            q_ask = 0.0;
        }
        if slot <= self.sig.pull_bid_until && e >= -0.5 {
            q_bid = 0.0;
        }
        if self.safe_mode {
            if pos > 0.0 { q_bid = 0.0 } else { q_ask = 0.0 }
        }

        // Position cap: no fill may take us past it.
        let cap = p.pos_cap;
        q_bid = q_bid.min((cap - pos).max(0.0));
        q_ask = q_ask.min((cap + pos).max(0.0));
        let round = |x: f64| (x / self.chain.lot_size).floor() * self.chain.lot_size;
        q_bid = round(q_bid);
        q_ask = round(q_ask);

        let mut bids = vec![];
        let mut asks = vec![];
        if q_bid >= 0.05 {
            bids.push((ot - s_bid, q_bid));
        }
        if q_ask >= 0.05 {
            asks.push((ot + s_ask, q_ask));
        }
        (bids, asks)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let p = Params::load(&env);
    let hello = turns.hello().clone();
    info!(authority = %trader.authority(), account = %trader.account, ?hello, ?p, "connected");
    let mut chain = Chain::new(trader, &env).context("chain setup")?;
    let oracle = chain.oracle().await.unwrap_or(0.0);
    if let Err(e) = chain.prime_trades() {
        warn!(err = %e, "priming trades failed");
    }
    chain.start_slot = hello.start_slot;
    if let Err(e) = chain.discover_takers() {
        warn!(err = %e, "taker discovery failed");
    }
    let _ = chain.taker_flow();
    if let Err(e) = chain.read_spline_inner() {
        warn!(err = %e, "spline read failed");
    }
    let (equity, _) = chain.equity().await.unwrap_or((0.0, false));
    let mut bot = Bot {
        sig: Signals::new(p.clone(), hello.start_slot, hello.end_slot, oracle.max(1e-9)),
        p,
        chain,
        my_id: None,
        known_ids: BTreeSet::new(),
        last_quotes: None,
        fills_since_quote: false,
        peak_equity: equity,
        equity,
        safe_mode: false,
        oracle_ticks: 0,
    };
    while let Some(step) = turns.next()? {
        if let Err(e) = bot.turn(step).await {
            warn!(k = step.k, err = %e, "turn failed");
        }
    }
    info!("session ended");
    Ok(())
}
