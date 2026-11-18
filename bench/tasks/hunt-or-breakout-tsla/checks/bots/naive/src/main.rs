//! The ladder's non-reading rungs, picked by `MTX_NAIVE`:
//!
//! - `fade`: on every run of the book through a round level, the cap
//!   against it; out once the book is back at the level.
//! - `follow`: the cap with it; out once the oracle has moved and stalled,
//!   or the book is back through the level.
//! - `wait-oracle`: reads the oracle and nothing else: follows a run the
//!   oracle moves with within 30 slots, and fades one it doesn't while the
//!   book is still through.
//!
//! - `push-fade`: the first screen's best bot (gpt-6.1-sol), its core: it
//!   reads every trader account, and takes a clip of 2.2 TSLA or more from
//!   an account flat before it as a stop hunter's push: it rides the push
//!   into the run, fades the run once the book is through the level, and
//!   follows every run nobody pushed. It reads sizes and flatness, never
//!   who an account is.
//!
//! - `ride-sweep`: reads no identity either: a clip of 2.2 TSLA or more from
//!   an account flat before it is taken as a push into a run, ridden with
//!   the cap until the book is through the level, then closed. It earns
//!   only if every push is followed by a run.
//!
//! A run back through a level traded in the last 300 slots is the same run.

use mtx_bot_hunt_or_breakout::{Bet, Crosses, LEVEL_STEP_USD, Trade, Watch, go_to, mid};
use mtx_kit::{
    prelude::*,
    run,
    venue::{PhoenixVenue, key},
};

const ORACLE_SLOTS: u64 = 30;
const ORACLE_BPS: f64 = 6.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Fade,
    Follow,
    WaitOracle,
    PushFade,
    RideSweep,
}

/// The size of a push, in TSLA.
const PUSH_UNITS: f64 = 2.2;
const THROUGH: f64 = 0.0008;

struct Naive {
    mode: Mode,
    cap: f64,
    crosses: Crosses,
    pending: Option<(f64, f64, u64, f64)>,
    trade: Option<Trade>,
    last_level: Option<(f64, u64)>,
    watch: Watch,
    /// A push seen: its way, slot and the mid then.
    push: Option<(f64, u64, f64)>,
}

impl Strategy for Naive {
    fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.cap;
        if matches!(self.mode, Mode::PushFade | Mode::RideSweep) {
            let venue = PhoenixVenue::from_json(&session.venue)?;
            let own = venue.trader_account(&key(&session.authority)?).to_string();
            self.watch = Watch::new(&venue, &session.symbol, Watch::every_trader(&own)?, 40)?;
        }
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let slot = market.slot;
        orders.cancel_all();
        let Some(mid) = mid(market) else {
            return Ok(());
        };
        let oracle = market.oracle;
        if matches!(self.mode, Mode::PushFade | Mode::RideSweep) {
            return self.push_fade(slot, mid, oracle, market, orders);
        }
        let cross = self.crosses.update(mid);
        if let Some(trade) = &mut self.trade {
            if trade.done(slot, mid, oracle) {
                self.trade = None;
            }
        } else if let Some((level, dir)) = cross {
            let again = self
                .last_level
                .is_some_and(|(l, at)| (l - level).abs() < 1e-6 && at + 300 > slot);
            if !again && self.pending.is_none() {
                self.pending = Some((dir, level, slot, oracle));
            }
        }
        if let Some((dir, level, at, was)) = self.pending {
            let bet = match self.mode {
                Mode::Fade => Some(Bet::Fade),
                Mode::Follow | Mode::PushFade | Mode::RideSweep => Some(Bet::Follow),
                Mode::WaitOracle => {
                    if dir * (oracle / was - 1.0) * 10_000.0 >= ORACLE_BPS {
                        Some(Bet::Follow)
                    } else if slot >= at + ORACLE_SLOTS && dir * (mid - level) > 0.0 {
                        Some(Bet::Fade)
                    } else {
                        None
                    }
                }
            };
            if let Some(bet) = bet {
                self.trade = Some(Trade::new(bet, dir, level, slot, was));
                self.last_level = Some((level, slot));
                self.pending = None;
            } else if slot >= at + ORACLE_SLOTS {
                self.pending = None;
            }
        }
        let target = match (&self.trade, market.last) {
            (Some(trade), false) => trade.target(self.cap),
            _ => 0.0,
        };
        go_to(target, market, orders);
        Ok(())
    }
}

impl Naive {
    fn push_fade(
        &mut self,
        slot: u64,
        mid: f64,
        oracle: f64,
        market: &Market,
        orders: &mut Orders,
    ) -> BotResult {
        self.watch.read(slot)?;
        let now = self.watch.now().to_vec();
        for (i, c) in self.watch.last_changes().into_iter().enumerate() {
            let before = now[i] - c;
            let new = self.watch.first.get(i).is_some_and(|p| p.abs() < 1e-9);
            if c.abs() >= PUSH_UNITS && before.abs() < 0.01 && new {
                self.push = Some((c.signum(), slot, mid));
            }
        }
        let cross = self.crosses.update(mid);
        if let Some(trade) = &mut self.trade {
            if trade.done(slot, mid, oracle) {
                self.trade = None;
            }
        }
        let mut riding = None;
        if let Some((dir, at, then)) = self.push {
            if dir * (mid / then - 1.0) >= THROUGH && self.mode == Mode::RideSweep {
                // The run came: take the jump and stand aside.
                self.push = None;
            } else if dir * (mid / then - 1.0) >= THROUGH {
                let level = if dir > 0.0 {
                    (then / LEVEL_STEP_USD).ceil() * LEVEL_STEP_USD
                } else {
                    (then / LEVEL_STEP_USD).floor() * LEVEL_STEP_USD
                };
                self.trade = Some(Trade::new(Bet::Fade, dir, level, slot, oracle));
                self.last_level = Some((level, slot));
                self.push = None;
            } else if slot > at + 12 {
                self.push = None;
            } else if self.trade.is_none() {
                riding = Some(dir);
            }
        } else if self.trade.is_none()
            && self.mode == Mode::PushFade
            && let Some((level, dir)) = cross
        {
            let again = self
                .last_level
                .is_some_and(|(l, at)| (l - level).abs() < 1e-6 && at + 300 > slot);
            if !again {
                self.trade = Some(Trade::new(Bet::Follow, dir, level, slot, oracle));
                self.last_level = Some((level, slot));
            }
        }
        let target = match (&self.trade, riding, market.last) {
            (_, _, true) => 0.0,
            (Some(trade), _, _) => trade.target(self.cap),
            (None, Some(dir), _) => dir * self.cap,
            _ => 0.0,
        };
        go_to(target, market, orders);
        Ok(())
    }
}

fn main() -> BotResult {
    let mode = match std::env::var("MTX_NAIVE").as_deref() {
        Ok("fade") => Mode::Fade,
        Ok("follow") => Mode::Follow,
        Ok("wait-oracle") => Mode::WaitOracle,
        Ok("push-fade") => Mode::PushFade,
        Ok("ride-sweep") => Mode::RideSweep,
        other => return Err(format!("MTX_NAIVE must be fade, follow, wait-oracle, push-fade or ride-sweep, not {other:?}").into()),
    };
    run(Naive {
        mode,
        cap: 0.0,
        crosses: Crosses::new(LEVEL_STEP_USD),
        pending: None,
        trade: None,
        last_level: None,
        watch: Watch::default(),
        push: None,
    })
}
