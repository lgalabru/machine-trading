//! The feed: what the chain did, handed from the threads that saw it (a
//! validator's, inside a geyser plugin) to the runtime's own thread, and
//! the accounts the runtime keeps from it.
//!
//! The sending side runs on threads that aren't ours and must never wait:
//! [`FeedSender::send`] drops an event the runtime has no room for, and
//! counts it. A feed that dropped anything is lossy, and what was read
//! from it can't be traded on.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
};

use mtx_core::feed::{AccountWrite, Address, FeedEvent};

/// The most events that wait between the chain's threads and the runtime.
/// A slot of a session writes a few hundred watched accounts at most, so
/// this is minutes of a runtime that stopped reading, and about 64 MiB of
/// 1 KiB accounts at worst.
pub const FEED_EVENTS_MAX: usize = 1 << 16;

/// The accounts the runtime wants: the sender drops every other write
/// before copying it.
#[derive(Debug, Default)]
pub struct Watch {
    addresses: RwLock<HashSet<Address>>,
    /// The transactions of the program the tape keeps ([`super::tape`]).
    pub tape: super::tape::Tape,
}

impl Watch {
    pub fn new(addresses: impl IntoIterator<Item = Address>) -> Self {
        Self {
            addresses: RwLock::new(addresses.into_iter().collect()),
            tape: super::tape::Tape::default(),
        }
    }

    /// Watch one more account: a strategy added while the runtime runs.
    pub fn add(&self, address: Address) {
        self.addresses
            .write()
            .expect("the watch lock is never held across a panic")
            .insert(address);
    }

    /// Whether `address` is watched. Anything that isn't 32 bytes isn't.
    pub fn has(&self, address: &[u8]) -> bool {
        let Ok(address) = Address::try_from(address) else {
            return false;
        };
        self.addresses
            .read()
            .expect("the watch lock is never held across a panic")
            .contains(&address)
    }
}

/// The chain's side of the feed. Cheap to clone; safe to call from any
/// thread.
#[derive(Debug, Clone)]
pub struct FeedSender {
    events: SyncSender<FeedEvent>,
    watch: Arc<Watch>,
    dropped: Arc<AtomicU64>,
    /// Events handed to the runtime so far.
    sent: Arc<AtomicU64>,
    /// The newest slot the chain told of.
    notified_slot: Arc<AtomicU64>,
    /// When each recent slot was told of, and applied.
    clocks: Arc<SlotClocks>,
}

/// Whether a slot's status means the node has a bank for it: what the
/// strategies step on, since the accounts they read are that bank's. A slot
/// whose shreds have only started arriving is the cluster's tip, ahead of
/// the accounts while the node catches up; a dead or merely completed one
/// has no bank to read.
pub fn banked(status: mtx_core::feed::SlotStatus) -> bool {
    !matches!(
        status,
        mtx_core::feed::SlotStatus::FirstShredReceived
            | mtx_core::feed::SlotStatus::Completed
            | mtx_core::feed::SlotStatus::Dead
    )
}

/// Slots whose times are kept: a few minutes of 400 ms slots, more than a
/// step ever looks back.
pub const SLOT_CLOCKS_MAX: usize = 512;

/// When the validator first told the plugin of each recent slot, and when
/// the runtime's feed applied a banked status of it: where a step's
/// [`mtx_core::events::StepTimings`] start.
#[derive(Debug, Default)]
pub struct SlotClocks {
    slots: std::sync::Mutex<std::collections::VecDeque<SlotClock>>,
}

#[derive(Debug, Clone, Copy)]
pub struct SlotClock {
    pub slot: u64,
    pub notified: std::time::Instant,
    pub applied: Option<std::time::Instant>,
}

impl SlotClocks {
    /// The validator told of `slot`: kept the first time only.
    fn notified(&self, slot: u64, at: std::time::Instant) {
        let Ok(mut slots) = self.slots.lock() else {
            return;
        };
        if slots.iter().rev().take(8).any(|c| c.slot == slot) {
            return;
        }
        if slots.len() == SLOT_CLOCKS_MAX {
            slots.pop_front();
        }
        slots.push_back(SlotClock {
            slot,
            notified: at,
            applied: None,
        });
    }

    /// The feed applied a banked status of `slot`: kept the first time.
    pub fn applied(&self, slot: u64, at: std::time::Instant) {
        let Ok(mut slots) = self.slots.lock() else {
            return;
        };
        if let Some(clock) = slots.iter_mut().rev().find(|c| c.slot == slot)
            && clock.applied.is_none()
        {
            clock.applied = Some(at);
        }
    }

    pub fn get(&self, slot: u64) -> Option<SlotClock> {
        let slots = self.slots.lock().ok()?;
        slots.iter().rev().find(|c| c.slot == slot).copied()
    }
}

impl FeedSender {
    /// Whether a write to `address` is worth copying.
    pub fn watches(&self, address: &[u8]) -> bool {
        self.watch.has(address)
    }

    /// Where the transactions of [`super::tape::Tape::program`] go: apart
    /// from the account feed, so a busy tape never drops an account write.
    pub fn tape(&self) -> &super::tape::Tape {
        &self.watch.tape
    }

    /// Hand `event` to the runtime, without waiting. With no room, the
    /// event is dropped and counted. A runtime that stopped takes nothing,
    /// and that isn't a loss.
    pub fn send(&self, event: FeedEvent) {
        if let FeedEvent::Slot { slot, status, .. } = &event {
            self.notified_slot.fetch_max(*slot, Ordering::Release);
            if banked(*status) {
                self.clocks.notified(*slot, std::time::Instant::now());
            }
        }
        match self.events.try_send(event) {
            Ok(()) => {
                self.sent.fetch_add(1, Ordering::Release);
            }
            Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// The runtime's side of the feed.
#[derive(Debug)]
pub struct FeedReceiver {
    events: Receiver<FeedEvent>,
    dropped: Arc<AtomicU64>,
    progress: FeedProgress,
}

/// How far the runtime has got through what the feed was handed: what a
/// step that must see every write so far waits on ([`FeedProgress::settle`]).
#[derive(Debug, Clone)]
pub struct FeedProgress {
    sent: Arc<AtomicU64>,
    applied: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    /// The newest slot the chain told of, and the newest the runtime has
    /// applied.
    notified_slot: Arc<AtomicU64>,
    applied_slot: Arc<AtomicU64>,
    clocks: Arc<SlotClocks>,
}

/// How often [`FeedProgress::settle`] looks again.
const SETTLE_LOOKS_EVERY: std::time::Duration = std::time::Duration::from_micros(200);

impl FeedProgress {
    /// The newest slot the runtime has applied.
    pub fn newest_applied_slot(&self) -> u64 {
        self.applied_slot.load(Ordering::Acquire)
    }

    /// When recent slots were told of and applied.
    pub fn clocks(&self) -> &SlotClocks {
        &self.clocks
    }

    /// How many events the senders dropped so far: above 0, the cache may
    /// be stale, and nothing read from it should be traded on.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// How many slots the runtime is behind what the chain told of: the
    /// slots still waiting in the feed. A runtime that far behind reads a
    /// market that has moved on.
    pub fn slots_behind(&self) -> u64 {
        self.notified_slot
            .load(Ordering::Acquire)
            .saturating_sub(self.applied_slot.load(Ordering::Acquire))
    }

    /// Wait until the runtime has applied every event handed to the feed
    /// before this call, for at most `within`: whether it did. Events sent
    /// after the call aren't waited for, so a busy chain can't hold it.
    pub fn settle(&self, within: std::time::Duration) -> bool {
        let target = self.sent.load(Ordering::Acquire);
        let deadline = std::time::Instant::now() + within;
        while self.applied.load(Ordering::Acquire) < target {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(SETTLE_LOOKS_EVERY);
        }
        true
    }
}

impl FeedReceiver {
    /// The next event, waiting for it: `None` once every sender is gone.
    pub fn next(&self) -> Option<FeedEvent> {
        self.events.recv().ok()
    }

    /// How many events the senders dropped so far.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Say the last event [`FeedReceiver::next`] gave is applied.
    pub fn applied(&self) {
        self.progress.applied.fetch_add(1, Ordering::Release);
    }

    /// Say a slot event for `slot` is applied: the runtime has caught up
    /// with the chain that far.
    pub fn applied_slot(&self, slot: u64) {
        self.progress
            .applied_slot
            .fetch_max(slot, Ordering::Release);
    }

    /// Say a banked status of `slot` is applied: when it was, for its
    /// steps' timings.
    pub fn slot_banked(&self, slot: u64) {
        self.progress
            .clocks
            .applied(slot, std::time::Instant::now());
    }

    /// A handle on how far the runtime has got, for another thread.
    pub fn progress(&self) -> FeedProgress {
        self.progress.clone()
    }
}

/// A feed of at most [`FEED_EVENTS_MAX`] waiting events, for the accounts
/// of `watch`.
pub fn channel(watch: Arc<Watch>) -> (FeedSender, FeedReceiver) {
    let (sender, receiver) = sync_channel(FEED_EVENTS_MAX);
    let dropped = Arc::new(AtomicU64::new(0));
    let sent = Arc::new(AtomicU64::new(0));
    let notified_slot = Arc::new(AtomicU64::new(0));
    let clocks = Arc::new(SlotClocks::default());
    (
        FeedSender {
            events: sender,
            watch,
            dropped: dropped.clone(),
            sent: sent.clone(),
            notified_slot: notified_slot.clone(),
            clocks: clocks.clone(),
        },
        FeedReceiver {
            events: receiver,
            dropped: dropped.clone(),
            progress: FeedProgress {
                sent,
                applied: Arc::new(AtomicU64::new(0)),
                dropped,
                notified_slot,
                applied_slot: Arc::new(AtomicU64::new(0)),
                clocks,
            },
        },
    )
}

/// The watched accounts, each as its latest write left it.
///
/// Each account's bytes are behind an [`Arc`]: a reader takes the accounts
/// it needs under the cache's lock (a reference count each) and decodes
/// them after letting go, so a strategy's read never holds the feed's
/// writes, nor another strategy's read, for as long as a decode takes.
#[derive(Debug, Default)]
pub struct AccountCache {
    accounts: HashMap<Address, Held>,
}

#[derive(Debug, Clone)]
struct Held {
    slot: u64,
    write_version: u64,
    data: Arc<Vec<u8>>,
}

/// Accounts a read decodes: the cache under its lock, or a [`Snapshot`]
/// taken from it.
pub trait Accounts {
    fn data(&self, address: &Address) -> Option<&[u8]>;
}

impl Accounts for AccountCache {
    fn data(&self, address: &Address) -> Option<&[u8]> {
        AccountCache::data(self, address)
    }
}

/// Some accounts of the cache as they stood when it was taken, held
/// without the cache's lock.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    accounts: HashMap<Address, Arc<Vec<u8>>>,
}

impl Accounts for Snapshot {
    fn data(&self, address: &Address) -> Option<&[u8]> {
        self.accounts.get(address).map(|data| data.as_slice())
    }
}

impl AccountCache {
    /// Keep `write` if it is the account's latest: a later slot, or the
    /// same slot and no lower a write version. Returns whether it was kept.
    pub fn apply(&mut self, write: AccountWrite) -> bool {
        self.keep(
            write.address,
            Held {
                slot: write.slot,
                write_version: write.write_version,
                // The write's bytes move in: no copy.
                data: Arc::new(write.data),
            },
        )
    }

    fn keep(&mut self, address: Address, write: Held) -> bool {
        let is_latest = self.accounts.get(&address).is_none_or(|held| {
            (write.slot, write.write_version) >= (held.slot, held.write_version)
        });
        if is_latest {
            self.accounts.insert(address, write);
        }
        is_latest
    }

    /// The account's data, if a write of it was seen.
    pub fn data(&self, address: &Address) -> Option<&[u8]> {
        self.accounts.get(address).map(|held| held.data.as_slice())
    }

    /// `addresses` as they stand, to read without the cache's lock: a
    /// reference count each. An address never written is left out.
    pub fn snapshot(&self, addresses: &[Address]) -> Snapshot {
        Snapshot {
            accounts: addresses
                .iter()
                .filter_map(|address| Some((*address, self.accounts.get(address)?.data.clone())))
                .collect(),
        }
    }

    /// Keep every write of `other` that is the latest of its account.
    pub fn merge(&mut self, other: AccountCache) {
        for (address, write) in other.accounts {
            self.keep(address, write);
        }
    }

    /// The slot of the account's latest write.
    pub fn slot(&self, address: &Address) -> Option<u64> {
        self.accounts.get(address).map(|held| held.slot)
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use mtx_core::feed::SlotStatus;

    use super::*;

    fn write(address: u8, slot: u64, write_version: u64, byte: u8) -> AccountWrite {
        AccountWrite {
            slot,
            address: [address; 32],
            owner: [0; 32],
            lamports: 1,
            data: vec![byte],
            write_version,
        }
    }

    #[test]
    fn the_cache_keeps_the_latest_write_whatever_order_they_come_in() {
        let mut cache = AccountCache::default();
        assert!(cache.apply(write(1, 10, 5, 1)));
        // An earlier slot, and an earlier write of the same slot: stale.
        assert!(!cache.apply(write(1, 9, 9, 2)));
        assert!(!cache.apply(write(1, 10, 4, 3)));
        assert_eq!(cache.data(&[1; 32]), Some(&[1][..]));
        // The same slot and version again (a host that numbers no writes),
        // and a later slot: both replace it.
        assert!(cache.apply(write(1, 10, 5, 4)));
        assert!(cache.apply(write(1, 11, 0, 5)));
        assert_eq!(cache.data(&[1; 32]), Some(&[5][..]));
        assert_eq!(cache.slot(&[1; 32]), Some(11));
        assert_eq!(cache.data(&[2; 32]), None);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn the_watch_knows_its_accounts_and_takes_more() {
        let watch = Watch::new([[1; 32]]);
        assert!(watch.has(&[1; 32]));
        assert!(!watch.has(&[2; 32]));
        assert!(!watch.has(&[1; 31]), "not an address");
        watch.add([2; 32]);
        assert!(watch.has(&[2; 32]));
    }

    #[test]
    fn settling_waits_for_what_was_sent_before_and_nothing_after() {
        let (sender, receiver) = channel(Arc::new(Watch::default()));
        let progress = receiver.progress();
        // Nothing sent: settled at once.
        assert!(progress.settle(std::time::Duration::ZERO));
        sender.send(FeedEvent::Account(write(1, 1, 0, 1)));
        sender.send(FeedEvent::Account(write(1, 2, 0, 2)));
        // Two sent, none applied: not settled.
        assert!(!progress.settle(std::time::Duration::from_millis(5)));
        let applier = std::thread::spawn(move || {
            for _ in 0..2 {
                receiver.next().unwrap();
                receiver.applied();
            }
            receiver
        });
        assert!(progress.settle(std::time::Duration::from_secs(5)));
        let _receiver = applier.join().unwrap();
        // A third, not applied: not settled.
        sender.send(FeedEvent::Account(write(1, 3, 0, 3)));
        assert!(!progress.settle(std::time::Duration::from_millis(5)));
    }

    #[test]
    fn a_runtime_behind_the_chain_knows_by_how_many_slots() {
        let (sender, receiver) = channel(Arc::new(Watch::default()));
        let progress = receiver.progress();
        let slot = |slot| FeedEvent::Slot {
            slot,
            parent: None,
            status: SlotStatus::Processed,
        };
        sender.send(slot(10));
        sender.send(slot(14));
        assert_eq!(progress.slots_behind(), 14);
        receiver.applied_slot(10);
        assert_eq!(progress.slots_behind(), 4);
        receiver.applied_slot(14);
        assert_eq!(progress.slots_behind(), 0);
    }

    #[test]
    fn a_full_feed_drops_and_counts_instead_of_waiting() {
        let (sender, receiver) = channel(Arc::new(Watch::default()));
        let slot = |slot| FeedEvent::Slot {
            slot,
            parent: None,
            status: SlotStatus::Processed,
        };
        for n in 0..FEED_EVENTS_MAX as u64 + 3 {
            sender.send(slot(n));
        }
        assert_eq!(receiver.dropped(), 3);
        // What was kept is the oldest, in order.
        assert_eq!(receiver.next(), Some(slot(0)));
        assert_eq!(receiver.next(), Some(slot(1)));
        drop(sender);
        let mut left = 2;
        while receiver.next().is_some() {
            left += 1;
        }
        assert_eq!(left, FEED_EVENTS_MAX);
    }

    #[test]
    fn a_stopped_runtime_is_not_a_loss() {
        let (sender, receiver) = channel(Arc::new(Watch::default()));
        let dropped = receiver.dropped.clone();
        drop(receiver);
        sender.send(FeedEvent::Slot {
            slot: 1,
            parent: None,
            status: SlotStatus::Rooted,
        });
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }
}
