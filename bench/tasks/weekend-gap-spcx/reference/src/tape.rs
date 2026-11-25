//! Who trades SPCX, and since when: every transaction on the market's
//! account (`getSignaturesForAddress` at `confirmed`), its signer
//! (`getTransaction`), and the slot each signer first traded at. Its own
//! key is left out.

use std::collections::{BTreeMap, BTreeSet};

use mtx_kit::{
    Pubkey,
    serde_json::{self, Value, json},
};

/// The most signatures one read lists: more than a step's transactions.
const SIGNATURES_PER_READ: u64 = 100;
/// The most signers it keeps: the family's takers are about ten wallets.
const SIGNERS_MAX: usize = 64;
/// The most signatures it remembers as read: a session's transactions.
const SEEN_MAX: usize = 20_000;

#[derive(Debug, Default)]
pub struct Tape {
    own: Option<Pubkey>,
    seen: BTreeSet<String>,
    /// Each signer, and the slot of its first transaction on the market.
    first_slot: BTreeMap<Pubkey, u64>,
}

impl Tape {
    pub fn new(own: Pubkey) -> Self {
        Self {
            own: Some(own),
            ..Self::default()
        }
    }

    /// Read the market's transactions since the last read.
    pub fn read(&mut self, market_account: &str) -> Result<(), String> {
        let listed = call(
            "getSignaturesForAddress",
            json!([market_account, { "limit": SIGNATURES_PER_READ, "commitment": "confirmed" }]),
        )?;
        let mut fresh: Vec<(u64, String)> = listed
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["err"].is_null())
            .filter_map(|s| Some((s["slot"].as_u64()?, s["signature"].as_str()?.to_string())))
            .filter(|(_, signature)| !self.seen.contains(signature))
            .collect();
        fresh.sort();
        for (slot, signature) in fresh {
            if self.seen.len() >= SEEN_MAX {
                return Err("more transactions than a session holds".into());
            }
            let transaction = call(
                "getTransaction",
                json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed" }]),
            )?;
            self.seen.insert(signature);
            let Some(signer) = transaction["transaction"]["message"]["accountKeys"][0]
                .as_str()
                .and_then(|k| k.parse::<Pubkey>().ok())
            else {
                continue;
            };
            if Some(signer) == self.own || self.first_slot.contains_key(&signer) {
                continue;
            }
            if self.first_slot.len() < SIGNERS_MAX {
                self.first_slot.insert(signer, slot);
            }
        }
        Ok(())
    }

    /// The signers whose first trade came at `from_slot` or later, at most
    /// `most` of them, earliest first.
    pub fn late(&self, from_slot: u64, most: usize) -> Vec<Pubkey> {
        let mut late: Vec<(u64, Pubkey)> = self
            .first_slot
            .iter()
            .filter(|(_, slot)| **slot >= from_slot)
            .map(|(signer, slot)| (*slot, *signer))
            .collect();
        late.sort();
        late.into_iter().take(most).map(|(_, s)| s).collect()
    }
}

fn call(method: &str, params: Value) -> Result<Value, String> {
    let text = mtx_kit::rpc(method, &params.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("{method}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_signers_are_the_ones_first_seen_late() {
        let mut tape = Tape::new(Pubkey::new_unique());
        let (early, late) = (Pubkey::new_unique(), Pubkey::new_unique());
        tape.first_slot.insert(early, 100);
        tape.first_slot.insert(late, 560);
        assert_eq!(tape.late(550, 8), vec![late]);
        assert_eq!(tape.late(0, 8).len(), 2);
        assert!(tape.late(600, 8).is_empty());
    }
}
