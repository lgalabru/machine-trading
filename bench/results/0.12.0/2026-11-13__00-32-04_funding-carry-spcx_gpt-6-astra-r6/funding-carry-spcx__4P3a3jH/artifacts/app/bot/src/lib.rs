//! SPCX funding and price-discovery strategy.
//!
//! Uses current market observations and read-only Phoenix account data.
//! A compact forecast estimates the next eight seconds' return after funding.
//! Position hysteresis avoids paying the spread repeatedly on weak signals.

mod features;
mod model;
use mtx_kit::venue::phoenix_rise::accounts::{perp_asset_map::PerpAssetMap, trader::Trader};
use mtx_kit::{log, prelude::*, venue::PhoenixVenue};
#[derive(Default)]
struct Bot {
    features: features::Features,
    period: f64,
    funding_cap: f64,
    stopped: bool,
    map_key: String,
    trader_key: String,
    base_scale: f64,
    peak: f64,
}
impl Strategy for Bot {
    fn start(&mut self, s: &Session) -> BotResult {
        self.period = 32.0;
        self.funding_cap = 0.015;
        if let Ok(v) = PhoenixVenue::from_json(&s.venue) {
            if let Ok(authority) = mtx_kit::venue::key(&s.authority) {
                self.trader_key = v.trader_account(&authority).to_string();
            }
            if let Ok(key) = v.perp_asset_map() {
                self.map_key = key.to_string();
                if let Ok(Some(data)) = mtx_kit::rpc_account(&key.to_string()) {
                    if let Ok(map) = PerpAssetMap::try_from_account_bytes(&data) {
                        if let Ok(Some(e)) = map.find_by_symbol("SPCX") {
                            let f = e.metadata.funding_accumulator();
                            self.period = f.funding_period_seconds.as_inner() as f64;
                            if let Ok(mc) = v.market("SPCX") {
                                self.base_scale = 10f64.powi(mc.base_lots_decimals as i32);
                                self.funding_cap = f.max_funding_rate.as_inner() as f64
                                    * 10f64.powi(mc.base_lots_decimals as i32 - 6)
                                    * self.period
                                    / f.funding_interval_seconds.as_inner() as f64
                                    / s.start_mark;
                            }
                        }
                    }
                }
            }
        }
        if self.base_scale <= 0.0
            || !self.period.is_finite()
            || self.period <= 0.0
            || !self.funding_cap.is_finite()
            || self.funding_cap <= 0.0
        {
            self.stopped = true;
            log("Could not initialize market units and funding; trading disabled.");
        }
        log(&format!(
            "config period={} cap={}",
            self.period, self.funding_cap
        ));
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let b = match (m.best_bid, m.best_ask) {
            (Some(b), Some(a)) => (a + b) * 0.5,
            _ => m.mark,
        };
        let idx = if m.index > 0.0 { m.index } else { m.oracle };
        if !b.is_finite() || !idx.is_finite() || b <= 0.0 || idx <= 0.0 {
            return Ok(());
        }
        // Include fees, realized PnL, and settled and unsettled funding.
        let mut equity = 10000.0 + m.pnl;
        let mut account_read = false;
        if let Ok(data) = mtx_kit::rpc_accounts(&[&self.map_key, &self.trader_key]) {
            if let (Some(Some(md)), Some(Some(td))) = (data.first(), data.get(1)) {
                if let (Ok(map), Ok(trader)) = (
                    PerpAssetMap::try_from_account_bytes(md),
                    Trader::try_from_account_bytes(td),
                ) {
                    if let Ok(Some(entry)) = map.find_by_symbol("SPCX") {
                        let cum = entry
                            .metadata
                            .funding_accumulator()
                            .cumulative_funding_rate
                            .as_inner() as f64;
                        equity =
                            trader.header().state().quote_lot_collateral.as_inner() as f64 / 1e6;
                        for (_, pr) in trader.positions() {
                            let pr = pr.to_position();
                            let lots = pr.base_lot_position.as_inner() as f64;
                            equity += pr.virtual_quote_lot_position.as_inner() as f64 / 1e6
                                + lots / self.base_scale * b
                                - (cum - pr.cumulative_funding_snapshot.as_inner() as f64) * lots
                                    / 1e6;
                        }
                        account_read = equity.is_finite() && self.base_scale > 0.0;
                    }
                }
            }
        }
        if !account_read {
            self.stopped = true;
        }
        self.peak = self.peak.max(10000.0).max(equity);
        let dd = self.peak - equity;
        let t = m.k as f64 * 0.5;
        let p = (b / idx - 1.0).clamp(-0.1, 0.1);
        let x = idx.ln();
        let features = self.features.update(
            x,
            p,
            t,
            self.period,
            self.funding_cap,
            m.steps_left as f64 * 0.5,
        );
        let signal = model::predict(&features);
        let mut target = if signal > 0.0015 {
            m.cap
        } else if signal < -0.0015 {
            -m.cap
        } else {
            m.position.size
        };
        // Reserve room for a 6% adverse book jump and 15% of the drawdown
        // budget for execution costs and differences in book valuation.
        let safe_cap = ((m.max_drawdown * 0.85 - dd) / (b * 0.06))
            .max(0.0)
            .min(m.cap);
        target = target.clamp(-safe_cap, safe_cap);
        if dd > m.max_drawdown * 0.75 {
            self.stopped = true;
        }
        if self.stopped {
            target = 0.0;
        }
        // A last-step trade only adds costs. Use live quotes for execution:
        // the last-trade mark can lag a scenario price change.
        if !m.last && (target - m.position.size).abs() > 0.01 {
            let delta = target - m.position.size;
            let mut remaining = delta.abs();
            let limit = if delta > 0.0 {
                m.best_ask.unwrap_or(b) * 1.004
            } else {
                m.best_bid.unwrap_or(b) * 0.996
            };
            for _ in 0..15 {
                if remaining < 0.001 {
                    break;
                }
                let size = remaining.min(10.0);
                if delta > 0.0 {
                    o.buy(size, limit);
                } else {
                    o.sell(size, limit);
                }
                remaining -= size;
            }
        }
        log(&format!(
            "k={} b={:.5} i={:.5} p={:.6} slope={:.7} sig={:.6} target={} pos={} pnl={:.3} dd={:.3}",
            m.k,
            b,
            idx,
            p,
            features[14],
            signal,
            target,
            m.position.size,
            equity - 10000.0,
            dd
        ));
        Ok(())
    }
}
mtx_kit::export!(Bot);

#[cfg(test)]
mod tests {
    use super::*;
    use mtx_kit::{Intent, Side};

    fn market(last: bool) -> Market {
        Market {
            mark: 100.0,
            index: 150.0,
            oracle: 150.0,
            best_bid: Some(149.99),
            best_ask: Some(150.01),
            position: Position {
                size: 70.0,
                entry_usd: 150.0,
            },
            cap: 70.0,
            max_drawdown: 1000.0,
            steps_left: if last { 0 } else { 100 },
            last,
            ..Market::default()
        }
    }

    #[test]
    fn unreadable_account_exits_in_bounded_slices_using_live_quotes() {
        // Native tests have no RPC backend: the risk read must fail closed.
        let mut bot = Bot {
            period: 32.0,
            funding_cap: 0.015,
            base_scale: 1000.0,
            ..Bot::default()
        };
        let mut orders = Orders::new();
        bot.step(&market(false), &mut orders).unwrap();
        assert!(bot.stopped);
        let mut total = 0.0;
        for intent in orders.into_intents() {
            match intent {
                Intent::Market { side, size, limit } => {
                    assert_eq!(side, Side::Ask);
                    assert!(size > 0.0 && size <= 10.0);
                    assert!(limit > 149.0 && limit < 149.99);
                    total += size;
                }
                _ => panic!("unexpected order type"),
            }
        }
        assert!((total - 70.0).abs() < 1e-9);
    }

    #[test]
    fn no_terminal_trade_cost() {
        let mut bot = Bot {
            period: 32.0,
            funding_cap: 0.015,
            base_scale: 1000.0,
            ..Bot::default()
        };
        let mut orders = Orders::new();
        bot.step(&market(true), &mut orders).unwrap();
        assert!(orders.into_intents().is_empty());
    }
}
