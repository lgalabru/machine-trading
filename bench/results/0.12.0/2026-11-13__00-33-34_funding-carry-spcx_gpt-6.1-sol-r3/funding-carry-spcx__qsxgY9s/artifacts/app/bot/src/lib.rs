//! SPCX funding and price-discovery strategy.
//!
//! Forecasts come from trees trained on independently generated public-family
//! scenarios, including synthetic oracle wobble. Runtime inputs are exclusively
//! the session, current market data, and read-only Phoenix accounts.
mod features;
mod model;

use mtx_kit::{log, prelude::*};
use mtx_kit::venue::{
    PhoenixVenue,
    phoenix_rise::accounts::owned::{PerpAssetMapOwned, SplineCollection, Trader},
};

const CLOCK: &str = "SysvarC1ock11111111111111111111111111111111";
const ENTRY_BPS: f64 = 25.0;

#[derive(Default)]
struct Carry {
    features: Option<features::Features>,
    asset_id: u64,
    lots_per_unit: f64,
    usd_per_tick: f64,
    asset_map: String,
    trader: String,
    splines: String,
    equity_peak: f64,
    stopped: bool,
}

struct ChainState {
    centre: f64,
    equity: f64,
    period: f64,
    cap_per_second: f64,
    funding_features: [f64; 6],
}

impl Carry {
    fn read_state(&self, market: &Market, fallback_mid: f64) -> Option<ChainState> {
        let accounts = mtx_kit::rpc_accounts(&[
            &self.asset_map, &self.trader, &self.splines, CLOCK,
        ]).ok()?;
        let asset_data = accounts.first()?.as_deref()?;
        let trader_data = accounts.get(1)?.as_deref()?;
        let assets = PerpAssetMapOwned::try_from_account_bytes(asset_data).ok()?;
        let trader = Trader::try_from_account_bytes(trader_data).ok()?;
        let asset = assets.get_by_symbol("SPCX")?;
        let funding = &asset.funding_accumulator;
        let period = funding.funding_period_seconds.as_inner() as f64;
        let interval = funding.funding_interval_seconds.as_inner() as f64;
        if period <= 0.0 || interval <= 0.0 || self.lots_per_unit <= 0.0 {
            return None;
        }

        // A taker fill consumes one side of the book. Use makers' quoted
        // centres so our own fills cannot create artificial price signals.
        let mut centre = fallback_mid;
        if let Some(Some(data)) = accounts.get(2) {
            if let Ok(splines) = SplineCollection::try_from_account_bytes(data) {
                let newest = splines.active_splines()
                    .filter(|s| s.mid_price.as_inner() > 0)
                    .map(|s| s.user_update_slot).max().unwrap_or(0);
                let mut mids: Vec<u64> = splines.active_splines()
                    .filter(|s| s.mid_price.as_inner() > 0 && s.user_update_slot == newest)
                    .map(|s| s.mid_price.as_inner()).collect();
                mids.sort_unstable();
                if let Some(ticks) = mids.get(mids.len() / 2) {
                    centre = *ticks as f64 * self.usd_per_tick;
                }
            }
        }
        if !centre.is_finite() || centre <= 0.0 { return None; }

        // Settled interval rates can remain unpaid until the next trade.
        // Include that liability in equity even when collateral has not moved.
        let cumulative = funding.cumulative_funding_rate.as_inner() as f64;
        let mut equity = trader.state.quote_lot_collateral.as_inner() as f64 / 1e6;
        for (asset_id, position) in &trader.positions.entries {
            if *asset_id == self.asset_id {
                let lots = position.base_lot_position.as_inner() as f64;
                equity += lots / self.lots_per_unit * centre
                    + position.virtual_quote_lot_position.as_inner() as f64 / 1e6;
                equity -= lots * (cumulative
                    - position.cumulative_funding_snapshot.as_inner() as f64) / 1e6;
            }
        }

        let now = accounts.get(3).and_then(|v| v.as_ref())
            .and_then(|data| data.get(32..40))
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(i64::from_le_bytes)
            .unwrap_or(funding.last_funding_update_timestamp.as_inner() as i64) as f64;
        let last_diff = funding.last_diff.as_inner() as f64 * self.lots_per_unit / 1e6;
        let accrued = funding.acc.as_inner() as f64 * self.lots_per_unit / 1e6 / period;
        let cap_per_second = funding.max_funding_rate.as_inner() as f64
            * self.lots_per_unit / 1e6 / interval;
        let funding_features = [
            1.0 / interval,
            (now - funding.start_interval_timestamp.as_inner() as f64) / interval,
            accrued / centre * 1e4,
            last_diff / market.oracle,
            (centre - market.oracle - last_diff) / market.oracle,
            (now - funding.last_funding_update_timestamp.as_inner() as f64) / 10.0,
        ];
        Some(ChainState { centre, equity, period, cap_per_second, funding_features })
    }

    fn execution(market: &Market, centre: f64) -> Execution {
        // The last-trade mark can lag a scenario jump. Keep the order limit
        // wide enough to reach today's book when reducing risk after that jump.
        let mark_gap = if market.mark > 0.0 {
            (centre / market.mark - 1.0).abs() * 1e4
        } else { 350.0 };
        Execution {
            max_per_step: market.cap * 2.0,
            slice: 5.0,
            slippage_bps: 150.0_f64.max(mark_gap + 100.0),
            min_trade: 0.5,
        }
    }
}

impl Strategy for Carry {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let config = venue.market("SPCX")?;
        self.asset_map = venue.perp_asset_map()?.to_string();
        self.trader = venue.trader_account(&mtx_kit::venue::key(&session.authority)?).to_string();
        self.splines = config.spline_pubkey.clone();
        self.asset_id = config.asset_id as u64;
        self.lots_per_unit = 10_f64.powi(config.base_lots_decimals as i32);
        self.usd_per_tick = config.tick_size as f64
            * 10_f64.powi(config.base_lots_decimals as i32 - 6);
        self.features = Some(features::Features::new(session.start_oracle));
        self.equity_peak = 10_000.0;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        // The final book already determines terminal value. Trading here
        // would incur execution costs without changing the terminal exposure.
        if market.last { return Ok(()); }
        let fallback_mid = match (market.best_bid, market.best_ask) {
            (Some(bid), Some(ask)) => (bid + ask) * 0.5,
            _ => market.mark,
        };
        if !market.oracle.is_finite() || market.oracle <= 0.0 { return Ok(()); }
        let Some(state) = self.read_state(market, fallback_mid) else {
            orders.target_position_with(0.0, Self::execution(market, fallback_mid));
            return Ok(());
        };

        let core = self.features.get_or_insert_with(|| features::Features::new(market.oracle))
            .step(market.oracle, state.centre, state.period, state.cap_per_second,
                market.k as f64 * 0.5, market.slots_left as f64 * 0.25);
        let mut inputs = [0_f32; 39];
        inputs[..33].copy_from_slice(&core);
        for (i, value) in state.funding_features.iter().enumerate() {
            inputs[33 + i] = *value as f32;
        }
        // Six- and fifteen-second net returns, including funding. Each model
        // averages mirrored inputs to preserve the exchange's side symmetry.
        let score = model::predict1(&inputs) + 0.5 * model::predict2(&inputs);
        let mut target = if score > ENTRY_BPS { market.cap }
            else if score < -ENTRY_BPS { -market.cap }
            else if score * market.position.size > 0.0 { market.position.size }
            else { 0.0 };

        self.equity_peak = self.equity_peak.max(state.equity);
        let drawdown = (self.equity_peak - state.equity).max(0.0);
        let limit = market.max_drawdown;
        if drawdown > limit * 0.65 { self.stopped = true; }
        let risk_cap = market.cap * ((limit * 0.85 - drawdown) / (limit * 0.65)).clamp(0.0, 1.0);
        target = target.clamp(-risk_cap, risk_cap);
        if self.stopped { target = 0.0; }
        orders.target_position_with(target, Self::execution(market, state.centre));

        if market.k % 16 == 0 || (target - market.position.size).abs() > 1.0 {
            log(&format!("step={} centre={:.4} index={:.4} forecast_bps={:.2} position={:.2} target={:.2} equity={:.2} drawdown={:.2}",
                market.k, state.centre, market.oracle, score, market.position.size, target, state.equity, drawdown));
        }
        for send in &market.sends {
            if let Some(reason) = send.state.reason() { log(reason); }
        }
        Ok(())
    }
}

mtx_kit::export!(Carry);
