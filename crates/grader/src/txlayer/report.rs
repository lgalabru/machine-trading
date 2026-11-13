//! What a session's transaction layer reports: each slot's block in order,
//! what became of each of the bot's sends, and what it paid. The blocks are
//! in the session report, so a replay compares ordering too.

use serde::{Deserialize, Serialize};

use super::{RouteKind, fees::FeeLedger};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    /// The actors that sent through the pool: each one's ledger, checked
    /// against its own wallet, and what it counts of its own doing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actors: Vec<ActorFees>,
}

/// What one actor paid to land, and did.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ActorFees {
    /// `actor:<kind>/<i>`.
    pub sender: String,
    pub wallet: String,
    pub fees: FeeLedger,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub counts: std::collections::BTreeMap<String, u64>,
    /// What it notes of its own: its inventory's change, what it made.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub values: std::collections::BTreeMap<String, f64>,
}

/// One slot's block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockRecord {
    /// Slots from the session's start.
    pub at: u64,
    pub regime: String,
    /// What the block cleared at, in micro-lamports per compute unit.
    pub price: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub jito: bool,
    /// What landed, in order.
    pub landed: Vec<Landed>,
    /// Bundles refused or not committed, with why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<Refused>,
}

/// One transaction of a block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Refused {
    pub sender: String,
    pub reason: String,
}

/// One of the bot's entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SendRecord {
    /// The step it was sent in, and its sequence in the step.
    pub step: u64,
    pub seq: u64,
    pub route: RouteKind,
    pub transactions: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bundle: bool,
    /// `landed`, `failed`, `expired`, `lost`, `pending`, or `merged` (signed
    /// again into another of its sends).
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
    pub fee: u64,
    pub tip: u64,
    pub attempts: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub resigned: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grade's report reads back (see [`crate::grade::rescore`]), fields
    /// it leaves out included.
    #[test]
    fn the_report_reads_back_what_it_wrote() {
        let report = TxLayerReport {
            calibration: "fixture".into(),
            blocks: vec![BlockRecord {
                at: 3,
                regime: "calm".into(),
                price: 0,
                skipped: false,
                jito: false,
                landed: vec![Landed {
                    sender: "bot".into(),
                    fee: 5_000,
                    tip: 0,
                    cu: 10_000,
                    ok: true,
                    bundle: None,
                }],
                refused: Vec::new(),
            }],
            sends: vec![SendRecord {
                step: 1,
                seq: 0,
                route: RouteKind::Rpc,
                transactions: 1,
                bundle: false,
                state: "landed".into(),
                at: Some(3),
                fee: 5_000,
                tip: 0,
                attempts: 1,
                resigned: 0,
                reason: None,
            }],
            fees: FeeLedger::default(),
            pending_at_end: 0,
            actors: Vec::new(),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("refused") && !json.contains("resigned"));
        assert_eq!(
            serde_json::from_str::<TxLayerReport>(&json).unwrap(),
            report
        );
    }
}
