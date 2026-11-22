//! The market's trades, as the validator runs them: every landed
//! transaction that ran Phoenix, kept as the log instructions Phoenix
//! invoked on itself, which record its fills. A shadow strategy's paper
//! book reads them to fill its quotes the way the book would have: a real
//! taker's sale at or below a paper bid fills it.
//!
//! The plugin hands transactions over from the validator's replay threads,
//! several at once, so the tape is a short ring behind a mutex: a reader
//! keeps a cursor, and one that falls more than [`TAPE_TRANSACTIONS_MAX`]
//! behind is told how many it missed. It is a separate path from the
//! account feed on purpose: a dropped trade costs a paper fill, while a
//! dropped account write pauses every strategy.
//!
//! Transactions come at the processed commitment: one in a slot that is
//! later skipped is on the tape anyway. A paper fill on it is rare and
//! harmless; the tape doesn't wait for confirmation.

use std::{collections::VecDeque, sync::Mutex};

use mtx_kit::{
    chain::{InnerInstruction, TransactionSummary},
    protocols::{
        fills::MarketFill,
        phoenix::eternal::{PerpMarketRecords, PhoenixMarket, phoenix_rise},
    },
};
use solana_pubkey::Pubkey;
use solana_transaction::Signature;

/// Transactions kept: Phoenix runs a few hundred a second at most, so this
/// is several seconds, and a strategy reads the tape every step (0.8 s).
pub const TAPE_TRANSACTIONS_MAX: usize = 8192;

/// The Phoenix log instructions one transaction may carry: a large
/// aggregator route stays under this; past it the transaction is left out.
pub const TAPE_INSTRUCTIONS_MAX: usize = 256;

/// A landed transaction that ran Phoenix: the data of each instruction
/// Phoenix invoked, with the index of the transaction's instruction that
/// led to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapeTransaction {
    pub slot: u64,
    pub signature: [u8; 64],
    pub calls: Vec<(u8, Vec<u8>)>,
}

/// The ring of recent transactions.
#[derive(Debug, Default)]
pub struct Tape {
    ring: Mutex<Ring>,
}

#[derive(Debug, Default)]
struct Ring {
    /// The sequence number the next transaction gets.
    next: u64,
    transactions: VecDeque<TapeTransaction>,
}

/// What a reader got from the tape since its cursor.
#[derive(Debug, Default)]
pub struct Read {
    pub transactions: Vec<TapeTransaction>,
    /// Transactions it fell too far behind to see.
    pub missed: u64,
}

impl Tape {
    /// The program whose transactions go on the tape.
    pub fn program() -> Pubkey {
        *phoenix_rise::ix::PHOENIX_PROGRAM_ID
    }

    /// Add a transaction, dropping the oldest when full.
    pub fn push(&self, transaction: TapeTransaction) {
        assert!(
            transaction.calls.len() <= TAPE_INSTRUCTIONS_MAX,
            "the plugin bounds a transaction's calls"
        );
        // Poisoned: a reader panicked holding it. The tape feeds paper
        // fills only, so a transaction is dropped rather than trust it,
        // and never a panic on the validator's thread.
        let Ok(mut ring) = self.ring.lock() else {
            return;
        };
        if ring.transactions.len() == TAPE_TRANSACTIONS_MAX {
            ring.transactions.pop_front();
        }
        ring.transactions.push_back(transaction);
        ring.next += 1;
        assert!(
            ring.transactions.len() <= TAPE_TRANSACTIONS_MAX,
            "the tape is a ring"
        );
    }

    /// The transactions after `cursor`, and the cursor to read from next.
    /// A cursor of `None` starts at the tape's end: nothing before now.
    pub fn since(&self, cursor: Option<u64>) -> (Read, u64) {
        // Poisoned: as in `push`, nothing is read from it.
        let Ok(ring) = self.ring.lock() else {
            return (Read::default(), cursor.unwrap_or(0));
        };
        let Some(cursor) = cursor else {
            return (Read::default(), ring.next);
        };
        assert!(cursor <= ring.next, "a cursor the tape handed out");
        let first = ring.next - ring.transactions.len() as u64;
        let missed = first.saturating_sub(cursor);
        let skip = usize::try_from(cursor.saturating_sub(first)).unwrap_or(usize::MAX);
        let transactions = ring.transactions.iter().skip(skip).cloned().collect();
        (
            Read {
                transactions,
                missed,
            },
            ring.next,
        )
    }
}

/// The fills `transactions` recorded on `market`, in order. A transaction
/// whose records don't decode is left out and counted.
pub fn fills(transactions: &[TapeTransaction], market: &PhoenixMarket) -> (Vec<MarketFill>, u64) {
    let mut records = PerpMarketRecords::default();
    let mut undecoded = 0;
    let now = chrono::Utc::now();
    for transaction in transactions {
        let summary = TransactionSummary {
            signature: Signature::from(transaction.signature),
            slot: transaction.slot,
            // The validator hands no block time with a transaction; the
            // tape is read within a second of it.
            block_time: Some(now),
            fee_lamports: 0,
            error: None,
            logs: Vec::new(),
            account_keys: Vec::new(),
            pre_balances: Vec::new(),
            post_balances: Vec::new(),
            token_balance_changes: Vec::new(),
            inner_instructions: transaction
                .calls
                .iter()
                .map(|(outer, data)| InnerInstruction {
                    outer: *outer,
                    program: Tape::program(),
                    data: data.clone(),
                })
                .collect(),
        };
        if records
            .add_transaction(&summary, std::slice::from_ref(market))
            .is_err()
        {
            undecoded += 1;
        }
    }
    (records.fills, undecoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transaction(slot: u64) -> TapeTransaction {
        TapeTransaction {
            slot,
            signature: [0; 64],
            calls: Vec::new(),
        }
    }

    #[test]
    fn a_reader_starts_at_the_end_and_reads_what_comes_after() {
        let tape = Tape::default();
        tape.push(transaction(1));
        let (read, cursor) = tape.since(None);
        assert!(read.transactions.is_empty());
        tape.push(transaction(2));
        tape.push(transaction(3));
        let (read, cursor) = tape.since(Some(cursor));
        assert_eq!(
            read.transactions.iter().map(|t| t.slot).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(read.missed, 0);
        let (read, _) = tape.since(Some(cursor));
        assert!(read.transactions.is_empty());
    }

    #[test]
    fn a_reader_too_far_behind_is_told_what_it_missed() {
        let tape = Tape::default();
        let (_, cursor) = tape.since(None);
        for slot in 0..(TAPE_TRANSACTIONS_MAX as u64 + 5) {
            tape.push(transaction(slot));
        }
        let (read, _) = tape.since(Some(cursor));
        assert_eq!(read.missed, 5);
        assert_eq!(read.transactions.len(), TAPE_TRANSACTIONS_MAX);
        assert_eq!(read.transactions[0].slot, 5);
    }
}
