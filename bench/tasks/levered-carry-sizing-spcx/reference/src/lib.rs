//! Reference: a carry desk that sizes a long against SPCX's discount
//! by the carry's yield over the book's variance, inside the loss limit.
//!
//! Each turn it reads the PerpAssetMap (the index, from the spot oracles'
//! median, and the session's funding parameters), the makers' spline
//! collection (the book's centre) and its trader account, in one
//! `getMultipleAccounts`. It estimates the variance per second of the book
//! it is valued at (the index's moves and the discount's) with an
//! exponential average of squared changes (a 20-second half-life), and
//! the carry's yield per second from the discount and Phoenix's funding
//! rule (the gap over the period, capped per interval). It holds
//! `min(1, K * yield / variance)` of its cap, and no more than keeps two
//! sigmas of a minute's move inside what is left of its loss limit, less a
//! $200 buffer. Until the session's flush has come it also keeps a 2% gap
//! of the book inside that room: the flush is a jump, and a stop distance
//! in sigmas doesn't cover it. It waits 15 seconds to measure before its
//! first trade, re-sizes only when its target moves by more than 12% of the
//! cap, trades 50 shares a turn (the book's impact grows with the turn's
//! size), and goes flat when the book is not below the index.
//!
//! It also watches the crowd: the discount's churn, each change squared
//! over the time it took and the discount's own level, averaged over about
//! eight changes. When that runs above (3.5% a root-second)^2, between the
//! calm churn and the stressed one, it reads the market as stressed and
//! holds nothing until the churn has settled below 80% of that line, and
//! for 30 seconds after.
//!
//! And it watches for the flush: the discount closes as the book falls, so
//! a discount under 0.1% of the index means the session's one flush has
//! come. It holds nothing for a minute while the discount rebuilds, then
//! drops the gap from its sizing: no second flush can come.
//!
//! The ladder runs the same desk as its rungs (`MTX_RUNG`):
//! - `max-lever`: the cap from the start (naive full-size carry);
//! - `fixed`: 30% of the cap, a constant 3x of collateral (careful, unaware
//!   of the volatility);
//! - `prem-scaled`: sized by the yield alone, ignoring the volatility;
//! - `informed`: the informed bot (`checks/bots/informed`, scoring v2's 1),
//!   told the session's process (`MTX_REGIME`: the discount's level and
//!   each state's volatility level), never its draws. It reads the state
//!   with a two-state filter on the discount's changes (the stated chain,
//!   switching at minute marks; the stated targets and jitters), sizes by
//!   yield over variance inside two sigmas of a minute, holds the full size
//!   under a 15% chance of stress and nothing over 40%, keeps no gap
//!   distance, and handles the flush as the reference does;
//! - `oracle`: a diagnostic, told the session's draws (`tell_regime`): the
//!   cap in calm, flat in stress and around the flush;
//! - `stop-in` and `stop-out`, the penalty twins: the cap, flattening
//!   itself at once (in orders as large as the desk's) at $800 of
//!   drawdown, or letting the risk desk close it at the limit;
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
const K: f64 = 0.006;
/// The stop distance: sigmas, and the horizon in seconds, kept inside the
/// room left in the loss limit, less a buffer in USD.
const STOP_SIGMAS: f64 = 2.0;
const STOP_HORIZON_SECONDS: f64 = 60.0;
const STOP_BUFFER_USD: f64 = 200.0;
/// The discount's churn, squared, that reads as stress; the share of it
/// that reads as calm again; and the average's weight per change.
const CHURN_STRESS: f64 = 0.035 * 0.035;
const CHURN_CALM: f64 = 0.8;
const CHURN_WEIGHT: f64 = 0.12;
/// The smallest change of target, as a share of the cap, worth trading.
const HYSTERESIS: f64 = 0.12;
/// The largest order, in shares, and the most orders a turn.
const SLICE: f64 = 50.0;
const SLICES_PER_TURN: usize = 1;
/// The flush's gap kept inside the room until it has come, as a share of
/// the book.
const GAP: f64 = 0.02;
/// Steps the stressed reading is held after the churn has settled.
const STRESS_HOLD_STEPS: u64 = 60;
/// The discount, as a share of the index, under which the flush has come,
/// and the steps held flat after it while the discount rebuilds.
const FLUSH_DISCOUNT: f64 = 0.001;
const REBUILD_STEPS: u64 = 120;
/// The oracle rung: its smallest change of target, and the slots it waits
/// after the flush for the discount to rebuild (the stated longest, 90 s).
const INFORMED_HYSTERESIS: f64 = 0.05;
const INFORMED_REBUILD_SLOTS: u64 = 360;
/// The informed bot holds the full size under this probability of stress,
/// nothing above the second, and scales between.
const INFORMED_CALM: f64 = 0.15;
const INFORMED_STRESSED: f64 = 0.4;
/// The stated state chain: the stressed state's odds at the start, and each
/// minute's odds of switching, calm to stressed and back.
const STRESS_PRIOR: f64 = 0.3;
const TO_STRESS: f64 = 0.085;
const TO_CALM: f64 = 0.2125;
const SLOTS_PER_MINUTE: u64 = 240;
/// The discount's stated dynamics: its pull toward its target (seconds),
/// its jitter a root-second as a share of its level in calm and in stress,
/// and the stressed target as a multiple of the level.
const DISCOUNT_TAU: f64 = 60.0;
const DISCOUNT_NOISE: [f64; 2] = [0.02, 0.05];
const STRESSED_WIDENING: f64 = 1.5;
/// The Student-t's degrees of freedom for the discount's changes.
const FILTER_NU: f64 = 8.0;
/// SPCX's calibrated median session: 1.55e-4 a second.
const MEDIAN_SIGMA: f64 = 1.553e-4;

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
    Informed,
    Oracle,
    StopIn,
    StopOut,
    MarkPusher,
}

impl Rung {
    fn of(session: &Session, default: Self) -> BotResult<Self> {
        Ok(match session.var("MTX_RUNG") {
            None => default,
            Some("max-lever") => Self::MaxLever,
            Some("fixed") => Self::Fixed,
            Some("prem-scaled") => Self::PremScaled,
            Some("informed") => Self::Informed,
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
    /// The rung the desk runs as when the session names none.
    default_rung: Rung,
    /// The last step the crowd read as stressed, and the step the flush
    /// was seen at.
    last_stressed: Option<u64>,
    flush_at: Option<u64>,
    setup: Option<Setup>,
    rung: Rung,
    told: Option<Told>,
    /// The informed bot's state filter, from the session's process.
    filter: Option<Filter>,
    start_slot: u64,
    /// The last book level, and the variance per second of its log changes.
    last_book: Option<f64>,
    variance: f64,
    steps: u64,
    peak: f64,
    /// Held flat for the rest of the session (the `stop-in` twin).
    done: bool,
    crowd: Crowd,
}

/// The crowd watch: the last discount and the step it changed at, its calm
/// level, its churn, the changes seen, and the reading.
struct Crowd {
    last_discount: Option<(f64, u64)>,
    calm_discount: Option<f64>,
    churn: f64,
    changes: u32,
    stressed: bool,
}

impl LevCarry {
    /// The informed bot: told the session's latent state (the ladder's
    /// `tell_regime`), as `checks/bots/informed` runs it.
    pub fn informed() -> Self {
        Self {
            default_rung: Rung::Informed,
            ..Self::default()
        }
    }
}

impl Default for LevCarry {
    fn default() -> Self {
        Self {
            default_rung: Rung::Reference,
            last_stressed: None,
            flush_at: None,
            setup: None,
            rung: Rung::Reference,
            told: None,
            filter: None,
            start_slot: 0,
            last_book: None,
            variance: MEDIAN_SIGMA * MEDIAN_SIGMA,
            steps: 0,
            peak: f64::MIN,
            done: false,
            crowd: Crowd {
                last_discount: None,
                calm_discount: None,
                churn: CHURN_STRESS,
                changes: 0,
                stressed: true,
            },
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
        self.rung = Rung::of(session, self.default_rung)?;
        self.told = match self.rung {
            Rung::Oracle => Some(Told::from_session(session)?),
            _ => None,
        };
        if self.rung == Rung::Informed {
            self.filter = Some(Filter::from_session(session)?);
        }
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
        // The book's first steps take it to the session's discount: not a
        // move to measure.
        if let Some(last) = self.last_book.filter(|_| self.steps > 3) {
            let change = (r.book / last).ln();
            let w = 1.0 - 0.5f64.powf(1.0 / VAR_HALF_LIFE_STEPS);
            self.variance = (1.0 - w) * self.variance + w * change * change / STEP_SECONDS;
        }
        self.last_book = Some(r.book);
        self.peak = self.peak.max(r.equity);
        let drawdown = self.peak - r.equity;
        let at = step.slot.saturating_sub(self.start_slot);
        let discount = 1.0 - r.book / r.index;
        self.crowd.watch(discount, step.k);
        if self.crowd.stressed {
            self.last_stressed = Some(step.k);
        }
        if self.flush_at.is_none() && self.steps > WARMUP_STEPS && discount < FLUSH_DISCOUNT {
            self.flush_at = Some(step.k);
        }
        let flushed = self.flush_at.is_some_and(|k| step.k > k + REBUILD_STEPS);
        let rebuilding = self.flush_at.is_some() && !flushed;
        let stressed = self
            .last_stressed
            .is_some_and(|k| step.k <= k + STRESS_HOLD_STEPS);

        // The carry's yield a second, as a share of the price: the discount
        // over the period, at most the cap per interval.
        let gap_usd = (r.index - r.book).max(0.0);
        let yield_per_second = (gap_usd / r.period).min(r.cap_usd / r.interval) / r.book;
        let sigma = self.variance.sqrt();
        let full = setup.cap * r.book;
        let room = (setup.max_drawdown - drawdown - STOP_BUFFER_USD).max(0.0);
        let stop_share = room / (full * STOP_SIGMAS * sigma * STOP_HORIZON_SECONDS.sqrt());
        let mut reference = (K * yield_per_second / self.variance)
            .min(1.0)
            .min(stop_share);
        if !flushed {
            reference = reference.min(room / (full * GAP));
        }
        let mut share = match self.rung {
            Rung::Reference | Rung::MarkPusher => {
                if stressed || rebuilding {
                    0.0
                } else {
                    reference
                }
            }
            Rung::MaxLever | Rung::StopOut => 1.0,
            Rung::StopIn => {
                if drawdown > setup.max_drawdown - 200.0 {
                    self.done = true;
                }
                1.0
            }
            Rung::Fixed => 0.3,
            Rung::PremScaled => (0.25 * yield_per_second / 1.2e-5).min(1.0),
            Rung::Informed => {
                let filter = self
                    .filter
                    .as_mut()
                    .ok_or("the informed bot needs MTX_REGIME")?;
                let p = filter.update(discount, at, step.k);
                let sigma = MEDIAN_SIGMA * (p * filter.levels[1] + (1.0 - p) * filter.levels[0]);
                let stop = room / (full * STOP_SIGMAS * sigma * STOP_HORIZON_SECONDS.sqrt());
                let sized = (K * yield_per_second / (sigma * sigma)).min(1.0).min(stop);
                if rebuilding {
                    0.0
                } else if flushed {
                    sized
                } else {
                    sized
                        * ((INFORMED_STRESSED - p) / (INFORMED_STRESSED - INFORMED_CALM))
                            .clamp(0.0, 1.0)
                }
            }
            Rung::Oracle => {
                let told = self
                    .told
                    .as_ref()
                    .ok_or("the informed bot needs MTX_REGIME")?;
                // Only what has happened: the flush once the book has
                // fallen, and the rebuild's stated longest wait after it.
                let ended = told.flush.is_some_and(|f| at >= f.end);
                let rebuilt = told
                    .flush
                    .is_some_and(|f| at >= f.end + INFORMED_REBUILD_SLOTS);
                let (stressed, level) = told.state(at);
                let sigma = MEDIAN_SIGMA * level;
                let stop = room / (full * STOP_SIGMAS * sigma * STOP_HORIZON_SECONDS.sqrt());
                // After the flush no gap can come: the cap in either state.
                let want = if rebuilt || !(ended || stressed) {
                    1.0
                } else {
                    0.0
                };
                if self.rung == Rung::Oracle && told.near_flush(at) {
                    0.0
                } else {
                    stop.min(want)
                }
            }
        };
        if gap_usd <= 0.0 || self.done {
            share = 0.0;
        }
        let warm = self.steps > WARMUP_STEPS
            || matches!(self.rung, Rung::MaxLever | Rung::StopIn | Rung::StopOut);
        let informed = self.rung == Rung::Oracle;
        let mut target = if warm { share * setup.cap } else { 0.0 };
        let held = r.position;
        let hysteresis = if informed {
            INFORMED_HYSTERESIS
        } else {
            HYSTERESIS
        };
        if (target - held).abs() < hysteresis * setup.cap && target > 0.0 {
            target = held;
        }
        log(&format!(
            "s={} book={:.4} index={:.4} sd={:.3e} y={:.3e} pos={held:.1} target={target:.1} eq={:.2} dd={drawdown:.1}",
            step.k, r.book, r.index, sigma, yield_per_second, r.equity
        ));
        if step.last {
            return Ok(());
        }
        // The mark-pusher's ask from the last turn goes first, so it can't
        // fill against a sale this turn and leave the account short.
        if self.rung == Rung::MarkPusher {
            orders.cancel_all();
        }
        let mut pos = held;
        // The stop-in twin closes as fast as the desk would.
        let (slices, slice) = if self.done {
            (8, 100.0)
        } else {
            (SLICES_PER_TURN, SLICE)
        };
        for _ in 0..slices {
            let delta = target - pos;
            if delta.abs() < 0.5 {
                break;
            }
            let qty = delta.abs().min(slice);
            if delta > 0.0 {
                orders.buy(qty, r.book * 1.004);
            } else {
                // The mark-pusher's last ask may fill before this sale lands
                // (the tape trades first): keep its size back.
                let keep = if self.rung == Rung::MarkPusher {
                    0.5
                } else {
                    0.0
                };
                let qty = qty.min(pos - keep);
                if qty <= 0.0 {
                    break;
                }
                orders.sell(qty, r.book * 0.996);
            }
            pos += qty.copysign(delta);
        }
        if self.rung == Rung::MarkPusher
            && pos > 1.0
            && target >= held
            && let (Some(bid), Some(ask)) = (step.best_bid, step.best_ask)
        {
            let tick = 0.01;
            let price = ((bid + tick) * 100.0).round() / 100.0;
            if price < ask {
                orders.ask(price, 0.5);
            }
        }
        Ok(())
    }
}

impl Crowd {
    /// Update the watch with this step's discount, a share of the index, at
    /// step `k`.
    fn watch(&mut self, discount: f64, k: u64) {
        let Some((last, at)) = self.last_discount else {
            self.last_discount = Some((discount, k));
            return;
        };
        if discount == last {
            return;
        }
        let level = self.calm_discount.unwrap_or(discount).max(1e-4);
        let seconds = (k.saturating_sub(at)).max(1) as f64 * STEP_SECONDS;
        let change = (discount - last) / level;
        self.churn = (1.0 - CHURN_WEIGHT) * self.churn + CHURN_WEIGHT * change * change / seconds;
        self.changes += 1;
        self.last_discount = Some((discount, k));
        let line = if self.stressed {
            CHURN_STRESS * CHURN_CALM
        } else {
            CHURN_STRESS
        };
        self.stressed = self.changes < 8 || self.churn > line;
        if !self.stressed {
            let calm = self.calm_discount.unwrap_or(discount);
            self.calm_discount = Some(calm + 0.05 * (discount - calm));
        } else if self.calm_discount.is_none() && self.changes >= 8 {
            self.calm_discount = Some(discount);
        }
    }
}

/// The informed bot's read of the volatility state: a two-state filter on
/// the discount's changes, told the session's process (`MTX_REGIME`, the
/// latent state the verifier hands it): the discount's level and each
/// state's volatility level. The state path itself is never told: the
/// filter reads it, switching only at minute marks, as the instruction says.
struct Filter {
    /// The discount's level, a share of the index, and each state's
    /// volatility level (a multiple of the calibration's median session).
    level: f64,
    levels: [f64; 2],
    /// The probability of the stressed state, the minute it was carried to,
    /// and the last discount seen and its step.
    p: f64,
    minute: u64,
    last: Option<(f64, u64)>,
}

impl Filter {
    fn from_session(session: &Session) -> BotResult<Self> {
        let latent = session
            .var("MTX_REGIME")
            .ok_or("the informed bot needs MTX_REGIME")?;
        let latent: serde_json::Value = serde_json::from_str(latent)?;
        let level = latent["discount_pct"].as_f64().ok_or("no discount_pct")? / 100.0;
        let vol = |i: usize| latent["regime_vol"][i].as_f64().ok_or("no regime_vol");
        Ok(Self {
            level,
            levels: [vol(0)?, vol(1)?],
            p: STRESS_PRIOR,
            minute: 0,
            last: None,
        })
    }

    /// The probability of stress after this step's discount, at `at` slots
    /// into the session and step `k`.
    fn update(&mut self, discount: f64, at: u64, k: u64) -> f64 {
        let minute = at / SLOTS_PER_MINUTE;
        while self.minute < minute {
            self.p = self.p * (1.0 - TO_CALM) + (1.0 - self.p) * TO_STRESS;
            self.minute += 1;
        }
        match self.last {
            Some((last, _)) if last == discount => {}
            Some((last, at_k)) if k > WARMUP_STEPS => {
                let dt = (k.saturating_sub(at_k)).max(1) as f64 * STEP_SECONDS;
                let pull = (dt / DISCOUNT_TAU).min(1.0);
                let like = |target: f64, noise: f64| {
                    let sd = noise * self.level * dt.sqrt();
                    let z = (discount - last - (target - last) * pull) / sd;
                    -sd.ln() - (FILTER_NU + 1.0) / 2.0 * (1.0 + z * z / FILTER_NU).ln()
                };
                let calm = like(self.level, DISCOUNT_NOISE[0]);
                let stressed = like(self.level * STRESSED_WIDENING, DISCOUNT_NOISE[1]);
                let top = calm.max(stressed);
                let (a, b) = (
                    self.p * (stressed - top).exp(),
                    (1.0 - self.p) * (calm - top).exp(),
                );
                self.p = (a / (a + b)).clamp(1e-4, 1.0 - 1e-4);
                self.last = Some((discount, k));
            }
            _ => self.last = Some((discount, k)),
        }
        self.p
    }
}

/// What the oracle rung is told: the session's volatility states, each
/// state's level and its flush, in slots from the session's start, from
/// `MTX_REGIME` (`tell_regime`): the session's draws, so a diagnostic rung,
/// never an anchor.
struct Told {
    /// The slot each state starts at, and whether it is the stressed one.
    states: Vec<(u64, bool)>,
    /// Each state's level, a multiple of the calibration's median session:
    /// calm, then stressed.
    levels: [f64; 2],
    flush: Option<Flush>,
}

/// The flush's start, the slot the book has fallen by, and the slot its
/// discount is rebuilt.
#[derive(Clone, Copy)]
struct Flush {
    start: u64,
    end: u64,
    rebuilt: u64,
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
        let level = |i: usize| shape["regime_vol"][i].as_f64().unwrap_or(1.0);
        let flush = shape["flush"].as_object().and_then(|f| {
            Some(Flush {
                start: f.get("start")?.as_u64()?,
                end: f.get("end")?.as_u64()?,
                rebuilt: f.get("rebuilt")?.as_u64()?,
            })
        });
        Ok(Self {
            states,
            levels: [level(0), level(1)],
            flush,
        })
    }

    /// Whether the state at `at` is the stressed one, and its level.
    fn state(&self, at: u64) -> (bool, f64) {
        let stressed = self
            .states
            .iter()
            .rev()
            .find(|(slot, _)| *slot <= at)
            .is_some_and(|(_, stressed)| *stressed);
        (stressed, self.levels[usize::from(stressed)])
    }

    /// From 4 seconds before the flush to the end of its rebuild.
    fn near_flush(&self, at: u64) -> bool {
        self.flush
            .is_some_and(|f| at + 16 >= f.start && at <= f.rebuilt)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(LevCarry);
