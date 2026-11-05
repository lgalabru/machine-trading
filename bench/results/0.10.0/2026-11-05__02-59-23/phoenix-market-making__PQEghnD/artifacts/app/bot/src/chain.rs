//! Chain reads: the FIFO order book (competitor quotes, our orders, recent
//! trades), the oracle, and our position.

use std::str::FromStr;

use anyhow::{Context, Result};
use mtx_botkit::{Trader, phoenix_rise};
use phoenix_rise::accounts::orderbook::Orderbook;
use phoenix_rise::accounts::spline_collection::SplineCollection;
use phoenix_rise::accounts::trader::Trader as TraderAccount;
#[allow(unused_imports)]
use phoenix_rise::math::WrapperNum;
use solana_pubkey::Pubkey;

#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub ticks: u64,
    pub lots: u64,
    /// Number of distinct trader ids resting at this price (excluding us).
    pub traders: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct MyOrder {
    pub ticks: u64,
    pub lots: u64,
    pub initial_lots: u64,
    pub seq: u64,
    pub slot: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Trade {
    pub seq: u64,
    pub slot: u64,
    pub ticks: u64,
    /// Signed base lots from the maker's side (sign as Phoenix stores it).
    pub maker_lots: i64,
    pub maker: Pubkey,
}

#[derive(Debug, Clone, Copy)]
pub struct RawOrder {
    pub ticks: u64,
    pub lots: u64,
    pub initial_lots: u64,
    pub seq: u64,
    pub slot: u64,
    pub trader_id: u32,
}

#[derive(Debug, Default)]
pub struct BookView {
    /// Resting bids that are not ours, best first, aggregated per price.
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub my_bids: Vec<MyOrder>,
    pub my_asks: Vec<MyOrder>,
    pub raw_bids: Vec<RawOrder>,
    pub raw_asks: Vec<RawOrder>,
    /// Trades in the ring buffer, oldest first.
    pub trades: Vec<Trade>,
    pub order_seq: u64,
    pub trade_seq: u64,
}

/// Cumulative spline fills, in base lots, summed over the market's splines.
#[derive(Debug, Clone, Copy, Default)]
pub struct SplineFills {
    pub bid_filled: u64,
    pub ask_filled: u64,
    pub active: u32,
    pub mid_ticks: u64,
}

pub struct Chain {
    pub orderbook: Pubkey,
    pub splines: Pubkey,
    pub trader_account: Pubkey,
    pub asset_id: u32,
    /// Our trader id in the book's node space, once learned.
    pub my_trader_id: Option<u32>,
}

/// Copies account bytes into a 16-byte aligned buffer for zero-copy views.
fn aligned(data: &[u8]) -> Vec<u128> {
    let words = data.len().div_ceil(16);
    let mut buf: Vec<u128> = vec![0; words];
    bytemuck::cast_slice_mut::<u128, u8>(&mut buf)[..data.len()].copy_from_slice(data);
    buf
}

impl Chain {
    pub fn new(trader: &Trader) -> Result<Self> {
        let market = trader.phoenix.market(&trader.env.symbol)?;
        Ok(Self {
            orderbook: Pubkey::from_str(&market.market_pubkey)?,
            splines: Pubkey::from_str(&market.spline_pubkey)?,
            trader_account: trader.account,
            asset_id: market.asset_id,
            my_trader_id: None,
        })
    }

    pub fn read_book(&self, trader: &Trader) -> Result<BookView> {
        let data = trader
            .rpc
            .get_account_data(&self.orderbook)
            .context("orderbook account")?;
        let buf = aligned(&data);
        let bytes = &bytemuck::cast_slice::<u128, u8>(&buf)[..data.len()];
        let book = Orderbook::try_from_account_bytes(bytes)
            .map_err(|e| anyhow::anyhow!("orderbook decode: {e}"))?;
        let mut view = BookView {
            order_seq: book.header.order_sequence_number().sequence_number,
            trade_seq: book.header.trade_sequence_number().sequence_number,
            ..Default::default()
        };
        let collect = |iter: phoenix_rise::accounts::orderbook::OrderbookSideIter<'_>| {
            iter.map(|e| RawOrder {
                ticks: e.price_in_ticks().as_inner(),
                lots: e.num_base_lots_remaining().as_inner(),
                initial_lots: e.order().initial_trade_size().as_inner(),
                seq: e.order_sequence_number(),
                slot: e.order().initial_slot(),
                trader_id: e
                    .order()
                    .trader_position_id()
                    .trader_id()
                    .to_u32_or_sentinel(),
            })
            .collect::<Vec<_>>()
        };
        view.raw_bids = collect(book.bid_orders());
        view.raw_asks = collect(book.ask_orders());
        for t in book.header.trade_list().iter() {
            view.trades.push(Trade {
                seq: t.trade_sequence_number,
                slot: t.slot,
                ticks: t.maker_fill.price.as_inner(),
                maker_lots: t.maker_fill.quantity.as_inner(),
                maker: Pubkey::new_from_array(t.maker),
            });
        }
        view.trades.sort_by_key(|t| t.seq);
        self.split(&mut view);
        Ok(view)
    }

    /// Separates our orders from the rest and aggregates the rest per price.
    pub fn split(&self, view: &mut BookView) {
        let mine = self.my_trader_id;
        let split_side = |raw: &[RawOrder]| {
            let mut levels: Vec<Level> = Vec::new();
            let mut my = Vec::new();
            let mut last_traders: Vec<u32> = Vec::new();
            for o in raw {
                if Some(o.trader_id) == mine {
                    my.push(MyOrder {
                        ticks: o.ticks,
                        lots: o.lots,
                        initial_lots: o.initial_lots,
                        seq: o.seq,
                        slot: o.slot,
                    });
                    continue;
                }
                match levels.last_mut() {
                    Some(l) if l.ticks == o.ticks => {
                        l.lots += o.lots;
                        if !last_traders.contains(&o.trader_id) {
                            last_traders.push(o.trader_id);
                            l.traders += 1;
                        }
                    }
                    _ => {
                        last_traders.clear();
                        last_traders.push(o.trader_id);
                        levels.push(Level {
                            ticks: o.ticks,
                            lots: o.lots,
                            traders: 1,
                        });
                    }
                }
            }
            (levels, my)
        };
        let (bids, my_bids) = split_side(&view.raw_bids);
        let (asks, my_asks) = split_side(&view.raw_asks);
        view.bids = bids;
        view.asks = asks;
        view.my_bids = my_bids;
        view.my_asks = my_asks;
    }

    /// Cumulative fills of Phoenix's own makers (the splines).
    pub fn read_splines(&self, trader: &Trader) -> Result<SplineFills> {
        let data = trader
            .rpc
            .get_account_data(&self.splines)
            .context("spline collection account")?;
        let coll = SplineCollection::try_from_account_bytes(&data)
            .map_err(|e| anyhow::anyhow!("spline decode: {e}"))?;
        let mut out = SplineFills::default();
        for sp in coll.iter() {
            let sp = sp.map_err(|e| anyhow::anyhow!("spline entry: {e}"))?;
            out.bid_filled += sp.bid_filled_amount().as_inner();
            out.ask_filled += sp.ask_filled_amount().as_inner();
            if sp.is_active() {
                out.active += 1;
                out.mid_ticks = sp.mid_price().as_inner();
            }
        }
        Ok(out)
    }

    /// Our signed position in base lots, read from the trader account.
    pub fn read_position_lots(&self, trader: &Trader) -> Result<i64> {
        let data = trader
            .rpc
            .get_account_data(&self.trader_account)
            .context("trader account")?;
        let buf = aligned(&data);
        let bytes = &bytemuck::cast_slice::<u128, u8>(&buf)[..data.len()];
        let acct = TraderAccount::try_from_account_bytes(bytes)
            .map_err(|e| anyhow::anyhow!("trader decode: {e}"))?;
        for (asset_id, pos) in acct.positions() {
            if asset_id == u64::from(self.asset_id) {
                return Ok(pos.base_lot_position().as_inner());
            }
        }
        Ok(0)
    }
}
