//! The informed bot, scoring v2's 1: the reference's following, told who
//! the rival makers are, and nothing of the session's draws.
//!
//! It reads `MTX_REGIME`, what the verifier's `--informed` hands it: the
//! informed skewer's wallet and the decoys' (`wallets.skewer-informed`,
//! `wallets.skewer-decoy`, authorities the session resolves from the
//! latent's roster). It isn't told any view, when one leans, when the price
//! jumps or which way, and it trades the same market under the same
//! mandate, costs and latency as any bot.
//!
//! The book names a resting order's trader by a number, not a wallet, so it
//! names each maker by the slots its quotes were placed in: the told wallet
//! that sent a transaction in that slot, when only one did, read off the
//! wallets' signatures. Once a number has more such votes for the informed
//! wallet than for any other, it follows that maker's lean as the reference
//! follows the maker it chose.

use std::collections::{BTreeMap, BTreeSet};

use mtx_bot_skew_maker_follow::{
    BookReader, Follow, Follower, IndexHistory, Leans, index_of, makers,
};
use mtx_kit::{log, prelude::*};
use serde_json::Value;

/// The strategy.
#[derive(Default)]
pub struct Informed {
    book: BookReader,
    history: IndexHistory,
    leans: Leans,
    follower: Follower,
    /// The told wallets, the informed one first, and the slots each sent a
    /// transaction in.
    told: Vec<String>,
    sent: Vec<BTreeSet<u64>>,
    /// Each trader number's votes, by told wallet.
    votes: BTreeMap<u32, Vec<u32>>,
    /// The quotes already counted, by trader and slot.
    counted: BTreeSet<(u32, u64)>,
    chosen: Option<u32>,
}

/// The slots of a wallet's latest transactions.
fn sent_slots(wallet: &str) -> Vec<u64> {
    let params = serde_json::json!([wallet, { "limit": 20, "commitment": "confirmed" }]);
    let Ok(answer) = rpc("getSignaturesForAddress", &params.to_string()) else {
        return Vec::new();
    };
    let Ok(answer) = serde_json::from_str::<Value>(&answer) else {
        return Vec::new();
    };
    answer
        .as_array()
        .map(|list| list.iter().filter_map(|s| s["slot"].as_u64()).collect())
        .unwrap_or_default()
}

impl Informed {
    /// The trader number the informed wallet quotes from, once its votes
    /// say so.
    fn informed_trader(&self) -> Option<u32> {
        self.votes
            .iter()
            .filter(|(_, v)| v[0] >= 2 && v[1..].iter().all(|o| v[0] > 2 * o))
            .max_by_key(|(_, v)| v[0])
            .map(|(trader, _)| *trader)
    }
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.book = BookReader::new(session)?;
        self.follower = Follower::new(Follow::default());
        let told = session
            .var("MTX_REGIME")
            .ok_or("no MTX_REGIME: the informed bot is told the session's latent state")?;
        let told: Value = serde_json::from_str(told).map_err(|e| e.to_string())?;
        let wallets = |kind: &str| -> Vec<String> {
            told["wallets"][kind]
                .as_array()
                .map(|l| {
                    l.iter()
                        .filter_map(|w| w.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let informed = wallets("skewer-informed");
        let [informed] = informed.as_slice() else {
            return Err("MTX_REGIME names no single informed skewer".into());
        };
        self.told = std::iter::once(informed.clone())
            .chain(wallets("skewer-decoy"))
            .collect();
        self.sent = vec![BTreeSet::new(); self.told.len()];
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let index = index_of(market);
        self.history.push(market.slot, index);
        let book = self.book.read();
        let makers = makers(&book, self.book.tick_usd);
        self.leans.update(&makers, &self.history);
        let fresh: Vec<(u32, u64)> = makers
            .iter()
            .map(|(trader, q)| (*trader, q.slot))
            .filter(|key| !self.counted.contains(key))
            .collect();
        if !fresh.is_empty() {
            for (i, wallet) in self.told.iter().enumerate() {
                self.sent[i].extend(sent_slots(wallet));
                while self.sent[i].len() > 200 {
                    self.sent[i].pop_first();
                }
            }
            for (trader, slot) in fresh {
                let senders: Vec<usize> = (0..self.told.len())
                    .filter(|i| self.sent[*i].contains(&slot))
                    .collect();
                // A quote whose transaction isn't listed yet is read again
                // at the next steps.
                if senders.is_empty() && slot + 40 > market.slot {
                    continue;
                }
                self.counted.insert((trader, slot));
                if let [one] = senders[..] {
                    let n = self.told.len();
                    self.votes.entry(trader).or_insert_with(|| vec![0; n])[one] += 1;
                }
            }
        }
        let trader = self.informed_trader();
        if trader.is_some() && trader != self.chosen {
            log(&format!(
                "slot {}: the informed skewer quotes as trader {}",
                market.slot,
                trader.unwrap_or(0)
            ));
            self.chosen = trader;
        }
        let signal = self.chosen.map_or(0.0, |m| self.leans.of(m));
        self.follower.step(market, orders, signal);
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);
