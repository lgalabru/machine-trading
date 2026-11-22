//! Markets the runtime watches without trading them (`control.watch`): what
//! a sidecar choosing where to run strategies needs to see of every
//! market, not only the ones running something.
//!
//! Each watched market's accounts join the feed's watch list and the
//! account cache, as a strategy's do. After the strategies' steps, a few
//! are read each slot on a round-robin, so every one is read once in
//! [`WatchConfig::every_slots`], and each read is told on the event stream
//! as a `quote`; their trades come as `trade` events. Reading a Phoenix
//! market costs a few hundred microseconds: 110 markets every 25 slots is
//! about five reads, a millisecond or two, a slot.

use std::path::PathBuf;

use serde::Deserialize;

use super::perp::PerpReader;

/// Watched markets at most: Phoenix lists about 110.
pub const WATCHED_MAX: usize = 256;

/// The fewest slots between two reads of a market: a market read every
/// slot would be a strategy.
pub const EVERY_SLOTS_MIN: u64 = 5;

fn default_every_slots() -> u64 {
    25
}

/// What to watch.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    /// Phoenix's `/exchange` response, where the markets are listed.
    pub exchange: PathBuf,
    /// The markets, by symbol; none for every market that trades now.
    #[serde(default)]
    pub symbols: Vec<String>,
    /// Each market is read once in this many slots (25: ten seconds).
    #[serde(default = "default_every_slots")]
    pub every_slots: u64,
}

/// The symbols to watch: those asked for that the venue lists, or every
/// active market, in the venue's order, at most [`WATCHED_MAX`].
pub fn symbols(venue: &mtx_kit::venue::PhoenixVenue, wanted: &[String]) -> Vec<String> {
    let listed = venue.markets();
    let chosen: Vec<String> = if wanted.is_empty() {
        listed
            .iter()
            .filter(|m| m.active)
            .map(|m| m.instrument.symbol.clone())
            .collect()
    } else {
        wanted
            .iter()
            .filter(|w| listed.iter().any(|m| &m.instrument.symbol == *w))
            .cloned()
            .collect()
    };
    chosen.into_iter().take(WATCHED_MAX).collect()
}

/// Markets read in a slot so that `n` are each read once in `every` slots.
pub fn per_slot(n: usize, every: u64) -> usize {
    let every = usize::try_from(every.max(1)).unwrap_or(usize::MAX);
    n.div_ceil(every)
}

/// The watched markets and where the round-robin is.
pub struct Watching {
    markets: Vec<(String, PerpReader)>,
    every_slots: u64,
    cursor: usize,
}

impl Watching {
    pub fn new(markets: Vec<(String, PerpReader)>, every_slots: u64) -> Self {
        assert!(markets.len() <= WATCHED_MAX, "watched markets are bounded");
        Self {
            markets,
            every_slots: every_slots.max(EVERY_SLOTS_MIN),
            cursor: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.markets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.markets.is_empty()
    }

    /// The markets to read this slot, moving the round-robin on.
    pub fn due(&mut self) -> Vec<&(String, PerpReader)> {
        let n = self.markets.len();
        if n == 0 {
            return Vec::new();
        }
        let take = per_slot(n, self.every_slots).min(n);
        let start = self.cursor;
        self.cursor = (self.cursor + take) % n;
        (0..take).map(|i| &self.markets[(start + i) % n]).collect()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// Over `every` slots, the round-robin reads every market at least
        /// once, and never more than the slot's share.
        #[test]
        fn every_market_is_read_within_its_period(n in 0usize..300, every in 5u64..60) {
            let per = per_slot(n, every);
            prop_assert!(per as u64 * every >= n as u64);
            if n > 0 { prop_assert!(per <= n); }
            // A cursor walk over `every` slots covers every index.
            let mut seen = vec![false; n];
            let mut cursor = 0usize;
            for _ in 0..every {
                if n == 0 { break; }
                let take = per.min(n);
                for i in 0..take { seen[(cursor + i) % n] = true; }
                cursor = (cursor + take) % n;
            }
            prop_assert!(seen.iter().all(|s| *s));
        }
    }

    #[test]
    fn the_venue_says_what_can_be_watched() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let json =
            std::fs::read_to_string(root.join("snapshots/phoenix-spcx-2026-10-29/exchange.json"))
                .unwrap();
        let venue = mtx_kit::venue::PhoenixVenue::from_json(&json).unwrap();
        let all = symbols(&venue, &[]);
        assert!(all.len() > 50 && all.len() <= WATCHED_MAX);
        assert!(all.contains(&"TAO".to_string()));
        let some = symbols(&venue, &["TAO".into(), "NOPE".into()]);
        assert_eq!(some, vec!["TAO".to_string()]);
    }
}
