mod model;
mod risk;
mod signal;
use mtx_kit::prelude::*;
use mtx_kit::{Phoenix, Pubkey};
use tracing::{info, warn};

#[derive(Default)]
struct ShockBot {
    signal: signal::Signal,
    forecast: model::Forecast,
    phoenix: Option<Phoenix>,
    authority: Option<Pubkey>,
    last_position: f64,
    costs: f64,
    peak_estimate: f64,
    peak_equity: f64,
    halted: bool,
}
impl Strategy for ShockBot {
    async fn start(&mut self, s: &Session) -> BotResult {
        if s.env.symbol != "SPCX" {
            return Err("only SPCX is permitted".into());
        }
        self.phoenix = Some(Phoenix::new(s.env.exchange()?, s.env.rpc_url.clone()));
        self.authority = Some(s.trader.authority());
        Ok(())
    }
    async fn step(&mut self, m: &Market, o: &mut Orders<'_>) -> BotResult {
        if m.last {
            return Ok(());
        }
        if !(m.oracle.is_finite() && m.oracle > 0. && m.mark.is_finite() && m.mark > 0.) {
            o.target_position_with(
                0.,
                Execution {
                    slippage_bps: 100.,
                    ..Execution::default()
                },
            );
            return Ok(());
        }
        // This conservative estimate is used only by offline tests; live risk uses equity.
        self.costs += (m.position.size - self.last_position).abs() * m.mark * 0.0012;
        self.last_position = m.position.size;
        let estimate = m.pnl - self.costs;
        self.peak_estimate = self.peak_estimate.max(estimate);
        let mut dd = self.peak_estimate - estimate;
        let mut risk_read_ok = true;
        if let (Some(ph), Some(authority)) = (&self.phoenix, self.authority) {
            match ph.hawkeye(authority).view_margin(authority, 0, 0).await {
                Ok(read) => {
                    let margin = read.value;
                    let equity = (margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots)
                        as f64
                        / 1e6;
                    self.peak_equity = self.peak_equity.max(equity);
                    dd = self.peak_equity - equity;
                    if margin.is_liquidatable != 0 {
                        self.halted = true;
                    }
                }
                Err(error) => {
                    warn!(%error,"margin read failed; flattening until the next reliable read");
                    risk_read_ok = false;
                }
            }
        }
        if risk::should_halt(dd, m.max_drawdown) {
            self.halted = true;
        }
        let mut target = 0.;
        let mut pred = [0.; 3];
        // The oracle avoids treating the impact of our own fills as a fresh shock.
        if let Some((x, d)) = self.signal.update(m.oracle, m.steps_left) {
            pred = self.forecast.predict(&x).map(|v| v * d);
            if pred[0].abs() > 0.09 {
                target = m.cap * pred[0].signum();
            } else if pred[1].abs() > 0.12 {
                target = m.cap * pred[1].signum();
            } else if m.position.size != 0. && m.position.size * pred[1] > -0.15 * m.cap {
                target = m.position.size;
            }
        }
        // Leave enough room for the next price-limit jump plus execution costs.
        let safe = risk::position_limit(m.mark, dd, m.max_drawdown, m.cap);
        target = target.clamp(-safe, safe).trunc();
        if self.halted || !risk_read_ok || m.last {
            target = 0.;
        }
        if (target - m.position.size).abs() >= 1. {
            o.target_position_with(
                target,
                Execution {
                    slippage_bps: if self.halted && risk_read_ok {
                        risk::exit_slippage_bps(m.mark, m.position.size, dd, m.max_drawdown)
                    } else {
                        100.
                    },
                    ..Execution::default()
                },
            );
        }
        info!(
            k = m.k,
            mark = m.mark,
            oracle = m.oracle,
            position = m.position.size,
            target,
            dd,
            edge = pred[0],
            switching_value = pred[1],
            "step"
        );
        Ok(())
    }
}
fn main() -> BotResult {
    mtx_kit::run(ShockBot::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loss_halt_flattens_and_stays_halted() {
        let mut bot = ShockBot::default();
        let mut market = Market {
            mark: 143.,
            oracle: 143.,
            cap: 70.,
            max_drawdown: 1000.,
            pnl: -800.,
            position: Position {
                size: 70.,
                entry_usd: 143.,
            },
            ..Market::default()
        };
        let orders = step_offline(&mut bot, &market).unwrap();
        assert!(matches!(orders.first(),Some(Intent::Target {position,..}) if *position==0.));
        market.k = 1;
        market.pnl = 100.;
        market.oracle = 150.;
        market.mark = 150.;
        let orders = step_offline(&mut bot, &market).unwrap();
        assert!(matches!(orders.first(),Some(Intent::Target {position,..}) if *position==0.));
    }
    #[test]
    fn final_valuation_turn_does_not_pay_for_an_unnecessary_trade() {
        let mut bot = ShockBot::default();
        let market = Market {
            last: true,
            mark: 143.,
            oracle: 143.,
            cap: 70.,
            max_drawdown: 1000.,
            position: Position {
                size: 70.,
                entry_usd: 140.,
            },
            ..Market::default()
        };
        assert!(step_offline(&mut bot, &market).unwrap().is_empty());
    }
}
