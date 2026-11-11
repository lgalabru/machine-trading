//! The turn protocol, `mtx-turn-1`: the world moves in steps, and waits for
//! the bot between them.
//!
//! The grader hands the bot one end of a socket as file descriptor
//! [`TURN_FD`] (named by `MTX_TURN_FD`) and talks JSON lines over it:
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
//! A scenario that opts in also puts the step's trades, the books' depth and
//! the world's clock on each [`Step`]; [`Turns::tape`], [`Turns::depth`] and
//! [`Turns::unix_timestamp`] read them.
//!
//! [`Turns`] does the bot's side: [`Turns::next`] answers `done` for the
//! previous step and waits for the next one.
//!
//! ```ignore
//! # async fn example(trader: mtx_kit::Trader) -> mtx_kit::Result<()> {
//! let mut turns = mtx_kit::Turns::connect()?;
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

use crate::{Error, Result, Side};

pub const PROTOCOL: &str = "mtx-turn-1";
/// The file descriptor the grader passes the turn socket on.
pub const TURN_FD: i32 = 3;
pub const TURN_FD_VAR: &str = "MTX_TURN_FD";

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
///
/// A scenario that opts in to turn data adds what a bot can't cheaply read
/// from the chain itself; a step of any other scenario carries none of the
/// optional fields, and its line is the same as ever.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub k: u64,
    pub slot: u64,
    /// The session's last step: the account is valued after it.
    pub last: bool,
    /// The world's clock at this step, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unix_timestamp: Option<i64>,
    /// Every trade since the previous step, on every venue, between any
    /// accounts, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tape: Vec<Trade>,
    /// The top of each venue's book at this step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depth: Vec<BookLevels>,
}

impl Step {
    /// A step with no turn data.
    pub fn new(k: u64, slot: u64, last: bool) -> Self {
        Self {
            k,
            slot,
            last,
            ..Self::default()
        }
    }
}

/// Which side of a trade the bot was on.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Taker,
    Maker,
}

/// One trade on a venue's tape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Trade {
    /// Where it traded, as the mandate names venues: `phoenix` or `openbook`.
    pub venue: String,
    /// The taker's side.
    pub side: Side,
    /// Price, quote units per base unit, before fees.
    pub price: f64,
    /// Size, base units.
    pub base: f64,
    /// The slot it landed in, or was first seen in.
    pub slot: u64,
    /// Which side of it the bot was, when it was one of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<Role>,
}

/// One price level of a book.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Level {
    pub price: f64,
    /// Size resting at the price, base units.
    pub base: f64,
}

/// The top levels of one venue's book, best first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookLevels {
    pub venue: String,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
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
    /// The step's turn data, kept for [`Turns::tape`] and its siblings.
    tape: Vec<Trade>,
    depth: Vec<BookLevels>,
    unix_timestamp: Option<i64>,
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
            tape: Vec::new(),
            depth: Vec::new(),
            unix_timestamp: None,
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

    /// The current step's trades on every venue since the previous step,
    /// oldest first: empty unless the scenario reports a tape.
    pub fn tape(&self) -> &[Trade] {
        &self.tape
    }

    /// The top of each venue's book at the current step: empty unless the
    /// scenario reports depth.
    pub fn depth(&self) -> &[BookLevels] {
        &self.depth
    }

    /// The world's clock at the current step, in Unix seconds, when the
    /// scenario reports it.
    pub fn unix_timestamp(&self) -> Option<i64> {
        self.unix_timestamp
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
                    self.tape.clone_from(&step.tape);
                    self.depth.clone_from(&step.depth);
                    self.unix_timestamp = step.unix_timestamp;
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
                Message::Step(Step::new(k, 102 + 2 * k, k == 1)),
            );
            assert_eq!(recv(), Message::Done { k });
        }
        send(&mut grader, Message::End);
        assert_eq!(bot.join().unwrap(), vec![0, 1]);
    }

    #[test]
    fn a_step_without_turn_data_is_the_line_it_always_was() {
        let line = serde_json::to_string(&Message::Step(Step::new(3, 120, false))).unwrap();
        assert_eq!(line, r#"{"t":"step","k":3,"slot":120,"last":false}"#);
        let Message::Step(step) = serde_json::from_str(&line).unwrap() else {
            panic!("a step");
        };
        assert!(step.tape.is_empty() && step.depth.is_empty() && step.unix_timestamp.is_none());
    }

    #[test]
    fn turn_data_round_trips_and_is_kept_for_the_bot() {
        let (bot, grader) = UnixStream::pair().unwrap();
        let mut grader_reader = BufReader::new(grader.try_clone().unwrap());
        let mut grader = grader;
        let hello = Message::Hello(Hello {
            protocol: PROTOCOL.into(),
            steps: 2,
            slots_per_step: 2,
            start_slot: 100,
            end_slot: 104,
            turn_wall_ms: 1_000,
        });
        let line = |m: &Message| {
            let mut line = serde_json::to_string(m).unwrap();
            line.push('\n');
            line
        };
        grader.write_all(line(&hello).as_bytes()).unwrap();
        let bot = std::thread::spawn(move || {
            let mut turns = Turns::over(bot).unwrap();
            let mut seen = vec![];
            while let Some(step) = turns.next().unwrap() {
                seen.push((
                    step.k,
                    turns.tape().to_vec(),
                    turns.depth().to_vec(),
                    turns.unix_timestamp(),
                ));
            }
            seen
        });
        let mut ready = String::new();
        grader_reader.read_line(&mut ready).unwrap();
        let trade = Trade {
            venue: "phoenix".into(),
            side: Side::Sell,
            price: 150.25,
            base: 2.0,
            slot: 101,
            own: Some(Role::Maker),
        };
        let book = BookLevels {
            venue: "openbook".into(),
            bids: vec![Level {
                price: 150.0,
                base: 4.0,
            }],
            asks: vec![],
        };
        let first = Step {
            tape: vec![trade.clone()],
            depth: vec![book.clone()],
            unix_timestamp: Some(1_700_000_000),
            ..Step::new(0, 102, false)
        };
        for step in [first.clone(), Step::new(1, 104, true)] {
            let k = step.k;
            grader.write_all(line(&Message::Step(step)).as_bytes()).unwrap();
            let mut done = String::new();
            grader_reader.read_line(&mut done).unwrap();
            assert_eq!(
                serde_json::from_str::<Message>(&done).unwrap(),
                Message::Done { k }
            );
        }
        grader.write_all(line(&Message::End).as_bytes()).unwrap();
        let seen = bot.join().unwrap();
        assert_eq!(
            seen[0],
            (0, vec![trade], vec![book], Some(1_700_000_000))
        );
        assert_eq!(seen[1], (1, vec![], vec![], None));
        let json = serde_json::to_string(&first).unwrap();
        assert_eq!(serde_json::from_str::<Step>(&json).unwrap(), first);
    }
}
