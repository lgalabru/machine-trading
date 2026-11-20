//! What a chain tells a host as it happens: account writes, slots and
//! blocks, in the order the chain produced them. A geyser plugin sends
//! these from inside a validator, and a test sends recorded ones; the host
//! that trades on them (`mtx-runtime`'s `live`) sees no difference.
//!
//! Keys are raw bytes: this crate holds no chain code.

/// An account's or a program's address.
pub type Address = [u8; 32];

/// One thing the chain did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedEvent {
    /// An account was written in `slot`.
    Account(AccountWrite),
    /// A slot reached `status`.
    Slot {
        slot: u64,
        parent: Option<u64>,
        status: SlotStatus,
    },
    /// A block was produced: its hash is what the next transactions name.
    Block {
        slot: u64,
        blockhash: String,
        block_time_unix: Option<i64>,
        block_height: Option<u64>,
    },
}

/// An account as a write left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountWrite {
    pub slot: u64,
    pub address: Address,
    pub owner: Address,
    pub lamports: u64,
    pub data: Vec<u8>,
    /// Orders the writes of one account within a slot: the higher is later.
    pub write_version: u64,
}

/// How far along a slot is, as a validator reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotStatus {
    /// The first shred of the slot arrived: its leader has started.
    FirstShredReceived,
    /// Every shred of the slot arrived.
    Completed,
    /// The validator made a bank for the slot.
    CreatedBank,
    /// The slot was replayed: the highest slot of the heaviest fork.
    Processed,
    /// A supermajority voted on the slot.
    Confirmed,
    /// The slot is finalized.
    Rooted,
    /// The slot's fork was abandoned.
    Dead,
}

impl SlotStatus {
    /// The name a journal writes it under.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FirstShredReceived => "first_shred_received",
            Self::Completed => "completed",
            Self::CreatedBank => "created_bank",
            Self::Processed => "processed",
            Self::Confirmed => "confirmed",
            Self::Rooted => "rooted",
            Self::Dead => "dead",
        }
    }
}
