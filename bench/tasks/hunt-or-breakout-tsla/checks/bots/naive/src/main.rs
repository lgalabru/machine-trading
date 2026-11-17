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
//! A run back through a level traded in the last 300 slots is the same run.

use mtx_bot_hunt_or_breakout::{Bet, Crosses, LEVEL_STEP_USD, Trade, go_to, mid};
use mtx_kit::{prelude::*, run};

const ORACLE_SLOTS: u64 = 30;
const ORACLE_BPS: f64 = 6.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Fade,
    Follow,
    WaitOracle,
}

struct Naive {
    mode: Mode,
    cap: f64,
    crosses: Crosses,
    pending: Option<(f64, f64, u64, f64)>,
    trade: Option<Trade>,
    last_level: Option<(f64, u64)>,
}

impl Strategy for Naive {
    fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.cap;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let slot = market.slot;
        orders.cancel_all();
        let Some(mid) = mid(market) else {
            return Ok(());
        };
        let oracle = market.oracle;
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
                Mode::Follow => Some(Bet::Follow),
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

fn main() -> BotResult {
    let mode = match std::env::var("MTX_NAIVE").as_deref() {
        Ok("fade") => Mode::Fade,
        Ok("follow") => Mode::Follow,
        Ok("wait-oracle") => Mode::WaitOracle,
        other => return Err(format!("MTX_NAIVE must be fade, follow or wait-oracle, not {other:?}").into()),
    };
    run(Naive {
        mode,
        cap: 0.0,
        crosses: Crosses::new(LEVEL_STEP_USD),
        pending: None,
        trade: None,
        last_level: None,
    })
}
