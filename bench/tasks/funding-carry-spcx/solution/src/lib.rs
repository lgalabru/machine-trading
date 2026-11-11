//! Reference: a carry desk fitted to the family, fading the premiums the
//! crowd's positioning opened and following the ones the index follows,
//! with what fading or following earns over the next seconds after funding
//! forecast by trees trained on generated sessions.
//!
//! This is the best of g0's three GPT-6.1 Sol screen bots, frozen (see the
//! task's DESIGN.md, "Generations"): on g1's hidden sessions it made 1.7x the
//! gradient-boosted reference g1 started with, and more than that bot told
//! each episode's kind. Each turn it reads the PerpAssetMap (the index, from
//! the spot oracles' median, and the session's funding parameters), the
//! makers' spline collection (the book's centre, which its own fills don't
//! move) and its trader account, in one `getMultipleAccounts`. It tracks the
//! current premium episode and the session's history (how many episodes the
//! index followed), and an ensemble of four tree models (embedded in
//! `model.bin`: four-second returns after funding, and position values with
//! switching costs) forecasts the return of being long. It holds the usable
//! cap long past +10 bp, short past -10 bp, keeps its side inside the band,
//! and sizes the cap down as the drawdown grows (flat for the session past
//! 62% of the limit). It trades in market orders of up to 20 shares,
//! checking its position after each in the process it began as; as a
//! strategy it counts each slice as filled.
//!
//! Changed from the frozen bot: a turn no longer stops slicing after 1.2 s
//! of wall time (a session must replay the same under any load), and the
//! ladder's `told` rung (`MTX_RUNG=told`) reads each episode's kind from
//! `MTX_REGIME` and corrects the reference's own target with it: never
//! fade an informed premium before the index catches up, follow it with the
//! cap once the index has started to; fade a crowded one with the cap while
//! it lasts, never follow it.

//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`), the anchor hidden sets are
//! scored against, which the ladder also runs as its `told` rung.

use mtx_kit::{
    log,
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

/// The carry desk, as a [`Strategy`].
pub struct Carry {
    setup: Option<Setup>,
    told: Option<Told>,
    start_slot: u64,
    model: model::Model,
    features: model::Features,
    peak: f64,
    stopped: bool,
}

impl Default for Carry {
    fn default() -> Self {
        Self {
            setup: None,
            told: None,
            start_slot: 0,
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
        self.told = Told::from_session(session)?;
        self.start_slot = session.start_slot;
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
        if let Some(told) = &self.told {
            target = told.target(
                step.slot as f64 - self.start_slot as f64,
                target,
                usable_cap,
                book / index - 1.0,
            );
        }
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

/// The ladder's told rung: each episode's kind and timing, in slots from the
/// session's start, from `MTX_REGIME`, for the process only.
struct Told {
    /// Informed, start, follow (the slot the index starts to follow), close,
    /// end, and the premium's side.
    episodes: Vec<(bool, f64, f64, f64, f64, f64)>,
}

impl Told {
    fn from_session(session: &Session) -> BotResult<Option<Self>> {
        if session.var("MTX_RUNG") != Some("told") {
            return Ok(None);
        }
        let regime = session.var("MTX_REGIME").ok_or("told needs MTX_REGIME")?;
        let shape: serde_json::Value = serde_json::from_str(regime)?;
        let episodes = shape["episodes"]
            .as_array()
            .ok_or("no episodes")?
            .iter()
            .map(|e| {
                let n = |k: &str| e[k].as_f64();
                (
                    e["kind"] == "informed",
                    n("start").unwrap_or(0.0),
                    n("follow").unwrap_or(f64::MAX),
                    n("close").unwrap_or(0.0),
                    n("end").unwrap_or(0.0),
                    n("premium_pct").unwrap_or(0.0).signum(),
                )
            })
            .collect();
        Ok(Some(Self { episodes }))
    }

    /// The reference's own target, corrected by what it's told. An informed
    /// premium: never fade it before the index has caught up, and follow it
    /// with the cap once the index has started to follow. A crowded one:
    /// fade it with the cap while it's more than 8 bp, never follow it.
    /// Elsewhere, the reference's target.
    fn target(&self, at: f64, target: f64, cap: f64, gap: f64) -> f64 {
        for &(informed, start, follow, close, end, side) in &self.episodes {
            if !(start..end).contains(&at) {
                continue;
            }
            return match informed {
                true if at >= follow && at < close => cap * side,
                true if at < close && target * side < 0.0 => 0.0,
                false if gap * side > 0.0008 => -cap * side,
                false if target * side > 0.0 => 0.0,
                _ => target,
            };
        }
        target
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Carry);
