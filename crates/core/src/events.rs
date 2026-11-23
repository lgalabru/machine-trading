//! What a live runtime tells a sidecar as it happens (`docs/sidecar.md`):
//! a stream of [`Event`]s, one JSON object a line, pushed on the runtime's
//! event socket to every connection, beside the control socket's commands
//! ([`crate::control`]).
//!
//! A connection's first line is a [`Event::Hello`]: the protocol, the feed
//! and every strategy as they stand, so a sidecar starts from a snapshot and
//! needs no poll. What follows is what changes: each slot the runtime
//! stepped on, each strategy's step, each paper fill and its markouts, the
//! market's trades, every pause and every command.
//!
//! The runtime never waits on a reader. Each connection has a bounded
//! queue; a reader that falls behind loses the oldest events and is told
//! how many with [`Event::Lagged`], and should read `status` again. Events
//! are numbers and enums, with text only for a reason or an error the
//! runtime wrote itself: nothing a stranger can put on the chain reaches a
//! sidecar through them.

use serde::{Deserialize, Serialize};

use crate::control::{CommandKind, ControlError, FeedStatus, StrategyStatus};

/// The protocol's version, of both sockets: a sidecar refuses a
/// [`Event::Hello`] with another. Raised when an event or a reply changes
/// in a way an old reader would misread. A new variant of [`Event`] isn't
/// such a change: a reader skips a line whose `event` it doesn't know.
///
/// 2: times in microseconds (`at_us`), a step's [`StepTimings`], commands
/// and refusals typed ([`CommandKind`], [`crate::control::ErrorCode`]),
/// skipped and stopped steps, and the fates of live sends.
pub const PROTOCOL: u32 = 2;

/// The most bytes of one event line: a hello with a few hundred strategies.
pub const EVENT_BYTES_MAX: usize = 1 << 20;

/// One line of the stream: when the runtime wrote it, and what.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Line {
    /// Microseconds since the Unix epoch, by the runtime's clock, as the
    /// line was handed to the socket. A sidecar on the same host reads its
    /// own delay off it.
    pub at_us: u64,
    #[serde(flatten)]
    pub event: Event,
}

impl Line {
    pub fn at_ms(&self) -> u64 {
        self.at_us / 1_000
    }
}

/// Where a step's time went, each stage in microseconds, measured on the
/// runtime's monotonic clock from the validator's notice of the slot to
/// the step's last send. A stage the runtime couldn't time is 0.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StepTimings {
    /// The plugin's callback with the slot's first banked status (the node
    /// has its bank) to the runtime's feed applying it.
    pub feed_us: u64,
    /// The feed applying it to the strategies' loop starting the step.
    pub wake_us: u64,
    /// Waiting for the feed to apply the writes before the slot.
    pub settle_us: u64,
    /// Reading the strategy's market from the account cache.
    pub read_us: u64,
    /// The strategy's `step`, in its wasm sandbox.
    pub call_us: u64,
    /// Planning the sheet, signing and sending its orders.
    pub send_us: u64,
    /// From the plugin's callback to the last send: the sum, and what a
    /// strategy's reaction time is.
    pub total_us: u64,
}

/// Which way a fill or a trade went.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

/// How a shadow strategy's paper book filled (the runtime's
/// `live::journal`): `through`, a lower bound, or `touch`, an upper one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaperModel {
    Through,
    Touch,
}

/// Whether a fill made or took liquidity.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Liquidity {
    Maker,
    Taker,
}

/// Who paused a strategy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// The enforcer: the mandate was broken.
    Mandate,
    /// The enforcer: the feed was behind or dropped events.
    Feed,
}

/// What happened.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The first line of a connection: where things stand.
    Hello {
        protocol: u32,
        feed: FeedStatus,
        strategies: Vec<StrategyStatus>,
    },
    /// The runtime stepped on a bank slot.
    Slot {
        slot: u64,
        behind: u64,
        dropped: u64,
    },
    /// A strategy's step ended.
    Step {
        strategy: String,
        slot: u64,
        k: u64,
        /// From the slot reaching the strategies to the step's end, as
        /// `mtx live status` reports it.
        step_ms: u64,
        timings: StepTimings,
        /// Whether its `step` returned a sheet.
        answered: bool,
        shadow: bool,
        /// Orders the step sent (planned and sent, in a shadow).
        orders: u32,
        position: f64,
        pnl_usd: f64,
        drawdown_usd: f64,
        /// Still stepping after this step: the enforcer didn't stop it.
        running: bool,
    },
    /// A shadow strategy's order filled on paper.
    PaperFill {
        strategy: String,
        model: PaperModel,
        slot: u64,
        side: Side,
        price: f64,
        size: f64,
        liquidity: Liquidity,
        /// How far on the good side of the mid it filled, in bps.
        edge_bps: f64,
        /// The paper position after it.
        position: f64,
    },
    /// A paper fill, marked to the mid `horizon_slots` later, in bps:
    /// positive if the market went its way.
    Markout {
        strategy: String,
        model: PaperModel,
        fill_slot: u64,
        horizon_slots: u64,
        bps: f64,
    },
    /// A trade on a market a strategy runs on, as the validator ran it.
    Trade {
        symbol: String,
        slot: u64,
        /// The taker's side.
        taker: Side,
        price: f64,
        size: f64,
    },
    /// What became of a transaction a live strategy sent without waiting
    /// (over the TPU): it landed at the `confirmed` commitment, or failed
    /// on chain, or wasn't seen landing in time (`landed` false, no slot).
    Fate {
        strategy: String,
        signature: String,
        landed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot: Option<u64>,
        /// The chain's error, for one that failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// From the send to the fate being known.
        after_us: u64,
    },
    /// A strategy's step came while its last was still running, and was
    /// skipped: a strategy slower than its step.
    StepSkipped { strategy: String, slot: u64 },
    /// A strategy can't step again: it trapped, ran out of fuel or time.
    Stopped { strategy: String, why: String },
    /// The enforcer paused a strategy.
    Paused {
        strategy: String,
        reason: PauseReason,
        why: String,
    },
    /// A control command that changes something was done or refused: a
    /// person (or a sidecar) acted, and the strategies may have changed.
    Command {
        command: CommandKind,
        /// Why it was refused; `None` if it was done.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ControlError>,
    },
    /// This connection fell behind and lost `dropped` events: read
    /// `status` to know where things stand.
    Lagged { dropped: u64 },
    /// A market the runtime watches without trading it
    /// (`control.watch`), read on a round-robin: its book's best bid and
    /// ask, if it has them, and its mark. The market's trades come as
    /// `trade` events.
    Quote {
        symbol: String,
        slot: u64,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
        mark: f64,
    },
    /// A pool the runtime watches without trading it (`control.pools`),
    /// read in the slot a write changed it and every so often besides:
    /// its price, and what swaps of growing size get from it each way, so
    /// a sidecar can price a route through several pools without holding
    /// any chain code. Amounts are in whole tokens; `base` and `quote` are
    /// the pool's two mints, in the pool's own order.
    Pool {
        /// The pool's program, as venues name it: `orca`, `raydium`.
        venue: String,
        address: String,
        /// The slot the runtime read it in, and the slot of the write it
        /// read: a pool nobody traded keeps its `written_slot`.
        slot: u64,
        written_slot: u64,
        base: String,
        quote: String,
        /// Quote per base, before fees.
        mid: f64,
        /// The swap fee, in bps of what goes in.
        fee_bps: f64,
        /// The liquidity in range, in the program's own unit.
        liquidity: f64,
        /// Swaps that pay base and receive quote, smallest first.
        sells: Vec<PoolRung>,
        /// Swaps that pay quote and receive base, smallest first.
        buys: Vec<PoolRung>,
        /// How long the read and its quotes took, in microseconds.
        read_us: u64,
    },
}

/// One exact swap through a pool as it stood: the swap that moves its
/// price by `bps`, what it pays and what it receives, fees included. A
/// pool's rungs each start from the same state, so each holds the ones
/// before it: between two rungs a swap gets at least their straight line.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct PoolRung {
    pub bps: f64,
    pub amount_in: f64,
    pub amount_out: f64,
}

/// Read the runtime's events at `socket`, a line at a time, starting with
/// its hello. Blocks on each read; the iterator ends when the runtime
/// closes the connection.
#[cfg(unix)]
pub fn subscribe(
    socket: &std::path::Path,
) -> std::io::Result<impl Iterator<Item = std::io::Result<Line>>> {
    use std::io::{BufRead, BufReader, Read};
    let stream = std::os::unix::net::UnixStream::connect(socket)?;
    let mut reader = BufReader::new(stream);
    Ok(std::iter::from_fn(move || {
        let mut text = String::new();
        let read = (&mut reader)
            .take(EVENT_BYTES_MAX as u64)
            .read_line(&mut text);
        match read {
            Ok(0) => None,
            Ok(_) if !text.ends_with('\n') => Some(Err(std::io::Error::other(format!(
                "an event line past {EVENT_BYTES_MAX} bytes"
            )))),
            Ok(_) => Some(serde_json::from_str(&text).map_err(std::io::Error::other)),
            Err(error) => Some(Err(error)),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_reads_as_it_is_written() {
        let line = Line {
            at_us: 7,
            event: Event::Markout {
                strategy: "mm-sol".into(),
                model: PaperModel::Through,
                fill_slot: 100,
                horizon_slots: 25,
                bps: -1.5,
            },
        };
        let text = serde_json::to_string(&line).unwrap();
        assert_eq!(
            text,
            r#"{"at_us":7,"event":"markout","strategy":"mm-sol","model":"through","fill_slot":100,"horizon_slots":25,"bps":-1.5}"#
        );
        assert_eq!(serde_json::from_str::<Line>(&text).unwrap(), line);
        let command: Line =
            serde_json::from_str(r#"{"at_us":8,"event":"command","command":"pause"}"#).unwrap();
        assert_eq!(
            command.event,
            Event::Command {
                command: CommandKind::Pause,
                error: None
            }
        );
    }
}
