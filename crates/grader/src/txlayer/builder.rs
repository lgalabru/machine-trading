//! The block builder: what lands in one slot, and in what order.
//!
//! Given the entries that may land in slot `s` (arrived, delivered, their
//! blockhash still valid), the builder:
//!
//! 1. lands nothing if `s`'s leader skipped it;
//! 2. on a Jito leader, runs the auction: bundles ranked by what they bid
//!    per requested compute unit (tip and priority fees), then by tip, each
//!    landing whole if it doesn't conflict with one taken before and fits
//!    what's left of the bundle share of the block. A bundle buys the same
//!    block space a transaction does, so its bid has to clear the same
//!    price: the block's and every hot account's it writes. Its tip also
//!    has to outbid the ghosts' bundles on those hot accounts (Jito's tip
//!    accounts aside, which every bundle pays). A bundle with a
//!    `jitodontfront` transaction anywhere but first is invalid. Bundles go
//!    at the top of the block;
//! 3. sorts the transactions by what they pay per requested compute unit
//!    (their priority fee plus their base fee), then by arrival (step, then
//!    sequence in the step), then by a seeded hash of the sender and its
//!    sequence number: never by a transaction's bytes, which a sender could
//!    grind. One lands if it pays at least what clears the block and every
//!    hot account it writes, and its requested units fit what's left of the
//!    block and of each account it writes, after the ghosts' share;
//! 4. with the calibrated small chance, swaps two adjacent conflicting
//!    transactions: the scheduler isn't a strict sort.
//!
//! A signature lands once: an entry holding a transaction another one took
//! for the block waits, and the session lets it go once that one landed.
//!
//! It is a pure function of its inputs: the session commits its picks in
//! order.

use std::collections::{BTreeMap, BTreeSet};

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
    /// Entry ids to commit, in order: bundles first.
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
    let block_clearing = slot.ghost.block(slot.slot);
    let mut block_left = ((1.0 - block_clearing.utilization) * slot.cu_cap as f64) as u64;
    let mut account_used: BTreeMap<Pubkey, u64> = BTreeMap::new();
    let account_left = |used: &BTreeMap<Pubkey, u64>, account: &Pubkey| {
        let clearing = slot.ghost.account(account, slot.slot);
        let cap = ((1.0 - clearing.utilization) * slot.account_cu_cap as f64) as u64;
        cap.saturating_sub(used.get(account).copied().unwrap_or(0))
    };
    let tie = |e: &Entry| {
        hash(
            slot.seed,
            "tie",
            &[slot.slot, e.sender_seq],
            e.sender.as_bytes(),
        )
    };

    let (bundles, txs): (Vec<&Entry>, Vec<&Entry>) =
        eligible.iter().copied().partition(|e| e.is_bundle());
    let mut taken: BTreeSet<&str> = BTreeSet::new();

    // The auction.
    if slot.jito_leader {
        let mut ranked: Vec<&Entry> = Vec::new();
        for bundle in bundles {
            if let Some(i) = bundle.txs.iter().position(|t| t.dont_front)
                && i != 0
            {
                block.invalid.push((
                    bundle.id,
                    "a jitodontfront transaction that isn't the bundle's first".into(),
                ));
                continue;
            }
            ranked.push(bundle);
        }
        ranked.sort_by(|a, b| {
            b.bid()
                .cmp(&a.bid())
                .then(b.tip().cmp(&a.tip()))
                .then(a.arrival.cmp(&b.arrival))
                .then(tie(a).cmp(&tie(b)))
        });
        let mut bundle_left = (BUNDLE_SHARE * block_left as f64) as u64;
        let mut locked: Vec<Pubkey> = Vec::new();
        for bundle in ranked {
            let tip = bundle.tip();
            // The tip accounts take every bundle's tip: they set no order.
            let writes: Vec<Pubkey> = bundle
                .writable()
                .filter(|w| !jito::is_tip_account(w))
                .copied()
                .collect();
            let outbid = tip >= jito::MIN_TIP
                && bundle.bid() >= slot.ghost.price_to_land(slot.slot, &writes)
                && writes
                    .iter()
                    .filter(|a| slot.ghost.is_hot(a))
                    .all(|a| tip >= (slot.ghost_tip)(a));
            let cu = bundle.cu();
            if !outbid
                || cu > bundle_left
                || writes.iter().any(|w| locked.contains(w))
                || writes.iter().any(|w| account_left(&account_used, w) < cu)
                || bundle
                    .txs
                    .iter()
                    .any(|t| taken.contains(t.signature.as_str()))
            {
                continue;
            }
            taken.extend(bundle.txs.iter().map(|t| t.signature.as_str()));
            bundle_left -= cu;
            block_left -= cu;
            for w in &writes {
                *account_used.entry(*w).or_default() += cu;
                locked.push(*w);
            }
            block.picks.push(bundle.id);
        }
    }

    // Normal transactions.
    let mut sorted = txs;
    sorted.sort_by(|a, b| {
        let price = |e: &Entry| e.txs[0].effective_price();
        price(b)
            .cmp(&price(a))
            .then(a.arrival.cmp(&b.arrival))
            .then(tie(a).cmp(&tie(b)))
    });
    let mut landed: Vec<&Entry> = Vec::new();
    for entry in sorted {
        let tx = &entry.txs[0];
        if entry.bid() < slot.ghost.price_to_land(slot.slot, &tx.writable)
            || taken.contains(tx.signature.as_str())
        {
            continue;
        }
        if tx.cu_limit > block_left
            || tx
                .writable
                .iter()
                .any(|w| account_left(&account_used, w) < tx.cu_limit)
        {
            continue;
        }
        block_left -= tx.cu_limit;
        for w in &tx.writable {
            *account_used.entry(*w).or_default() += tx.cu_limit;
        }
        landed.push(entry);
    }
    // The scheduler's inversions.
    if slot.inversion_p > 0.0 {
        for i in 0..landed.len().saturating_sub(1) {
            let (a, b) = (&landed[i], &landed[i + 1]);
            let conflict = a.txs[0]
                .writable
                .iter()
                .any(|w| b.txs[0].writable.contains(w));
            if conflict && draw(slot.seed, "inversion", &[slot.slot, i as u64]) < slot.inversion_p {
                landed.swap(i, i + 1);
            }
        }
    }
    block.picks.extend(landed.iter().map(|e| e.id));
    block
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
    fn bundles_go_first_by_tip_and_dont_front_holds() {
        let g = ghost(false);
        let tip = |_: &Pubkey| 0;
        let quiet = (950..1_000).find(|s| g.block(*s).price == 0).unwrap();
        let payer = Keypair::new();
        let h = Hash::new_unique();
        // Each from its own payer: two bundles that write the same account
        // (a payer included) don't land in one auction.
        let bundle = |id: u64, tip: u64, salt: &[u8]| Entry {
            bundle: Some(format!("b{id}")),
            route: RouteKind::Jito,
            ..entry(
                id,
                "bot",
                id,
                signed(
                    &Keypair::new(),
                    &[Pubkey::new_unique()],
                    Some(100_000),
                    None,
                    Some(tip),
                    salt,
                    h,
                ),
                0,
            )
        };
        let low = bundle(0, 2_000, b"l");
        let high = bundle(1, 50_000, b"h");
        let too_low = bundle(2, 500, b"t");
        let tx = entry(
            3,
            "bot",
            3,
            signed(&payer, &[], Some(100_000), Some(1_000_000), None, b"n", h),
            0,
        );
        let block = build(&slot(&g, quiet, &tip), &[&tx, &low, &high, &too_low]);
        assert_eq!(block.picks, [1, 0, 3]);
        // A second bundle from the same payer conflicts with the first.
        let shared = Entry {
            bundle: Some("b5".into()),
            route: RouteKind::Jito,
            ..entry(
                5,
                "bot",
                5,
                signed(&payer, &[], Some(100_000), None, Some(3_000), b"s", h),
                0,
            )
        };
        let shared_too = Entry {
            bundle: Some("b6".into()),
            route: RouteKind::Jito,
            ..entry(
                6,
                "bot",
                6,
                signed(&payer, &[], Some(100_000), None, Some(2_500), b"t", h),
                0,
            )
        };
        assert_eq!(
            build(&slot(&g, quiet, &tip), &[&shared, &shared_too]).picks,
            [5]
        );
        // A non-Jito leader lands no bundle.
        let mut plain = slot(&g, quiet, &tip);
        plain.jito_leader = false;
        assert_eq!(build(&plain, &[&tx, &low, &high]).picks, [3]);
        // jitodontfront anywhere but first: invalid.
        let mut fronted = bundle(4, 9_000, b"f");
        let mut second = fronted.txs[0].clone();
        second.dont_front = true;
        fronted.txs.push(second);
        let block = build(&slot(&g, quiet, &tip), &[&fronted]);
        assert!(block.picks.is_empty());
        assert_eq!(block.invalid.len(), 1);
    }
}
