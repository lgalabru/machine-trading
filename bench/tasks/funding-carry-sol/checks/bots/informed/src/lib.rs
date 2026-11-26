//! Informed bot (scoring v2's 1): the reference's carry desk, told the
//! session's latent state (`MTX_REGIME`): the premium's process (its level
//! for the session, its spread around it and its half-life), funding's
//! settings, the flow's pace and the arbitrageur's.
//!
//! What it knows is what a desk that had watched SOL's basis for days would
//! know: where this week's premium sits. It doesn't wait to learn it. At its
//! first step it prices the funding a short would collect over the rest of
//! the session (the level times the seconds left over the period, plus what
//! the premium's distance from the level still pays while it fades), takes
//! off what getting in costs, and weighs what is left against the index's
//! risk as scoring v2 weighs losses (1.5 times): it holds the cap when that
//! pays, less as it thins, nothing when it doesn't, against a discount the
//! other way. It only adds, like the reference, and flattens past the same
//! drawdown. Its loss budget is what the carry pays plus one standard
//! deviation of the index over the session, both as it priced them at its
//! first step (bp of the cap's notional, never under [`MIN_BUDGET_BPS`]):
//! once the position's loss against the mark passes it, the index has taken
//! more than the carry and an ordinary session's noise account for, and it
//! gets out for the session.
//!
//! It knows the process, never the path, the premium as it will wander, or
//! when anyone trades.

use mtx_kit::{
    log,
    prelude::*,
    rpc_account, serde_json,
    venue::{PhoenixVenue, phoenix_rise::accounts::perp_asset_map::PerpAssetMap},
};

/// What getting in costs, bp: as the reference counts it.
pub const ENTRY_COST_BPS: f64 = 7.0;
/// The index's sd a second, bp: SOL's calibration
/// (`fair_value.per_second_sd`, 8.22e-5), for the median session.
pub const INDEX_SD_BPS_PER_S: f64 = 0.822;
/// How scoring v2 weighs a loss against a gain.
pub const LOSS_WEIGHT: f64 = 1.5;
/// The loss-weighted edge, bp, at which it holds the whole cap.
pub const FULL_EDGE_BPS: f64 = 4.0;
/// The share of the drawdown limit past which it flattens.
pub const STOP_SHARE: f64 = 0.6;
/// The least loss budget, bp of the cap's notional: what getting in costs.
pub const MIN_BUDGET_BPS: f64 = 7.0;
/// The market order it adds by, SOL, how often, and how far through the
/// book it reaches: as the reference.
pub const SLICE: f64 = 5.0;
pub const PACE_SLOTS: u64 = 4;
pub const REACH: f64 = 0.002;
pub const MIN_ORDER: f64 = 0.5;
const SLOT_SECONDS: f64 = 0.25;

/// The session's premium process, from `MTX_REGIME`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Process {
    pub mean_bps: f64,
    pub half_life_s: f64,
    pub period_s: f64,
}

impl Process {
    pub fn from_regime(regime: &str) -> BotResult<Self> {
        let latent: serde_json::Value = serde_json::from_str(regime)?;
        let number = |v: &serde_json::Value, what: &str| {
            v.as_f64()
                .ok_or_else(|| format!("MTX_REGIME has no {what}"))
        };
        let mean_bps = number(&latent["premium_mean_bps"], "premium_mean_bps")?;
        let half_life_slots = number(
            &latent["premium_half_life_slots"],
            "premium_half_life_slots",
        )?;
        let period_s = number(
            &latent["funding"]["period_seconds"],
            "funding.period_seconds",
        )?;
        if !(half_life_slots > 0.0 && period_s > 0.0) {
            return Err("MTX_REGIME: a half-life and a period must be positive".into());
        }
        Ok(Self {
            mean_bps,
            half_life_s: half_life_slots * SLOT_SECONDS,
            period_s,
        })
    }

    /// The funding a short collects over the next `seconds`, bp of its
    /// notional, with the premium at `now_bps`: the level's, and the
    /// distance's while it fades.
    pub fn expected_bps(&self, now_bps: f64, seconds: f64) -> f64 {
        let tau = self.half_life_s / std::f64::consts::LN_2;
        let fading = (now_bps - self.mean_bps) * tau * (1.0 - (-seconds / tau).exp());
        (self.mean_bps * seconds + fading) / self.period_s
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

/// The book's premium over the index, bp.
fn premium_bps(market: &Market) -> Option<f64> {
    let (bid, ask) = (market.best_bid?, market.best_ask?);
    if !(bid > 0.0 && ask >= bid && market.index > 0.0) {
        return None;
    }
    Some(((bid + ask) / 2.0 / market.index - 1.0) * 10_000.0)
}

fn trade_toward(orders: &mut Orders, market: &Market, held: f64, target: f64, slices: u32) {
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

/// Whether the PerpAssetMap holds the session's funding settings yet: they
/// land in setup, and it waits for them before it trades.
fn funding_landed(map: &str, symbol: &str, period_s: f64) -> BotResult<bool> {
    let Some(data) = rpc_account(map)? else {
        return Ok(false);
    };
    let map = PerpAssetMap::try_from_account_bytes(&data).map_err(|e| e.to_string())?;
    let Some(entry) = map.find_by_symbol(symbol).map_err(|e| e.to_string())? else {
        return Ok(false);
    };
    let period = entry
        .metadata
        .funding_accumulator()
        .funding_period_seconds
        .as_inner() as f64;
    Ok(period == period_s)
}

/// The loss budget in USD: `budget_bps` of the cap's notional at `mark`.
pub fn budget_usd(budget_bps: f64, cap: f64, mark: f64) -> f64 {
    assert!(budget_bps >= 0.0 && cap >= 0.0);
    budget_bps / 10_000.0 * cap * mark
}

/// The strategy.
#[derive(Default)]
pub struct Informed {
    process: Option<Process>,
    symbol: String,
    map: String,
    cap: f64,
    max_drawdown: f64,
    landed: bool,
    stopped: bool,
    /// The session's loss budget, bp of the cap's notional, set at its
    /// first priced step.
    budget_bps: Option<f64>,
    last_trade_slot: u64,
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        let regime = session
            .var("MTX_REGIME")
            .ok_or("the informed bot runs told the session's latent state (MTX_REGIME)")?;
        let allowance = session
            .mandate
            .universe
            .iter()
            .find(|a| a.instrument.to_string() == format!("phoenix:perp:{}", session.symbol))
            .ok_or("the session's market is outside the mandate")?;
        self.process = Some(Process::from_regime(regime)?);
        let venue = PhoenixVenue::from_json(&session.venue)?;
        self.map = venue.perp_asset_map()?.to_string();
        self.symbol = session.symbol.clone();
        self.cap = session.cap.min(allowance.max_position);
        self.max_drawdown = session
            .mandate
            .limits
            .max_drawdown_usd
            .map_or(session.max_drawdown, |l| l.min(session.max_drawdown));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let process = self.process.ok_or("no session")?;
        if !self.landed {
            self.landed = funding_landed(&self.map, &self.symbol, process.period_s)?;
        }
        let held = market.position.size;
        if market.drawdown > STOP_SHARE * self.max_drawdown {
            self.stopped = true;
        }
        if let Some(budget_bps) = self.budget_bps
            && held != 0.0
            && -market.pnl > budget_usd(budget_bps, self.cap, market.mark)
        {
            log(&format!(
                "slot {}: the loss passed what the carry pays; out",
                market.slot
            ));
            self.stopped = true;
        }
        if self.stopped {
            trade_toward(orders, market, held, 0.0, 8);
            return Ok(());
        }
        if !self.landed || market.last {
            return Ok(());
        }
        let now_bps = premium_bps(market).unwrap_or(process.mean_bps);
        let seconds_left = market.slots_left as f64 * SLOT_SECONDS;
        let expected = process.expected_bps(now_bps, seconds_left);
        if self.budget_bps.is_none() {
            let noise = INDEX_SD_BPS_PER_S * seconds_left.sqrt();
            self.budget_bps = Some((expected.abs() + noise).max(MIN_BUDGET_BPS));
        }
        let side = -expected.signum();
        let risk = INDEX_SD_BPS_PER_S * seconds_left.sqrt();
        let edge = weighted_edge(expected.abs() - ENTRY_COST_BPS, risk);
        let target = side * (edge / FULL_EDGE_BPS).clamp(0.0, 1.0) * self.cap;
        let wanted = if target * held >= 0.0 && target.abs() > held.abs() {
            target
        } else {
            held
        };
        log(&format!(
            "slot {}: premium {now_bps:.2} bp, expected {expected:.1} bp, edge {edge:.1}, held {held:.2}, target {wanted:.2}",
            market.slot
        ));
        if wanted != held && market.slot >= self.last_trade_slot + PACE_SLOTS {
            trade_toward(orders, market, held, wanted, 1);
            self.last_trade_slot = market.slot;
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_regime_is_read() {
        let p = Process::from_regime(
            r#"{"premium_mean_bps": 1.5, "premium_sd_bps": 0.841, "premium_half_life_slots": 43.68,
                "funding": {"interval_seconds": 3, "period_seconds": 24, "max_rate": 1931},
                "orders_per_minute": 1.335, "informed_share": 0.12,
                "arbitrageur_latency_slots": 1, "arbitrageur_edge_bps": 4.2}"#,
        )
        .unwrap();
        assert_eq!(p.period_s, 24.0);
        // At the level, the expected funding is the level's.
        assert!((p.expected_bps(1.5, 240.0) - 15.0).abs() < 1e-9);
        // Above it, a little more, and never more than the excess's integral.
        let more = p.expected_bps(3.5, 240.0) - 15.0;
        assert!(more > 0.0 && more < 2.0 * 10.92 / std::f64::consts::LN_2 / 24.0 + 1e-9);
    }

    #[test]
    fn the_budget_is_the_carry_on_the_cap() {
        // 20 bp of 125 SOL at $80: $20.
        assert!((budget_usd(20.0, 125.0, 80.0) - 20.0).abs() < 1e-9);
    }

    #[test]
    fn the_weighted_edge_charges_losses() {
        assert!(weighted_edge(10.0, 0.0) == 10.0);
        assert!(weighted_edge(10.0, 16.0) < 10.0);
        assert!(weighted_edge(10.0, 16.0) > 7.0);
        assert!(weighted_edge(-1.0, 16.0) < -1.0);
        assert!((erfc(0.0) - 1.0).abs() < 1e-6);
        assert!((erfc(1.0) - 0.157_299_2).abs() < 1e-6);
    }
}
