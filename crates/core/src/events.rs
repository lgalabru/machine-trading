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

use crate::control::{FeedStatus, StrategyStatus};

/// The protocol's version: a sidecar refuses a [`Event::Hello`] with
/// another. Raised when an event changes in a way an old reader would
/// misread; a new variant isn't such a change if readers skip what they
/// don't know.
pub const PROTOCOL: u32 = 1;

/// The most bytes of one event line: a hello with a few hundred strategies.
pub const EVENT_BYTES_MAX: usize = 1 << 20;

/// One line of the stream: when the runtime wrote it, and what.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Line {
    /// Milliseconds since the Unix epoch, by the runtime's clock.
    pub at_ms: u64,
    #[serde(flatten)]
    pub event: Event,
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
        step_ms: u64,
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
    /// The enforcer paused a strategy.
    Paused {
        strategy: String,
        reason: PauseReason,
        why: String,
    },
    /// A control command that changes something was done or refused: a
    /// person (or a sidecar) acted, and the strategies may have changed.
    Command {
        command: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// This connection fell behind and lost `dropped` events: read
    /// `status` to know where things stand.
    Lagged { dropped: u64 },
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
            at_ms: 7,
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
            r#"{"at_ms":7,"event":"markout","strategy":"mm-sol","model":"through","fill_slot":100,"horizon_slots":25,"bps":-1.5}"#
        );
        assert_eq!(serde_json::from_str::<Line>(&text).unwrap(), line);
        let command: Line =
            serde_json::from_str(r#"{"at_ms":8,"event":"command","command":"pause","ok":true}"#)
                .unwrap();
        assert_eq!(
            command.event,
            Event::Command {
                command: "pause".into(),
                ok: true,
                error: None
            }
        );
    }
}
