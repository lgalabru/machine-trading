//! Reference: read the weekend's flow, ride what it predicts, and hold the
//! gap into the reopen.
//!
//! While SPCX's own market is closed, Phoenix's index is the perp's price,
//! smoothed: it carries nothing the perp doesn't, and the mark sits on it.
//! The reference reads the flow instead. Each turn it reads the makers' mid
//! (the perp's price), Phoenix's index, and every taker order since the last
//! turn (`getSignaturesForAddress` on the market at `confirmed`, then
//! `getTransaction`: the signer, side, size and limit of each Phoenix
//! order), and keeps a tape (`features.rs`): its structural model's read
//! (`model.rs`: which one-sided account's impact stays, where that puts the
//! open, whether it's still trading) and flow statistics per account.
//!
//! Every 8 slots a forecaster (`gbm.rs`, gradient-boosted trees trained on
//! generated sessions) predicts the perp's move over the next 64 slots, the
//! reopen's included when it's nearer. The position is the cap times the
//! forecast over a scale, changed only when the target moves by a dead band;
//! within 64 slots of the reopen it's also sized so the worst open any likely
//! hypothesis allows stays inside the loss limit. It stops, flat, past a
//! drawdown of its own, and holds through the reopen to the end.
//!
//! Its parameters and its forecaster come from offline work: trained on 8,000 generated
//! sessions, tuned on 1,000 others.
//!
//! The ladder runs this binary as several rungs (`MTX_RUNG`): `reference`
//! (the default); `g0`, the first generation's reference (the structural
//! model alone, the gap only); `told`, whose forecaster also knows which
//! accounts are the informed trader and the worker (`MTX_REGIME`);
//! `path-oracle`, told the perp's whole path, which plans its positions by
//! dynamic programming after costs (`oracle.rs`);
//! `follow`, the cap with the perp's move from the last external price;
//! `fade`, the cap against it; `basis`, which takes the index for fair value;
//! and `fitted`, a regression on generic features fitted to generated
//! sessions.

mod features;
mod gbm;
mod model;
mod oracle;

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use features::Tape;
use gbm::Forest;
use model::{Params, Trade};
use mtx_kit::{
    BotEnv, Side, Trader, Turns,
    phoenix_rise::ix::{PhoenixInstruction, order_packet::OrderPacket},
};
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Market orders of at most this many shares (one order that crosses much
/// of the book doesn't fit a transaction's compute budget), and at most
/// this many shares a turn.
const SLICE: f64 = 10.0;
const MAX_STEP_TRADE: f64 = 35.0;
/// How far past the perp's price a market order may fill, in percent.
const SLIPPAGE_PCT: f64 = 1.0;
/// The forecaster's policy: the 64-slot forecast (in percent) that takes the
/// full position, the dead band (in shares), and the drawdown at which it
/// stops (in USD).
const FORECAST_SCALE_PCT: f64 = 0.3;
const DEAD_BAND: f64 = 60.0;
const STOP_DRAWDOWN_USD: f64 = 450.0;
/// The forecasters, trained offline (see `gbm.rs`).
const FORECAST: &str = include_str!("../model/forecast.json");
const FORECAST_TOLD: &str = include_str!("../model/forecast_told.json");

/// The `fitted` rung's coefficients: a ridge regression of the gap between
/// the open and the perp on generic features (see `fitted_features`),
/// fitted on generated sessions.
const FITTED: [f64; 9] = [
    0.0541, 0.5295, -1.5255, 1.1073, 0.5213, 0.00444, -0.0265, -0.0577, 0.5581,
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Rung {
    Reference,
    G0,
    Told,
    PathOracle,
    Follow,
    Fade,
    Basis,
    Fitted,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    // Answer the grader's hello first: the world waits for it.
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let cap = env.max_position;
    let start_slot = turns.hello().start_slot;
    let reopen_slot: u64 = std::env::var("MTX_REOPEN_SLOT")
        .context("MTX_REOPEN_SLOT")?
        .parse()
        .context("MTX_REOPEN_SLOT")?;
    let reopen = reopen_slot - start_slot;
    let rung = match std::env::var("MTX_RUNG").as_deref() {
        Ok("g0") => Rung::G0,
        Ok("told") => Rung::Told,
        Ok("path-oracle") => Rung::PathOracle,
        Ok("follow") => Rung::Follow,
        Ok("fade") => Rung::Fade,
        Ok("basis") => Rung::Basis,
        Ok("fitted") => Rung::Fitted,
        _ => Rung::Reference,
    };
    let regime: Option<Value> = std::env::var("MTX_REGIME")
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok());
    let slots_of = |key: &str| -> HashSet<u64> {
        regime
            .as_ref()
            .and_then(|r| r[key].as_array())
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default()
    };
    let (informed_slots, worker_slots) = (slots_of("informed_slots"), slots_of("worker_slots"));
    let half_life = regime
        .as_ref()
        .and_then(|r| r["half_life_slots"].as_f64())
        .unwrap_or(100.0);
    let open = regime
        .as_ref()
        .and_then(|r| r["open_pct"].as_f64())
        .unwrap_or(0.0);
    let signed: Vec<usize> = if rung == Rung::Told {
        features::SIGNED
            .iter()
            .chain(features::SIGNED_TOLD_EXTRA.iter())
            .copied()
            .collect()
    } else {
        features::SIGNED.to_vec()
    };
    let g0 = Params::default();
    let params = Params::default();
    let forest = Forest::parse(if rung == Rung::Told {
        FORECAST_TOLD
    } else {
        FORECAST
    })
    .context("forecaster")?;
    let market = trader.phoenix.market(&env.symbol)?.clone();
    let orderbook = market.market_pubkey.clone();
    let me = trader.authority().to_string();
    info!(?rung, reopen, "weekend");

    let mut tape = Tape::default();
    let mut ids: HashMap<String, u16> = HashMap::new();
    let (mut informed_id, mut worker_id): (Option<u16>, Option<u16>) = (None, None);
    // Per account: net, gross, last slot (the fitted rung's flow).
    let mut flows: HashMap<u16, (f64, f64, u64)> = HashMap::new();
    let mut flow_total = 0.0;
    let mut start_mid: Option<f64> = None;
    let mut half_spread_usd = 0.0;
    let mut mids: HashMap<u64, f64> = HashMap::new();
    let mut hold = 0.0;
    let mut decided: Option<f64> = None;
    let mut history: Vec<(u64, f64)> = Vec::new();
    let mut plan: Option<Vec<f64>> = None;
    let mut seen_sigs: HashSet<String> = HashSet::new();
    // The account's value from the start, marked each turn, against its
    // peak: the stop.
    let (mut value, mut peak, mut last_mark, mut stopped) = (0.0_f64, 0.0_f64, None, false);
    while let Some(step) = turns.next()? {
        let at = step.slot - start_slot;
        // The perp's price: the makers' mid.
        let Ok(bbo) = trader.inner.bbo().await else {
            continue;
        };
        let (Some(bid), Some(ask)) = (bbo.best_bid_ticks(), bbo.best_ask_ticks()) else {
            continue;
        };
        let (bid, ask) = (
            trader.phoenix.ticks_to_usd(&market, bid),
            trader.phoenix.ticks_to_usd(&market, ask),
        );
        let mid = 0.5 * (bid + ask);
        let index = trader.phoenix.ticks_to_usd(&market, bbo.index_price_ticks);
        let mark = trader.phoenix.ticks_to_usd(&market, bbo.mark_price_ticks);
        let s0 = *start_mid.get_or_insert_with(|| {
            half_spread_usd = 0.5 * (ask - bid);
            mid
        });
        mids.insert(at, mid);
        let level = (mid / s0 - 1.0) * 100.0;
        let index_level = (index / s0 - 1.0) * 100.0;
        let position = trader.position().await.map(|p| p.size).unwrap_or(0.0);
        if let Some(last) = last_mark {
            value += position * (mark - last);
        }
        last_mark = Some(mark);
        peak = peak.max(value);
        if !stopped && peak - value > STOP_DRAWDOWN_USD {
            warn!(value, peak, "drawdown past the stop; flat from here");
            stopped = true;
        }
        if at < reopen {
            tape.observe(at, level, index_level);
        }
        // The orders since the last turn: every Phoenix order on SPCX but
        // ours, from the market's transaction history.
        for (tx_slot, signer, side, size, limit_usd) in
            new_orders(&trader, &orderbook, &market, &mut seen_sigs)
        {
            if signer == me || tx_slot < start_slot {
                continue;
            }
            // The turn that first showed the order: it executes in the slot
            // the last turn ended at, and the next turn shows it, whenever
            // the listing does. It was priced off that turn's price.
            let landed = tx_slot - start_slot;
            let seen = (landed / 2 * 2 + 2).min(at);
            let priced = mids.get(&(seen.saturating_sub(2))).copied().unwrap_or(mid);
            let limit = if half_spread_usd > 0.0 {
                (limit_usd - priced).abs() / half_spread_usd
            } else {
                0.0
            };
            let next = ids.len() as u16;
            let account = *ids.entry(signer).or_insert(next);
            if informed_slots.contains(&seen.saturating_sub(2)) {
                informed_id.get_or_insert(account);
            }
            if worker_slots.contains(&seen.saturating_sub(2)) {
                worker_id.get_or_insert(account);
            }
            if seen < reopen {
                tape.trade(Trade {
                    seen,
                    account,
                    side,
                    size,
                    limit,
                });
            }
            let f = flows.entry(account).or_insert((0.0, 0.0, 0));
            f.0 += side * size;
            f.1 += size;
            f.2 = at;
            flow_total += side * size;
        }

        let target = if stopped {
            0.0
        } else if at >= reopen && rung != Rung::PathOracle {
            hold
        } else {
            match rung {
                Rung::Reference | Rung::Told => {
                    if at >= 16 && at.is_multiple_of(8) {
                        let x = if rung == Rung::Told {
                            tape.features_told(
                                &params,
                                at,
                                reopen,
                                features::Told {
                                    informed: informed_id,
                                    worker: worker_id,
                                    half_life,
                                    open,
                                },
                            )
                        } else {
                            tape.features(&params, at, reopen)
                        };
                        let forecast = forest.predict(&x, &signed);
                        let mut target = cap * (forecast / FORECAST_SCALE_PCT).clamp(-1.0, 1.0);
                        if rung == Rung::Reference
                            && at + 64 >= reopen
                            && let Some(e) = tape.model.estimate(&params, at)
                        {
                            let room = model::target(
                                &Params {
                                    scale_pct: 1e-9,
                                    ..params
                                },
                                &e,
                                level,
                                s0,
                                cap,
                            )
                            .abs();
                            target = target.clamp(-room, room);
                        }
                        // Told where SPCX opens, it holds the gap into the
                        // reopen outright.
                        let gap_window = rung == Rung::Told && at + 64 >= reopen;
                        if gap_window {
                            target = cap * ((open - level) / FORECAST_SCALE_PCT).clamp(-1.0, 1.0);
                        }
                        if gap_window || (target - hold).abs() >= DEAD_BAND {
                            hold = target;
                        }
                        info!(at, level, forecast, target, hold, "forecast");
                    }
                    hold
                }
                Rung::G0 => {
                    if at + g0.enter_slots >= reopen && at.is_multiple_of(8) {
                        hold = tape
                            .model
                            .estimate(&g0, at)
                            .map_or(0.0, |e| model::target(&g0, &e, level, s0, cap));
                    }
                    hold
                }
                Rung::PathOracle => {
                    let plan = plan.get_or_insert_with(|| {
                        let path = regime.as_ref().and_then(|r| r["path"].as_array());
                        let level_at = |slot: u64| -> f64 {
                            path.into_iter()
                                .flatten()
                                .take_while(|w| w["slot"].as_u64().is_some_and(|s| s <= slot))
                                .last()
                                .and_then(|w| w["level_pct"].as_f64())
                                .unwrap_or(0.0)
                        };
                        let steps = (env.end_slot - start_slot) / 2;
                        let levels: Vec<f64> = (1..=steps).map(|k| level_at(2 * k)).collect();
                        let cost_bps = 1e4 * half_spread_usd / s0 + 3.5;
                        oracle::plan(&levels, s0, cost_bps, 0.15, cap, MAX_STEP_TRADE)
                    });
                    plan.get((at / 2).saturating_sub(1) as usize)
                        .copied()
                        .unwrap_or(0.0)
                }
                Rung::Follow | Rung::Fade => {
                    if at + 40 >= reopen {
                        let sign = if rung == Rung::Follow { 1.0 } else { -1.0 };
                        *decided.get_or_insert(if level.abs() > 1.0 {
                            sign * cap * level.signum()
                        } else {
                            0.0
                        })
                    } else {
                        0.0
                    }
                }
                Rung::Basis => {
                    let premium = level - index_level;
                    if premium.abs() > 0.8 {
                        hold = -cap * premium.signum();
                    } else if premium.abs() < 0.2 {
                        hold = 0.0;
                    }
                    hold
                }
                Rung::Fitted => {
                    if at + 60 >= reopen && at.is_multiple_of(8) {
                        let back = |n: u64| {
                            level
                                - history
                                    .iter()
                                    .rev()
                                    .find(|(s, _)| *s + n <= at)
                                    .map_or(0.0, |h| h.1)
                        };
                        let x = fitted_features(
                            level,
                            index_level,
                            back(40),
                            back(120),
                            &flows,
                            flow_total,
                            at,
                        );
                        let pred: f64 = x.iter().zip(FITTED).map(|(a, b)| a * b).sum();
                        hold = cap * (pred / 2.0).clamp(-1.0, 1.0);
                    }
                    hold
                }
            }
        };
        history.push((at, level));
        if let Err(e) = trade_toward(&trader, target.clamp(-cap, cap), mid).await {
            warn!("trading: {e}");
        }
    }
    let position = trader.position().await?;
    info!(position = position.size, "session over");
    Ok(())
}

fn fitted_features(
    level: f64,
    index_level: f64,
    back40: f64,
    back120: f64,
    flows: &HashMap<u16, (f64, f64, u64)>,
    total: f64,
    at: u64,
) -> Vec<f64> {
    let (mut top, mut top_net, mut top_recent) = (0.0, 0.0, 0.0);
    for (net, gross, last) in flows.values() {
        let score = net.abs() * (net.abs() / gross.max(1e-9));
        if score > top {
            top = score;
            top_net = *net;
            top_recent = if at.saturating_sub(*last) < 40 {
                1.0
            } else {
                0.0
            };
        }
    }
    vec![
        1.0,
        level,
        level - index_level,
        back40,
        back120,
        top_net,
        top_net * top_recent,
        total,
        level * top_recent,
    ]
}

/// The Phoenix orders on `orderbook` not seen before: their signer, side
/// (+1 buys) and size in shares, oldest first. Reads the market's recent
/// signatures, then each new transaction.
fn new_orders(
    trader: &Trader,
    orderbook: &str,
    market: &mtx_kit::phoenix_rise::types::exchange::ExchangeMarketConfig,
    seen: &mut std::collections::HashSet<String>,
) -> Vec<(u64, String, f64, f64, f64)> {
    let sigs: Value = match trader.rpc.send(
        RpcRequest::GetSignaturesForAddress,
        json!([orderbook, { "limit": 100, "commitment": "confirmed" }]),
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("getSignaturesForAddress: {e}");
            return Vec::new();
        }
    };
    let mut fresh: Vec<(u64, String)> = sigs
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["err"].is_null())
        .filter_map(|s| Some((s["slot"].as_u64()?, s["signature"].as_str()?.to_string())))
        .filter(|(_, sig)| !seen.contains(sig))
        .collect();
    fresh.sort_by_key(|(slot, _)| *slot);
    let mut out = Vec::new();
    for (slot, sig) in fresh {
        seen.insert(sig.clone());
        let tx: Value = match trader.rpc.send(
            RpcRequest::GetTransaction,
            json!([sig, { "encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed" }]),
        ) {
            Ok(tx) => tx,
            Err(_) => continue,
        };
        out.extend(
            orders_in(&tx, trader, market)
                .into_iter()
                .map(|(signer, side, size, limit)| (slot, signer, side, size, limit)),
        );
    }
    tracing::debug!(orders = out.len(), "new orders");
    out
}

/// The Phoenix orders in one transaction (`getTransaction`, json).
fn orders_in(
    tx: &Value,
    trader: &Trader,
    market: &mtx_kit::phoenix_rise::types::exchange::ExchangeMarketConfig,
) -> Vec<(String, f64, f64, f64)> {
    let mut out = Vec::new();
    if !tx["meta"]["err"].is_null() {
        return out;
    }
    let message = &tx["transaction"]["message"];
    let keys: Vec<&str> = message["accountKeys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let Some(signer) = keys.first() else {
        return out;
    };
    for ix in message["instructions"].as_array().into_iter().flatten() {
        let data = ix["data"]
            .as_str()
            .and_then(|d| bs58::decode(d).into_vec().ok())
            .unwrap_or_default();
        let Some(kind) = data
            .get(..8)
            .and_then(|tag| PhoenixInstruction::from_discriminant(tag.try_into().ok()?))
        else {
            continue;
        };
        if !matches!(
            kind,
            PhoenixInstruction::PlaceLimitOrder | PhoenixInstruction::PlaceMarketOrder
        ) {
            continue;
        }
        let Ok(packet) = <OrderPacket as borsh::BorshDeserialize>::deserialize(&mut &data[8..])
        else {
            continue;
        };
        let Ok(value) = serde_json::to_value(&packet) else {
            continue;
        };
        let Some((_, fields)) = value["kind"].as_object().and_then(|o| o.iter().next()) else {
            continue;
        };
        let side = match fields["side"].as_str() {
            Some(s) if s.eq_ignore_ascii_case("bid") => 1.0,
            Some(_) => -1.0,
            None => continue,
        };
        let Some(lots) = fields["num_base_lots"].as_i64() else {
            continue;
        };
        let limit = fields["price_in_ticks"]
            .as_u64()
            .map_or(0.0, |ticks| trader.phoenix.ticks_to_usd(market, ticks));
        out.push((
            signer.to_string(),
            side,
            trader.phoenix.lots_to_base_units(market, lots),
            limit,
        ));
    }
    out
}

/// Move the position toward `target` by up to `MAX_STEP_TRADE` shares, in
/// market orders of at most `SLICE`.
async fn trade_toward(trader: &Trader, target: f64, mid: f64) -> Result<()> {
    let position = trader.position().await?.size;
    let mut left = (target - position).clamp(-MAX_STEP_TRADE, MAX_STEP_TRADE);
    while left.abs() >= 0.5 {
        let size = left.abs().min(SLICE);
        let (side, limit) = if left > 0.0 {
            (Side::Bid, mid * (1.0 + SLIPPAGE_PCT / 100.0))
        } else {
            (Side::Ask, mid * (1.0 - SLIPPAGE_PCT / 100.0))
        };
        trader.place_market(side, size, limit).await?;
        left -= size * left.signum();
    }
    Ok(())
}
