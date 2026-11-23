//! The block builder: what lands in one slot, and in what order.
//!
//! Given the entries that may land in slot `s` (arrived, delivered, their
//! blockhash still valid), the builder:
//!
//! 1. lands nothing if `s`'s leader skipped it;
//! 2. ranks the entries by when they reached the leader, then on **one
//!    scale**. Entries that reached it in the same slot are one window
//!    (for a trigger's slot, every send of the trigger step's turns, which
//!    run with the chain paused); an earlier window goes first, whatever a
//!    later one pays. Within a window, transactions and bundles alike rank
//!    by what they pay the leader per compute unit they request (priority
//!    fees, tips on a Jito leader, and the leader's half of the base fees,
//!    over all their transactions' units: [`Entry::pay_per_cu`]); then by
//!    the step sent in, earlier first; then by a seeded hash of the sender,
//!    drawn afresh each slot, never by a transaction's bytes, which a
//!    sender could grind; then by each sender's own order. The order the
//!    grader runs a step's turns in (actors before the bot) is no race;
//! 3. takes them in that order. One lands if its bid clears what landing
//!    costs in the slot (the block's price and that of every hot account it
//!    writes) and its requested units fit what the ghosts left of the block
//!    and of each account it writes. A bundle also has to win Jito's
//!    auction: only a Jito leader takes it, with at least the least tip, a
//!    tip above the ghosts' bundles on every contended hot account it
//!    writes (Jito's tip accounts aside, which every bundle pays), no
//!    account written by a bundle taken before it, room in the bundle share
//!    of the block, and a `jitodontfront` transaction only first. A bundle
//!    lands where its rank puts it, not at the top of the block: Jito's
//!    validators schedule bundles beside transactions through the slot, and
//!    mainnet's liquidations land mid-block (median index 347) whether they
//!    tip or not (`docs/tx-layer.md`, "Ranking");
//! 4. with the calibrated small chance, swaps two adjacent conflicting
//!    transactions: the scheduler isn't a strict sort.
//!
//! So of the entries that reach a trigger's slot together, the one that
//! pays the most per unit lands right behind the trigger, and one that
//! arrives a slot later lands after them all. Mainnet's contested slots
//! read the same way (`docs/tx-layer.md`, "Ranking"): among contenders that
//! landed within five positions of each other the winner paid the most per
//! unit in 20 of 22 SOL/USDC backruns and 28 of 30 Kamino liquidations,
//! while across the whole slot it did in only 13 and 11, the richer ones
//! having arrived later.
//!
//! A signature lands once: an entry holding a transaction another one took
//! for the block waits, and the session lets it go once that one landed.
//!
//! It is a pure function of its inputs: the session commits its picks in
//! order.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

use solana_pubkey::Pubkey;

use super::{draw, ghost::GhostPath, hash, jito, pool::Entry};

/// The share of the block bundles may take.
pub const BUNDLE_SHARE: f64 = 0.25;

/// What the builder needs of the slot.
pub struct Slot<'a> {
    pub slot: u64,
    pub seed: u64,
    pub skipped: bool,
    pub jito_leader: bool,
    pub ghost: &'a GhostPath,
    pub cu_cap: u64,
    pub account_cu_cap: u64,
    pub inversion_p: f64,
    /// The ghosts' tip on a contended hot account: what a bundle writing
    /// it must outbid.
    pub ghost_tip: &'a dyn Fn(&Pubkey) -> u64,
}

/// The builder's decision for one slot.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Block {
    /// Entry ids to commit, in order.
    pub picks: Vec<u64>,
    /// Bundles refused outright, with why.
    pub invalid: Vec<(u64, String)>,
}

/// Decide slot `s`'s block from the `eligible` entries.
pub fn build(slot: &Slot<'_>, eligible: &[&Entry]) -> Block {
    let mut block = Block::default();
    if slot.skipped {
        return block;
    }
    let mut ranked: Vec<&Entry> = Vec::with_capacity(eligible.len());
    for entry in eligible.iter().copied() {
        if entry.is_bundle() {
            // A leader without Jito's client takes no bundle: it waits.
            if !slot.jito_leader {
                continue;
            }
            if let Some(i) = entry.txs.iter().position(|t| t.dont_front)
                && i != 0
            {
                block.invalid.push((
                    entry.id,
                    "a jitodontfront transaction that isn't the bundle's first".into(),
                ));
                continue;
            }
        }
        ranked.push(entry);
    }
    ranked.sort_by(|a, b| rank(slot, a, b));

    let mut space = Space::new(slot);
    let mut taken: BTreeSet<&str> = BTreeSet::new();
    let mut landed: Vec<&Entry> = Vec::with_capacity(ranked.len());
    for entry in ranked {
        if entry
            .txs
            .iter()
            .any(|t| taken.contains(t.signature.as_str()))
        {
            continue;
        }
        // The tip accounts take every bundle's tip: they set no order.
        let writes: Vec<Pubkey> = entry
            .writable()
            .filter(|w| !jito::is_tip_account(w))
            .copied()
            .collect();
        if entry.bid_on(slot.jito_leader) < slot.ghost.price_to_land(slot.slot, &writes) {
            continue;
        }
        if entry.is_bundle() && !space.wins_auction(entry, &writes) {
            continue;
        }
        if !space.fits(entry.cu(), &writes) {
            continue;
        }
        space.take(entry, &writes);
        taken.extend(entry.txs.iter().map(|t| t.signature.as_str()));
        landed.push(entry);
    }
    invert(slot, &mut landed);
    block.picks.extend(landed.iter().map(|e| e.id));
    assert!(
        block.picks.iter().collect::<BTreeSet<_>>().len() == block.picks.len(),
        "an entry is picked once"
    );
    block
}

/// The builder's order: the slot an entry reached the leader in, earliest
/// first (an earlier arrival isn't reordered behind a later one, whatever
/// the later one pays); within it, pay per requested unit, highest first;
/// then the step sent in, earliest first; then a seeded coin between
/// senders, drawn afresh each slot; then each sender's own sends in the
/// order it made them.
fn rank(slot: &Slot<'_>, a: &Entry, b: &Entry) -> Ordering {
    let tie = |e: &Entry| hash(slot.seed, "tie", &[slot.slot], e.sender.as_bytes());
    a.ready_slot
        .cmp(&b.ready_slot)
        .then(
            b.pay_per_cu(slot.jito_leader)
                .cmp(&a.pay_per_cu(slot.jito_leader)),
        )
        .then(a.arrival.step.cmp(&b.arrival.step))
        .then(tie(a).cmp(&tie(b)))
        .then(a.sender_seq.cmp(&b.sender_seq))
        .then(a.id.cmp(&b.id))
}

/// What is left of the slot's block space as entries are taken.
struct Space<'s, 'a> {
    slot: &'s Slot<'a>,
    block_left: u64,
    bundle_left: u64,
    account_used: BTreeMap<Pubkey, u64>,
    /// Accounts written by the bundles taken: Jito's auction lands one
    /// bundle per account.
    bundle_locked: BTreeSet<Pubkey>,
}

impl<'s, 'a> Space<'s, 'a> {
    fn new(slot: &'s Slot<'a>) -> Self {
        let clearing = slot.ghost.block(slot.slot);
        let block_left = ((1.0 - clearing.utilization) * slot.cu_cap as f64) as u64;
        Self {
            slot,
            block_left,
            bundle_left: (BUNDLE_SHARE * block_left as f64) as u64,
            account_used: BTreeMap::new(),
            bundle_locked: BTreeSet::new(),
        }
    }

    fn account_left(&self, account: &Pubkey) -> u64 {
        let clearing = self.slot.ghost.account(account, self.slot.slot);
        let cap = ((1.0 - clearing.utilization) * self.slot.account_cu_cap as f64) as u64;
        cap.saturating_sub(self.account_used.get(account).copied().unwrap_or(0))
    }

    /// Whether `cu` requested units fit the block and every account in
    /// `writes`.
    fn fits(&self, cu: u64, writes: &[Pubkey]) -> bool {
        cu <= self.block_left && writes.iter().all(|w| self.account_left(w) >= cu)
    }

    /// Whether a bundle wins Jito's auction for what it writes: the least
    /// tip, the ghosts' bundles outbid on every contended hot account, no
    /// account a bundle taken before it writes, and room in the bundle
    /// share.
    fn wins_auction(&self, bundle: &Entry, writes: &[Pubkey]) -> bool {
        assert!(
            bundle.is_bundle() && self.slot.jito_leader,
            "a Jito leader's bundle"
        );
        let tip = bundle.tip();
        tip >= jito::MIN_TIP
            && writes
                .iter()
                .filter(|a| self.slot.ghost.is_hot(a))
                .all(|a| tip >= (self.slot.ghost_tip)(a))
            && writes.iter().all(|w| !self.bundle_locked.contains(w))
            && bundle.cu() <= self.bundle_left
    }

    fn take(&mut self, entry: &Entry, writes: &[Pubkey]) {
        let cu = entry.cu();
        assert!(self.fits(cu, writes), "an entry taken fits");
        self.block_left -= cu;
        for w in writes {
            *self.account_used.entry(*w).or_default() += cu;
        }
        if entry.is_bundle() {
            self.bundle_left -= cu;
            self.bundle_locked.extend(writes.iter().copied());
        }
    }
}

/// The scheduler's inversions: with the calibrated chance, two adjacent
/// transactions that write an account in common land the other way round.
/// A bundle never swaps: Jito's block engine fixes where it goes.
fn invert(slot: &Slot<'_>, landed: &mut [&Entry]) {
    if slot.inversion_p <= 0.0 {
        return;
    }
    for i in 0..landed.len().saturating_sub(1) {
        let (a, b) = (landed[i], landed[i + 1]);
        if a.is_bundle() || b.is_bundle() {
            continue;
        }
        let conflict = a.txs[0]
            .writable
            .iter()
            .any(|w| b.txs[0].writable.contains(w));
        if conflict && draw(slot.seed, "inversion", &[slot.slot, i as u64]) < slot.inversion_p {
            landed.swap(i, i + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use solana_keypair::Keypair;
    use solana_transaction::Hash;

    use super::*;
    use crate::txlayer::{
        RouteKind,
        calibration::Calibration,
        pool::{Arrival, EntryState, PooledTx, tests::signed},
        tests::layer,
    };

    fn ghost(spike: bool) -> GhostPath {
        let mut layer = layer();
        if !spike {
            layer.regimes.clear();
        }
        let calibration = Calibration::builtin(&layer.calibration).unwrap();
        GhostPath::new(&layer, &calibration, 1_000, 1_080, 2, None)
    }

    fn entry(id: u64, sender: &str, seq: u64, wire: Vec<u8>, step: u64) -> Entry {
        let tx = PooledTx::parse(wire, &|_| None).unwrap();
        Entry {
            id,
            sender: sender.into(),
            sender_seq: seq,
            arrival: Arrival { step, seq: id },
            route: RouteKind::Rpc,
            bundle: None,
            txs: vec![tx],
            ready_slot: 0,
            expiry_height: u64::MAX,
            retry_until_slot: None,
            resign: false,
            attempts: 1,
            delivered: true,
            dropped: false,
            state: EntryState::Pending,
            resigned_from: Vec::new(),
        }
    }

    fn slot<'a>(ghost: &'a GhostPath, s: u64, tip: &'a dyn Fn(&Pubkey) -> u64) -> Slot<'a> {
        Slot {
            slot: s,
            seed: 11,
            skipped: false,
            jito_leader: true,
            ghost,
            cu_cap: 100_000_000,
            account_cu_cap: 12_000_000,
            inversion_p: 0.0,
            ghost_tip: tip,
        }
    }

    #[test]
    fn higher_fees_land_first_and_cheap_ones_wait_out_a_spike() {
        let g = ghost(true);
        let pool: Pubkey = layer().hot_accounts[0].address.parse().unwrap();
        let tip = |_: &Pubkey| 0;
        // A spike slot where the pool is contended.
        let s = (1_021..1_041)
            .find(|s| g.account(&pool, *s).price > 0)
            .expect("a contended slot in the spike");
        let clearing = g.account(&pool, s).price;
        let payer = Keypair::new();
        let h = Hash::new_unique();
        let cheap = entry(
            0,
            "bot",
            0,
            signed(&payer, &[pool], Some(200_000), Some(0), None, b"a", h),
            3,
        );
        let rich = entry(
            1,
            "bot",
            1,
            signed(
                &payer,
                &[pool],
                Some(200_000),
                Some(clearing),
                None,
                b"b",
                h,
            ),
            3,
        );
        let richer = entry(
            2,
            "bot",
            2,
            signed(
                &payer,
                &[pool],
                Some(200_000),
                Some(clearing * 2),
                None,
                b"c",
                h,
            ),
            3,
        );
        let block = build(&slot(&g, s, &tip), &[&cheap, &rich, &richer]);
        assert_eq!(block.picks, [2, 1]);
        // A quiet slot takes the cheap one too.
        let quiet = (950..1_000)
            .find(|s| g.price_to_land(*s, &[pool]) == 0)
            .unwrap();
        let block = build(&slot(&g, quiet, &tip), &[&cheap, &rich, &richer]);
        assert_eq!(block.picks, [2, 1, 0]);
        // A skipped slot lands nothing.
        let mut skipped = slot(&g, quiet, &tip);
        skipped.skipped = true;
        assert!(build(&skipped, &[&cheap]).picks.is_empty());
    }

    #[test]
    fn requested_units_count_against_the_account_cap() {
        let g = ghost(false);
        let pool: Pubkey = layer().hot_accounts[0].address.parse().unwrap();
        let tip = |_: &Pubkey| 0;
        let quiet = (950..1_000)
            .find(|s| g.price_to_land(*s, &[pool]) == 0)
            .unwrap();
        let payer = Keypair::new();
        let entries: Vec<Entry> = (0..20)
            .map(|i| {
                entry(
                    i,
                    "bot",
                    i,
                    signed(
                        &payer,
                        &[pool],
                        Some(1_400_000),
                        Some(1),
                        None,
                        &[i as u8],
                        Hash::new_unique(),
                    ),
                    0,
                )
            })
            .collect();
        let refs: Vec<&Entry> = entries.iter().collect();
        let block = build(&slot(&g, quiet, &tip), &refs);
        let left = (1.0 - g.account(&pool, quiet).utilization) * 12_000_000.0;
        assert_eq!(block.picks.len(), (left / 1_400_000.0) as usize);
    }

    #[test]
    fn ties_never_turn_on_a_transactions_bytes() {
        // Two senders, the same price and arrival: whoever wins the tie
        // wins it whatever bytes the bot grinds into its transaction.
        let g = ghost(false);
        let tip = |_: &Pubkey| 0;
        let quiet = (950..1_000).find(|s| g.block(*s).price == 0).unwrap();
        let bot = Keypair::new();
        let actor = Keypair::new();
        let h = Hash::new_unique();
        let them = Entry {
            arrival: Arrival { step: 4, seq: 0 },
            ..entry(
                1,
                "actor:bidder/0",
                0,
                signed(&actor, &[], Some(200_000), Some(5_000), None, b"x", h),
                4,
            )
        };
        let mut winners = std::collections::BTreeSet::new();
        for grind in 0..200u32 {
            let ours = Entry {
                arrival: Arrival { step: 4, seq: 0 },
                ..entry(
                    0,
                    "bot",
                    0,
                    signed(
                        &bot,
                        &[],
                        Some(200_000),
                        Some(5_000),
                        None,
                        &grind.to_le_bytes(),
                        h,
                    ),
                    4,
                )
            };
            winners.insert(build(&slot(&g, quiet, &tip), &[&ours, &them]).picks[0]);
        }
        assert_eq!(winners.len(), 1, "the tie went both ways: {winners:?}");
        // The seeded hash does decide it: another slot can go the other way.
        let firsts: std::collections::BTreeSet<u64> = (950..1_000)
            .filter(|s| g.block(*s).price == 0)
            .map(|s| {
                let ours = Entry {
                    arrival: Arrival { step: 4, seq: 0 },
                    ..entry(
                        0,
                        "bot",
                        0,
                        signed(&bot, &[], Some(200_000), Some(5_000), None, b"y", h),
                        4,
                    )
                };
                build(&slot(&g, s, &tip), &[&ours, &them]).picks[0]
            })
            .collect();
        assert_eq!(firsts.len(), 2);
    }

    #[test]
    fn a_bundle_pays_what_a_transaction_pays_to_land() {
        // A congestion floor of 200,000 micro-lamports per unit over the
        // first 30 steps.
        let layer = layer();
        let calibration = Calibration::builtin(&layer.calibration).unwrap();
        let g = GhostPath::new(
            &layer,
            &calibration,
            1_000,
            1_080,
            2,
            Some((0, 30, 200_000)),
        );
        let tip = |_: &Pubkey| 0;
        let s = 1_010;
        let h = Hash::new_unique();
        let to = Pubkey::new_unique();
        let clearing = g.price_to_land(s, &[to]);
        assert!(clearing >= 200_000);
        // 200,000 units at the clearing price, in lamports.
        let at_clearing = (clearing * 200_000).div_ceil(1_000_000);
        let bundle = |id: u64, price: Option<u64>, tip: u64| Entry {
            bundle: Some(format!("b{id}")),
            route: RouteKind::Jito,
            ..entry(
                id,
                "bot",
                id,
                signed(
                    &Keypair::new(),
                    &[to],
                    Some(200_000),
                    price,
                    Some(tip),
                    &[id as u8],
                    h,
                ),
                0,
            )
        };
        let lands = |e: &Entry| build(&slot(&g, s, &tip), &[e]).picks == [e.id];
        // The least tip, and no priority fee: it waits out the floor.
        assert!(!lands(&bundle(0, None, jito::MIN_TIP)));
        // A tip worth the clearing price lands it, as does the least tip
        // with a priority fee at the clearing price.
        assert!(lands(&bundle(1, None, at_clearing)));
        assert!(lands(&bundle(2, Some(clearing), jito::MIN_TIP)));
        assert!(!lands(&bundle(3, Some(clearing / 2), jito::MIN_TIP)));
        // A transaction at half the price doesn't land either.
        let plain = entry(
            4,
            "bot",
            4,
            signed(
                &Keypair::new(),
                &[to],
                Some(200_000),
                Some(clearing / 2),
                None,
                b"p",
                h,
            ),
            0,
        );
        assert!(!lands(&plain));
    }

    #[test]
    fn a_transaction_in_a_bundle_and_alone_is_taken_once() {
        let g = ghost(false);
        let tip = |_: &Pubkey| 0;
        let quiet = (950..1_000).find(|s| g.block(*s).price == 0).unwrap();
        let h = Hash::new_unique();
        let x = signed(
            &Keypair::new(),
            &[Pubkey::new_unique()],
            Some(10_000),
            Some(10),
            None,
            b"x",
            h,
        );
        let t = signed(
            &Keypair::new(),
            &[Pubkey::new_unique()],
            Some(10_000),
            None,
            Some(5_000),
            b"t",
            h,
        );
        let alone = entry(0, "bot", 0, x.clone(), 0);
        let mut bundled = Entry {
            bundle: Some("b".into()),
            route: RouteKind::Jito,
            ..entry(1, "bot", 1, t, 0)
        };
        bundled.txs.push(PooledTx::parse(x, &|_| None).unwrap());
        assert_eq!(
            build(&slot(&g, quiet, &tip), &[&alone, &bundled]).picks,
            [1]
        );
    }

    #[test]
    fn a_transaction_in_two_plain_entries_is_taken_once() {
        let g = ghost(false);
        let tip = |_: &Pubkey| 0;
        let quiet = (950..1_000).find(|s| g.block(*s).price == 0).unwrap();
        let x = signed(
            &Keypair::new(),
            &[Pubkey::new_unique()],
            Some(10_000),
            Some(10),
            None,
            b"x",
            Hash::new_unique(),
        );
        let first = entry(0, "bot", 0, x.clone(), 0);
        let again = entry(1, "bot", 1, x, 0);
        assert_eq!(build(&slot(&g, quiet, &tip), &[&first, &again]).picks, [0]);
    }

    /// A bundle on `account` from its own payer, its tip inside its one
    /// transaction.
    fn bundle_on(id: u64, account: Pubkey, cu: u32, price: Option<u64>, tip: u64) -> Entry {
        Entry {
            bundle: Some(format!("b{id}")),
            route: RouteKind::Jito,
            ..entry(
                id,
                "bot",
                id,
                signed(
                    &Keypair::new(),
                    &[account],
                    Some(cu),
                    price,
                    Some(tip),
                    &id.to_le_bytes(),
                    Hash::new_unique(),
                ),
                0,
            )
        }
    }

    /// A transaction on `account` from its own payer.
    fn plain_on(id: u64, sender: &str, account: Pubkey, cu: u32, price: u64) -> Entry {
        entry(
            id,
            sender,
            id,
            signed(
                &Keypair::new(),
                &[account],
                Some(cu),
                Some(price),
                None,
                &id.to_le_bytes(),
                Hash::new_unique(),
            ),
            0,
        )
    }

    #[test]
    fn bundles_and_transactions_rank_on_one_scale() {
        let g = ghost(false);
        let tip = |_: &Pubkey| 0;
        let quiet = (950..1_000).find(|s| g.block(*s).price == 0).unwrap();
        let pool = Pubkey::new_unique();
        // 100,000 units each. The bundle's 50,000-lamport tip is 500,000
        // micro-lamports a unit; the transaction's priority fee is
        // 1,000,000: the transaction pays more, and lands first, ahead of
        // the bundle on the same pool.
        let bundle = bundle_on(0, pool, 100_000, None, 50_000);
        let rich = plain_on(1, "actor:searcher/0", pool, 100_000, 1_000_000);
        let block = build(&slot(&g, quiet, &tip), &[&bundle, &rich]);
        assert_eq!(block.picks, [1, 0]);
        // A transaction that pays less than the bundle lands behind it.
        let poor = plain_on(2, "actor:searcher/0", pool, 100_000, 100_000);
        let block = build(&slot(&g, quiet, &tip), &[&poor, &bundle]);
        assert_eq!(block.picks, [0, 2]);
        // Per unit: the same tip over twice the units bids half as much.
        let heavy = bundle_on(3, pool, 200_000, None, 150_000);
        let light = plain_on(4, "actor:searcher/0", pool, 100_000, 800_000);
        let block = build(&slot(&g, quiet, &tip), &[&heavy, &light]);
        assert_eq!(block.picks, [4, 3], "750,000 a unit against 800,000");
        // On a leader without Jito, the bundle waits and its tip buys
        // nothing.
        let mut plain = slot(&g, quiet, &tip);
        plain.jito_leader = false;
        assert_eq!(build(&plain, &[&bundle, &poor]).picks, [2]);
    }

    #[test]
    fn the_first_to_reach_a_trigger_at_the_best_pay_lands_right_behind_it() {
        // Sends in one step's turns arrive together: the order the grader
        // runs the turns in (actors before the bot) wins no tie. An equal
        // bid is a seeded coin, whatever the kind of send.
        let g = ghost(false);
        let tip = |_: &Pubkey| 0;
        let quiet: Vec<u64> = (950..1_000).filter(|s| g.block(*s).price == 0).collect();
        let pool = Pubkey::new_unique();
        let firsts: BTreeSet<String> = quiet
            .iter()
            .map(|s| {
                let actor = Entry {
                    arrival: Arrival { step: 7, seq: 0 },
                    ..plain_on(0, "actor:cycle-searcher/0", pool, 100_000, 500_000)
                };
                let bot = Entry {
                    arrival: Arrival { step: 7, seq: 1 },
                    ..plain_on(1, "bot", pool, 100_000, 500_000)
                };
                let picks = build(&slot(&g, *s, &tip), &[&actor, &bot]).picks;
                assert_eq!(picks.len(), 2);
                if picks[0] == 0 { "actor" } else { "bot" }.to_string()
            })
            .collect();
        assert_eq!(firsts.len(), 2, "the tie goes both ways across slots");
        // A send that arrived a step earlier, at the same pay, goes first.
        let s = quiet[0];
        let early = Entry {
            arrival: Arrival { step: 6, seq: 3 },
            ..plain_on(0, "actor:cycle-searcher/0", pool, 100_000, 500_000)
        };
        let late = Entry {
            arrival: Arrival { step: 7, seq: 0 },
            ..plain_on(1, "bot", pool, 100_000, 500_000)
        };
        assert_eq!(build(&slot(&g, s, &tip), &[&late, &early]).picks, [0, 1]);
        // One micro-lamport more a unit and the later one goes first.
        let later_richer = Entry {
            arrival: Arrival { step: 7, seq: 0 },
            ..plain_on(2, "bot", pool, 100_000, 500_001)
        };
        assert_eq!(
            build(&slot(&g, s, &tip), &[&early, &later_richer]).picks,
            [2, 0]
        );
    }

    #[test]
    fn an_earlier_arrival_isnt_reordered_behind_a_richer_later_one() {
        let g = ghost(false);
        let tip = |_: &Pubkey| 0;
        let quiet = (950..1_000).find(|s| g.block(*s).price == 0).unwrap();
        let pool = Pubkey::new_unique();
        // It reached the leader a slot earlier and waited (a skipped slot,
        // say): it goes first, though the later one pays ten times more.
        let early = Entry {
            ready_slot: quiet - 1,
            ..plain_on(0, "actor:cycle-searcher/0", pool, 100_000, 50_000)
        };
        let late = Entry {
            ready_slot: quiet,
            ..bundle_on(1, pool, 100_000, None, 50_000)
        };
        assert_eq!(
            build(&slot(&g, quiet, &tip), &[&late, &early]).picks,
            [0, 1]
        );
    }

    #[test]
    fn jitos_auction_still_holds_for_bundles() {
        let g = ghost(false);
        let tip = |_: &Pubkey| 0;
        let quiet = (950..1_000).find(|s| g.block(*s).price == 0).unwrap();
        let payer = Keypair::new();
        let h = Hash::new_unique();
        // Two bundles that write the same account: Jito's auction lands the
        // better one.
        let shared = Pubkey::new_unique();
        let high = bundle_on(0, shared, 100_000, None, 50_000);
        let low = bundle_on(1, shared, 100_000, None, 2_000);
        assert_eq!(build(&slot(&g, quiet, &tip), &[&low, &high]).picks, [0]);
        // Below the least tip: refused by the auction, whatever its fee.
        let too_low = bundle_on(2, Pubkey::new_unique(), 100_000, Some(1_000_000), 500);
        assert!(build(&slot(&g, quiet, &tip), &[&too_low]).picks.is_empty());
        // A second bundle from the same payer conflicts with the first.
        let from = |id: u64, tip: u64| Entry {
            bundle: Some(format!("b{id}")),
            route: RouteKind::Jito,
            ..entry(
                id,
                "bot",
                id,
                signed(&payer, &[], Some(100_000), None, Some(tip), &[id as u8], h),
                0,
            )
        };
        let (first, second) = (from(5, 3_000), from(6, 2_500));
        assert_eq!(build(&slot(&g, quiet, &tip), &[&first, &second]).picks, [5]);
        // A contended hot account's ghosts tip more than the bundle: it
        // loses Jito's auction there, though a transaction paying less per
        // unit lands.
        let pool: Pubkey = layer().hot_accounts[0].address.parse().unwrap();
        let ghosts = |a: &Pubkey| if *a == pool { 1_000_000 } else { 0 };
        let open = (950..1_000)
            .find(|s| g.block(*s).price == 0 && g.price_to_land(*s, &[pool]) == 0)
            .unwrap();
        let tipped = bundle_on(7, pool, 100_000, None, 500_000);
        let plain = plain_on(8, "bot", pool, 100_000, 10);
        assert_eq!(
            build(&slot(&g, open, &ghosts), &[&tipped, &plain]).picks,
            [8]
        );
        // jitodontfront anywhere but first: invalid.
        let mut fronted = bundle_on(4, Pubkey::new_unique(), 100_000, None, 9_000);
        let mut second = fronted.txs[0].clone();
        second.dont_front = true;
        fronted.txs.push(second);
        let block = build(&slot(&g, quiet, &tip), &[&fronted]);
        assert!(block.picks.is_empty());
        assert_eq!(block.invalid.len(), 1);
    }
}
