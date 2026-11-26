//! Reference: a SOL carry desk that sells the perp's premium for Phoenix's
//! funding when the funding it expects to collect pays for the trade and
//! the risk.
//!
//! It knows what the instruction states of SOL's basis across weeks (a
//! level of +1.43 bp, spread 0.67 bp, a wander of 0.84 bp around it that
//! halves every 3.64 funding intervals), never this session's level. Each
//! step it reads the index and the book's best bid and ask (the run loop's
//! read), and once, from the PerpAssetMap, the session's funding interval
//! and period. It estimates the session's level by shrinking the premium's
//! running mean toward the stated prior, weighting the mean by how many
//! half-lives it has seen ([`Estimate`]). From the estimate it prices the
//! funding a short collects over the rest of the session (the level over
//! the period, each second), takes off what getting in costs
//! ([`ENTRY_COST_BPS`]), and weighs what is left against the index's risk
//! over the same time as scoring v2 weighs losses (1.5 times). It holds a
//! share of [`CAP_SHARE`] of the cap when that edge reaches [`FULL_EDGE_BPS`],
//! less as it thins, nothing when it doesn't.
//!
//! It adds in [`SLICE`] SOL market orders every [`PACE_SLOTS`] slots, so the
//! book and the arbitrageur come back between. It sheds toward its target
//! only when the estimate turns against the position by more than getting
//! out costs (the basis ran the other way), and otherwise holds: a position
//! held at the close pays no fee. It flattens if the drawdown passes
//! [`STOP_SHARE`] of the limit.
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

/// What the instruction states of SOL's premium across five-day windows,
/// bp: the level's mean and spread, and the wander's spread and half-life
/// in funding intervals.
pub const PRIOR_MEAN_BPS: f64 = 1.434;
pub const PRIOR_SD_BPS: f64 = 0.665;
pub const WANDER_SD_BPS: f64 = 0.841;
pub const WANDER_HALF_LIFE_INTERVALS: f64 = 3.64;
/// What getting in costs, bp: Phoenix's 3.5 bp taker fee, and about as much
/// again of walk, which the arbitrageur buys back from 3.5 to 5 bp under
/// the index.
pub const ENTRY_COST_BPS: f64 = 7.0;
/// The index's sd a second, bp: SOL's calibration (`per_second_sd`).
pub const INDEX_SD_BPS_PER_S: f64 = 0.822;
/// How scoring v2 weighs a loss against a gain.
pub const LOSS_WEIGHT: f64 = 1.5;
/// The loss-weighted edge, bp, at which it holds its whole share.
pub const FULL_EDGE_BPS: f64 = 6.0;
/// The share of the cap it uses.
pub const CAP_SHARE: f64 = 0.6;
/// Slots it watches before it trades.
pub const WARMUP_SLOTS: u64 = 40;
/// The share of the drawdown limit past which it flattens.
pub const STOP_SHARE: f64 = 0.6;
/// The market order it trades by, SOL, how often, and how far through the
/// book it reaches.
pub const SLICE: f64 = 5.0;
pub const PACE_SLOTS: u64 = 4;
pub const REACH: f64 = 0.002;
/// Less than this, SOL, isn't worth an order.
pub const MIN_ORDER: f64 = 0.5;
const SLOT_SECONDS: f64 = 0.25;

/// The session's funding settings, from the PerpAssetMap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FundingClock {
    pub interval_s: f64,
    pub period_s: f64,
}

/// The session's premium level, estimated: the premium's running mean,
/// shrunk toward the stated prior by how much it has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Estimate {
    sum_bps: f64,
    slots: f64,
}

impl Estimate {
    /// Adds `premium_bps` read over `slots` slots.
    pub fn add(&mut self, premium_bps: f64, slots: f64) {
        assert!(slots >= 0.0);
        self.sum_bps += premium_bps * slots;
        self.slots += slots;
    }

    /// The level, bp, given how many slots a funding interval lasts. The
    /// running mean of an AR(1) wander has the variance of one read over
    /// about `slots / (2 tau)` independent ones (tau its time constant),
    /// and the half-tick quantization adds about as much again.
    pub fn level_bps(&self, interval_slots: f64) -> f64 {
        assert!(interval_slots > 0.0);
        if self.slots <= 0.0 {
            return PRIOR_MEAN_BPS;
        }
        let tau = WANDER_HALF_LIFE_INTERVALS * interval_slots / std::f64::consts::LN_2;
        let samples = (self.slots / (2.0 * tau)).max(0.0);
        let read_var = 2.0 * WANDER_SD_BPS * WANDER_SD_BPS;
        let mean_var = read_var / samples.max(1e-9);
        let prior_var = PRIOR_SD_BPS * PRIOR_SD_BPS;
        let mean = self.sum_bps / self.slots;
        (PRIOR_MEAN_BPS / prior_var + mean / mean_var) / (1.0 / prior_var + 1.0 / mean_var)
    }
}

/// `m` less half the expected loss of a normal with mean `m` and sd `s`:
/// what a unit of position earns, as v2 weighs losses `LOSS_WEIGHT` times.
pub fn weighted_edge(m: f64, s: f64) -> f64 {
    if s <= 0.0 {
        return m.min(m * LOSS_WEIGHT);
    }
    let z = m / s;
    let pdf = (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let cdf_neg = 0.5 * erfc(z / std::f64::consts::SQRT_2);
    let expected_loss = s * pdf - m * cdf_neg;
    m - (LOSS_WEIGHT - 1.0) * expected_loss
}

/// The complementary error function (Numerical Recipes' erfcc: 1.2e-7).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let r = t
        * (-z * z - 1.265_512_23
            + t * (1.000_023_68
                + t * (0.374_091_96
                    + t * (0.096_784_18
                        + t * (-0.186_288_06
                            + t * (0.278_868_07
                                + t * (-1.135_203_98
                                    + t * (1.488_515_87
                                        + t * (-0.822_152_23 + t * 0.170_872_77)))))))))
            .exp();
    if x >= 0.0 { r } else { 2.0 - r }
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
    // Mainnet's hourly settings are the snapshot's until the session's land.
    if interval_s < 1.0 || period_s < interval_s || interval_s >= 3_600.0 {
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

/// What it holds next, from what it holds and what it wants: it adds toward
/// the target, and sheds only when the estimate turned against the position
/// by more than getting out costs.
pub fn next_position(held: f64, target: f64, against_bps: f64) -> f64 {
    let adding = target * held >= 0.0 && target.abs() > held.abs();
    if adding {
        return target;
    }
    if against_bps > ENTRY_COST_BPS {
        return target;
    }
    held
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
    estimate: Estimate,
    last_slot: u64,
    last_trade_slot: u64,
    stopped: bool,
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
        let held = market.position.size;
        if market.drawdown > STOP_SHARE * self.max_drawdown {
            self.stopped = true;
        }
        if self.stopped {
            trade_toward(orders, market, held, 0.0, 8);
            return Ok(());
        }
        let Some(premium) = premium_bps(market) else {
            return Ok(());
        };
        let elapsed = (market.slot - self.last_slot) as f64;
        self.last_slot = market.slot;
        self.estimate.add(premium, elapsed);
        let Some(funding) = self.funding else {
            return Ok(());
        };
        if market.slot < self.start_slot + WARMUP_SLOTS || market.last {
            return Ok(());
        }
        let interval_slots = funding.interval_s / SLOT_SECONDS;
        let level = self.estimate.level_bps(interval_slots);
        // The funding a short collects over the rest of the session, bp of
        // its notional, at the estimated level.
        let seconds_left = market.slots_left as f64 * SLOT_SECONDS;
        let expected = level * seconds_left / funding.period_s;
        let side = -expected.signum();
        let risk = INDEX_SD_BPS_PER_S * seconds_left.sqrt();
        let edge = weighted_edge(expected.abs() - ENTRY_COST_BPS, risk);
        let target = side * (edge / FULL_EDGE_BPS).clamp(0.0, 1.0) * self.cap;
        // How much the estimate now says the held side pays against it.
        let against = if held * side < 0.0 {
            expected.abs()
        } else {
            0.0
        };
        let wanted = next_position(held, target, against);
        log(&format!(
            "slot {}: premium {premium:.2} bp, level {level:.2}, expected {expected:.1} bp, edge {edge:.1}, held {held:.2}, target {wanted:.2}",
            market.slot - self.start_slot
        ));
        if (wanted - held).abs() >= MIN_ORDER && market.slot >= self.last_trade_slot + PACE_SLOTS {
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

    /// With nothing seen it is the prior; with a long look it is the mean.
    #[test]
    fn the_estimate_shrinks_toward_the_prior() {
        let mut e = Estimate::default();
        assert_eq!(e.level_bps(12.0), PRIOR_MEAN_BPS);
        e.add(0.0, 40.0);
        let early = e.level_bps(12.0);
        assert!(early < PRIOR_MEAN_BPS && early > 0.9, "{early}");
        e.add(0.0, 1_500.0);
        assert!(e.level_bps(12.0) < 0.5, "{}", e.level_bps(12.0));
    }

    #[test]
    fn it_adds_and_sheds_only_against_its_cost() {
        assert_eq!(next_position(-10.0, -20.0, 0.0), -20.0);
        assert_eq!(next_position(-20.0, -10.0, 0.0), -20.0);
        assert_eq!(next_position(-20.0, 0.0, 3.0), -20.0);
        assert_eq!(next_position(-20.0, 0.0, 9.0), 0.0);
    }

    #[test]
    fn the_weighted_edge_charges_losses() {
        assert!(weighted_edge(10.0, 16.0) < 10.0);
        assert!(weighted_edge(10.0, 16.0) > 7.0);
        assert!((erfc(1.0) - 0.157_299_2).abs() < 1e-6);
    }
}
