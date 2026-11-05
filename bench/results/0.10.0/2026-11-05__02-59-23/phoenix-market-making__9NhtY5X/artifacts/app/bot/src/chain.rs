//! Chain access: one batched read per turn, and one transaction per turn.

use anyhow::{Context, Result, anyhow};
use mtx_botkit::{
    Keypair, Pubkey, RpcClient, Signer, Trader,
    phoenix_rise::{
        accounts::{owned::Orderbook, owned::Trader as TraderAccount, perp_asset_map::PerpAssetMap},
        core::MarketOrderTicket,
        ix::types::CancelId,
        math::Side,
    },
};
use std::str::FromStr;

#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub ticks: u64,
    pub lots: u64,
    pub trader_id: Option<u32>,
    pub initial_slot: u64,
    pub seq: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct TradeEv {
    pub slot: u64,
    pub seq: u64,
    pub price_ticks: u64,
    /// Maker fill quantity in base lots, signed as Phoenix reports it.
    pub qty_lots: i64,
    pub maker: Pubkey,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub trades: Vec<TradeEv>,
    pub oracle_ticks: u64,
    pub position_lots: i64,
    /// Unused placeholder for attributing my own taker trades (always 0).
    pub my_tx_slot_hint: u64,
}

pub struct Chain {
    pub rpc: RpcClient,
    pub keypair: Keypair,
    pub authority: Pubkey,
    pub account: Pubkey,
    pub orderbook: Pubkey,
    pub perp_asset_map: Pubkey,
    pub symbol: String,
    pub asset_id: u64,
    pub tick_usd: f64,
    pub lot_units: f64,
    pub next_coid: u64,
}

impl Chain {
    pub fn new(trader: &Trader) -> Result<Self> {
        let market = trader.phoenix.market(&trader.env.symbol)?;
        let tick_usd = market.tick_size as f64 * 10f64.powi(market.base_lots_decimals as i32 - 6);
        let lot_units = 10f64.powi(market.base_lots_decimals as i32);
        Ok(Self {
            rpc: mtx_botkit::rpc_client(&trader.env.rpc_url),
            keypair: trader.keypair.insecure_clone(),
            authority: trader.authority(),
            account: trader.account,
            orderbook: Pubkey::from_str(&market.market_pubkey)?,
            perp_asset_map: trader.phoenix.perp_asset_map()?,
            symbol: trader.env.symbol.clone(),
            asset_id: market.asset_id as u64,
            tick_usd,
            lot_units,
            next_coid: 1,
        })
    }

    pub fn read(&self) -> Result<Snapshot> {
        let keys = [self.orderbook, self.account, self.perp_asset_map];
        let accounts = self.rpc.get_multiple_accounts(&keys)?;
        let book_data = accounts[0].as_ref().ok_or_else(|| anyhow!("no orderbook"))?;
        let book = Orderbook::try_from_account_bytes(&book_data.data)
            .map_err(|e| anyhow!("orderbook: {e}"))?;
        let conv = |e: &mtx_botkit::phoenix_rise::accounts::owned::OrderbookEntry| Level {
            ticks: e.order_id.price_in_ticks.as_inner(),
            lots: e.order.num_base_lots_remaining,
            trader_id: e.order.trader_position_id.trader_id,
            initial_slot: e.order.initial_slot,
            seq: e.order_id.order_sequence_number,
        };
        let mut bids: Vec<Level> = book.bids.iter().map(conv).collect();
        let mut asks: Vec<Level> = book.asks.iter().map(conv).collect();
        bids.sort_by(|a, b| b.ticks.cmp(&a.ticks).then(a.seq.cmp(&b.seq)));
        asks.sort_by(|a, b| a.ticks.cmp(&b.ticks).then(a.seq.cmp(&b.seq)));
        let trades = book
            .header
            .trade_list
            .iter()
            .map(|t| TradeEv {
                slot: t.slot,
                seq: t.trade_sequence_number,
                price_ticks: t.maker_fill.price,
                qty_lots: t.maker_fill.quantity,
                maker: t.maker,
            })
            .collect();

        let position_lots = match accounts[1].as_ref() {
            Some(acc) => {
                let t = TraderAccount::try_from_account_bytes(&acc.data)
                    .map_err(|e| anyhow!("trader: {e}"))?;
                t.position_entries()
                    .find(|p| p.asset_id == self.asset_id)
                    .map(|p| p.position.base_lot_position.as_inner())
                    .unwrap_or(0)
            }
            None => 0,
        };

        let pam = accounts[2].as_ref().ok_or_else(|| anyhow!("no perp asset map"))?;
        let map = PerpAssetMap::try_from_account_bytes(&pam.data).map_err(|e| anyhow!("pam: {e}"))?;
        let entry = map
            .find_by_symbol(&self.symbol)
            .map_err(|e| anyhow!("pam: {e}"))?
            .ok_or_else(|| anyhow!("no market"))?;
        let price = &entry.metadata.oracle_price().mark_price;
        let reported = |t: u64| t > 0 && t != u64::MAX;
        let mut ticks: Vec<u64> = price
            .spot_price_component
            .last_exchange_spot_price
            .iter()
            .map(|p| p.ticks.as_inner())
            .filter(|&t| reported(t))
            .collect();
        if ticks.is_empty() {
            ticks = price
                .perp_price_component
                .last_exchange_perp_price
                .iter()
                .map(|p| p.ticks.as_inner())
                .filter(|&t| reported(t))
                .collect();
        }
        if ticks.is_empty() {
            return Err(anyhow!("no oracle"));
        }
        ticks.sort_unstable();
        let oracle_ticks = ticks[ticks.len() / 2];
        Ok(Snapshot {
            bids,
            asks,
            trades,
            oracle_ticks,
            position_lots,
            my_tx_slot_hint: 0,
        })
    }

    /// One transaction: optionally cancel everything, optionally take, then
    /// rest post-only quotes. `bids`/`asks` are (ticks, lots).
    pub async fn send_turn(
        &mut self,
        trader: &Trader,
        cancel_all: bool,
        cancels: &[(u64, u64)],
        takes: &[(Side, u64, u64)],
        bids: &[(u64, u64)],
        asks: &[(u64, u64)],
    ) -> Result<()> {
        let tx = trader.phoenix.tx();
        let mut ixs = Vec::new();
        if cancel_all {
            ixs.extend(tx.build_cancel_all_orders(self.authority, self.account, &self.symbol)?);
        } else if !cancels.is_empty() {
            let ids: Vec<CancelId> = cancels.iter().map(|&(px, seq)| CancelId::new(px, seq)).collect();
            ixs.extend(tx.build_cancel_orders(self.authority, self.account, &self.symbol, ids)?);
        }
        for &(side, lots, limit_ticks) in takes {
            let ticket = MarketOrderTicket::builder()
                .authority(self.authority)
                .trader_account(self.account)
                .symbol(self.symbol.as_str())
                .side(side)
                .price(limit_ticks as f64 * self.tick_usd)
                .num_base_lots(lots)
                .client_order_id(self.coid())
                .build()?;
            ixs.extend(tx.place_market_order(ticket).await?);
        }
        if !bids.is_empty() || !asks.is_empty() {
            let b: Vec<(f64, u64)> = bids.iter().map(|&(t, l)| (t as f64 * self.tick_usd, l)).collect();
            let a: Vec<(f64, u64)> = asks.iter().map(|&(t, l)| (t as f64 * self.tick_usd, l)).collect();
            ixs.extend(tx.build_multi_limit_order(self.authority, self.account, &self.symbol, &b, &a, true)?);
        }
        if ixs.is_empty() {
            return Ok(());
        }
        mtx_botkit::send(&self.rpc, &self.keypair, ixs, "turn").context("send turn")?;
        Ok(())
    }

    /// Collateral (USD) and unrealized PnL at the oracle (USD), via Hawkeye.
    pub async fn equity(&self, trader: &Trader, oracle_ticks: u64) -> Result<(f64, f64)> {
        let hawkeye = trader.phoenix.hawkeye(self.authority);
        let margin = hawkeye.view_margin(self.authority, 0, 0).await?.value;
        let asset = trader.luce.margin().await?;
        let collateral = margin.collateral_quote_lots as f64 / 1e6;
        let base_units = asset.base_lots as f64 / self.lot_units;
        let entry = asset.entry_price_quote_lots_per_base_lot as f64 * self.lot_units / 1e6;
        let upnl = base_units * (oracle_ticks as f64 * self.tick_usd - entry);
        Ok((collateral, upnl))
    }

    fn coid(&mut self) -> u128 {
        let id = self.next_coid;
        self.next_coid += 1;
        id as u128
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }
}
