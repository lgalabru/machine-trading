//! The turn protocol, `mth-turn-1`: the world moves in steps, and waits for
//! the bot between them.
//!
//! The grader hands the bot one end of a socket as file descriptor
//! [`TURN_FD`] (named by `MTH_TURN_FD`) and talks JSON lines over it:
//!
//! 1. Grader to bot: a [`Hello`] with the session's clock. The bot answers
//!    `{"t":"ready"}`.
//! 2. For each step: the grader moves the world (its blocks, the market's
//!    move, the other traders' orders), then sends a [`Step`]. The chain now
//!    holds the state after that step. The bot reads it, sends its
//!    transactions, and answers `{"t":"done","k":<step>}`. Nothing else moves
//!    until it does, up to [`Hello::turn_wall_ms`]; after that the world moves
//!    on, and sends that arrive while it moves are refused.
//! 3. Grader to bot: `{"t":"end"}`, then the bot is stopped.
//!
//! [`Turns`] does the bot's side: [`Turns::next`] answers `done` for the
//! previous step and waits for the next one.
//!
//! ```no_run
//! # async fn example(trader: mth_botkit::Trader) -> mth_botkit::Result<()> {
//! let mut turns = mth_botkit::Turns::connect()?;
//! while let Some(step) = turns.next()? {
//!     let mark = trader.mark_usd().await?;
//!     // ... decide, and send orders: they land before the next step.
//! #   let _ = (step, mark);
//! }
//! # Ok(())
//! # }
//! ```

use std::{
    io::{BufRead, BufReader, Write},
    os::{fd::FromRawFd, unix::net::UnixStream},
};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const PROTOCOL: &str = "mth-turn-1";
/// The file descriptor the grader passes the turn socket on.
pub const TURN_FD: i32 = 3;
pub const TURN_FD_VAR: &str = "MTH_TURN_FD";

/// The session's clock, sent once before the first step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    pub protocol: String,
    /// Steps in the session, and the slots each one advances.
    pub steps: u64,
    pub slots_per_step: u64,
    /// The slot the first step starts from, and the slot the session ends at.
    pub start_slot: u64,
    pub end_slot: u64,
    /// How long, in wall-clock milliseconds, the world waits for `done`
    /// before moving on without it.
    pub turn_wall_ms: u64,
}

/// One step: the world has moved to `slot`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub k: u64,
    pub slot: u64,
    /// The session's last step: the account is valued after it.
    pub last: bool,
}

/// Every message, either way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    Ready,
    Step(Step),
    Done { k: u64 },
    End,
}

/// The bot's end of the turn socket.
pub struct Turns {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    hello: Hello,
    /// The step the bot is on, answered `done` by the next [`Turns::next`].
    open: Option<u64>,
}

impl Turns {
    /// Take the socket the grader passed, read its [`Hello`] and answer
    /// `ready`.
    pub fn connect() -> Result<Self> {
        let fd: i32 = std::env::var(TURN_FD_VAR)
            .map_err(|_| turn_error(format!("{TURN_FD_VAR} is not set")))?
            .parse()
            .map_err(|_| turn_error(format!("{TURN_FD_VAR} is not a file descriptor")))?;
        // SAFETY: the grader opened this descriptor for us and nothing else
        // in the process owns it.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        Self::over(stream)
    }

    /// The bot's side over `stream`: read the hello, answer `ready`.
    pub fn over(stream: UnixStream) -> Result<Self> {
        let writer = stream.try_clone().map_err(io)?;
        let mut turns = Self {
            reader: BufReader::new(stream),
            writer,
            hello: Hello {
                protocol: String::new(),
                steps: 0,
                slots_per_step: 0,
                start_slot: 0,
                end_slot: 0,
                turn_wall_ms: 0,
            },
            open: None,
        };
        match turns.read()? {
            Some(Message::Hello(hello)) if hello.protocol == PROTOCOL => turns.hello = hello,
            Some(Message::Hello(hello)) => {
                return Err(turn_error(format!("unknown protocol {}", hello.protocol)));
            }
            other => return Err(turn_error(format!("expected a hello, got {other:?}"))),
        }
        turns.write(&Message::Ready)?;
        Ok(turns)
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Answer `done` for the step the bot is on, then wait for the next one.
    /// `None` once the session has ended.
    #[allow(clippy::should_implement_trait)] // fallible, unlike Iterator::next
    pub fn next(&mut self) -> Result<Option<Step>> {
        if let Some(k) = self.open.take() {
            self.write(&Message::Done { k })?;
        }
        loop {
            match self.read()? {
                Some(Message::Step(step)) => {
                    self.open = Some(step.k);
                    return Ok(Some(step));
                }
                Some(Message::End) | None => return Ok(None),
                Some(_) => continue,
            }
        }
    }

    fn read(&mut self) -> Result<Option<Message>> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).map_err(io)? == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&line)?))
    }

    fn write(&mut self, message: &Message) -> Result<()> {
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        // A grader that already ended the session closed its end; there's
        // nobody left to tell.
        match self.writer.write_all(line.as_bytes()) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            other => other.map_err(io),
        }
    }
}

fn io(source: std::io::Error) -> Error {
    Error::Io {
        path: "turn socket".into(),
        source,
    }
}

fn turn_error(reason: String) -> Error {
    Error::Env {
        name: TURN_FD_VAR,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bot_answers_each_step_once_and_stops_at_the_end() {
        let (bot, grader) = UnixStream::pair().unwrap();
        let hello = Hello {
            protocol: PROTOCOL.into(),
            steps: 2,
            slots_per_step: 2,
            start_slot: 100,
            end_slot: 104,
            turn_wall_ms: 1_000,
        };
        let mut grader_reader = BufReader::new(grader.try_clone().unwrap());
        let mut grader = grader;
        let send = |grader: &mut UnixStream, message: Message| {
            let mut line = serde_json::to_string(&message).unwrap();
            line.push('\n');
            grader.write_all(line.as_bytes()).unwrap();
        };
        let mut recv = || {
            let mut line = String::new();
            grader_reader.read_line(&mut line).unwrap();
            serde_json::from_str::<Message>(&line).unwrap()
        };
        send(&mut grader, Message::Hello(hello.clone()));
        let bot = std::thread::spawn(move || {
            let mut turns = Turns::over(bot).unwrap();
            let mut seen = vec![];
            while let Some(step) = turns.next().unwrap() {
                seen.push(step.k);
            }
            seen
        });
        assert_eq!(recv(), Message::Ready);
        for k in 0..2 {
            send(
                &mut grader,
                Message::Step(Step {
                    k,
                    slot: 102 + 2 * k,
                    last: k == 1,
                }),
            );
            assert_eq!(recv(), Message::Done { k });
        }
        send(&mut grader, Message::End);
        assert_eq!(bot.join().unwrap(), vec![0, 1]);
    }
}
