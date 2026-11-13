//! SPCX carry, price discovery, and short-lived index dislocations.
mod features;
mod short;
mod trees;

use mtx_kit::{
    log, prelude::*, rpc_accounts,
    venue::{self, PhoenixVenue, phoenix_rise::accounts::{
        perp_asset_map::PerpAssetMap, spline_collection::SplineCollection, trader::Trader,
    }},
};

#[derive(Default)]
struct SpcxStrategy {
    features: features::Features,
    heuristic_book: f64,
    smoothed: f64,
    pam: String,
    trader: String,
    spline: String,
    tick: f64,
    asset_id: u64,
    lots_per_unit: f64,
    period: f64,
    interval: f64,
    funding_cap: f64,
    cumulative: f64,
    target: f64,
    peak: f64,
    halted: bool,
    prev_bid: f64,
    prev_ask: f64,
    book: f64,
}

impl Strategy for SpcxStrategy {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market("SPCX")?;
        self.features = features::Features::new(session.start_oracle);
        self.heuristic_book = session.start_mark;
        self.spline = market.spline_pubkey.clone();
        self.tick = venue::ticks_to_usd(market, 1);
        self.asset_id = market.asset_id as u64;
        self.lots_per_unit = 10f64.powi(market.base_lots_decimals as i32);
        self.pam = venue.perp_asset_map()?.to_string();
        self.trader = venue.trader_account(&venue::key(&session.authority)?).to_string();
        self.period = 32.0;
        self.peak = 10_000.0;
        self.book = session.start_mark;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let bid = market.best_bid.unwrap_or(market.mark);
        let ask = market.best_ask.unwrap_or(market.mark);
        let index = if market.index > 0.0 { market.index } else { market.oracle };
        let mut book = (bid + ask) * 0.5;
        // The fallback avoids interpreting our consumption of one side as a new fair price.
        if ask - bid > 0.055 && self.prev_bid > 0.0
            && ((ask - self.prev_ask).abs() < 0.005 || (bid - self.prev_bid).abs() < 0.005)
        {
            book = self.heuristic_book;
        }
        self.heuristic_book = book;
        let heuristic_book = book;
        self.prev_bid = bid;
        self.prev_ask = ask;
        let mut equity = 0.0;
        let mut funding_ok = false;
        let mut account_ok = false;

        // One read, current turn: live funding, actual equity, and makers' price centres.
        if let Ok(accounts) = rpc_accounts(&[&self.pam, &self.trader, &self.spline]) {
            if let Some(Some(data)) = accounts.first() {
                if let Ok(map) = PerpAssetMap::try_from_account_bytes(data) {
                    for entry in map.iter().flatten() {
                        if entry.symbol.as_str() == "SPCX" {
                            let funding = entry.metadata.funding_accumulator();
                            self.period = u64::from(funding.funding_period_seconds) as f64;
                            self.interval = u64::from(funding.funding_interval_seconds) as f64;
                            self.funding_cap = i64::from(funding.max_funding_rate) as f64
                                * self.lots_per_unit / 1e6;
                            self.cumulative = i64::from(funding.cumulative_funding_rate) as f64;
                            funding_ok = self.period > 0.0 && self.interval > 0.0;
                        }
                    }
                }
            }
            if let Some(Some(data)) = accounts.get(2) {
                if let Ok(splines) = SplineCollection::try_from_account_bytes(data) {
                    let mut mids: Vec<_> = splines.splines().flatten()
                        .filter(|spline| spline.is_enabled())
                        .map(|spline| u64::from(spline.mid_price()) as f64 * self.tick)
                        .collect();
                    mids.sort_by(f64::total_cmp);
                    if !mids.is_empty() {
                        let mid = mids[mids.len() / 2];
                        if mid > 0.0 && (mid - book).abs() < book * 0.01 { book = mid; }
                    }
                }
            }
            if let Some(Some(data)) = accounts.get(1) {
                if let Ok(trader) = Trader::try_from_account_bytes(data) {
                    equity = i64::from(trader.header().state().quote_lot_collateral) as f64 / 1e6;
                    for (id, position) in trader.positions() {
                        if id == self.asset_id {
                            let lots = i64::from(position.base_lot_position()) as f64;
                            let snapshot = i64::from(position.cumulative_funding_snapshot()) as f64;
                            equity += i64::from(position.virtual_quote_lot_position()) as f64 / 1e6
                                + lots * book / self.lots_per_unit
                                - lots * (self.cumulative - snapshot) / 1e6;
                        }
                    }
                    account_ok = true;
                }
            }
        }
        self.book = book;
        if !(book > 0.0 && index > 0.0 && book.is_finite() && index.is_finite()) {
            return Ok(());
        }
        if !(account_ok && funding_ok) { self.halted = true; }
        if account_ok && funding_ok { self.peak = self.peak.max(equity); }
        let drawdown = if account_ok && funding_ok { self.peak - equity } else { market.drawdown };
        if drawdown > market.max_drawdown * 0.5 { self.halted = true; }

        let (features, direction) = self.features.step(
            market.k as f64 * 0.5 + 0.5, index, heuristic_book,
            self.period.max(1.0), self.interval.max(1.0), self.funding_cap,
        );
        // Forecasts use the observed book, with our own consumption filtered out.
        let forecast = (trees::predict(&features) + short::predict(&features)) * 0.5 * direction;
        self.smoothed = self.smoothed * 0.5 + forecast * 0.5;
        if self.smoothed > 12.0 { self.target = market.cap; }
        if self.smoothed < -12.0 { self.target = -market.cap; }

        // Reserve room for a five percent adverse move, beyond the normal stop.
        let risk_cap = market.cap.min(
            ((market.max_drawdown * 0.9 - drawdown).max(0.0) / (book * 0.05)).floor()
        );
        self.target = self.target.clamp(-risk_cap, risk_cap);
        if self.halted { self.target = 0.0; }

        // Pace ordinary transitions to reduce impact; risk exits close immediately.
        if !market.last && (self.target - market.position.size).abs() > 0.1 {
            let pace = if self.halted { market.cap * 2.0 } else { 40.0 };
            let delta = (self.target - market.position.size).clamp(-pace, pace);
            let limit = if delta > 0.0 { ask + 0.8 } else { bid - 0.8 };
            let mut remaining = delta.abs();
            while remaining > 0.05 {
                let size = remaining.min(7.0);
                if delta > 0.0 { orders.buy(size, limit); } else { orders.sell(size, limit); }
                remaining -= size;
            }
        }
        if market.k % 5 == 0 || orders.lines().next().is_some() {
            log(&format!("step={} book={book:.3} index={index:.3} position={:.3} target={:.1} forecast={:.2} equity={equity:.2} drawdown={drawdown:.2} halted={}",
                market.k, market.position.size, self.target, self.smoothed, self.halted));
        }
        for sent in &market.sends {
            if let Some(reason) = sent.state.reason() { log(&format!("order: {reason}")); }
        }
        Ok(())
    }
}

mtx_kit::export!(SpcxStrategy);
