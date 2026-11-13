//! A causal fitted-value shock strategy with chain-valued risk limits.
use mtx_kit::{log, prelude::*};
mod model;

#[derive(Default, Clone, Copy)]
struct Impulse {
    step: usize,
    change: f64,
    before: f64,
    seen: bool,
}
#[derive(Default)]
struct ShockBot {
    trader: String,
    lot_scale: f64,
    asset_id: u64,
    initial: f64,
    scale: f64,
    history: Vec<f64>,
    direction: f64,
    impulses: [Impulse; 7],
    halted: bool,
    prev_pos: f64,
    costs: f64,
    peak: f64,
}
// Leave a reserve for one further adverse jump and execution costs.
fn risk_size(mark: f64, cap: f64, drawdown: f64, limit: f64) -> f64 {
    if !mark.is_finite() || mark <= 0.0 || !limit.is_finite() || limit <= 0.0 {
        return 0.0;
    }
    cap.max(0.0)
        .min(((0.85 * limit - drawdown).max(0.0) / (mark * 0.05)).floor())
}
impl ShockBot {
    fn features(&mut self, p: f64, left: u64) -> Vec<f32> {
        let k = self.history.len();
        let prev = self.history.last().copied().unwrap_or(self.initial);
        let dp = (p - prev) / prev;
        if dp.abs() >= 0.003 {
            self.direction = dp.signum();
        }
        let d = self.direction;
        for (i, threshold) in [0.0015, 0.0025, 0.0035, 0.006, 0.01, 0.02, 1e-7]
            .iter()
            .enumerate()
        {
            if dp.abs() >= *threshold {
                self.impulses[i] = Impulse {
                    step: k,
                    change: dp,
                    before: prev,
                    seen: true,
                };
            }
        }
        self.history.push(p);
        let bps = 10000.0 / self.scale;
        let mut x = vec![
            k as f32 * 0.5,
            left as f32 * 0.5,
            (d * (p - self.scale) * bps) as f32,
        ];
        for lag in [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128] {
            let old = if k >= lag {
                self.history[k - lag]
            } else {
                self.initial
            };
            x.push((d * (p - old) * bps) as f32);
        }
        for w in [8, 16, 32, 64, 128] {
            let start = (k + 1).saturating_sub(w);
            let padding = w - (k + 1 - start);
            let mut high = p;
            let mut low = p;
            let mut sum = self.initial * padding as f64;
            if padding > 0 {
                high = high.max(self.initial);
                low = low.min(self.initial);
            }
            for &v in &self.history[start..] {
                high = high.max(v);
                low = low.min(v);
                sum += v;
            }
            x.push((if d > 0.0 { p - high } else { low - p } * bps) as f32);
            x.push((if d > 0.0 { p - low } else { high - p } * bps) as f32);
            x.push((d * (p - sum / w as f64) * bps) as f32);
        }
        for imp in &self.impulses[..6] {
            x.push(if imp.seen {
                (k - imp.step) as f32
            } else {
                500.0
            });
            x.push(if imp.seen {
                (d * imp.change * 10000.0) as f32
            } else {
                0.0
            });
            x.push(if imp.seen {
                (d * (p - imp.before) * bps) as f32
            } else {
                0.0
            });
        }
        let imp = self.impulses[6];
        x.push(if imp.seen {
            (k - imp.step) as f32
        } else {
            500.0
        });
        x.push(if imp.seen {
            (d * imp.change * 10000.0) as f32
        } else {
            0.0
        });
        for v in &mut x {
            if v.abs() < 1e-6 {
                *v = 0.0;
            }
        }
        x
    }
    fn equity(&self, mark: f64) -> Option<f64> {
        let bytes = mtx_kit::rpc_account(&self.trader).ok()??;
        let account =
            mtx_kit::venue::phoenix_rise::accounts::trader::Trader::try_from_account_bytes(&bytes)
                .ok()?;
        let mut equity = account
            .header()
            .trader_state
            .quote_lot_collateral
            .as_inner() as f64
            / 1e6
            - 10000.0;
        for (id, pos) in account.positions() {
            if id == self.asset_id {
                equity += pos.virtual_quote_lot_position().as_inner() as f64 / 1e6
                    + pos.base_lot_position().as_inner() as f64 / self.lot_scale * mark;
            }
        }
        Some(equity)
    }
}
impl Strategy for ShockBot {
    fn start(&mut self, s: &Session) -> BotResult {
        let venue = mtx_kit::venue::PhoenixVenue::from_json(&s.venue)?;
        self.trader = venue
            .trader_account(&mtx_kit::venue::key(&s.authority)?)
            .to_string();
        let config = venue.market(&s.symbol)?;
        self.lot_scale = 10f64.powi(config.base_lots_decimals as i32);
        self.asset_id = config.asset_id as u64;
        self.initial = s.start_oracle;
        self.scale = s.start_mark;
        self.direction = 1.0;
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle;
        if !p.is_finite() || p <= 0.0 || !m.mark.is_finite() || m.mark <= 0.0 {
            o.target_position(0.0);
            return Ok(());
        }
        let x = self.features(p, m.steps_left);
        let q = model::predict(&x) * self.direction;
        self.costs += (m.position.size - self.prev_pos).abs() * p * 0.001;
        self.prev_pos = m.position.size;
        let value = self.equity(m.mark);
        let equity = value.unwrap_or(m.pnl - self.costs);
        self.peak = self.peak.max(equity);
        let dd = (self.peak - equity).max(m.drawdown);
        if dd > 0.78 * m.max_drawdown {
            self.halted = true;
        }
        let size = risk_size(m.mark, m.cap, dd, m.max_drawdown);
        let sign = if q > 12.0 {
            1.0
        } else if q < -12.0 {
            -1.0
        } else if m.position.size > 0.0 && q > -10.0 {
            1.0
        } else if m.position.size < 0.0 && q < 10.0 {
            -1.0
        } else {
            0.0
        };
        let mut target = sign * size;
        if self.halted || value.is_none() {
            target = 0.0;
        }
        if m.last && !self.halted {
            target = m.position.size;
        }
        if (target - m.position.size).abs() >= 1.0 {
            o.target_position_with(
                target,
                Execution {
                    slippage_bps: 50.0,
                    ..Execution::default()
                },
            );
            log(&format!(
                "k={} oracle={:.4} edge={:.3} position={:.1} target={:.1} equity={:.2} dd={:.2}",
                m.k, p, q, m.position.size, target, equity, dd
            ));
        }
        Ok(())
    }
}
mtx_kit::export!(ShockBot);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forecast_matches_training_pipeline() {
        let mut bot = ShockBot {
            initial: 143.05,
            scale: 143.11,
            direction: 1.0,
            ..ShockBot::default()
        };
        let prices = [
            143.05, 143.05, 143.05, 143.05, 143.05, 143.05, 143.05, 143.05, 143.05, 143.05, 143.05,
            143.05, 143.05, 143.05, 143.05, 143.05, 143.05, 143.05, 143.05, 143.05, 141.2, 140.8,
            140.8, 140.8, 140.6, 140.6, 140.6, 140.6, 140.6, 140.6, 141.7, 141.7, 142.2, 142.2,
            142.2,
        ];
        let expected = [
            4.3327307281257355,
            4.3327307281257355,
            4.3327307281257355,
            4.3327307281257355,
            4.3327307281257355,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.300537439530432,
            4.879639858731498,
            11.97377112545054,
            15.64467301065266,
            4.699472947810356,
            -13.807511894180521,
            -19.981594169305303,
            -22.54274589163362,
            -22.185734235558577,
            -22.537978360294524,
            -23.8363305847471,
            13.695567592618191,
            15.524968450492183,
            10.962163687749529,
            12.2090366272632,
            10.933874400598288,
        ];
        for (k, p) in prices.iter().enumerate() {
            let x = bot.features(*p, (prices.len() - 1 - k) as u64);
            let y = model::predict(&x);
            assert!(
                (y - expected[k]).abs() < 1e-6,
                "step {}: {} != {}",
                k,
                y,
                expected[k]
            );
        }
    }
}

#[cfg(test)]
mod risk_tests {
    use super::*;
    #[test]
    fn one_jump_and_execution_stay_inside_loss_limit() {
        for price in 90..=200 {
            for dd in 0..=780 {
                let p = price as f64;
                let size = risk_size(p, 70.0, dd as f64, 1000.0);
                assert!((0.0..=70.0).contains(&size));
                let stressed = dd as f64 + size * p * 0.0381 + (70.0 + size) * p * 0.0055;
                assert!(
                    stressed < 1000.0,
                    "price {} dd {} size {} stressed {}",
                    price,
                    dd,
                    size,
                    stressed
                );
            }
        }
    }
}
