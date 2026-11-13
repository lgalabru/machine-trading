//! Account-level SPCX order flow, explicit equity accounting, and reopening risk.
use mtx_kit::venue::phoenix_rise::accounts::trader::Trader;
use mtx_kit::{log, prelude::*, rpc, rpc_account};
use serde_json::{Value, json};

mod flow;
mod models;
mod policy;

const MODEL: usize = 1;

#[derive(Default)]
struct Bot {
    reopen: u64,
    start_slot: u64,
    start: f64,
    address: String,
    own: String,
    last_sig: String,
    tick: f64,
    lot_scale: f64,
    flow: flow::Flow,
    trader: String,
    asset_id: u64,
    peak: f64,
    last_mark: f64,
    policy: policy::Policy,
    previous_position: f64,
    last_side: f64,
}

impl Bot {
    /// Account value at the mark, including realized PnL and trading fees.
    fn risk(&mut self, market: &Market) -> Option<(f64, f64)> {
        let data = rpc_account(&self.trader).ok()??;
        let trader = Trader::try_from_account_bytes(&data).ok()?;
        let mut cash = trader.header.trader_state.quote_lot_collateral.as_inner() as f64
            / 1e6 - 10_000.0;
        let mut position = 0.0;
        for (asset, holding) in trader.positions() {
            if asset == self.asset_id {
                cash += holding.virtual_quote_lot_position().as_inner() as f64 / 1e6;
                position = holding.base_lot_position().as_inner() as f64 / self.lot_scale;
            }
        }
        let pnl = cash + position * market.mark;
        // Recover the previous turn's equity after its fills, before the new mark.
        self.peak = self.peak.max(pnl).max(cash + position * self.last_mark);
        self.last_mark = market.mark;
        Some((pnl, (self.peak - pnl).max(0.0)))
    }

    fn read_flow(&mut self, market: &Market) -> Vec<(f64, String, f64, f64)> {
        let options = if self.last_sig.is_empty() {
            json!({"commitment": "confirmed", "limit": 100})
        } else {
            json!({"commitment": "confirmed", "limit": 100, "until": self.last_sig})
        };
        let Ok(answer) = rpc("getSignaturesForAddress", &json!([self.address, options]).to_string())
            else { return vec![]; };
        let Ok(value) = serde_json::from_str::<Value>(&answer) else { return vec![]; };
        let Some(signatures) = value.as_array() else { return vec![]; };
        let mut result = Vec::new();

        for item in signatures.iter().rev() {
            let slot = item["slot"].as_u64().unwrap_or(0);
            if slot < self.start_slot || !item["err"].is_null() { continue; }
            let Some(signature) = item["signature"].as_str() else { continue; };
            let Ok(raw) = rpc("getTransaction", &json!([signature, {
                "commitment": "confirmed", "encoding": "json",
                "maxSupportedTransactionVersion": 0
            }]).to_string()) else { continue; };
            let Ok(transaction) = serde_json::from_str::<Value>(&raw) else { continue; };
            if !transaction["meta"]["err"].is_null() { continue; }
            let message = &transaction["transaction"]["message"];
            let Some(signer) = message["accountKeys"][0].as_str() else { continue; };
            if signer == self.own { continue; }
            let Some(instructions) = message["instructions"].as_array() else { continue; };

            for instruction in instructions {
                let Some(encoded) = instruction["data"].as_str() else { continue; };
                let Ok(bytes) = bs58::decode(encoded).into_vec() else { continue; };
                // Phoenix PlaceMarketOrder, IOC packet with a bounded limit price.
                if bytes.len() < 27
                    || bytes[..8] != [90, 118, 192, 252, 192, 99, 39, 145]
                    || bytes[8] != 2 || bytes[10] != 1 {
                    continue;
                }
                let side = if bytes[9] == 0 { 1.0 } else { -1.0 };
                let price = u64::from_le_bytes(bytes[11..19].try_into().unwrap()) as f64 * self.tick;
                let quantity = u64::from_le_bytes(bytes[19..27].try_into().unwrap()) as f64
                    / self.lot_scale;
                let at = (slot - self.start_slot) as f64;
                let previous = self.flow.history.iter().rev()
                    .find(|(time, _)| *time <= at)
                    .map_or(self.flow.p, |(_, price)| *price);
                let previous_usd = self.start * (1.0 + previous / 100.0);
                let limit = ((price / previous_usd - 1.0) * 10_000.0 * side + 0.3)
                    .clamp(5.0, 110.0);
                result.push((at, signer.to_string(), quantity * side, limit));
            }
        }
        if let Some(signature) = signatures.first().and_then(|item| item["signature"].as_str()) {
            self.last_sig = signature.to_string();
        }
        let _ = market;
        result
    }
}

impl Strategy for Bot {
    fn start(&mut self, session: &Session) -> BotResult {
        self.reopen = session.var("MTX_REOPEN_SLOT")
            .and_then(|slot| slot.parse().ok()).unwrap_or(session.end_slot);
        self.start_slot = session.start_slot;
        self.start = session.start_mark;
        self.own = session.authority.clone();
        self.address = session.venues.first().and_then(|venue| venue.address.clone())
            .unwrap_or_default();
        let venue = mtx_kit::venue::PhoenixVenue::from_json(&session.venue)?;
        let config = venue.market(&session.symbol)?;
        self.tick = mtx_kit::venue::ticks_to_usd(config, 1);
        self.lot_scale = 10f64.powi(config.base_lots_decimals as i32);
        self.trader = venue.trader_account(&mtx_kit::venue::key(&session.authority)?).to_string();
        self.asset_id = config.asset_id as u64;
        self.last_mark = session.start_mark;
        self.flow.reopen = (self.reopen - self.start_slot) as f64;
        self.flow.spread = 12.0;
        log(&format!("SPCX start={} reopen={} mark={}", self.start_slot, self.reopen, self.start));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        if market.slot >= self.reopen { return Ok(()); }
        let risk = self.risk(market);
        let mid = market.best_bid.zip(market.best_ask)
            .map_or(market.mark, |(bid, ask)| (bid + ask) * 0.5);
        let trades = self.read_flow(market);
        let at = (market.slot - self.start_slot) as f64;
        if let Some((bid, ask)) = market.best_bid.zip(market.best_ask) {
            let spread = ((ask - bid) / mid * 5000.0 - 0.35).clamp(8.0, 16.0);
            self.flow.spread = if self.flow.history.is_empty() { spread }
                else { self.flow.spread.min(spread) };
        }
        let change = market.position.size - self.previous_position;
        if change.abs() > 0.001 { self.last_side = change.signum(); }
        self.previous_position = market.position.size;
        // A large own fill can consume one side of the top of the book.
        // If the spread has widened, estimate the center from the other side.
        let fair = if let Some((bid, ask)) = market.best_bid.zip(market.best_ask) {
            let normal = self.flow.spread + 0.35;
            if (ask - bid) / mid * 5000.0 > normal + 0.5 {
                if self.last_side > 0.0 { bid / (1.0 - normal / 10_000.0) }
                else if self.last_side < 0.0 { ask / (1.0 + normal / 10_000.0) }
                else { mid }
            } else { mid }
        } else { mid };
        let price = (fair / self.start - 1.0) * 100.0;
        self.flow.update(at, price, trades);
        let features = self.flow.features(1.0);
        let mirrored = self.flow.features(-1.0);
        let forecast = (models::forecast(&features, MODEL)
            - models::forecast(&mirrored, MODEL)) * 0.5;
        let remaining = self.reopen - market.slot;
        let target = if let Some((_, drawdown)) = risk {
            self.policy.target(market, mid, self.start, remaining, drawdown, forecast)
        } else { 0.0 };
        let delta = target - market.position.size;
        let trade = policy::needs_trade(market.position.size, target, remaining, self.policy.stopped);
        if trade {
            let mut left = delta.abs();
            while left >= 0.1 {
                let size = left.min(5.0);
                if delta > 0.0 { orders.buy(size, mid * 1.004); }
                else { orders.sell(size, mid * 0.996); }
                left -= size;
            }
        }
        if trade || market.k % 16 == 0 {
            let (pnl, drawdown) = risk.unwrap_or((market.pnl, market.drawdown));
            log(&format!("slot={} book={:.4} position={:.3} pnl={:.2} drawdown={:.2} forecast_pct={:.4} target={:.3}",
                market.slot, mid, market.position.size, pnl, drawdown, forecast, target));
        }
        Ok(())
    }
}

mtx_kit::export!(Bot);
