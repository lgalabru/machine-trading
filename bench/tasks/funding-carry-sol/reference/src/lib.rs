//! Reference: a SOL carry desk that sells the perp's premium for Phoenix's
//! funding when the funding it expects to collect pays for the trade.
//!
//! Each step it reads the index and the book's best bid and ask (the run
//! loop's read), and once, from the PerpAssetMap, the session's funding
//! interval and period. It keeps an average of the premium (the book's mid
//! over the index) with a half-life of [`AVERAGE_SLOTS`], and from it the
//! funding a short position would collect over the rest of the session:
//! the premium times the seconds left over the period. Past
//! [`WARMUP_SLOTS`], when that is more than [`ENTRY_MULT`] times what
//! getting in costs (the taker fee and what the arbitrageur takes back of
//! the book its sale walked down), it sells (buys, against a discount),
//! sized by how rich the premium is: the cap at [`FULL_BPS`] and more. It
//! sells a slice of [`SLICE`] every [`PACE_SLOTS`], so the book and the
//! arbitrageur can come back between. It only adds to its position: a
//! position held at the close costs nothing more, and the funding keeps
//! paying while the premium does. It flattens if the drawdown passes
//! [`STOP_SHARE`] of the limit.
//!
//! It doesn't know the session's premium level, only what it has seen, and
//! it takes: what a desk would do with a slow basis and no view.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`), the anchor hidden sets are
//! scored against.

use mtx_kit::{
    log,
    prelude::*,
    rpc_account,
    venue::{PhoenixVenue, phoenix_rise::accounts::perp_asset_map::PerpAssetMap},
};

/// The premium average's half-life, slots.
pub const AVERAGE_SLOTS: f64 = 120.0;
/// Slots it watches before it trades.
pub const WARMUP_SLOTS: u64 = 160;
/// What getting in costs, bp: Phoenix's 3.5 bp taker fee, and about as much
/// again of walk, which the arbitrageur buys back from 3.5 to 5 bp under
/// the index.
pub const ENTRY_COST_BPS: f64 = 7.0;
/// The expected funding must pay this many times the entry.
pub const ENTRY_MULT: f64 = 2.0;
/// The premium, bp, at which it holds the whole cap.
pub const FULL_BPS: f64 = 2.5;
/// The share of the cap it uses.
pub const CAP_SHARE: f64 = 0.8;
/// The share of the drawdown limit past which it flattens.
pub const STOP_SHARE: f64 = 0.6;
/// The market order it adds by, SOL, how often, and how far through the
/// book it reaches.
pub const SLICE: f64 = 5.0;
pub const PACE_SLOTS: u64 = 4;
pub const REACH: f64 = 0.002;
/// Less than this, SOL, isn't worth an order.
pub const MIN_ORDER: f64 = 0.5;

/// The session's funding settings, from the PerpAssetMap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FundingClock {
    pub interval_s: f64,
    pub period_s: f64,
}

/// The carry desk, as a [`Strategy`].
#[derive(Default)]
pub struct Carry {
    symbol: String,
    map: String,
    cap: f64,
    max_drawdown: f64,
    start_slot: u64,
    funding: Option<FundingClock>,
    average_bps: Option<f64>,
    last_slot: u64,
    last_trade_slot: u64,
    stopped: bool,
}

/// The session's funding interval and period, read from the PerpAssetMap.
pub fn read_funding(map: &str, symbol: &str) -> BotResult<Option<FundingClock>> {
    let Some(data) = rpc_account(map)? else {
        return Ok(None);
    };
    let map = PerpAssetMap::try_from_account_bytes(&data).map_err(|e| e.to_string())?;
    let Some(entry) = map.find_by_symbol(symbol).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let f = entry.metadata.funding_accumulator();
    let interval_s = f.funding_interval_seconds.as_inner() as f64;
    let period_s = f.funding_period_seconds.as_inner() as f64;
    if interval_s < 1.0 || period_s < interval_s {
        return Ok(None);
    }
    Ok(Some(FundingClock {
        interval_s,
        period_s,
    }))
}

/// The book's premium over the index, bp, from the best bid and ask.
pub fn premium_bps(market: &Market) -> Option<f64> {
    let (bid, ask) = (market.best_bid?, market.best_ask?);
    if !(bid > 0.0 && ask >= bid && market.index > 0.0) {
        return None;
    }
    Some(((bid + ask) / 2.0 / market.index - 1.0) * 10_000.0)
}

/// Orders toward `target` from `held`: up to `slices` market orders of at
/// most [`SLICE`], reaching [`REACH`] through the book.
pub fn trade_toward(orders: &mut Orders, market: &Market, held: f64, target: f64, slices: u32) {
    let price = match (market.best_bid, market.best_ask) {
        (Some(bid), Some(ask)) => (bid + ask) / 2.0,
        _ => market.mark,
    };
    let mut left = target - held;
    for _ in 0..slices {
        if left.abs() < MIN_ORDER {
            break;
        }
        let qty = left.abs().min(SLICE);
        if left > 0.0 {
            orders.buy(qty, price * (1.0 + REACH));
        } else {
            orders.sell(qty, price * (1.0 - REACH));
        }
        left -= qty.copysign(left);
    }
}

impl Strategy for Carry {
    fn start(&mut self, session: &Session) -> BotResult {
        let allowance = session
            .mandate
            .universe
            .iter()
            .find(|a| a.instrument.to_string() == format!("phoenix:perp:{}", session.symbol))
            .ok_or("the session's market is outside the mandate")?;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        self.map = venue.perp_asset_map()?.to_string();
        self.symbol = session.symbol.clone();
        self.cap = session.cap.min(allowance.max_position) * CAP_SHARE;
        self.max_drawdown = session
            .mandate
            .limits
            .max_drawdown_usd
            .map_or(session.max_drawdown, |l| l.min(session.max_drawdown));
        self.start_slot = session.start_slot;
        self.last_slot = session.start_slot;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        if self.funding.is_none() {
            self.funding = read_funding(&self.map, &self.symbol)?;
        }
        let Some(premium) = premium_bps(market) else {
            return Ok(());
        };
        let elapsed = (market.slot - self.last_slot) as f64;
        self.last_slot = market.slot;
        let keep = 0.5f64.powf(elapsed / AVERAGE_SLOTS);
        let average = match self.average_bps {
            Some(a) => keep * a + (1.0 - keep) * premium,
            None => premium,
        };
        self.average_bps = Some(average);
        let held = market.position.size;
        if market.drawdown > STOP_SHARE * self.max_drawdown {
            self.stopped = true;
        }
        if self.stopped {
            trade_toward(orders, market, held, 0.0, 8);
            return Ok(());
        }
        let Some(funding) = self.funding else {
            return Ok(());
        };
        if market.slot < self.start_slot + WARMUP_SLOTS || market.last {
            return Ok(());
        }
        // The funding a short collects over the rest of the session, bp of
        // its notional: the premium over the period, each second.
        let seconds_left = market.slots_left as f64 * 0.25;
        let expected_bps = average.abs() * seconds_left / funding.period_s;
        let side = -average.signum();
        let size = (average.abs() / FULL_BPS).min(1.0) * self.cap;
        let target = if expected_bps > ENTRY_MULT * ENTRY_COST_BPS {
            side * size
        } else {
            0.0
        };
        // Only add: never pay to trade out of a position the funding still
        // pays, and never flip on a passing wobble.
        let wanted = if target * held >= 0.0 && target.abs() > held.abs() {
            target
        } else {
            held
        };
        log(&format!(
            "slot {}: premium {premium:.2} bp, average {average:.2}, expected {expected_bps:.1} bp, held {held:.2}, target {wanted:.2}",
            market.slot - self.start_slot
        ));
        if wanted != held && market.slot >= self.last_trade_slot + PACE_SLOTS {
            trade_toward(orders, market, held, wanted, 1);
            self.last_trade_slot = market.slot;
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Carry);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_premium_is_the_mid_over_the_index() {
        let market = Market {
            index: 100.0,
            best_bid: Some(100.01),
            best_ask: Some(100.03),
            ..Market::default()
        };
        assert!((premium_bps(&market).unwrap() - 2.0).abs() < 1e-9);
        assert!(premium_bps(&Market::default()).is_none());
    }

    #[test]
    fn it_slices_toward_the_target() {
        let market = Market {
            index: 100.0,
            best_bid: Some(100.0),
            best_ask: Some(100.02),
            ..Market::default()
        };
        let mut orders = Orders::new();
        trade_toward(&mut orders, &market, 0.0, -12.0, 8);
        assert_eq!(orders.intents().len(), 3);
        let mut orders = Orders::new();
        trade_toward(&mut orders, &market, 0.0, -12.0, 1);
        assert_eq!(orders.intents().len(), 1);
    }
}
