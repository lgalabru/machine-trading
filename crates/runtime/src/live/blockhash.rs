//! The newest blockhash the validator produced, from the plugin's block
//! notifications: what every live transaction is signed on, with no
//! round trip to the node's RPC.
//!
//! A blockhash is good for 150 blocks, about a minute; this one is used
//! while it is under [`FRESH_FOR`] old, and the chain clients ask the RPC
//! past that (a node that stopped producing blocks, or a plugin that never
//! told one).

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use solana_transaction::Hash;

/// How long a blockhash the plugin told of is used: well inside its 150
/// blocks (about 60 s), so a transaction signed on it still lands after a
/// few slots in flight.
pub const FRESH_FOR: Duration = Duration::from_secs(20);

#[derive(Debug, Default)]
pub struct LatestBlockhash {
    latest: Mutex<Option<(u64, Hash, Instant)>>,
}

impl LatestBlockhash {
    /// The validator produced `slot`'s block with `hash`. An older slot than
    /// the one kept changes nothing.
    pub fn produced(&self, slot: u64, hash: Hash) {
        // Poisoned: a thread panicked holding it. The chain clients then
        // find no fresh hash and ask the RPC: slower, never wrong.
        let Ok(mut latest) = self.latest.lock() else {
            return;
        };
        if latest.is_none_or(|(kept, _, _)| slot > kept) {
            *latest = Some((slot, hash, Instant::now()));
        }
    }

    /// The newest blockhash, if it is fresh.
    pub fn fresh(&self) -> Option<Hash> {
        // Poisoned: as in `produced`, no fresh hash.
        let latest = self.latest.lock().ok()?;
        latest
            .filter(|(_, _, at)| at.elapsed() < FRESH_FOR)
            .map(|(_, hash, _)| hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_block_wins_and_an_older_one_changes_nothing() {
        let latest = LatestBlockhash::default();
        assert_eq!(latest.fresh(), None);
        let (a, b) = (Hash::new_from_array([1; 32]), Hash::new_from_array([2; 32]));
        latest.produced(10, a);
        latest.produced(9, b);
        assert_eq!(latest.fresh(), Some(a));
        latest.produced(11, b);
        assert_eq!(latest.fresh(), Some(b));
    }
}
