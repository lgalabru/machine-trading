//! Informed bot (scoring v2's 1): the reference's maker, told the session's
//! latent state (`MTX_REGIME`): which wallets are the informed takers, and
//! the process they trade by (they see the price `informed_horizon_slots`
//! ahead, right `informed_hit_rate` of the time).
//!
//! Each step it reads the informed wallets' Phoenix trader accounts (one
//! `getMultipleAccounts`) and takes each one's position change since its
//! last look as what that wallet just traded. A wallet that bought holds
//! its view (the price is going up) for its horizon, so the bot keeps each
//! wallet's last side and when it traded, and sums them into a signal in
//! [-1, 1], each weighted by what is left of its horizon and by how often a
//! view is right (`2 h - 1`). With it, it leans its reservation price the
//! informed way, skews toward holding up to half the cap that way (built
//! with its quotes: it takes only to reduce), and stops quoting the side the
//! informed are hitting while it holds less than that. Everything
//! else is the reference's: quotes one tick inside the competing makers,
//! skewed against inventory, leaning with the drift, backing off one-sided
//! fills and one-sided runs on the tape, never taking.
//!
//! It knows who is informed and how they trade, never the path, a jump, or
//! when anyone will trade: what it reads is what they already did.

mod markouts;
mod tape;

use std::collections::VecDeque;

use mtx_kit::{
    log,
    prelude::*,
    rpc_account, serde_json,
    venue::{PhoenixVenue, phoenix_rise::accounts::spline_collection::SplineCollection},
};

const SKEW_BPS_PER_UNIT: f64 = 2.5;
/// Its skew where every taker is noise and it holds twice the reference's
/// inventory: twice as steep, so it sheds what it holds as fast in time
/// and a jump (4 sessions in 72, at any time) finds it holding less.
const UNINFORMED_SKEW_BPS_PER_UNIT: f64 = 5.0;
const DRIFT_WEIGHT: f64 = 0.5;
const DRIFT_SLOTS: f64 = 80.0;
const DRIFT_MAX_BPS: f64 = 5.0;
/// The fills it marks out (`MARKOUT_SLOTS`) before it judges whether they
/// hold up (give back no more than `MARKOUT_GIVEBACK` of its half-spread);
/// until they do, it steps off one-sided runs on the tape.
const MARKOUT_FILLS: u32 = 10;
const MARKOUT_SLOTS: u64 = 40;
const MARKOUT_GIVEBACK: f64 = 0.5;

/// Whether its fills have shown they aren't run over: their markout takes
/// back no more than `MARKOUT_GIVEBACK` of the half-spread it quotes at.
fn fills_hold_up(markouts: &markouts::Markouts, half_spread_bps: f64) -> bool {
    markouts
        .mean_bps(MARKOUT_FILLS)
        .is_some_and(|mean| mean >= -MARKOUT_GIVEBACK * half_spread_bps)
}

const INVENTORY_LIMIT: f64 = 0.25;
/// Its inventory limit where every taker is noise, a share of the cap.
const UNINFORMED_INVENTORY_LIMIT: f64 = 0.5;
/// As the reference's: the stated same-side odds (0.61) make an average
/// one-sided run of `1 / (1 - 0.61)` orders.
const RUN_ORDERS: f64 = 1.0 / (1.0 - 0.608);

/// Its quote size: what keeps an average one-sided run of fills inside its
/// inventory limit, as the reference's (6.4 CBRS at half a 33 cap).
fn quote_size(cap: f64, limit: f64) -> f64 {
    assert!(cap > 0.0 && limit > 0.0 && limit <= 1.0);
    let size = (limit * cap / RUN_ORDERS * 1_000.0).floor() / 1_000.0;
    assert!(size > 0.0 && size <= cap);
    size
}
const TOXIC_UNITS: f64 = 1.5;
const TOXIC_WINDOW_SLOTS: u64 = 8;
const TOXIC_SLOTS: u64 = 24;
const REQUOTE_BPS: f64 = 1.5;
const REQUOTE_SLOTS: u64 = 12;
/// How far a full informed signal leans the reservation price, bp: about
/// what the informed see coming, a minute's mean absolute move (12 bp a
/// minute at the calibration's 1.55 bp a second, times 0.8) times the
/// share of it still ahead on average.
const LEAN_BPS: f64 = 6.0;
/// The share of the cap a full signal holds the informed way.
const TARGET_SHARE: f64 = 0.5;
/// The signal past which the side the informed hit is pulled.
const PULL_SIGNAL: f64 = 0.1;
/// The tape's window, slots (10 s), and how one-sided its takers' net
/// volume in it must be to call it a run: this many typical trades more one
/// way than the other (on Phoenix's ANT the next order is on the same side
/// 56% of the time, on CBRS 61%), or a floor before any trade is seen.
const FLOW_WINDOW_SLOTS: u64 = 40;
const FLOW_PULL_TRADES: f64 = 3.0;
const FLOW_PULL_FLOOR_UNITS: f64 = 4.0;

/// The net taker volume past which the tape is one-sided, base units.
fn flow_pull(tape: &tape::Tape) -> f64 {
    tape.typical()
        .map_or(FLOW_PULL_FLOOR_UNITS, |typical| FLOW_PULL_TRADES * typical)
}
/// The most informed wallets a session seats (the family seats four).
const WALLETS_MAX: usize = 16;

/// The strategy.
#[derive(Default)]
pub struct Informed {
    flow: Flow,
    cap: f64,
    half_spread_bps: f64,
    /// (slot, position) at each step of the toxic window.
    history: VecDeque<(u64, f64)>,
    /// Its own fills' markouts, which set how much size it shows.
    markouts: markouts::Markouts,
    toxic_until: u64,
    /// Which way the toxic fills took the inventory: +1 long, -1 short.
    toxic_side: f64,
    quoted: Option<(u64, f64, f64)>,
    requotes: u64,
    /// The oracle's recent average, and the slot it was last updated at.
    average: Option<(u64, f64)>,
    /// Which sides the informed signal and the tape pulled at the last
    /// requote, and whether it quoted inside the competitor.
    pulled: (bool, bool),
    inside: bool,
    /// One tick, in bp of the starting oracle.
    tick_bps: f64,
    /// The market's recent trades, read off the chain, as the reference.
    tape: tape::Tape,
}

impl Informed {
    fn skew_bps_per_unit(&self) -> f64 {
        if self.flow.uninformed {
            UNINFORMED_SKEW_BPS_PER_UNIT
        } else {
            SKEW_BPS_PER_UNIT
        }
    }
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.mandate.universe[0].max_position;
        self.markouts = markouts::Markouts::new(MARKOUT_SLOTS);
        self.half_spread_bps = competitor_half_spread_bps(session)?;
        self.tick_bps = tick_bps(session)?;
        let regime = session
            .var("MTX_REGIME")
            .ok_or("the informed bot runs told the session's latent state (MTX_REGIME)")?;
        self.flow = Flow::from_regime(regime, session)?;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        self.tape = tape::Tape::new(&venue, &session.symbol, FLOW_WINDOW_SLOTS)?;
        log(&format!(
            "one tick inside the competitor: {:.3} bps; quoting a cap of {}",
            self.half_spread_bps, self.cap
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (slot, mark, held, cap) = (market.slot, market.oracle, market.position.size, self.cap);
        self.markouts.step(slot, held, mark);
        // It never takes, so its whole position came from its quotes' fills.
        self.history.push_back((slot, held));
        while self
            .history
            .front()
            .is_some_and(|(s, ..)| s + TOXIC_WINDOW_SLOTS < slot)
        {
            self.history.pop_front();
        }
        if let Some(&(_, held_then)) = self.history.front()
            && (held - held_then).abs() >= TOXIC_UNITS
            && held.abs() > held_then.abs()
            && self.toxic_until <= slot
        {
            log(&format!("slot {slot}: one-sided fills; backing off"));
            self.toxic_until = slot + TOXIC_SLOTS;
            self.toxic_side = (held - held_then).signum();
        }
        let toxic = slot < self.toxic_until;
        let signal = self.flow.read(slot);
        let flow = self.tape.read(slot);
        let pull = flow_pull(&self.tape);

        let mean = match self.average {
            Some((at, mean)) => {
                let weight = 1.0 - (-((slot - at) as f64) / DRIFT_SLOTS).exp();
                mean + weight * (mark - mean)
            }
            None => mark,
        };
        self.average = Some((slot, mean));
        let drift_bps =
            (DRIFT_WEIGHT * (mark / mean - 1.0) * 10_000.0).clamp(-DRIFT_MAX_BPS, DRIFT_MAX_BPS);
        // It builds a position the informed way with its quotes (it may
        // take only to reduce): the skew pulls toward the target, not zero.
        let target = TARGET_SHARE * cap * signal;
        let lean_bps = LEAN_BPS * signal;
        let reservation = mark
            * (1.0
                + (drift_bps + lean_bps - self.skew_bps_per_unit() * (held - target)) / 10_000.0);
        // The side the informed are hitting stays out of their way, unless
        // it sheds what the bot holds.
        // A run on the tape is dodged as the reference does, only while its
        // fills are run over, and never where it knows every taker is noise.
        let toxic_tape =
            !self.flow.uninformed && !fills_hold_up(&self.markouts, self.half_spread_bps);
        let informed_buying =
            (signal >= PULL_SIGNAL || (toxic_tape && flow >= pull)) && held <= target;
        let informed_selling =
            (signal <= -PULL_SIGNAL || (toxic_tape && flow <= -pull)) && held >= target;
        // Told the competing maker's rule (it looks every `react_slots` and
        // steps inside whoever quotes inside it), it quotes a tick inside
        // the competitor for one step after each of its looks and at the
        // competitor's price the rest of the time: inside some of the time
        // without starting a race toward the competitor's floor.
        let inside = self.flow.inside_now(slot);
        let half = if inside {
            self.half_spread_bps
        } else {
            self.half_spread_bps + self.tick_bps
        };
        let bid = reservation * (1.0 - half / 10_000.0);
        let ask = reservation * (1.0 + half / 10_000.0);
        // Told every taker is noise, an inventory a run of fills builds
        // reverts as the flow does: it holds twice the reference's limit,
        // and sizes to it as the reference does.
        let limit = if self.flow.uninformed {
            UNINFORMED_INVENTORY_LIMIT
        } else {
            INVENTORY_LIMIT
        };
        let size = quote_size(cap, limit);
        let (bid_size, ask_size) = (size.min(cap - held), size.min(cap + held));
        let pulled = (informed_buying, informed_selling);
        let moved = self.pulled != pulled
            || self.inside != inside
            || self.quoted.is_none_or(|(at, b, a)| {
                slot >= at + REQUOTE_SLOTS
                    || ((bid - b) / mark).abs() * 10_000.0 >= REQUOTE_BPS
                    || ((ask - a) / mark).abs() * 10_000.0 >= REQUOTE_BPS
            });
        if !moved {
            return Ok(());
        }

        orders.cancel_all();
        let mut quotes = [
            (
                Side::Bid,
                bid,
                bid_size,
                held < target + limit * cap
                    && !(toxic && self.toxic_side > 0.0)
                    && !informed_selling,
            ),
            (
                Side::Ask,
                ask,
                ask_size,
                held > target - limit * cap
                    && !(toxic && self.toxic_side < 0.0)
                    && !informed_buying,
            ),
        ];
        if self.requotes % 2 == 1 {
            quotes.reverse();
        }
        self.requotes += 1;
        for (side, price, size, wanted) in quotes {
            if wanted && size >= 0.01 {
                match side {
                    Side::Bid => orders.bid(price, size),
                    Side::Ask => orders.ask(price, size),
                };
            }
        }
        self.quoted = Some((slot, bid, ask));
        self.pulled = pulled;
        self.inside = inside;
        Ok(())
    }
}

/// The informed wallets and what each last did.
#[derive(Default)]
struct Flow {
    /// Each wallet's trader account, its last position, and its last
    /// trade: the side (+1 bought, -1 sold) and the slot.
    accounts: Vec<String>,
    positions: Vec<Option<f64>>,
    last: Vec<Option<(f64, u64)>>,
    asset_id: u64,
    lot_scale: f64,
    horizon_slots: u64,
    confidence: f64,
    /// Whether every taker is noise: the latent's informed and index-chasing
    /// shares are both 0, as on CBRS.
    uninformed: bool,
    /// The competitor's slots between looks, when the latent states them.
    competitor_react_slots: Option<u64>,
}

impl Flow {
    fn from_regime(regime: &str, session: &Session) -> BotResult<Self> {
        let latent: serde_json::Value = serde_json::from_str(regime).map_err(|e| e.to_string())?;
        // A market whose takers know nothing (CBRS's) seats no informed
        // wallet: the signal is then always 0.
        let none = Vec::new();
        let wallets = latent["wallets"]["informed"].as_array().unwrap_or(&none);
        if wallets.len() > WALLETS_MAX {
            return Err(format!("{} informed wallets", wallets.len()).into());
        }
        let share = |field: &str| {
            latent[field]
                .as_f64()
                .ok_or(format!("MTX_REGIME has no {field}"))
        };
        let uninformed = share("informed_share")? == 0.0 && share("index_share")? == 0.0;
        let horizon_slots = latent["informed_horizon_slots"]
            .as_u64()
            .ok_or("MTX_REGIME has no informed_horizon_slots")?;
        let hit_rate = latent["informed_hit_rate"]
            .as_f64()
            .ok_or("MTX_REGIME has no informed_hit_rate")?;
        if !(0.5..=1.0).contains(&hit_rate) || horizon_slots == 0 {
            return Err("an informed process out of range".into());
        }
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?;
        let accounts = wallets
            .iter()
            .map(|w| {
                let authority = mtx_kit::venue::key(w.as_str().ok_or("a wallet that isn't text")?)?;
                // Fills name the trader account, a PDA of the authority.
                Ok(venue.trader_account(&authority).to_string())
            })
            .collect::<Result<Vec<_>, String>>()?;
        let n = accounts.len();
        log(&format!(
            "{n} informed wallets, horizon {horizon_slots}, hit rate {hit_rate}"
        ));
        Ok(Self {
            accounts,
            positions: vec![None; n],
            last: vec![None; n],
            asset_id: market.asset_id as u64,
            lot_scale: 10f64.powi(market.base_lots_decimals as i32),
            horizon_slots,
            confidence: 2.0 * hit_rate - 1.0,
            uninformed,
            competitor_react_slots: latent["competitor_react_slots"].as_u64().filter(|r| *r > 0),
        })
    }

    /// The informed signal at `slot`, in [-1, 1]: each wallet's last side,
    /// weighted by the share of its horizon still ahead, summed, times how
    /// much better than a coin a view is (`2 h - 1`), clamped. A read that
    /// fails leaves the last state.
    fn read(&mut self, slot: u64) -> f64 {
        if self.accounts.is_empty() {
            return 0.0;
        }
        let addresses: Vec<&str> = self.accounts.iter().map(String::as_str).collect();
        if let Ok(datas) = mtx_kit::rpc_accounts(&addresses) {
            for (i, data) in datas.into_iter().enumerate().take(self.accounts.len()) {
                let Some(position) = data.and_then(|d| self.position(&d)) else {
                    continue;
                };
                if let Some(before) = self.positions[i]
                    && (position - before).abs() > 0.5 / self.lot_scale
                {
                    self.last[i] = Some(((position - before).signum(), slot));
                    log(&format!(
                        "slot {slot}: informed wallet {i} traded {:+.3}",
                        position - before
                    ));
                }
                self.positions[i] = Some(position);
            }
        }
        // A trade at `at` says where the price is going by `at + horizon`:
        // what is left of that, by the share of the horizon still ahead.
        let horizon = self.horizon_slots as f64;
        let sum: f64 = self
            .last
            .iter()
            .flatten()
            .filter(|(_, at)| at + self.horizon_slots > slot)
            .map(|(side, at)| side * (at + self.horizon_slots - slot) as f64 / horizon)
            .sum();
        // Each wallet's view is its own evidence: two agreeing say more than
        // one, and a wallet that hasn't traded lately says nothing.
        let signal = (self.confidence * sum).clamp(-1.0, 1.0);
        if self.positions.iter().all(Option::is_none) {
            log(&format!("slot {slot}: no informed wallet's position read"));
        }
        assert!(signal.abs() <= 1.0);
        signal
    }

    /// Whether to quote inside the competitor at `slot`: one step (two
    /// slots) in each of its look periods, `react_slots + 2` long; always,
    /// as the reference does, when its rule isn't told.
    fn inside_now(&self, slot: u64) -> bool {
        match self.competitor_react_slots {
            Some(react) => slot % (react + 2) < 2,
            None => true,
        }
    }

    /// A trader account's position in the market, in base units.
    fn position(&self, data: &[u8]) -> Option<f64> {
        let trader =
            mtx_kit::venue::phoenix_rise::accounts::owned::Trader::try_from_account_bytes(data)
                .ok()?;
        Some(
            trader
                .positions
                .entries
                .iter()
                .filter(|(id, _)| u64::from(*id) == self.asset_id)
                .map(|(_, p)| f64::from(p.base_lot_position) / self.lot_scale)
                .sum(),
        )
    }
}

/// The market's tick in bp of the starting oracle.
fn tick_bps(session: &Session) -> BotResult<f64> {
    let venue = PhoenixVenue::from_json(&session.venue)?;
    let market = venue.market(&session.symbol)?;
    Ok(venue.ticks_to_usd(market, 1) / session.start_oracle * 10_000.0)
}

/// The competing makers' half-spread less one tick, in basis points of the
/// starting oracle: the closest any enabled spline quotes to its mid.
fn competitor_half_spread_bps(session: &Session) -> BotResult<f64> {
    let venue = PhoenixVenue::from_json(&session.venue)?;
    let market = venue.market(&session.symbol)?;
    let data = rpc_account(&market.spline_pubkey)?.ok_or("no spline account")?;
    let collection = SplineCollection::try_from_account_bytes(&data).map_err(|e| e.to_string())?;
    let inner = collection
        .splines()
        .filter_map(|s| s.ok())
        .filter(|s| s.is_enabled())
        .flat_map(|s| {
            s.active_bid_regions()
                .iter()
                .chain(s.active_ask_regions())
                .map(|r| r.start_offset().as_inner())
                .collect::<Vec<_>>()
        })
        .min()
        .ok_or("no competing quotes")?;
    let tick_usd = venue.ticks_to_usd(market, 1);
    Ok(((inner.saturating_sub(1)) as f64 * tick_usd / session.start_oracle * 10_000.0).max(1.0))
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);
