//! The funding-carry reference (`bots/carry`) as a wasm strategy: the same
//! reads, features, trees and targets; see `bots/carry/src/main.rs` for the
//! strategy. The trees are `bots/carry`'s own: `src/model.rs`, which embeds
//! `model.bin`, both linked from there.
//!
//! Two differences come from the order sheet. The process reads its position
//! after every slice and stops slicing when one doesn't fill; the component
//! counts each slice as filled, so it sends the slices that reach the target
//! and the grader stops at the first that fails. And it has no `told` rung:
//! the ladder's anchors stay processes.

use mtx_kit::{
    component::log,
    prelude::*,
    rpc_accounts,
    venue::{
        PhoenixVenue,
        phoenix_rise::{
            accounts::{
                perp_asset_map::PerpAssetMap, spline_collection::SplineCollection,
                trader::Trader as RawTrader,
            },
            types::exchange::ExchangeMarketConfig,
        },
    },
};

mod model;

/// What `start` reads of the session.
struct Setup {
    venue: PhoenixVenue,
    market: ExchangeMarketConfig,
    symbol: String,
    /// The perp asset map, the makers' splines and the bot's trader account.
    addresses: [String; 3],
    max_drawdown: f64,
    cap: f64,
}

struct Carry {
    setup: Option<Setup>,
    model: model::Model,
    features: model::Features,
    peak: f64,
    stopped: bool,
}

impl Default for Carry {
    fn default() -> Self {
        Self {
            setup: None,
            model: model::Model::load(),
            features: model::Features::new(),
            peak: 10100.0,
            stopped: false,
        }
    }
}

impl Strategy for Carry {
    fn start(&mut self, session: &Session) -> BotResult {
        let allowance = session
            .mandate
            .universe
            .iter()
            .find(|a| a.instrument.to_string() == "phoenix:perp:SPCX")
            .ok_or("SPCX is outside the mandate")?;
        if session.symbol != "SPCX" {
            return Err("SPCX mandate required".into());
        }
        let max_position = session.cap.min(allowance.max_position);
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?.clone();
        let authority = mtx_kit::venue::key(&session.authority)?;
        let addresses = [
            venue.perp_asset_map()?.to_string(),
            market.spline_pubkey.clone(),
            venue.trader_account(&authority).to_string(),
        ];
        self.setup = Some(Setup {
            venue,
            market,
            symbol: session.symbol.clone(),
            addresses,
            max_drawdown: session
                .mandate
                .limits
                .max_drawdown_usd
                .map_or(session.max_drawdown, |limit| {
                    session.max_drawdown.min(limit)
                }),
            cap: (max_position - 0.1).clamp(0.0, 69.9),
        });
        Ok(())
    }

    fn step(&mut self, step: &Market, orders: &mut Orders) -> BotResult {
        let setup = self.setup.as_ref().ok_or("no session")?;
        let (venue, market) = (&setup.venue, &setup.market);
        let addresses: Vec<&str> = setup.addresses.iter().map(String::as_str).collect();
        let accounts = match rpc_accounts(&addresses) {
            Ok(a) => a,
            Err(e) => {
                log(&format!("read: {e}"));
                return Ok(());
            }
        };
        let [Some(map_data), Some(spline_data), Some(trader_data)] = &accounts[..] else {
            return Ok(());
        };
        let map = PerpAssetMap::try_from_account_bytes(map_data).map_err(|e| e.to_string())?;
        let entry = map
            .find_by_symbol(&setup.symbol)
            .map_err(|e| e.to_string())?
            .ok_or("market missing")?;
        let f = entry.metadata.funding_accumulator();
        let period = f.funding_period_seconds.as_inner() as f64;
        let interval = f.funding_interval_seconds.as_inner() as f64;
        if period < 1.0 || interval < 1.0 {
            return Ok(());
        }
        let reports = &entry.metadata.oracle_price().mark_price;
        let mut ticks: Vec<u64> = reports
            .spot_price_component
            .last_exchange_spot_price
            .iter()
            .map(|p| p.ticks.as_inner())
            .filter(|&x| x > 0 && x != u64::MAX)
            .collect();
        if ticks.is_empty() {
            ticks = reports
                .perp_price_component
                .last_exchange_perp_price
                .iter()
                .map(|p| p.ticks.as_inner())
                .filter(|&x| x > 0 && x != u64::MAX)
                .collect();
        }
        if ticks.is_empty() {
            return Ok(());
        }
        ticks.sort_unstable();
        let index = venue.ticks_to_usd(market, ticks[ticks.len() / 2]);
        let splines =
            SplineCollection::try_from_account_bytes(spline_data).map_err(|e| e.to_string())?;
        let mids: Vec<_> = splines
            .iter()
            .flatten()
            .filter(|s| s.is_enabled())
            .map(|s| (s.user_update_slot(), s.mid_price().as_inner()))
            .collect();
        let Some(newest) = mids.iter().map(|s| s.0).max() else {
            return Ok(());
        };
        let mut prices: Vec<u64> = mids.iter().filter(|s| s.0 == newest).map(|s| s.1).collect();
        prices.sort_unstable();
        let book = venue.ticks_to_usd(market, prices[prices.len() / 2]);
        let raw = RawTrader::try_from_account_bytes(trader_data).map_err(|e| e.to_string())?;
        let mut pos = 0.0;
        let mut virtual_quote = 0.0;
        let mut unsettled = 0.0;
        if let Some((_, p)) = raw.positions().find(|p| p.0 == market.asset_id as u64) {
            let lots = p.base_lot_position().as_inner();
            pos = venue.lots_to_base_units(market, lots);
            virtual_quote = p.virtual_quote_lot_position().as_inner() as f64 / 1e6;
            unsettled = -(f.cumulative_funding_rate.as_inner()
                - p.cumulative_funding_snapshot().as_inner()) as f64
                * lots as f64
                / 1e6;
        }
        let equity = 100.0
            + raw.header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6
            + pos * book
            + virtual_quote
            + unsettled;
        self.peak = self.peak.max(equity);
        let drawdown = self.peak - equity;
        if drawdown > setup.max_drawdown * 0.62 {
            self.stopped = true;
        }
        let funding_cap_usd = f.max_funding_rate.as_inner().abs() as f64
            * 10f64.powi(market.base_lots_decimals as i32 - 6);
        let risk_cap = ((setup.max_drawdown - drawdown - 75.0).max(0.0)
            / (book * 0.04 + funding_cap_usd))
            .min(setup.cap);
        let lots_per_share = 10f64.powi(market.base_lots_decimals as i32);
        let usable_cap = (risk_cap * lots_per_share).floor() / lots_per_share;
        let cap_per_second = f.max_funding_rate.as_inner() as f64
            * 10f64.powi(market.base_lots_decimals as i32 - 6)
            / interval
            / 143.11
            * 10000.0;
        let (x, sign) = self.features.update(index, book, period, cap_per_second);
        let alpha = self.model.predict(&x) * sign;
        let (entry_threshold, exit_threshold) = (10.0, -10.0);
        let mut target = if alpha > entry_threshold {
            usable_cap
        } else if alpha < -entry_threshold {
            -usable_cap
        } else if pos > 0.0 && alpha > exit_threshold {
            usable_cap
        } else if pos < 0.0 && alpha < -exit_threshold {
            -usable_cap
        } else {
            0.0
        };
        if self.stopped {
            target = 0.0;
        }
        log(&format!(
            "s={} book={book:.5} index={index:.5} pos={pos:.2} target={target:.2} alpha={alpha:.3} eq={equity:.3} dd={:.2}",
            step.k,
            self.peak - equity
        ));
        if step.last {
            return Ok(());
        }
        for _ in 0..8 {
            let delta = target - pos;
            if delta.abs() < 0.15 {
                break;
            }
            let qty = delta.abs().min(20.0);
            if delta > 0.0 {
                orders.buy(qty, book * 1.004);
            } else {
                orders.sell(qty, book * 0.996);
            }
            pos += qty.copysign(delta);
        }
        Ok(())
    }
}

mtx_kit::export!(Carry);
