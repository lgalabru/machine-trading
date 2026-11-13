//! What a session's transaction layer reports: each slot's block in order,
//! what became of each of the bot's sends, and what it paid. The blocks are
//! in the session report, so a replay compares ordering too.

use serde::Serialize;

use super::{RouteKind, fees::FeeLedger};

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct TxLayerReport {
    pub calibration: String,
    /// The slots where something landed or was refused, or the leader
    /// skipped, in order.
    pub blocks: Vec<BlockRecord>,
    /// Each of the bot's entries, in the order sent.
    pub sends: Vec<SendRecord>,
    pub fees: FeeLedger,
    /// Entries still pending when the session ended: they never landed.
    pub pending_at_end: usize,
}

/// One slot's block.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BlockRecord {
    /// Slots from the session's start.
    pub at: u64,
    pub regime: &'static str,
    /// What the block cleared at, in micro-lamports per compute unit.
    pub price: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub jito: bool,
    /// What landed, in order.
    pub landed: Vec<Landed>,
    /// Bundles refused or not committed, with why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<Refused>,
}

/// One transaction of a block.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Landed {
    pub sender: String,
    /// Lamports: its fee and tip; its requested compute units.
    pub fee: u64,
    pub tip: u64,
    pub cu: u64,
    pub ok: bool,
    /// Which bundle of the block it belongs to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<u32>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Refused {
    pub sender: String,
    pub reason: String,
}

/// One of the bot's entries.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SendRecord {
    /// The step it was sent in, and its sequence in the step.
    pub step: u64,
    pub seq: u64,
    pub route: RouteKind,
    pub transactions: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bundle: bool,
    /// `landed`, `failed`, `expired`, `lost` or `pending`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
    pub fee: u64,
    pub tip: u64,
    pub attempts: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub resigned: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}
