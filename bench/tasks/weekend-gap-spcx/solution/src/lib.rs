//! Reference: read the weekend's flow, ride what it predicts, and hold the
//! gap into the reopen.
//!
//! While SPCX's own market is closed, Phoenix's index is the perp's price,
//! smoothed: it carries nothing the perp doesn't, and the mark sits on it.
//! The reference reads the flow instead. Each step it takes the makers' mid
//! (the perp's price, from the book's best bid and ask), Phoenix's index, and
//! every taker order since the last step (`getSignaturesForAddress` on the
//! market at `confirmed`, then `getTransaction`: the signer, side, size and
//! limit of each Phoenix order), and keeps a tape (`features.rs`): its
//! structural model's read (`model.rs`: which one-sided account's impact
//! stays, where that puts the open, whether it's still trading) and flow
//! statistics per account.
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
//! It builds two ways: a strategy component (`bot.wasm`, what the task's
//! oracle solution is), and a process (`src/main.rs`), which the ladder runs
//! as several rungs (`MTX_RUNG`): `reference` (the default); `g0`, the first
//! generation's reference (the structural model alone, the gap only);
//! `told`, whose forecaster also knows which accounts are the informed
//! trader and the worker (`MTX_REGIME`); `path-oracle`, told the perp's
//! whole path, which plans its positions by dynamic programming after costs
//! (`oracle.rs`); `follow`, the cap with the perp's move from the last
//! external price; `fade`, the cap against it; `basis`, which takes the
//! index for fair value; and `fitted`, a regression on generic features
//! fitted to generated sessions.

mod features;
mod gbm;
mod model;
mod oracle;

use std::collections::{HashMap, HashSet};

use features::Tape;
use gbm::Forest;
use model::{Params, Trade};
use mtx_kit::{
    log,
    prelude::*,
    rpc,
    venue::{
        PhoenixVenue,
        phoenix_rise::{
            ix::{PhoenixInstruction, order_packet::OrderPacket},
            types::exchange::ExchangeMarketConfig,
        },
    },
};
use serde_json::{Value, json};

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

/// What `start` reads of the session.
struct Setup {
    venue: PhoenixVenue,
    market: ExchangeMarketConfig,
    start_slot: u64,
    end_slot: u64,
    /// The reopen, in slots from the start.
    reopen: u64,
    cap: f64,
    me: String,
    rung: Rung,
    regime: Option<Value>,
    informed_slots: HashSet<u64>,
    worker_slots: HashSet<u64>,
    half_life: f64,
    open: f64,
    signed: Vec<usize>,
    forest: Forest,
}

/// The weekend reference, as a [`Strategy`].
#[derive(Default)]
pub struct Weekend {
    setup: Option<Setup>,
    params: Params,
    g0: Params,
    tape: Tape,
    ids: HashMap<String, u16>,
    informed_id: Option<u16>,
    worker_id: Option<u16>,
    /// Per account: net, gross, last slot (the fitted rung's flow).
    flows: HashMap<u16, (f64, f64, u64)>,
    flow_total: f64,
    start_mid: Option<f64>,
    half_spread_usd: f64,
    mids: HashMap<u64, f64>,
    hold: f64,
    decided: Option<f64>,
    history: Vec<(u64, f64)>,
    plan: Option<Vec<f64>>,
    seen_sigs: HashSet<String>,
    /// The account's value from the start, marked each step, against its
    /// peak: the stop.
    value: f64,
    peak: f64,
    last_mark: Option<f64>,
    stopped: bool,
}

impl Strategy for Weekend {
    fn start(&mut self, session: &Session) -> BotResult {
        let reopen_slot: u64 = session
            .var("MTX_REOPEN_SLOT")
            .ok_or("MTX_REOPEN_SLOT")?
            .parse()
            .map_err(|_| "MTX_REOPEN_SLOT")?;
        let reopen = reopen_slot - session.start_slot;
        let rung = match session.var("MTX_RUNG") {
            Some("g0") => Rung::G0,
            Some("told") => Rung::Told,
            Some("path-oracle") => Rung::PathOracle,
            Some("follow") => Rung::Follow,
            Some("fade") => Rung::Fade,
            Some("basis") => Rung::Basis,
            Some("fitted") => Rung::Fitted,
            _ => Rung::Reference,
        };
        let regime: Option<Value> = session
            .var("MTX_REGIME")
            .and_then(|r| serde_json::from_str(r).ok());
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
        let forest = Forest::parse(if rung == Rung::Told {
            FORECAST_TOLD
        } else {
            FORECAST
        })
        .ok_or("the forecaster")?;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?.clone();
        self.setup = Some(Setup {
            venue,
            market,
            start_slot: session.start_slot,
            end_slot: session.end_slot,
            reopen,
            cap: session.cap,
            me: session.authority.clone(),
            rung,
            regime,
            informed_slots,
            worker_slots,
            half_life,
            open,
            signed,
            forest,
        });
        log(&format!("weekend: rung {rung:?}, reopen at {reopen}"));
        Ok(())
    }

    fn step(&mut self, step: &Market, orders: &mut Orders) -> BotResult {
        let setup = self.setup.as_ref().ok_or("no session")?;
        let (start_slot, reopen, cap, rung) =
            (setup.start_slot, setup.reopen, setup.cap, setup.rung);
        let at = step.slot - start_slot;
        // The perp's price: the makers' mid.
        let (Some(bid), Some(ask)) = (step.best_bid, step.best_ask) else {
            return Ok(());
        };
        let mid = 0.5 * (bid + ask);
        let (index, mark) = (step.index, step.mark);
        let half_spread_usd = &mut self.half_spread_usd;
        let s0 = *self.start_mid.get_or_insert_with(|| {
            *half_spread_usd = 0.5 * (ask - bid);
            mid
        });
        self.mids.insert(at, mid);
        let level = (mid / s0 - 1.0) * 100.0;
        let index_level = (index / s0 - 1.0) * 100.0;
        let position = step.position.size;
        if let Some(last) = self.last_mark {
            self.value += position * (mark - last);
        }
        self.last_mark = Some(mark);
        self.peak = self.peak.max(self.value);
        if !self.stopped && self.peak - self.value > STOP_DRAWDOWN_USD {
            log("drawdown past the stop; flat from here");
            self.stopped = true;
        }
        if at < reopen {
            self.tape.observe(at, level, index_level);
        }
        // The orders since the last step: every Phoenix order on SPCX but
        // ours, from the market's transaction history.
        for (tx_slot, signer, side, size, limit_usd) in new_orders(
            &setup.venue,
            &setup.market.market_pubkey,
            &setup.market,
            &mut self.seen_sigs,
        ) {
            if signer == setup.me || tx_slot < start_slot {
                continue;
            }
            // The step that first showed the order: it executes in the slot
            // the last step ended at, and the next step shows it, whenever
            // the listing does. It was priced off that step's price.
            let landed = tx_slot - start_slot;
            let seen = (landed / 2 * 2 + 2).min(at);
            let priced = self
                .mids
                .get(&(seen.saturating_sub(2)))
                .copied()
                .unwrap_or(mid);
            let limit = if self.half_spread_usd > 0.0 {
                (limit_usd - priced).abs() / self.half_spread_usd
            } else {
                0.0
            };
            let next = self.ids.len() as u16;
            let account = *self.ids.entry(signer).or_insert(next);
            if setup.informed_slots.contains(&seen.saturating_sub(2)) {
                self.informed_id.get_or_insert(account);
            }
            if setup.worker_slots.contains(&seen.saturating_sub(2)) {
                self.worker_id.get_or_insert(account);
            }
            if seen < reopen {
                self.tape.trade(Trade {
                    seen,
                    account,
                    side,
                    size,
                    limit,
                });
            }
            let f = self.flows.entry(account).or_insert((0.0, 0.0, 0));
            f.0 += side * size;
            f.1 += size;
            f.2 = at;
            self.flow_total += side * size;
        }

        let params = self.params;
        let target = if self.stopped {
            0.0
        } else if at >= reopen && rung != Rung::PathOracle {
            self.hold
        } else {
            match rung {
                Rung::Reference | Rung::Told => {
                    if at >= 16 && at.is_multiple_of(8) {
                        let x = if rung == Rung::Told {
                            self.tape.features_told(
                                &params,
                                at,
                                reopen,
                                features::Told {
                                    informed: self.informed_id,
                                    worker: self.worker_id,
                                    half_life: setup.half_life,
                                    open: setup.open,
                                },
                            )
                        } else {
                            self.tape.features(&params, at, reopen)
                        };
                        let forecast = setup.forest.predict(&x, &setup.signed);
                        let mut target = cap * (forecast / FORECAST_SCALE_PCT).clamp(-1.0, 1.0);
                        if rung == Rung::Reference
                            && at + 64 >= reopen
                            && let Some(e) = self.tape.model.estimate(&params, at)
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
                            target =
                                cap * ((setup.open - level) / FORECAST_SCALE_PCT).clamp(-1.0, 1.0);
                        }
                        if gap_window || (target - self.hold).abs() >= DEAD_BAND {
                            self.hold = target;
                        }
                        log(&format!(
                            "forecast at={at} level={level} forecast={forecast} target={target} hold={}",
                            self.hold
                        ));
                    }
                    self.hold
                }
                Rung::G0 => {
                    let g0 = self.g0;
                    if at + g0.enter_slots >= reopen && at.is_multiple_of(8) {
                        self.hold = self
                            .tape
                            .model
                            .estimate(&g0, at)
                            .map_or(0.0, |e| model::target(&g0, &e, level, s0, cap));
                    }
                    self.hold
                }
                Rung::PathOracle => {
                    let half_spread_usd = self.half_spread_usd;
                    let plan = self.plan.get_or_insert_with(|| {
                        let path = setup.regime.as_ref().and_then(|r| r["path"].as_array());
                        let level_at = |slot: u64| -> f64 {
                            path.into_iter()
                                .flatten()
                                .take_while(|w| w["slot"].as_u64().is_some_and(|s| s <= slot))
                                .last()
                                .and_then(|w| w["level_pct"].as_f64())
                                .unwrap_or(0.0)
                        };
                        let steps = (setup.end_slot - start_slot) / 2;
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
                        *self.decided.get_or_insert(if level.abs() > 1.0 {
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
                        self.hold = -cap * premium.signum();
                    } else if premium.abs() < 0.2 {
                        self.hold = 0.0;
                    }
                    self.hold
                }
                Rung::Fitted => {
                    if at + 60 >= reopen && at.is_multiple_of(8) {
                        let back = |n: u64| {
                            level
                                - self
                                    .history
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
                            &self.flows,
                            self.flow_total,
                            at,
                        );
                        let pred: f64 = x.iter().zip(FITTED).map(|(a, b)| a * b).sum();
                        self.hold = cap * (pred / 2.0).clamp(-1.0, 1.0);
                    }
                    self.hold
                }
            }
        };
        self.history.push((at, level));
        trade_toward(orders, position, target.clamp(-cap, cap), mid);
        Ok(())
    }
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

/// The Phoenix orders on `orderbook` not seen before: their slot, signer,
/// side (+1 buys), size in shares and limit in USD, oldest first. Reads the
/// market's recent signatures, then each new transaction.
fn new_orders(
    venue: &PhoenixVenue,
    orderbook: &str,
    market: &ExchangeMarketConfig,
    seen: &mut HashSet<String>,
) -> Vec<(u64, String, f64, f64, f64)> {
    let call = |method: &str, params: Value| -> Option<Value> {
        serde_json::from_str(&rpc(method, &params.to_string()).ok()?).ok()
    };
    let Some(sigs) = call(
        "getSignaturesForAddress",
        json!([orderbook, { "limit": 100, "commitment": "confirmed" }]),
    ) else {
        return Vec::new();
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
        let Some(tx) = call(
            "getTransaction",
            json!([sig, { "encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed" }]),
        ) else {
            continue;
        };
        out.extend(
            orders_in(&tx, venue, market)
                .into_iter()
                .map(|(signer, side, size, limit)| (slot, signer, side, size, limit)),
        );
    }
    out
}

/// The Phoenix orders in one transaction (`getTransaction`, json).
fn orders_in(
    tx: &Value,
    venue: &PhoenixVenue,
    market: &ExchangeMarketConfig,
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
            .map_or(0.0, |ticks| venue.ticks_to_usd(market, ticks));
        out.push((
            signer.to_string(),
            side,
            venue.lots_to_base_units(market, lots),
            limit,
        ));
    }
    out
}

/// Market orders moving the position toward `target` by up to
/// [`MAX_STEP_TRADE`] shares, in slices of at most [`SLICE`].
fn trade_toward(orders: &mut Orders, position: f64, target: f64, mid: f64) {
    let mut left = (target - position).clamp(-MAX_STEP_TRADE, MAX_STEP_TRADE);
    while left.abs() >= 0.5 {
        let size = left.abs().min(SLICE);
        if left > 0.0 {
            orders.buy(size, mid * (1.0 + SLIPPAGE_PCT / 100.0));
        } else {
            orders.sell(size, mid * (1.0 - SLIPPAGE_PCT / 100.0));
        }
        left -= size * left.signum();
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Weekend);
