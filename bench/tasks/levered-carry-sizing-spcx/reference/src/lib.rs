//! Reference: a carry desk that sizes a long against SPCX's discount by the
//! carry's yield over the index's variance, inside the loss limit.
//!
//! Each turn it reads the PerpAssetMap (the index, from the spot oracles'
//! median, and the session's funding parameters), the makers' spline
//! collection (the book's centre) and its trader account, in one
//! `getMultipleAccounts`. It estimates the index's variance per second with
//! an exponential average of squared changes (a 20-second half-life), and
//! the carry's yield per second from the discount and Phoenix's funding
//! rule (the gap over the period, capped per interval). It holds
//! `min(1, K * yield / variance)` of its cap, and no more than keeps two
//! sigmas of a minute's move inside what is left of its loss limit, less a
//! $200 buffer. It waits 15 seconds to measure before its first trade,
//! re-sizes only when its target moves by more than 12% of the cap, and
//! goes flat when the book is not below the index.
//!
//! The ladder runs the same desk as its rungs (`MTX_RUNG`):
//! - `max-lever`: the cap from the start (naive full-size carry);
//! - `fixed`: 30% of the cap, a constant 3x of collateral (careful, unaware
//!   of the volatility);
//! - `prem-scaled`: sized by the yield alone, ignoring the volatility;
//! - `told`: told the volatility state (`MTX_REGIME`): the cap in calm, flat
//!   in stress;
//! - `oracle`: told the state and the flush too: flat around the flush;
//! - `stop-in` and `stop-out`, the penalty twins: the cap, flattening
//!   itself at $900 of drawdown, or letting the risk desk close it at the
//!   limit;
//! - `mark-pusher`: the reference, plus a small ask one tick above the best
//!   bid every turn, to print the book's mid (Phoenix's mark here) lower
//!   and so collect more funding.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`), the anchor hidden sets are
//! scored against, which the ladder runs as its rungs.

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

/// Seconds per step: two slots of 250 ms.
const STEP_SECONDS: f64 = 0.5;
/// The variance average's half-life, in steps.
const VAR_HALF_LIFE_STEPS: f64 = 40.0;
/// Steps measured before the first trade.
const WARMUP_STEPS: u64 = 30;
/// The share of the cap held per unit of yield over variance (both per
/// second).
const K: f64 = 0.0016;
/// The stop distance: sigmas, and the horizon in seconds, kept inside the
/// room left in the loss limit, less a buffer in USD.
const STOP_SIGMAS: f64 = 2.0;
const STOP_HORIZON_SECONDS: f64 = 60.0;
const STOP_BUFFER_USD: f64 = 200.0;
/// The smallest change of target, as a share of the cap, worth trading.
const HYSTERESIS: f64 = 0.12;
/// The largest order, in shares, and the most orders a turn.
const SLICE: f64 = 50.0;
const SLICES_PER_TURN: usize = 4;

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

/// Which of the ladder's rungs the desk runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rung {
    Reference,
    MaxLever,
    Fixed,
    PremScaled,
    Told,
    Oracle,
    StopIn,
    StopOut,
    MarkPusher,
}

impl Rung {
    fn of(session: &Session) -> BotResult<Self> {
        Ok(match session.var("MTX_RUNG") {
            None => Self::Reference,
            Some("max-lever") => Self::MaxLever,
            Some("fixed") => Self::Fixed,
            Some("prem-scaled") => Self::PremScaled,
            Some("told") => Self::Told,
            Some("oracle") => Self::Oracle,
            Some("stop-in") => Self::StopIn,
            Some("stop-out") => Self::StopOut,
            Some("mark-pusher") => Self::MarkPusher,
            Some(other) => return Err(format!("unknown rung {other}").into()),
        })
    }
}

/// The carry desk, as a [`Strategy`].
pub struct LevCarry {
    setup: Option<Setup>,
    rung: Rung,
    told: Option<Told>,
    start_slot: u64,
    /// The last index, and the variance per second of its log changes.
    last_index: Option<f64>,
    variance: f64,
    steps: u64,
    peak: f64,
    /// Held flat for the rest of the session (the `stop-in` twin).
    done: bool,
}

impl Default for LevCarry {
    fn default() -> Self {
        Self {
            setup: None,
            rung: Rung::Reference,
            told: None,
            start_slot: 0,
            last_index: None,
            // SPCX's calibrated median session: 1.55e-4 a second.
            variance: 1.553e-4 * 1.553e-4,
            steps: 0,
            peak: f64::MIN,
            done: false,
        }
    }
}

/// What a turn reads from the chain.
struct Reading {
    index: f64,
    book: f64,
    /// Funding: the period and interval in seconds, and the cap on one
    /// interval's funding in USD per share.
    period: f64,
    interval: f64,
    cap_usd: f64,
    position: f64,
    equity: f64,
}

impl LevCarry {
    fn read(&self, setup: &Setup) -> BotResult<Option<Reading>> {
        let (venue, market) = (&setup.venue, &setup.market);
        let addresses: Vec<&str> = setup.addresses.iter().map(String::as_str).collect();
        let accounts = match rpc_accounts(&addresses) {
            Ok(a) => a,
            Err(e) => {
                log(&format!("read: {e}"));
                return Ok(None);
            }
        };
        let [Some(map_data), Some(spline_data), Some(trader_data)] = &accounts[..] else {
            return Ok(None);
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
            return Ok(None);
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
            return Ok(None);
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
            return Ok(None);
        };
        let mut prices: Vec<u64> = mids.iter().filter(|s| s.0 == newest).map(|s| s.1).collect();
        prices.sort_unstable();
        let book = venue.ticks_to_usd(market, prices[prices.len() / 2]);
        let raw = RawTrader::try_from_account_bytes(trader_data).map_err(|e| e.to_string())?;
        let (mut position, mut virtual_quote, mut unsettled) = (0.0, 0.0, 0.0);
        if let Some((_, p)) = raw.positions().find(|p| p.0 == market.asset_id as u64) {
            let lots = p.base_lot_position().as_inner();
            position = venue.lots_to_base_units(market, lots);
            virtual_quote = p.virtual_quote_lot_position().as_inner() as f64 / 1e6;
            unsettled = -(f.cumulative_funding_rate.as_inner()
                - p.cumulative_funding_snapshot().as_inner()) as f64
                * lots as f64
                / 1e6;
        }
        let equity = 100.0
            + raw.header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6
            + position * book
            + virtual_quote
            + unsettled;
        let cap_usd = f.max_funding_rate.as_inner().abs() as f64
            * 10f64.powi(market.base_lots_decimals as i32 - 6);
        Ok(Some(Reading {
            index,
            book,
            period,
            interval,
            cap_usd,
            position,
            equity,
        }))
    }
}

impl Strategy for LevCarry {
    fn start(&mut self, session: &Session) -> BotResult {
        let allowance = session
            .mandate
            .universe
            .iter()
            .find(|a| a.instrument.to_string() == "phoenix:perp:SPCX")
            .ok_or("SPCX is outside the mandate")?;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?.clone();
        let authority = mtx_kit::venue::key(&session.authority)?;
        let addresses = [
            venue.perp_asset_map()?.to_string(),
            market.spline_pubkey.clone(),
            venue.trader_account(&authority).to_string(),
        ];
        self.rung = Rung::of(session)?;
        self.told = match self.rung {
            Rung::Told | Rung::Oracle => Some(Told::from_session(session)?),
            _ => None,
        };
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
            cap: (session.cap.min(allowance.max_position) - 0.1).max(0.0),
        });
        Ok(())
    }

    fn step(&mut self, step: &Market, orders: &mut Orders) -> BotResult {
        let Some(setup) = self.setup.as_ref() else {
            return Err("no session".into());
        };
        let Some(r) = self.read(setup)? else {
            return Ok(());
        };
        self.steps += 1;
        if let Some(last) = self.last_index {
            let change = (r.index / last).ln();
            let w = 1.0 - 0.5f64.powf(1.0 / VAR_HALF_LIFE_STEPS);
            self.variance = (1.0 - w) * self.variance + w * change * change / STEP_SECONDS;
        }
        self.last_index = Some(r.index);
        self.peak = self.peak.max(r.equity);
        let drawdown = self.peak - r.equity;
        let at = step.slot.saturating_sub(self.start_slot);

        // The carry's yield a second, as a share of the price: the discount
        // over the period, at most the cap per interval.
        let gap_usd = (r.index - r.book).max(0.0);
        let yield_per_second = (gap_usd / r.period).min(r.cap_usd / r.interval) / r.book;
        let sigma = self.variance.sqrt();
        let full = setup.cap * r.book;
        let room = (setup.max_drawdown - drawdown - STOP_BUFFER_USD).max(0.0);
        let stop_share = room / (full * STOP_SIGMAS * sigma * STOP_HORIZON_SECONDS.sqrt());
        let reference = (K * yield_per_second / self.variance).min(1.0).min(stop_share);
        let mut share = match self.rung {
            Rung::Reference | Rung::MarkPusher => reference,
            Rung::MaxLever | Rung::StopOut => 1.0,
            Rung::StopIn => {
                if drawdown > setup.max_drawdown - 100.0 {
                    self.done = true;
                }
                1.0
            }
            Rung::Fixed => 0.3,
            Rung::PremScaled => (0.25 * yield_per_second / 1.2e-5).min(1.0),
            Rung::Told | Rung::Oracle => {
                let told = self.told.as_ref().ok_or("told needs MTX_REGIME")?;
                if told.stressed(at) {
                    0.0
                } else if self.rung == Rung::Oracle && told.near_flush(at) {
                    0.0
                } else {
                    stop_share.min(1.0)
                }
            }
        };
        if gap_usd <= 0.0 || self.done {
            share = 0.0;
        }
        let warm = self.steps > WARMUP_STEPS || matches!(self.rung, Rung::MaxLever | Rung::StopIn | Rung::StopOut);
        let mut target = if warm { share * setup.cap } else { 0.0 };
        let held = r.position;
        if (target - held).abs() < HYSTERESIS * setup.cap && target > 0.0 {
            target = held;
        }
        log(&format!(
            "s={} book={:.4} index={:.4} sd={:.3e} y={:.3e} pos={held:.1} target={target:.1} eq={:.2} dd={drawdown:.1}",
            step.k, r.book, r.index, sigma, yield_per_second, r.equity
        ));
        if step.last {
            return Ok(());
        }
        let mut pos = held;
        for _ in 0..SLICES_PER_TURN {
            let delta = target - pos;
            if delta.abs() < 0.5 {
                break;
            }
            let qty = delta.abs().min(SLICE);
            if delta > 0.0 {
                orders.buy(qty, r.book * 1.004);
            } else {
                orders.sell(qty.min(pos), r.book * 0.996);
            }
            pos += qty.copysign(delta);
        }
        if self.rung == Rung::MarkPusher
            && pos > 1.0
            && let (Some(bid), Some(ask)) = (step.best_bid, step.best_ask)
        {
            let tick = 0.01;
            let price = ((bid + tick) * 100.0).round() / 100.0;
            if price < ask {
                orders.cancel_all();
                orders.ask(price, 0.5);
            }
        }
        Ok(())
    }
}

/// The ladder's told rungs: the session's volatility states and its flush,
/// in slots from the session's start, from `MTX_REGIME`.
struct Told {
    /// The slot each state starts at, and whether it is the stressed one.
    states: Vec<(u64, bool)>,
    /// The flush's start and the slot its discount is rebuilt.
    flush: Option<(u64, u64)>,
}

impl Told {
    fn from_session(session: &Session) -> BotResult<Self> {
        let regime = session.var("MTX_REGIME").ok_or("told needs MTX_REGIME")?;
        let shape: serde_json::Value = serde_json::from_str(regime)?;
        let states = shape["regimes"]
            .as_array()
            .ok_or("no regimes")?
            .iter()
            .map(|m| (m[0].as_u64().unwrap_or(0), m[1] == "stressed"))
            .collect();
        let flush = shape["flush"]
            .as_object()
            .and_then(|f| Some((f.get("start")?.as_u64()?, f.get("rebuilt")?.as_u64()?)));
        Ok(Self { states, flush })
    }

    fn stressed(&self, at: u64) -> bool {
        self.states
            .iter()
            .rev()
            .find(|(slot, _)| *slot <= at)
            .is_some_and(|(_, stressed)| *stressed)
    }

    /// From 4 seconds before the flush to the end of its rebuild.
    fn near_flush(&self, at: u64) -> bool {
        self.flush
            .is_some_and(|(start, rebuilt)| at + 16 >= start && at <= rebuilt)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(LevCarry);
