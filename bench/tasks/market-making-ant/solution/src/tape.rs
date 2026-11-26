//! The market's tape, as anyone reading the chain sees it: Phoenix keeps
//! the market's recent trades in its orderbook account (`trade_list`),
//! each with its slot, sequence number and the maker's signed fill. The
//! taker's side is the other way. One `getAccountInfo` a step.

use std::collections::VecDeque;

use mtx_kit::{
    rpc_account,
    venue::{PhoenixVenue, phoenix_rise::accounts::owned::OrderbookHeader},
};

/// The most trades the window keeps: a step brings a few, and the window
/// is tens of slots.
const TRADES_MAX: usize = 4_096;

/// Signed taker volume over the last `window_slots`.
#[derive(Default)]
pub struct Tape {
    market: String,
    lot_scale: f64,
    window_slots: u64,
    last_sequence: Option<u64>,
    /// (slot, signed taker size in base units: + bought, - sold).
    trades: VecDeque<(u64, f64)>,
}

impl Tape {
    pub fn new(venue: &PhoenixVenue, symbol: &str, window_slots: u64) -> Result<Self, String> {
        assert!(window_slots > 0);
        let market = venue.market(symbol)?;
        Ok(Self {
            market: market.market_pubkey.clone(),
            lot_scale: 10f64.powi(market.base_lots_decimals as i32),
            window_slots,
            last_sequence: None,
            trades: VecDeque::new(),
        })
    }

    /// The takers' net buying, base units, over the window to `slot`. A read
    /// that fails leaves the window as it was.
    pub fn read(&mut self, slot: u64) -> f64 {
        if let Ok(Some(data)) = rpc_account(&self.market)
            && let Ok(header) = OrderbookHeader::try_from_account_bytes(&data)
        {
            let first = self.last_sequence.is_none();
            let since = self.last_sequence.unwrap_or(0);
            let mut newest = since;
            for trade in &header.trade_list {
                newest = newest.max(trade.trade_sequence_number);
                // The first read only marks where the tape starts.
                if !first && trade.trade_sequence_number > since {
                    let taker = -(trade.maker_fill.quantity as f64) / self.lot_scale;
                    self.trades.push_back((trade.slot, taker));
                }
            }
            self.last_sequence = Some(newest);
        }
        while self
            .trades
            .front()
            .is_some_and(|(at, _)| at + self.window_slots < slot)
            || self.trades.len() > TRADES_MAX
        {
            self.trades.pop_front();
        }
        self.trades.iter().map(|(_, size)| size).sum()
    }
}
