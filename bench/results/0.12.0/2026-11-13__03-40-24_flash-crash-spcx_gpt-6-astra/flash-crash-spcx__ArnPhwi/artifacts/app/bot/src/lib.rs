//! SPCX shock trading with a learned continuation-value policy and an independent
//! equity-based risk limit. The model is compiled into the component.
use mtx_kit::venue::{PhoenixVenue, key, phoenix_rise::accounts::owned::Trader};
use mtx_kit::{log, prelude::*};
mod features;
mod model;
mod risk;

#[derive(Default)]
struct ShockBot {
    history: features::History,
    risk: risk::Risk,
    start_mark: f64,
    account: String,
    lot_scale: f64,
    asset_id: u64,
    drawdown_limit: f64,
}
impl ShockBot {
    // Collateral already includes realized trading fees and PnL. Virtual quote
    // plus signed base valued at the mark supplies unrealized PnL.
    fn account_pnl(&self, market: &Market) -> Option<f64> {
        let data = mtx_kit::rpc_account(&self.account).ok()??;
        let trader = Trader::try_from_account_bytes(&data).ok()?;
        let mut pnl = trader.state.quote_lot_collateral.as_i128() as f64 / 1e6 - 10_000.0;
        for (asset_id, pos) in trader.positions.entries {
            if asset_id != self.asset_id {
                return None;
            }
            pnl += pos.virtual_quote_lot_position.as_i128() as f64 / 1e6
                + pos.base_lot_position.as_i128() as f64 / self.lot_scale * market.mark;
        }
        pnl.is_finite().then_some(pnl)
    }
}
impl Strategy for ShockBot {
    fn start(&mut self, session: &Session) -> BotResult {
        if session.symbol != "SPCX" || !session.start_mark.is_finite() || session.start_mark <= 0.0
        {
            return Err("invalid SPCX session".into());
        }
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?;
        self.account = venue.trader_account(&key(&session.authority)?).to_string();
        self.lot_scale = 10f64.powi(market.base_lots_decimals as i32);
        self.asset_id = market.asset_id as u64;
        self.start_mark = session.start_mark;
        self.drawdown_limit = session.max_drawdown;
        self.history = features::History::new(session.steps);
        Ok(())
    }
    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        // There is no subsequent price move after the final observation.
        if market.last {
            return Ok(());
        }
        if !market.oracle.is_finite() || market.oracle <= 0.0 {
            orders.target_position(0.0);
            return Ok(());
        }
        let features = self.history.observe(market.oracle / self.start_mark - 1.0);
        let score = model::predict(&features) * self.history.sign;
        let Some(pnl) = self.account_pnl(market) else {
            // An unreadable account is never a reason to add exposure.
            orders.target_position(0.0);
            return Ok(());
        };
        let size = self
            .risk
            .size(pnl, market.mark, market.cap, self.drawdown_limit);
        let mut target = market.position.size.clamp(-size, size);
        // Hysteresis: switch only when the continuation value pays for trading.
        if score > 10.0 {
            target = size;
        } else if score < -10.0 {
            target = -size;
        }
        if self.risk.halted {
            target = 0.0;
        }
        if (target - market.position.size).abs() > 0.5
            || (self.risk.halted && market.position.size.abs() > 0.001)
        {
            orders.target_position_with(
                target,
                Execution {
                    slice: 10.0,
                    min_trade: if self.risk.halted { 0.001 } else { 0.1 },
                    slippage_bps: 200.0,
                    ..Execution::default()
                },
            );
        }
        log(&format!(
            "{} {:.4} {:.4} {:.3} {:.3} {:.2} {:.2} {:.3}",
            market.k,
            market.mark,
            market.oracle,
            market.position.size,
            target,
            pnl,
            self.risk.drawdown,
            score
        ));
        Ok(())
    }
}
mtx_kit::export!(ShockBot);
