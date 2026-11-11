//! The weekend-gap reference (`bots/weekend`) as a wasm strategy: the same
//! tape, structural model, forecaster and policy; see
//! `bots/weekend/src/main.rs` for the strategy. The modules and the
//! forecaster are `bots/weekend`'s own, linked from there.
//!
//! What changes is where it reads. The makers' best bid and ask, the index
//! and the mark come in the [`Market`] the grader hands `step`, from the same
//! view of Phoenix the process reads; the taker orders come through `rpc`
//! (`getSignaturesForAddress`, then `getTransaction`); the reopen's slot is
//! the session's `MTX_REOPEN_SLOT`. It plays the reference rung only: the
//! ladder's other rungs stay processes.

#[allow(dead_code)]
mod features;
#[allow(dead_code)]
mod gbm;
#[allow(dead_code)]
mod model;

use std::collections::{HashMap, HashSet};

use features::Tape;
use gbm::Forest;
use model::{Params, Trade};
use mtx_kit::{
    component::log,
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

const SLICE: f64 = 10.0;
const MAX_STEP_TRADE: f64 = 35.0;
const SLIPPAGE_PCT: f64 = 1.0;
const FORECAST_SCALE_PCT: f64 = 0.3;
const DEAD_BAND: f64 = 60.0;
const STOP_DRAWDOWN_USD: f64 = 450.0;
const FORECAST: &str = include_str!("../model/forecast.json");

/// What `start` reads of the session.
struct Setup {
    venue: PhoenixVenue,
    market: ExchangeMarketConfig,
    start_slot: u64,
    /// The reopen, in slots from the start.
    reopen: u64,
    cap: f64,
    me: String,
    forest: Forest,
}

#[derive(Default)]
struct Weekend {
    setup: Option<Setup>,
    params: Params,
    tape: Tape,
    ids: HashMap<String, u16>,
    start_mid: Option<f64>,
    half_spread_usd: f64,
    mids: HashMap<u64, f64>,
    hold: f64,
    seen_sigs: HashSet<String>,
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
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?.clone();
        let reopen = reopen_slot - session.start_slot;
        self.setup = Some(Setup {
            venue,
            market,
            start_slot: session.start_slot,
            reopen,
            cap: session.cap,
            me: session.authority.clone(),
            forest: Forest::parse(FORECAST).ok_or("the forecaster")?,
        });
        log(&format!("weekend: reopen at {reopen}"));
        Ok(())
    }

    fn step(&mut self, step: &Market, orders: &mut Orders) -> BotResult {
        let setup = self.setup.as_ref().ok_or("no session")?;
        let (start_slot, reopen, cap) = (setup.start_slot, setup.reopen, setup.cap);
        let at = step.slot - start_slot;
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
        for (tx_slot, signer, side, size, limit_usd) in new_orders(
            &setup.venue,
            &setup.market.market_pubkey,
            &setup.market,
            &mut self.seen_sigs,
        ) {
            if signer == setup.me || tx_slot < start_slot {
                continue;
            }
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
            if seen < reopen {
                self.tape.trade(Trade {
                    seen,
                    account,
                    side,
                    size,
                    limit,
                });
            }
        }

        let params = self.params;
        let target = if self.stopped {
            0.0
        } else if at >= reopen {
            self.hold
        } else {
            if at >= 16 && at.is_multiple_of(8) {
                let x = self.tape.features(&params, at, reopen);
                let forecast = setup.forest.predict(&x, &features::SIGNED);
                let mut target = cap * (forecast / FORECAST_SCALE_PCT).clamp(-1.0, 1.0);
                if at + 64 >= reopen
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
                if (target - self.hold).abs() >= DEAD_BAND {
                    self.hold = target;
                }
                log(&format!(
                    "forecast at={at} level={level} forecast={forecast} target={target} hold={}",
                    self.hold
                ));
            }
            self.hold
        };
        trade_toward(orders, position, target.clamp(-cap, cap), mid);
        Ok(())
    }
}

/// The Phoenix orders on `orderbook` not seen before: their slot, signer,
/// side (+1 buys), size in shares and limit in USD, oldest first.
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

mtx_kit::export!(Weekend);
