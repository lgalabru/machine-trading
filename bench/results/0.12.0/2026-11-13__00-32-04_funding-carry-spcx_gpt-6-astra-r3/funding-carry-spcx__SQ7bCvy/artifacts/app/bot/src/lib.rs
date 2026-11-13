//! SPCX funding carry, with separate opening, trend, and reversion decisions.
//! All observations come from the current turn and read-only Phoenix accounts.
mod risk;

use mtx_kit::{log, prelude::*};
use mtx_kit::venue::{
    PhoenixVenue,
    phoenix_rise::accounts::{perp_asset_map::PerpAssetMap, trader::Trader}
};

#[derive(Default)]
struct Bot {
    target: f64,
    stopped: bool,
    overlay: bool,
    episode_start: f64,
    last_gap: f64,
    noises: Vec<f64>,
    anchor: f64,
    quiet_side: f64,
    center: f64,
    last_bid: f64,
    last_ask: f64,
    history: Vec<f64>,
    last_index: f64,
    last_t: f64,
    trend: f64,
    sign: f64,
    // -1: collect carry; 1: follow the index; 2: opening premium ramp.
    state: f64,
    asset_map: String,
    trader: String,
    asset: u64,
    lot_scale: f64,
    period: f64,
    max_rate: f64,
    funding: f64,
    peak: f64,
}

impl Strategy for Bot {
    fn start(&mut self, s: &Session) -> BotResult {
        let v = PhoenixVenue::from_json(&s.venue)?;
        let m = v.market("SPCX")?;
        self.asset = m.asset_id as u64;
        self.lot_scale = 10f64.powi(m.base_lots_decimals as i32);
        self.asset_map = v.perp_asset_map()?.to_string();
        self.trader = v.trader_account(&mtx_kit::venue::key(&s.authority)?).to_string();
        self.peak = 10000.0;
        self.period = 32.0;
        self.anchor = s.start_oracle;
        Ok(())
    }

    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        // Ignore one-sided depletion caused by our fills; accept refreshed maker quotes.
        let mid = match (m.best_bid, m.best_ask) {
            (Some(b), Some(a)) => {
                if self.center == 0.0 || a - b < 0.051
                    || (b - self.last_bid) * (a - self.last_ask) > 0.0001
                {
                    self.center = (a + b) * 0.5;
                }
                self.last_bid = b;
                self.last_ask = a;
                self.center
            },
            _ => m.mark
        };
        let index = m.oracle;
        if !index.is_finite() || !mid.is_finite() || index <= 0.0 || mid <= 0.0 {
            return Ok(());
        }

        // Actual collateral plus book-valued inventory and unsettled funding.
        let mut equity = 10000.0 + m.pnl;
        if let Ok(data) = mtx_kit::rpc_accounts(&[&self.asset_map, &self.trader]) {
            if let Some(Some(b)) = data.first() {
                if let Ok(map) = PerpAssetMap::try_from_account_bytes(b) {
                    if let Ok(Some(e)) = map.find_by_symbol("SPCX") {
                        let f = e.metadata.funding_accumulator();
                        self.period = f.funding_period_seconds.as_inner() as f64;
                        self.funding = f.cumulative_funding_rate.as_inner() as f64;
                        self.max_rate = f.max_funding_rate.as_inner() as f64
                            * self.lot_scale / 1e6
                            / (f.funding_interval_seconds.as_inner() as f64);
                    }
                }
            }
            if let Some(Some(b)) = data.get(1) {
                if let Ok(t) = Trader::try_from_account_bytes(b) {
                    equity = t.header().trader_state.quote_lot_collateral.as_inner() as f64 / 1e6;
                    for (asset, p) in t.positions() {
                        if asset == self.asset {
                            let lots = p.base_lot_position().as_inner() as f64;
                            equity += lots / self.lot_scale * mid
                                + p.virtual_quote_lot_position().as_inner() as f64 / 1e6;
                            equity -= lots * (self.funding
                                - p.cumulative_funding_snapshot().as_inner() as f64) / 1e6;
                        }
                    }
                }
            }
        }
        self.peak = self.peak.max(equity);
        let dd = self.peak - equity;
        if dd > 0.65 * m.max_drawdown {
            self.stopped = true;
        }

        let gap = mid - index;
        let t = m.k as f64 * 0.5;
        if self.last_index == 0.0 {
            self.last_index = index;
            self.last_t = t;
        }
        let tick = (index - self.last_index).abs() > 0.00001;
        if tick {
            self.noises.push((index - self.last_index).abs());
            if self.noises.len() > 41 {
                self.noises.remove(0);
            }
            self.history.push((index - self.last_index) / (t - self.last_t).max(0.5));
            if self.history.len() > 7 {
                self.history.remove(0);
            }
            self.last_index = index;
            self.last_t = t;
        }

        // Longer, trimmed trend estimates in sessions with larger oracle wobble.
        let mut ns = self.noises.clone();
        ns.sort_by(|a, b| a.total_cmp(b));
        let noise = if ns.is_empty() { 0.05 } else { ns[ns.len() / 2] };
        let window = if noise < 0.09 { 3 } else { 7 };

        // A closed premium resets the reference level for quiet-period reversions.
        if self.sign != 0.0 && (self.sign * gap <= 0.0 || gap.abs() < index * 0.00025) {
            self.sign = 0.0;
            self.state = 0.0;
            self.anchor = index;
            self.quiet_side = 0.0;
            self.overlay = false;
        }

        // A small initial gap can be the beginning of a gradual premium opening.
        if self.sign == 0.0 && gap.abs() > index * 0.0006 {
            self.sign = gap.signum();
            self.state = if gap.abs() < index * 0.0027 { 2.0 } else { -1.0 };
            self.history.clear();
            self.trend = 0.0;
            self.quiet_side = 0.0;
            self.episode_start = t;
            self.last_gap = gap.abs();
        }
        if self.sign != 0.0 {
            let carry = (gap.abs() / self.period).min(self.max_rate);
            if self.state == 2.0
                && ((tick && gap.abs() < self.last_gap * 0.99) || t - self.episode_start > 5.0)
            {
                self.state = -1.0;
            }
            if tick {
                self.last_gap = gap.abs();
            }
            if self.history.len() >= window && self.state != 2.0 {
                let mut h = self.history[self.history.len() - window..].to_vec();
                h.sort_by(|a, b| a.total_cmp(b));
                self.trend = h[1..window - 1].iter().sum::<f64>() / (window - 2) as f64;
                if self.state < 0.0 && self.sign * self.trend > carry + 0.05 {
                    self.state = 1.0;
                } else if self.state > 0.0
                    && self.sign * self.trend < carry - if window == 3 { 0.025 } else { 0.075 }
                {
                    self.state = -1.0;
                }
            }

            // Fade large temporary index moves when the episode still looks crowded.
            if self.state == 1.0 {
                self.anchor = index;
                self.overlay = false;
            } else if self.state < 0.0 {
                let dev = index - self.anchor;
                let edge = -self.sign * dev - gap.abs() * (0.5 + 6.0 / self.period);
                if !self.overlay && edge > 0.3 {
                    self.overlay = true;
                } else if self.overlay && edge < -0.04 {
                    self.overlay = false;
                }
                if tick && dev.abs() < 0.25 {
                    self.anchor += 0.1 * dev;
                }
            } else {
                self.overlay = false;
            }
            self.target = self.sign * if self.state == 2.0 || self.overlay {
                70.0
            } else {
                self.state * 70.0
            };
        } else {
            if tick {
                let dev = index - self.anchor;
                if self.quiet_side == 0.0 && dev.abs() > 0.25 {
                    self.quiet_side = -dev.signum();
                } else if self.quiet_side * dev >= -0.04 {
                    self.quiet_side = 0.0;
                }
                if dev.abs() < 0.25 {
                    self.anchor += 0.1 * dev;
                }
            }
            self.target = self.quiet_side * 70.0;
        }

        let risk_cap = risk::position_cap(mid, dd, m.cap, m.max_drawdown);
        self.target = self.target.clamp(-risk_cap, risk_cap);
        if self.stopped {
            self.target = 0.0;
        }
        if (m.position.size - self.target).abs() > 1.0 || m.k % 20 == 0 {
            log(&format!("k={} mid={:.4} index={:.4} gap={:.4} target={} pos={} eq={:.2} dd={:.2} period={}",m.k,mid,index,gap,self.target,m.position.size,equity,dd,self.period));
        }
        if m.last || (m.steps_left < 2 && !self.stopped) {
            return Ok(());
        }

        // Price each slice from the live book: the last-trade mark can lag a jump.
        let mut missing = self.target - m.position.size;
        let buy_limit = ((m.best_ask.unwrap_or(mid) + 0.60) * 100.0).round() / 100.0;
        let sell_limit = ((m.best_bid.unwrap_or(mid) - 0.60) * 100.0).round() / 100.0;
        for _ in 0..28 {
            if missing.abs() < 0.5 {
                break;
            }
            let size = ((missing.abs() / 0.5).floor() * 0.5).min(5.0);
            if missing > 0.0 {
                o.buy(size, buy_limit);
                missing -= size;
            } else {
                o.sell(size, sell_limit);
                missing += size;
            }
        }
        Ok(())
    }
}

mtx_kit::export!(Bot);
