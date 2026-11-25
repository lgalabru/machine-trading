//! Watches that couldn't start, tried again (`control.watch`, `pools`,
//! `lending`). A plugin that starts before its validator has loaded the
//! accounts finds none of them: on mtx-alpha on 2026-11-25 the validator
//! restarted at 06:10:14 UTC, the plugin at 06:10:35, and the markets,
//! pools and obligations it watched stayed unwatched for 16 hours, until a
//! person reloaded it. Strategies came back on their own at 06:14.
//!
//! What failed is kept, and tried again on a [`Backoff`]: the first retry
//! [`FIRST_GAP_SLOTS`] after the start, each gap after it twice the last,
//! up to [`MAX_GAP_SLOTS`], for as long as anything is missing. The node is
//! asked in the background ([`super::strategies`] spawns it), so a retry
//! never holds a strategy's step; each slot the loop only checks whether
//! the cache now holds what was missing ([`all_seen`]), and starts watching
//! what it does. A retry is logged on its 1st, 2nd, 4th, 8th... attempt,
//! so a watch that never comes back logs a line an hour at most.

use mtx_core::feed::Address;

use super::feed::Accounts;

/// Slots before the first retry: ten seconds, about what a restarted
/// validator took to load its accounts on mtx-alpha.
pub const FIRST_GAP_SLOTS: u64 = 25;

/// The longest gap between two retries: ten minutes.
pub const MAX_GAP_SLOTS: u64 = 1_500;

/// When to try again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The slot the next attempt is due at, once the first slot is known.
    next_slot: Option<u64>,
    gap_slots: u64,
    attempts: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            next_slot: None,
            gap_slots: FIRST_GAP_SLOTS,
            attempts: 0,
        }
    }
}

impl Backoff {
    /// Whether an attempt is due at `slot`; if so, it counts it and
    /// schedules the next, its gap twice the last, up to [`MAX_GAP_SLOTS`].
    /// The first slot seen only starts the clock.
    pub fn due(&mut self, slot: u64) -> bool {
        let next = *self
            .next_slot
            .get_or_insert(slot.saturating_add(self.gap_slots));
        if slot < next {
            return false;
        }
        self.attempts = self.attempts.saturating_add(1);
        self.gap_slots = self.gap_slots.saturating_mul(2).min(MAX_GAP_SLOTS);
        self.next_slot = Some(slot.saturating_add(self.gap_slots));
        assert!(self.gap_slots >= FIRST_GAP_SLOTS && self.gap_slots <= MAX_GAP_SLOTS);
        true
    }

    /// The attempts made so far.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Whether the last attempt is one to log: the 1st, 2nd, 4th, 8th...
    pub fn worth_logging(&self) -> bool {
        self.attempts.is_power_of_two()
    }
}

/// Whether `accounts` holds every one of `addresses`.
pub fn all_seen(accounts: &impl Accounts, addresses: &[Address]) -> bool {
    addresses
        .iter()
        .all(|address| accounts.data(address).is_some())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A cache that lacks some accounts at first and gains them later.
    #[derive(Default)]
    struct Cache(BTreeMap<Address, Vec<u8>>);

    impl Accounts for Cache {
        fn data(&self, address: &Address) -> Option<&[u8]> {
            self.0.get(address).map(Vec::as_slice)
        }
    }

    #[test]
    fn retries_come_on_a_doubling_gap_up_to_the_cap() {
        let mut backoff = Backoff::default();
        let mut due = Vec::new();
        for slot in 1_000..30_000 {
            if backoff.due(slot) {
                due.push(slot);
            }
        }
        // 25 after the start, then 50, 100, 200, 400, 800, and 1,500 from then on.
        assert_eq!(
            &due[..7],
            &[1_025, 1_075, 1_175, 1_375, 1_775, 2_575, 4_075]
        );
        assert!(due.windows(2).skip(6).all(|w| w[1] - w[0] == MAX_GAP_SLOTS));
        assert_eq!(backoff.attempts() as usize, due.len());
    }

    #[test]
    fn a_retry_is_logged_at_powers_of_two() {
        let mut backoff = Backoff::default();
        let mut logged = Vec::new();
        for slot in 0..100_000 {
            if backoff.due(slot) && backoff.worth_logging() {
                logged.push(backoff.attempts());
            }
        }
        assert_eq!(&logged[..6], &[1, 2, 4, 8, 16, 32]);
    }

    #[test]
    fn what_was_missing_is_seen_once_the_cache_gains_it() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let mut cache = Cache::default();
        assert!(!all_seen(&cache, &[a, b]));
        cache.0.insert(a, vec![1]);
        assert!(!all_seen(&cache, &[a, b]));
        cache.0.insert(b, vec![2]);
        assert!(all_seen(&cache, &[a, b]));
        assert!(all_seen(&cache, &[]));
    }
}
