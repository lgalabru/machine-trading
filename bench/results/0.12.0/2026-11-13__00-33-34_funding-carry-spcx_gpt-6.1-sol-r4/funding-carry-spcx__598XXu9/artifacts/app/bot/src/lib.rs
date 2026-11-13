//! SPCX price and funding strategy. All observations are causal account reads.
mod model;
mod signals;
use base64::Engine as _;
use mtx_kit::venue::{
    self, PhoenixVenue,
    phoenix_rise::accounts::{
        perp_asset_map::PerpAssetMap, spline_collection::SplineCollection, trader::Trader,
    },
};
use mtx_kit::{log, prelude::*};

#[derive(Default)]
struct Bot {
    period: f64,
    interval: f64,
    max_funding: f64,
    map_key: String,
    spline_key: String,
    trader_key: String,
    tick: f64,
    quote_scale: f64,
    asset_id: u64,
    equity: f64,
    peak: f64,
    stopped: bool,
    prev_mid: f64,
    prev_pos: f64,
    signals: signals::Signals,
    map_image: Vec<u8>,
    map_offset: usize,
    map_length: usize,
}
impl Strategy for Bot {
    fn start(&mut self, s: &Session) -> BotResult {
        self.period = 36.;
        self.interval = 4.;
        self.tick = 0.01;
        self.quote_scale = 0.001;
        self.max_funding = 0.02;
        if let Ok(v) = PhoenixVenue::from_json(&s.venue) {
            if let Ok(cfg) = v.market("SPCX") {
                self.spline_key = cfg.spline_pubkey.clone();
                self.asset_id = cfg.asset_id as u64;
                self.tick = cfg.tick_size as f64 * 10f64.powi(cfg.base_lots_decimals as i32 - 6);
                self.quote_scale = 10f64.powi(cfg.base_lots_decimals as i32 - 6);
            }
            if let Ok(k) = v.perp_asset_map() {
                self.map_key = k.to_string();
            }
            if let Ok(k) = venue::key(&s.authority) {
                self.trader_key = v.trader_account(&k).to_string();
            }
        }
        self.equity = 10000.;
        self.peak = 10000.;
        Ok(())
    }
    fn step(&mut self, m: &Market, orders: &mut Orders) -> BotResult {
        let mut mid = m
            .best_bid
            .zip(m.best_ask)
            .map(|(a, b)| (a + b) * 0.5)
            .unwrap_or(m.mark);
        let index = if m.index > 0. { m.index } else { m.oracle };
        let mut cum = 0.;
        let mut accrued = 0.;
        let mut cash = None;
        // Cache the asset map layout; refresh only SPCX metadata on later turns.
        if self.map_image.is_empty() {
            if let Ok(Some(bytes)) = mtx_kit::rpc_account(&self.map_key) {
                if let Ok(map) = PerpAssetMap::try_from_account_bytes(&bytes) {
                    if let Ok(Some(e)) = map.find_by_symbol("SPCX") {
                        let needle = e.metadata.as_bytes();
                        if let Some(off) = bytes.windows(needle.len()).position(|w| w == needle) {
                            self.map_offset = off;
                            self.map_length = needle.len();
                        }
                    }
                }
                self.map_image = bytes;
                log(&format!(
                    "map {} slice {} {}",
                    self.map_image.len(),
                    self.map_offset,
                    self.map_length
                ));
            }
        } else if self.map_length > 0 {
            let params=serde_json::json!([self.map_key,{"encoding":"base64","dataSlice":{"offset":self.map_offset,"length":self.map_length}}]).to_string();
            if let Ok(answer) = mtx_kit::rpc("getAccountInfo", &params) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&answer) {
                    if let Some(enc) = v["value"]["data"][0].as_str() {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(enc) {
                            if bytes.len() == self.map_length {
                                self.map_image[self.map_offset..self.map_offset + self.map_length]
                                    .copy_from_slice(&bytes);
                            }
                        }
                    }
                }
            }
        }
        if let Ok(data) = mtx_kit::rpc_accounts(&[&self.spline_key, &self.trader_key]) {
            {
                let bytes = &self.map_image;
                if let Ok(map) = PerpAssetMap::try_from_account_bytes(bytes) {
                    if let Ok(Some(e)) = map.find_by_symbol("SPCX") {
                        let f = e.metadata.funding_accumulator();
                        self.period = (f.funding_period_seconds.as_inner() as f64).max(1.);
                        self.interval = (f.funding_interval_seconds.as_inner() as f64).max(1.);
                        self.max_funding = f.max_funding_rate.as_inner() as f64 * self.quote_scale;
                        cum = f.cumulative_funding_rate.as_inner() as f64 * self.quote_scale;
                        accrued = (f.acc.as_inner() as f64 / self.period * self.quote_scale)
                            .clamp(-self.max_funding, self.max_funding);
                    }
                }
            }
            // Spline centres remain stable when our own fills consume the book.
            if let Some(Some(bytes)) = data.get(0) {
                if let Ok(splines) = SplineCollection::try_from_account_bytes(bytes) {
                    let mut prices: Vec<f64> = splines
                        .iter()
                        .filter_map(|s| s.ok())
                        .filter(|s| s.is_enabled())
                        .map(|s| s.mid_price().as_inner() as f64 * self.tick)
                        .collect();
                    prices.sort_by(|a, b| a.total_cmp(b));
                    if !prices.is_empty() {
                        mid = prices[prices.len() / 2];
                    }
                }
            }
            if let Some(Some(bytes)) = data.get(1) {
                if let Ok(t) = Trader::try_from_account_bytes(bytes) {
                    // Account equity includes trading fees and settled funding.
                    let mut eq = t.header().state().quote_lot_collateral.as_inner() as f64 / 1e6;
                    for (asset, p) in t.positions() {
                        if asset as u32 as u64 == self.asset_id || asset == self.asset_id {
                            let pos =
                                p.base_lot_position().as_inner() as f64 * 1e-6 / self.quote_scale;
                            let q = p.virtual_quote_lot_position().as_inner() as f64 / 1e6;
                            let snapshot = p.cumulative_funding_snapshot().as_inner() as f64
                                * self.quote_scale;
                            eq += pos * mid + q - pos * (cum - snapshot);
                        }
                    }
                    cash = Some(eq);
                }
            }
        }
        if let Some(eq) = cash {
            self.equity = eq;
        }
        self.peak = self.peak.max(self.equity);
        if self.peak - self.equity > 650. {
            self.stopped = true;
        }
        let _gap = mid - index;
        // Forecasts are percent log returns after funding, before execution costs.
        let cap_pct = self.max_funding / self.interval / index * 100.;
        let (x, direction) = self.signals.observe(index, mid, self.period, cap_pct);
        let prediction = model::predict(&x) * direction * (m.slots_left as f64 * 0.25 / 6.).min(1.);
        let mut desired = m.position.size;
        if prediction > 0.10 {
            desired = m.cap;
        } else if prediction < -0.10 {
            desired = -m.cap;
        }

        let dd = (self.peak - self.equity).max(0.);
        // Reserve $150 plus room for a further 6% adverse move.
        let safe_cap = ((m.max_drawdown - dd - 150.) / (mid * 0.06)).clamp(0., m.cap);
        desired = desired.clamp(-safe_cap, safe_cap);
        if self.stopped {
            desired = 0.;
        }
        if m.last {
            return Ok(());
        }
        // Five-share slices avoid large Phoenix compute-budget transactions.
        orders.target_position_with(
            desired,
            Execution {
                max_per_step: 140.,
                slice: 5.,
                slippage_bps: 750.,
                min_trade: 0.1,
            },
        );
        if m.k % 5 == 0 || (self.prev_pos - m.position.size).abs() > 1. {
            log(&format!(
                "Q {} {:.4} {:.4} {:.4} {:.2} {:.2} {:.3} {:.3} {:.3} {:.4} {:.4} pred {:.5}",
                m.k,
                mid,
                index,
                m.mark,
                m.position.size,
                desired,
                self.equity,
                cum,
                accrued,
                m.best_bid.unwrap_or(0.),
                m.best_ask.unwrap_or(0.),
                prediction
            ));
        }
        self.prev_mid = mid;
        self.prev_pos = m.position.size;
        Ok(())
    }
}
mtx_kit::export!(Bot);
