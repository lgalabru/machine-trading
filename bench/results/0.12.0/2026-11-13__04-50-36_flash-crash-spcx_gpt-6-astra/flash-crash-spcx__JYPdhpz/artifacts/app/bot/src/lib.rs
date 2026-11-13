//! Causal shock forecasting, cost-aware position hysteresis, and account risk control.
use mtx_kit::{prelude::*, log, rpc_account, venue::{PhoenixVenue, key, phoenix_rise::accounts::trader::TraderHeader}};
mod signal;
mod model;
mod risk;

#[derive(Default)]
struct ShockBot {
    trader: String,
    start_price: f64,
    guard: risk::Guard,
    signal: signal::Signal,
}
impl Strategy for ShockBot {
    fn start(&mut self, s: &Session) -> BotResult {
        if s.symbol != "SPCX" || !s.start_mark.is_finite() || s.start_mark<=0.0 {
            return Err("this strategy requires a valid SPCX session".into());
        }
        let venue = PhoenixVenue::from_json(&s.venue)?;
        self.trader = venue.trader_account(&key(&s.authority)?).to_string();
        self.start_price = s.start_mark;
        self.guard = risk::Guard {peak:10000.0,halted:false};
        self.signal = signal::Signal::default();
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let price = if m.index > 0.0 { m.index } else { m.mark };
        if !price.is_finite() || price <= 0.0 { return Ok(()); }
        // Fees and settled PnL are in collateral; Market.pnl omits them.
        let equity = rpc_account(&self.trader).ok().flatten()
            .and_then(|data| TraderHeader::try_from_account_bytes(&data).ok().map(|h|
                h.state().quote_lot_collateral.as_inner() as f64 / 1e6))
            .map(|collateral| collateral + m.position.size * (m.mark - m.position.entry_usd));
        let (features,direction)=self.signal.update(100.0*(price/self.start_price-1.0),m.steps_left);
        // Keep updating price history even if the account read fails.
        let Some(dd) = self.guard.observe(equity,m.max_drawdown) else {
            // Missing risk data permits only an attempt to flatten.
            o.target_position(0.0);
            return Ok(());
        };
        let forecast=model::predict(&features)*direction;
        let mut target=m.position.size;
        if forecast > 0.10 {target=m.cap;}
        else if forecast < -0.10 {target=-m.cap;}
        else if (target>0.0 && forecast < -0.05) || (target<0.0 && forecast>0.05) {target=0.0;}
        let risk_cap = risk::capacity(dd,m.mark,m.cap,m.max_drawdown);
        target = target.clamp(-risk_cap, risk_cap).round().clamp(-m.cap,m.cap);
        if self.guard.halted { target = 0.0; }
        // The SDK sends immediate orders in 10-share slices and clips
        // the sheet to the mandate. No resting order can survive a halt.
        if (target - m.position.size).abs() >= 1.0 {o.target_position(target);}
        log(&format!("k={} index={:.4} pos={:.1} target={:.1} dd={:.2} forecast={:.5}",m.k,price,m.position.size,target,dd,forecast));
        Ok(())
    }
}
mtx_kit::export!(ShockBot);
