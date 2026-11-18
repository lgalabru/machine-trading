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
//! The book's trader numbers are slots Phoenix reuses, so it names a wall's
//! owner by the slot the wall was placed in: the told wallet that sent a
//! transaction in that slot (read off the wallet's signatures). A spoofer's
//! bid or an iceberg's ask is a fall coming, and it sells the rest at once;
//! a spoofer's ask or an iceberg's bid is a rise, and it holds, with nothing
//! resting, until the jump lands. Otherwise
//! it works the order as the reference does: an ask at the touch, and what
//! is left sold late.

use mtx_bot_spoof_or_iceberg_exit::{
    BookReader, JumpWatch, Kind, Order, OwnAsk, Rest, sell_now, sign_of, walls_in, work,
};
use mtx_kit::{log, prelude::*};

/// The strategy.
#[derive(Default)]
pub struct Informed {
    order: Order,
    book: BookReader,
    /// Each told wallet, and its kind.
    told: Vec<(String, Kind)>,
    /// Slots each told wallet sent a transaction in.
    sent: Vec<Vec<u64>>,
    selling: bool,
    /// A rise is coming: hold, with nothing resting, until it lands.
    rising: bool,
    jump: JumpWatch,
    own: OwnAsk,
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
    let params = serde_json::json!([wallet, { "limit": 20, "commitment": "confirmed" }]);
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
    /// Reads the told wallets' transactions since the last step.
    fn watch(&mut self) {
        for (i, (wallet, _)) in self.told.iter().enumerate() {
            for slot in sent_slots(wallet) {
                if !self.sent[i].contains(&slot) {
                    self.sent[i].push(slot);
                }
            }
        }
    }

    /// Whose a wall is: the told wallet that sent a transaction in the slot
    /// it was placed in, when only one did.
    fn owner(&self, wall: &Rest) -> Option<Kind> {
        let senders: Vec<Kind> = self
            .told
            .iter()
            .zip(&self.sent)
            .filter(|(_, sent)| sent.contains(&wall.slot))
            .map(|((_, kind), _)| *kind)
            .collect();
        match senders[..] {
            [kind] => Some(kind),
            _ => None,
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
        self.jump.update(market.slot, market.oracle);
        if !self.selling {
            let mut book = self.book.read();
            self.own.drop_from(&mut book, self.book.tick_usd);
            self.watch();
            let signal = walls_in(&book, self.book.tick_usd, market.best_bid, market.best_ask)
                .iter()
                .find_map(|r| self.owner(r).map(|k| sign_of(k, r.bid)));
            if signal.is_some_and(|s| s < 0.0) {
                log(&format!("slot {}: a fall is coming; selling", market.slot));
                self.selling = true;
            }
            if signal.is_some_and(|s| s > 0.0) && !self.jump.landed {
                self.rising = true;
            }
        }
        if self.selling {
            sell_now(market, orders, market.position.size);
        } else if !(self.rising && !self.jump.landed) {
            self.own.rested = work(market, orders, &self.order);
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);
