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
//! checking its position after each.
//!
//! Changed from the frozen bot: a turn no longer stops slicing after 1.2 s
//! of wall time (a session must replay the same under any load), and the
//! ladder's `told` rung (`MTX_RUNG=told`) reads each episode's kind from
//! `MTX_REGIME` and corrects the reference's own target with it: never
//! fade an informed premium before the index catches up, follow it with the
//! cap once the index has started to; fade a crowded one with the cap while
//! it lasts, never follow it.

use std::str::FromStr;

use anyhow::{Result, anyhow};
use mtx_kit::{
    BotEnv, Pubkey, Side, Trader, Turns,
    phoenix_rise::accounts::{
        perp_asset_map::PerpAssetMap, spline_collection::SplineCollection,
        trader::Trader as RawTrader,
    },
};
mod model;

fn position(trader: &Trader, base_decimals: i8) -> Result<f64> {
    let data = trader.rpc.get_account_data(&trader.account)?;
    let raw = RawTrader::try_from_account_bytes(&data)?;
    Ok(raw
        .positions()
        .find(|p| p.0 == trader.inner.market().asset_id as u64)
        .map(|p| p.1.base_lot_position().as_inner() as f64 / 10f64.powi(base_decimals as i32))
        .unwrap_or(0.0))
}

#[tokio::main]
async fn main() -> Result<()> {
    // Acknowledge the turn protocol before doing any account or metadata work.
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    // Fail closed if the supplied mandate and instrument disagree.
    let mandate = env.mandate()?;
    let allowance = mandate
        .universe
        .iter()
        .find(|a| a.instrument.to_string() == "phoenix:perp:SPCX")
        .ok_or_else(|| anyhow!("SPCX is outside the mandate"))?;
    let max_position = env.max_position.min(allowance.max_position);
    let max_drawdown = mandate
        .limits
        .max_drawdown_usd
        .map_or(env.max_drawdown_usd, |limit| {
            env.max_drawdown_usd.min(limit)
        });
    if env.symbol != "SPCX" {
        return Err(anyhow!("SPCX mandate required"));
    }
    let trader = Trader::connect(&env)?;
    let market = trader.phoenix.market(&env.symbol)?;
    let spline_addr = Pubkey::from_str(&market.spline_pubkey)?;
    let map_addr = trader.phoenix.perp_asset_map()?;
    let addrs = [map_addr, spline_addr, trader.account];
    let model = model::Model::load();
    let mut features = model::Features::new();
    let cap = (max_position - 0.1).clamp(0.0, 69.9);
    let mut peak = 10100.0f64;
    let mut stopped = false;
    let told = Told::from_env()?;
    let start_slot = turns.hello().start_slot as f64;
    while let Some(step) = turns.next()? {
        // One standard RPC request gives a consistent view while the world waits.
        let accounts = match trader.rpc.get_multiple_accounts(&addrs) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("read: {e}");
                continue;
            }
        };
        let Some(map_account) = &accounts[0] else {
            continue;
        };
        let Some(spline_account) = &accounts[1] else {
            continue;
        };
        let Some(trader_account) = &accounts[2] else {
            continue;
        };
        let map = PerpAssetMap::try_from_account_bytes(&map_account.data)?;
        let entry = map
            .find_by_symbol(&env.symbol)?
            .ok_or_else(|| anyhow!("market missing"))?;
        let f = entry.metadata.funding_accumulator();
        let period = f.funding_period_seconds.as_inner() as f64;
        let interval = f.funding_interval_seconds.as_inner() as f64;
        if period < 1.0 || interval < 1.0 {
            continue;
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
            continue;
        }
        ticks.sort_unstable();
        let index = trader.phoenix.ticks_to_usd(market, ticks[ticks.len() / 2]);
        // Maker quote centres remain observable after our own taker orders move the BBO.
        let splines = SplineCollection::try_from_account_bytes(&spline_account.data)?;
        let mids: Vec<_> = splines
            .iter()
            .flatten()
            .filter(|s| s.is_enabled())
            .map(|s| (s.user_update_slot(), s.mid_price().as_inner()))
            .collect();
        let Some(newest) = mids.iter().map(|s| s.0).max() else {
            continue;
        };
        let mut prices: Vec<u64> = mids.iter().filter(|s| s.0 == newest).map(|s| s.1).collect();
        prices.sort_unstable();
        let book = trader
            .phoenix
            .ticks_to_usd(market, prices[prices.len() / 2]);
        let raw = RawTrader::try_from_account_bytes(&trader_account.data)?;
        let mut pos = 0.0;
        let mut virtual_quote = 0.0;
        let mut unsettled = 0.0;
        if let Some((_, p)) = raw.positions().find(|p| p.0 == market.asset_id as u64) {
            let lots = p.base_lot_position().as_inner();
            pos = trader.phoenix.lots_to_base_units(market, lots);
            virtual_quote = p.virtual_quote_lot_position().as_inner() as f64 / 1e6;
            unsettled = -(f.cumulative_funding_rate.as_inner()
                - p.cumulative_funding_snapshot().as_inner()) as f64
                * lots as f64
                / 1e6;
        }
        // Value collateral, the open position at the maker centre, and unsettled funding.
        let equity = 100.0
            + raw.header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6
            + pos * book
            + virtual_quote
            + unsettled;
        peak = peak.max(equity);
        let drawdown = peak - equity;
        if drawdown > max_drawdown * 0.62 {
            stopped = true;
        }
        // Reserve room for a large book move, an interval's funding, and execution.
        // At ordinary prices and drawdowns this leaves the full desk position cap.
        let funding_cap_usd = f.max_funding_rate.as_inner().abs() as f64
            * 10f64.powi(market.base_lots_decimals as i32 - 6);
        let risk_cap =
            ((max_drawdown - drawdown - 75.0).max(0.0) / (book * 0.04 + funding_cap_usd)).min(cap);
        let lots_per_share = 10f64.powi(market.base_lots_decimals as i32);
        let usable_cap = (risk_cap * lots_per_share).floor() / lots_per_share;
        let cap_per_second = f.max_funding_rate.as_inner() as f64
            * 10f64.powi(market.base_lots_decimals as i32 - 6)
            / interval
            / 143.11
            * 10000.0;
        let (x, sign) = features.update(index, book, period, cap_per_second);
        let alpha = model.predict(&x) * sign;
        // A switching band covers fees and impact; keep existing exposure inside it.
        let entry_threshold = 10.0;
        let exit_threshold = -10.0;
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
        if let Some(told) = &told {
            target = told.target(
                step.slot as f64 - start_slot,
                target,
                usable_cap,
                book / index - 1.0,
            );
        }
        if stopped {
            target = 0.0;
        }
        eprintln!(
            "s={} book={:.5} index={:.5} pos={:.2} target={:.2} alpha={:.3} eq={:.3} dd={:.2}",
            step.k,
            book,
            index,
            pos,
            target,
            alpha,
            equity,
            peak - equity
        );
        if step.last {
            continue;
        }
        // Slice reversals and confirm the actual position after every executed slice.
        for _ in 0..8 {
            let delta = target - pos;
            if delta.abs() < 0.15 {
                break;
            }
            let qty = delta.abs().min(20.0);
            let side = if delta > 0.0 { Side::Bid } else { Side::Ask };
            let limit = book * if delta > 0.0 { 1.004 } else { 0.996 };
            if let Err(e) = trader.place_market(side, qty, limit).await {
                eprintln!("order: {e}");
                break;
            }
            let previous = pos;
            match position(&trader, market.base_lots_decimals) {
                Ok(p) => pos = p,
                Err(e) => {
                    eprintln!("position: {e}");
                    break;
                }
            }
            if (pos - previous).abs() < 0.001 {
                break;
            }
        }
    }
    Ok(())
}

/// The ladder's told rung: each episode's kind and timing, in slots from the
/// session's start, from `MTX_REGIME`.
struct Told {
    /// Informed, start, follow (the slot the index starts to follow), close,
    /// end, and the premium's side.
    episodes: Vec<(bool, f64, f64, f64, f64, f64)>,
}

impl Told {
    fn from_env() -> Result<Option<Self>> {
        if std::env::var("MTX_RUNG").as_deref() != Ok("told") {
            return Ok(None);
        }
        let regime = std::env::var("MTX_REGIME").map_err(|_| anyhow!("told needs MTX_REGIME"))?;
        let shape: serde_json::Value = serde_json::from_str(&regime)?;
        let episodes = shape["episodes"]
            .as_array()
            .ok_or_else(|| anyhow!("no episodes"))?
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
