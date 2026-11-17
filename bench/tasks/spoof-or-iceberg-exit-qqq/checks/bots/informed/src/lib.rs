//! The informed bot, scoring v2's 1: the reference's order handling, told
//! who the spoofer and the iceberg are, and nothing of the session's draws.
//!
//! It reads `MTX_REGIME`, what the verifier's `--informed` hands it: the
//! spoofer's wallet and the iceberg's (`wallets.spoofer`,
//! `wallets.iceberg`, authorities the session resolves from the latent's
//! roster). It isn't told which way the jump goes, when, or how far, nor
//! when or where either wall goes up, and it trades the same market under
//! the same mandate, costs and latency as any bot.
//!
//! The book numbers its traders, so it learns each told wallet's number
//! the first time that wallet walls: the order a wallet's transaction put
//! on the book in that transaction's slot, read off the wallet's
//! signatures. From then on it reads every wall by its owner: a spoofer's
//! bid or an iceberg's ask is a fall coming, and it sells the rest at once;
//! a spoofer's ask or an iceberg's bid is a rise, and it holds. Otherwise
//! it sells late, as the reference does.

use std::collections::BTreeMap;

use mtx_bot_spoof_or_iceberg_exit::{
    BookReader, Kind, Order, Rest, WALL_MIN, sell_late, sell_now, sign_of, walls_in,
};
use mtx_kit::{log, prelude::*};

/// The strategy.
#[derive(Default)]
pub struct Informed {
    order: Order,
    book: BookReader,
    /// Each told wallet, and its kind.
    told: Vec<(String, Kind)>,
    /// The book's number for each told wallet's trader, once known.
    traders: BTreeMap<u32, Kind>,
    /// Slots each told wallet sent a transaction in.
    sent: Vec<Vec<u64>>,
    selling: bool,
}

/// The first wallet listed under `kind` in the told state.
fn wallet(told: &str, kind: &str) -> Option<String> {
    told.split(&format!("\"{kind}\":[\""))
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .map(str::to_string)
}

/// The slots of a wallet's latest transactions.
fn sent_slots(wallet: &str) -> Vec<u64> {
    let params = serde_json::json!([wallet, { "limit": 10, "commitment": "confirmed" }]);
    let Ok(answer) = rpc("getSignaturesForAddress", &params.to_string()) else {
        return Vec::new();
    };
    let Ok(answer) = serde_json::from_str::<serde_json::Value>(&answer) else {
        return Vec::new();
    };
    answer
        .as_array()
        .map(|list| list.iter().filter_map(|s| s["slot"].as_u64()).collect())
        .unwrap_or_default()
}

impl Informed {
    /// Learns the trader number of any told wallet that just put a wall up.
    fn learn(&mut self, book: &[Rest]) {
        if self.traders.len() == self.told.len() {
            return;
        }
        for (i, (wallet, kind)) in self.told.clone().iter().enumerate() {
            if self.traders.values().any(|k| k == kind) {
                continue;
            }
            for slot in sent_slots(wallet) {
                if !self.sent[i].contains(&slot) {
                    self.sent[i].push(slot);
                }
            }
            let mine: Vec<u32> = book
                .iter()
                .filter(|r| r.initial >= WALL_MIN && self.sent[i].contains(&r.slot))
                .filter(|r| !self.traders.contains_key(&r.trader))
                .map(|r| r.trader)
                .collect();
            if let Some(&trader) = mine.first()
                && mine.iter().all(|t| *t == trader)
            {
                log(&format!("the {kind:?} {wallet} trades as {trader}"));
                self.traders.insert(trader, *kind);
            }
        }
    }
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.order = Order::from_session(session)?;
        self.book = BookReader::new(session)?;
        let told = session
            .var("MTX_REGIME")
            .ok_or("no MTX_REGIME: the informed bot is told the session's latent state")?;
        let spoofer = wallet(told, "spoofer").ok_or("MTX_REGIME names no spoofer")?;
        let iceberg = wallet(told, "iceberg").ok_or("MTX_REGIME names no iceberg")?;
        self.told = vec![(spoofer, Kind::Spoofer), (iceberg, Kind::Iceberg)];
        self.sent = vec![Vec::new(); 2];
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        orders.cancel_all();
        if !self.selling {
            let book = self.book.read();
            self.learn(&book);
            let signal = walls_in(&book, self.book.tick_usd, market.best_bid, market.best_ask)
                .iter()
                .find_map(|r| self.traders.get(&r.trader).map(|k| sign_of(*k, r.bid)));
            if signal.is_some_and(|s| s < 0.0) {
                log(&format!("slot {}: a fall is coming; selling", market.slot));
                self.selling = true;
            }
        }
        if self.selling {
            sell_now(market, orders, market.position.size);
        } else {
            sell_late(market, orders, &self.order);
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);
