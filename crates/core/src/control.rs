//! Controlling a live runtime while it runs (`docs/rfd/0004`, step 4): the
//! commands `mtx live` sends to the runtime's Unix socket, and what the
//! runtime answers. One JSON object a line each way, one command a
//! connection.
//!
//! A command takes effect at the strategy's next step boundary: a step
//! under way finishes first.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Mandate;

/// What a strategy's resting orders become when it is paused.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnPause {
    /// Cancel them, keep the position.
    #[default]
    Cancel,
    /// Leave them resting.
    Keep,
    /// Cancel them and close the position.
    Flatten,
}

/// A strategy to run: its component, its mandate and its key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AddStrategy {
    /// What the strategy is called in every other command.
    pub name: String,
    /// The strategy component (`bot.wasm`).
    pub component: PathBuf,
    pub mandate: Mandate,
    /// The keypair file it trades with: its own account.
    pub keypair: PathBuf,
    /// A file holding Phoenix's `/exchange` response for its market.
    pub exchange: PathBuf,
    /// Its market, as Phoenix names it (`SOL`).
    pub symbol: String,
    /// Slots between its steps.
    pub slots_per_step: u64,
    /// Start stepping at once, rather than paused.
    #[serde(default)]
    pub run: bool,
}

/// What `mtx live` asks of the runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Every strategy and how it is doing.
    Status,
    /// Stop stepping `name`, or every strategy.
    Pause {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        orders: OnPause,
    },
    /// Step `name` again, or every paused strategy.
    Resume {
        #[serde(default)]
        name: Option<String>,
    },
    /// Give `name` a new mandate: it restarts under it (`end`, then
    /// `start`), paused or running as it was.
    Mandate { name: String, mandate: Mandate },
    /// Compile, start and keep a strategy, paused unless it says `run`.
    Add(AddStrategy),
    /// Pause `name`, cancel its orders, `end` it and forget it.
    Remove { name: String },
    /// The kill switch: pause everything and cancel every order.
    Kill,
}

/// Why a strategy isn't stepping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StrategyState {
    Running,
    /// Paused by a command, or by the enforcer (`why`).
    Paused {
        why: String,
    },
    /// It can't step again: it trapped, or didn't load.
    Stopped {
        why: String,
    },
}

/// One strategy, as `mtx live status` shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrategyStatus {
    pub name: String,
    #[serde(flatten)]
    pub state: StrategyState,
    pub symbol: String,
    /// Its account's key, base58.
    pub authority: String,
    /// The mandate's position cap and drawdown limit on its market.
    pub cap: f64,
    pub max_drawdown: f64,
    /// Steps taken, and steps skipped because the previous one was still
    /// running when its slot came.
    pub steps: u64,
    pub skipped: u64,
    /// The slot of its last step, and what it read there.
    pub last_slot: Option<u64>,
    pub position: Option<f64>,
    pub pnl: Option<f64>,
    pub drawdown: Option<f64>,
}

/// What the runtime answers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// Done: every strategy as it stands after the command.
    Ok { strategies: Vec<StrategyStatus> },
    /// Refused, and nothing changed.
    Error { error: String },
}

/// The most bytes of a command or a reply: a mandate inline and a few
/// hundred strategies' status.
pub const MESSAGE_BYTES_MAX: usize = 1 << 20;

/// Send `command` to the runtime listening at `socket`, and read its reply.
#[cfg(unix)]
pub fn request(socket: &std::path::Path, command: &Command) -> std::io::Result<Reply> {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    let mut line = serde_json::to_string(command).map_err(std::io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    let read = BufReader::new(stream)
        .take(MESSAGE_BYTES_MAX as u64)
        .read_line(&mut reply)?;
    if read == 0 {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    serde_json::from_str(&reply).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_read_as_mtx_live_writes_them() {
        let pause: Command =
            serde_json::from_str(r#"{"command":"pause","name":"a","orders":"flatten"}"#).unwrap();
        assert_eq!(
            pause,
            Command::Pause {
                name: Some("a".into()),
                orders: OnPause::Flatten
            }
        );
        let all: Command = serde_json::from_str(r#"{"command":"pause"}"#).unwrap();
        assert_eq!(
            all,
            Command::Pause {
                name: None,
                orders: OnPause::Cancel
            }
        );
        assert!(serde_json::from_str::<Command>(r#"{"command":"pause","nmae":"a"}"#).is_err());
        let reply = Reply::Error {
            error: "no strategy b".into(),
        };
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Reply>(&text).unwrap(), reply);
    }
}
