use mtx_kit::{
    log,
    prelude::*,
    rpc_accounts,
    venue::{
        PhoenixVenue,
        phoenix_rise::accounts::{
            owned::Trader, perp_asset_map::PerpAssetMap, spline_collection::SplineCollection,
        },
    },
};

include!("model.rs");
include!("features.rs");

#[derive(Default)]
struct Carry {
    map: String,
    trader: String,
    spline: String,
    tick: f64,
    asset: u64,
    scale: f64,
    period: f64,
    peak: f64,
    stopped: bool,
    signals: Signals,
    interval: f64,
    funding_cap: f64,
}
impl Strategy for Carry {
    fn start(&mut self, s: &Session) -> BotResult {
        let v = PhoenixVenue::from_json(&s.venue)?;
        let m = v.market("SPCX")?;
        self.asset = m.asset_id as u64;
        self.spline = m.spline_pubkey.clone();
        self.tick = v.ticks_to_usd(m, 1);
        self.scale = 10f64.powi(m.base_lots_decimals as i32);
        self.map = v.perp_asset_map()?.to_string();
        self.trader = v
            .trader_account(&s.authority.parse().map_err(|_| "bad authority")?)
            .to_string();
        self.period = 35.;
        self.interval = 4.;
        self.funding_cap = 0.01;
        self.peak = 10000.;
        log("SPCX adaptive funding carry and index response strategy");
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        // The maker centre remains informative after our own fills move the BBO.
        let mut mid = m
            .best_bid
            .zip(m.best_ask)
            .map(|(b, a)| (b + a) * 0.5)
            .unwrap_or(m.mark);
        let index = if m.index > 0. { m.index } else { m.oracle };
        if !(mid > 0. && index > 0.) {
            return Ok(());
        }
        let mut eq = 10000. + m.pnl;
        // Read session funding parameters and equity, including unsettled funding.
        if let Ok(a) = rpc_accounts(&[&self.map, &self.trader, &self.spline]) {
            if let Some(Some(sd)) = a.get(2) {
                if let Ok(s) = SplineCollection::try_from_account_bytes(sd) {
                    let mut mids: Vec<f64> = s
                        .splines()
                        .filter_map(Result::ok)
                        .filter(|s| s.is_enabled())
                        .map(|s| s.mid_price().as_inner() as f64 * self.tick)
                        .collect();
                    mids.sort_by(|a, b| a.total_cmp(b));
                    if !mids.is_empty() {
                        mid = mids[mids.len() / 2];
                    }
                }
            }
            if let (Some(Some(md)), Some(Some(td))) = (a.first(), a.get(1)) {
                if let (Ok(map), Ok(t)) = (
                    PerpAssetMap::try_from_account_bytes(md),
                    Trader::try_from_account_bytes(td),
                ) {
                    if let Ok(Some(entry)) = map.find_by_symbol("SPCX") {
                        let f = entry.metadata.funding_accumulator();
                        self.period = f.funding_period_seconds.as_inner() as f64;
                        self.interval = f.funding_interval_seconds.as_inner() as f64;
                        self.funding_cap = f.max_funding_rate.as_inner() as f64 / 1e6 * self.scale
                            / index
                            * self.period
                            / self.interval;
                        eq = t.state.quote_lot_collateral.as_inner() as f64 / 1e6;
                        for (id, p) in &t.positions.entries {
                            if *id == self.asset {
                                let base = p.base_lot_position.as_inner() as f64;
                                eq += p.virtual_quote_lot_position.as_inner() as f64 / 1e6
                                    + base / self.scale * mid;
                                eq -= base
                                    * (f.cumulative_funding_rate.as_inner()
                                        - p.cumulative_funding_snapshot.as_inner())
                                        as f64
                                    / 1e6;
                            }
                        }
                    }
                }
            }
        }
        // Stop well inside the desk limit; reserve room for a discontinuous move.
        self.peak = self.peak.max(eq);
        let dd = self.peak - eq;
        if dd > 500. {
            self.stopped = true;
        }
        let gap = mid / index - 1.;
        // Forecast changes in book value after the cost of carrying the premium.
        let x = self.signals.update(
            index,
            mid,
            gap,
            self.period,
            self.funding_cap,
            self.interval,
        );
        let alpha = predict(&x);
        let cap = m
            .cap
            .min(70.)
            .min(((900. - dd) / (mid * 0.05).max(7.)).max(0.));
        let mut target = m.position.size;
        let threshold = (6. + 0.5 * x[40]).clamp(8., 22.);
        if alpha > threshold {
            target = cap;
        } else if alpha < -threshold {
            target = -cap;
        }
        target = target.clamp(-cap, cap);
        if self.stopped {
            target = 0.;
        }
        // Sweep only liquidity priced inside the forecast edge, in small slices.
        let delta = target - m.position.size;
        if delta.abs() >= 2. {
            let horizon = (m.slots_left as f64 * 0.25 / 6.).clamp(0., 1.);
            let edge = alpha * delta.signum() * horizon;
            let risk_reduce = self.stopped || m.position.size.abs() > cap + 0.1;
            if risk_reduce || edge > 4. {
                let budget = if risk_reduce {
                    0.05
                } else {
                    ((edge - 4.) * 0.6).clamp(2., 30.) * 1e-4
                };
                let limit = mid * (1. + delta.signum() * budget);
                let mut left = delta.abs();
                for _ in 0..28 {
                    if left < 0.1 {
                        break;
                    }
                    let slice = left.min(5.);
                    if delta > 0. {
                        o.buy(slice, limit);
                    } else {
                        o.sell(slice, limit);
                    }
                    left -= slice;
                }
            }
        }
        if m.k % 10 == 0 || (target - m.position.size).abs() > 2. {
            log(&format!(
                "k {} mid {:.4} ix {:.4} alpha {:.4} target {:.1} pos {:.3} eq {:.2} dd {:.2}",
                m.k, mid, index, alpha, target, m.position.size, eq, dd
            ));
        }
        Ok(())
    }
}
mtx_kit::export!(Carry);
