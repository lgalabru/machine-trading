//! SPCX funding-aware forecasting with execution costs and an account-equity risk guard.
mod features;
mod model;
use mtx_kit::venue::{
    PhoenixVenue,
    phoenix_rise::accounts::{
        perp_asset_map::PerpAssetMap, spline_collection::SplineCollection, trader::Trader,
    },
};
use mtx_kit::{log, prelude::*, rpc_accounts};

#[derive(Default)]
struct SpcxStrategy {
    map: String,
    trader: String,
    spline: String,
    tick: f64,
    start_slot: u64,
    asset: u64,
    lots: f64,
    period: f64,
    interval: f64,
    funding_cap: f64,
    peak: f64,
    halted: bool,
    errors: u32,
    features: features::Features,
    reference: f64,
    quote_slot: u64,
    previous_position: f64,
    consumed_buy: f64,
    consumed_sell: f64,
}
impl Strategy for SpcxStrategy {
    fn start(&mut self, s: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&s.venue)?;
        let cfg = venue.market("SPCX")?;
        self.asset = cfg.asset_id as u64;
        self.spline = cfg.spline_pubkey.clone();
        self.tick = venue.ticks_to_usd(cfg, 1);
        self.start_slot = s.start_slot;
        self.lots = 10f64.powi(cfg.base_lots_decimals as i32);
        self.map = venue.perp_asset_map()?.to_string();
        self.trader = venue.trader_account(&s.authority.parse()?).to_string();
        self.period = 32.0;
        self.interval = 4.0;
        self.funding_cap = 1.4;
        self.peak = 10_000.0;
        self.reference = s.start_mark;
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let mut mid = match (m.best_bid, m.best_ask) {
            (Some(b), Some(a)) if b > 0.0 && a >= b => (a + b) * 0.5,
            _ => m.mark,
        };
        if !(mid > 0.0 && m.oracle > 0.0) {
            return Ok(());
        }
        let mut quote_slot = 0;
        let mut equity = None;
        // A single permitted read: funding parameters, collateral/position, and maker quotes.
        if let Ok(v) = rpc_accounts(&[&self.map, &self.trader, &self.spline]) {
            // Quote centres are unaffected by our fills, unlike the displayed best bid/ask.
            if let Some(Some(data)) = v.get(2) {
                if let Ok(collection) = SplineCollection::try_from_account_bytes(data) {
                    quote_slot = collection
                        .iter()
                        .filter_map(|r| r.ok())
                        .filter(|s| s.is_enabled() && s.user_update_slot() >= self.start_slot)
                        .map(|s| s.user_update_slot())
                        .max()
                        .unwrap_or(0);
                    let mut mids: Vec<f64> = collection
                        .iter()
                        .filter_map(|r| r.ok())
                        .filter(|s| s.is_enabled() && s.user_update_slot() >= self.start_slot)
                        .map(|s| s.mid_price().as_inner() as f64 * self.tick)
                        .collect();
                    mids.sort_by(|a, b| a.total_cmp(b));
                    if !mids.is_empty() {
                        mid = mids[mids.len() / 2];
                    }
                }
            }
            if let (Some(Some(a)), Some(Some(b))) = (v.get(0), v.get(1)) {
                if let (Ok(map), Ok(trader)) = (
                    PerpAssetMap::try_from_account_bytes(a),
                    Trader::try_from_account_bytes(b),
                ) {
                    if let Ok(Some(e)) = map.find_by_symbol("SPCX") {
                        let f = e.metadata.funding_accumulator();
                        self.period = f.funding_period_seconds.as_inner() as f64;
                        self.interval = f.funding_interval_seconds.as_inner() as f64;
                        self.funding_cap = f.max_funding_rate.as_inner() as f64 * self.lots / 1e6
                            * self.period
                            / self.interval;
                        let cumulative = f.cumulative_funding_rate.as_inner() as f64;
                        // Include fees, realized PnL, and unsettled cumulative funding.
                        // Wallet USDC is constant, so it cancels out of drawdowns.
                        let mut value =
                            trader.header().state().quote_lot_collateral.as_inner() as f64 / 1e6;
                        for (id, p) in trader.positions() {
                            if id == self.asset {
                                let base = p.base_lot_position().as_inner() as f64;
                                value += base / self.lots * mid
                                    + p.virtual_quote_lot_position().as_inner() as f64 / 1e6;
                                value -= base
                                    * (cumulative
                                        - p.cumulative_funding_snapshot().as_inner() as f64)
                                    / 1e6;
                            }
                        }
                        equity = Some(value);
                    }
                }
            }
        }
        if let Some(value) = equity {
            self.errors = 0;
            self.peak = self.peak.max(value);
            if self.peak - value > 0.65 * m.max_drawdown {
                self.halted = true;
            }
        } else {
            self.errors += 1;
        }
        let gap = mid - m.oracle;
        let mut x = self.features.update(
            (m.oracle / self.reference - 1.) * 10000.,
            (mid / self.reference - 1.) * 10000.,
            self.period,
            self.funding_cap / self.reference * 10000.,
            (m.steps_left + 1) as f64 * 0.5,
        );
        // The models use sign-symmetric, causal features measured in basis points.
        let sign = x[47];
        x[47] = 0.;
        let prediction = model::predict(&x);
        let value =
            ((prediction[0] * 0.75 + prediction[1] * 0.25) * 0.5 + prediction[2] * 0.5) * sign;
        // Track actual fills against each side until makers replenish their quotes.
        if quote_slot != self.quote_slot {
            self.consumed_buy = 0.;
            self.consumed_sell = 0.;
            self.quote_slot = quote_slot;
        } else {
            let filled = m.position.size - self.previous_position;
            if filled > 0. {
                self.consumed_buy += filled / 70.;
            } else {
                self.consumed_sell -= filled / 70.;
            }
        }
        self.previous_position = m.position.size;
        let mut target = choose_target(
            value,
            m.position.size,
            m.cap,
            self.consumed_buy,
            self.consumed_sell,
        );
        if self.halted || self.errors > 5 {
            target = 0.0;
        }
        let dd = equity.map(|v| (self.peak - v).max(0.)).unwrap_or(0.);
        // Reserve room for a 5% adverse price jump, then stop permanently at 65% of the limit.
        let risk_cap = ((0.90 * m.max_drawdown - dd).max(0.) / (mid * 0.05))
            .min(m.cap)
            .min(70.);
        target = target.clamp(-risk_cap, risk_cap);
        let mut delta = target - m.position.size;
        // Each slice is its own transaction. IOC limits are based on the fresh book centre.
        for _ in 0..15 {
            if delta.abs() < 0.01 {
                break;
            }
            let amount = delta.abs().min(10.);
            if delta > 0. {
                o.buy(amount, mid * 1.004);
            } else {
                o.sell(amount, mid * 0.996);
            }
            delta -= delta.signum() * amount;
        }
        log(&format!(
            "{} mid {:.4} idx {:.4} gap {:.4} pos {:.2} target {:.1} eq {:.2} period {} cap {:.4} signal {:.3}",
            m.k,
            mid,
            m.oracle,
            gap,
            m.position.size,
            target,
            equity.unwrap_or(0.0),
            self.period,
            self.funding_cap,
            value
        ));
        Ok(())
    }
}
/// Choose among quarter-cap positions, subtracting fees and marginal book impact.
fn choose_target(value: f64, position: f64, cap: f64, buy: f64, sell: f64) -> f64 {
    let mut target = position;
    let mut best = 0.;
    for n in -4..=4 {
        let q = n as f64 * 17.5;
        if q.abs() > cap {
            continue;
        }
        let delta = (q - position) / 70.;
        let consumed = if delta > 0. { buy } else { sell };
        let score = delta * value - delta.abs() * (4. + 3.5 * (delta.abs() + 2. * consumed));
        if score > best + 0.00001 {
            best = score;
            target = q;
        }
    }
    target
}

#[cfg(test)]
#[test]
fn execution_respects_cap_and_consumed_liquidity() {
    assert_eq!(choose_target(9., 0., 70., 0., 0.), 52.5);
    assert_eq!(choose_target(9., 52.5, 70., 0.75, 0.), 52.5);
    assert_eq!(choose_target(9., 52.5, 70., 0., 0.), 70.);
    assert_eq!(choose_target(100., -70., 70., 0., 0.), 70.);
    assert_eq!(choose_target(-100., 70., 70., 0., 0.), -70.);
    assert_eq!(choose_target(100., 0., 35., 0., 0.), 35.);
}

mtx_kit::export!(SpcxStrategy);

#[cfg(test)]
mod check;
