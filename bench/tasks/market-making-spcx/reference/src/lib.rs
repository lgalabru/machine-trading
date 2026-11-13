//! Reference: quote one tick inside the competing makers, skew against
//! inventory, and trade with the flow when it turns toxic.
//!
//! The competitors' half-spread is read once, from their spline account; the
//! quotes sit that far less one tick either side of a reservation price:
//! the oracle, shifted against what the bot holds, so a long inventory lowers
//! both quotes and sheds itself (Avellaneda-Stoikov's idea, with a linear
//! skew). Holding a quarter of the cap, it stops quoting the side that would
//! add to it. Fills that pile up one way in a few slots look like informed
//! flow ahead of a jump: it buys (or sells) a few units the burst's way, pulls
//! the quote the burst was hitting for a while, and lets the skew unwind the
//! position after. Only fills that grow the inventory count toward a burst:
//! the skew unwinding a lean is a run of one-sided fills too, and a maker
//! that took it for one would lean back and forth on its own. And it leans
//! with the drift: the further the oracle runs from its recent average, the
//! further the reservation price follows, so it sells into a falling market
//! rather than buying all the way down.
//!
//! It prices off the oracle, not Phoenix's mark: the mark blends in the
//! book, and the bot's own best bid or ask pulls it along, so a maker that
//! quotes around the mark chases its own quotes.
//!
//! Its PnL is a published baseline, not the reward's scale (that's the
//! spread bound plus the hindsight bound). It reacts to its own fills only:
//! it doesn't read the tape, doesn't tell a fake-out from a real burst, and
//! leans the same few units whatever the burst's size.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`), the anchor hidden sets are
//! scored against.

use std::collections::VecDeque;

use mtx_kit::{
    log,
    prelude::*,
    rpc_account,
    venue::{PhoenixVenue, phoenix_rise::accounts::spline_collection::SplineCollection},
};

const SKEW_BPS_PER_UNIT: f64 = 2.5;
const DRIFT_WEIGHT: f64 = 0.5;
const DRIFT_SLOTS: f64 = 80.0;
const DRIFT_MAX_BPS: f64 = 5.0;
const QUOTE_SIZE: f64 = 1.0;
const INVENTORY_LIMIT: f64 = 0.25;
const TOXIC_UNITS: f64 = 1.5;
const TOXIC_WINDOW_SLOTS: u64 = 8;
const LEAN_UNITS: f64 = 5.0;
const TOXIC_SLOTS: u64 = 24;
const REQUOTE_BPS: f64 = 1.5;
const REQUOTE_SLOTS: u64 = 12;

/// The strategy.
#[derive(Default)]
pub struct Maker {
    cap: f64,
    half_spread_bps: f64,
    /// (slot, position, position from its quotes' fills) at each step.
    history: VecDeque<(u64, f64, f64)>,
    /// What its own market orders added to the position.
    own: f64,
    toxic_until: u64,
    /// Which way the toxic fills took the inventory: +1 long, -1 short.
    toxic_side: f64,
    quoted: Option<(u64, f64, f64)>,
    requotes: u64,
    /// The oracle's recent average, and the slot it was last updated at.
    average: Option<(u64, f64)>,
}

impl Strategy for Maker {
    fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.mandate.universe[0].max_position;
        self.half_spread_bps = competitor_half_spread_bps(session)?;
        log(&format!(
            "one tick inside the competitor: {:.3} bps; quoting a cap of {}",
            self.half_spread_bps, self.cap
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (slot, mark, held, cap) = (market.slot, market.oracle, market.position.size, self.cap);
        let filled = held - self.own;
        self.history.push_back((slot, held, filled));
        while self
            .history
            .front()
            .is_some_and(|(s, ..)| s + TOXIC_WINDOW_SLOTS < slot)
        {
            self.history.pop_front();
        }
        if let Some(&(_, held_then, then)) = self.history.front()
            && (filled - then).abs() >= TOXIC_UNITS
            && held.abs() > held_then.abs()
            && self.toxic_until <= slot
        {
            log(&format!("slot {slot}: one-sided fills; backing off"));
            self.toxic_until = slot + TOXIC_SLOTS;
            self.toxic_side = (filled - then).signum();
            let lean = -self.toxic_side * LEAN_UNITS - held;
            if lean.abs() >= 0.5 {
                let size = lean.abs().min(cap);
                orders.cancel_all();
                if lean > 0.0 {
                    orders.buy(size, round_cents(mark * 1.003));
                } else {
                    orders.sell(size, round_cents(mark * 0.997));
                }
                self.own += size.copysign(lean);
                return Ok(());
            }
        }
        let toxic = slot < self.toxic_until;

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
        let reservation = mark * (1.0 + (drift_bps - SKEW_BPS_PER_UNIT * held) / 10_000.0);
        let half = self.half_spread_bps;
        let bid = reservation * (1.0 - half / 10_000.0);
        let ask = reservation * (1.0 + half / 10_000.0);
        let moved = self.quoted.is_none_or(|(at, b, a)| {
            slot >= at + REQUOTE_SLOTS
                || ((bid - b) / mark).abs() * 10_000.0 >= REQUOTE_BPS
                || ((ask - a) / mark).abs() * 10_000.0 >= REQUOTE_BPS
        });
        if !moved {
            return Ok(());
        }

        orders.cancel_all();
        let bid_size = QUOTE_SIZE.min(cap - held);
        let ask_size = QUOTE_SIZE.min(cap + held);
        let mut quotes = [
            (
                Side::Bid,
                bid,
                bid_size,
                held < INVENTORY_LIMIT * cap && !(toxic && self.toxic_side > 0.0),
            ),
            (
                Side::Ask,
                ask,
                ask_size,
                held > -INVENTORY_LIMIT * cap && !(toxic && self.toxic_side < 0.0),
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
        Ok(())
    }
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

fn round_cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Maker);
