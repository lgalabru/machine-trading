//! Read settled collateral and the SPCX virtual quote position directly.
//! Constant wallet USDC cancels out of peak-to-current drawdown.

use mtx_kit::{
    prelude::*, rpc_account,
    venue::{PhoenixVenue, key, phoenix_rise::accounts::owned::Trader},
};

#[derive(Default)]
pub struct Risk {
    account: String,
    asset: u64,
    base_factor: f64,
    peak: f64,
    initial: f64,
    start_mark: f64,
    previous_position: f64,
    costs: f64,
    pub halted: bool,
    pub drawdown: f64,
    pub equity: f64,
}

impl Risk {
    pub fn start(&mut self, session: &Session) {
        self.start_mark = session.start_mark;
        if let Ok(venue) = PhoenixVenue::from_json(&session.venue) {
            if let Ok(authority) = key(&session.authority) {
                self.account = venue.trader_account(&authority).to_string();
            }
            if let Ok(market) = venue.market(&session.symbol) {
                self.asset = market.asset_id as u64;
                self.base_factor = 10f64.powi(market.base_lots_decimals as i32);
            }
        }
    }

    // One allowed getAccountInfo call, with decoding supplied by Phoenix.
    fn read(&self, mark: f64) -> Option<f64> {
        let data = rpc_account(&self.account).ok()??;
        let trader = Trader::try_from_account_bytes(&data).ok()?;
        let mut value = trader.state.quote_lot_collateral.as_inner() as f64 / 1e6;
        for (id, position) in &trader.positions.entries {
            if *id == self.asset && position.base_lot_position.as_inner() != 0 {
                value += position.base_lot_position.as_inner() as f64 / self.base_factor * mark
                    + position.virtual_quote_lot_position.as_inner() as f64 / 1e6;
            }
        }
        if value.is_finite() { Some(value) } else { None }
    }

    pub fn limit(&mut self, market: &Market) -> f64 {
        self.costs += (market.position.size - self.previous_position).abs() * market.mark * 0.001;
        self.previous_position = market.position.size;
        // A turnover-cost estimate remains available if an account read fails.
        let value = self.read(market.mark).unwrap_or(10_000.0 + market.pnl - self.costs);
        if self.initial == 0.0 { self.initial = value; self.peak = value; }
        self.peak = self.peak.max(value);
        self.equity = value;
        self.drawdown = (self.peak - value).max(0.0);
        if self.drawdown > 750.0 { self.halted = true; }
        if self.halted { return 0.0; }

        // Reserve $150 for fees, slippage, and valuation differences. Size for
        // a 4.5% adverse step, measured against at least the starting mark.
        market.cap.min(
            ((market.max_drawdown - 150.0 - self.drawdown)
                / (market.mark.max(self.start_mark) * 0.045)).max(0.0),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn market(price: f64, pnl: f64) -> Market {
        Market { mark: price, cap: 70.0, max_drawdown: 1000.0, pnl, ..Market::default() }
    }

    #[test]
    fn reserves_room_for_an_adverse_step_and_execution() {
        for price in [90.0_f64, 143.11, 190.0] {
            for drawdown in [0.0, 200.0, 400.0, 600.0, 749.0] {
                let mut risk = Risk { start_mark: 143.11, initial: 10000.0, peak: 10000.0, ..Risk::default() };
                let cap = risk.limit(&market(price, -drawdown));
                assert!(cap >= 0.0 && cap <= 70.0);
                assert!(drawdown + cap * price.max(143.11) * 0.045 + 150.0 <= 1000.000001);
            }
        }
    }

    #[test]
    fn drawdown_halt_does_not_reopen() {
        let mut risk = Risk { start_mark: 143.11, initial: 10000.0, peak: 10000.0, ..Risk::default() };
        assert_eq!(risk.limit(&market(143.11, -760.0)), 0.0);
        assert!(risk.halted);
        assert_eq!(risk.limit(&market(143.11, 100.0)), 0.0);
    }
}
