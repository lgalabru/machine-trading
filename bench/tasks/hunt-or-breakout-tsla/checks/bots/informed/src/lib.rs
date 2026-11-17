//! The informed bot, scoring v2's 1: told who hunts stops and who is
//! informed, and nothing of the session's draws.
//!
//! It reads `MTX_REGIME`, what the verifier's `--informed` hands it: the
//! informed wallets and the stop hunter's (`wallets.informed`,
//! `wallets.stop-hunter`, authorities the session resolves from the
//! latent's roster), the hunter's hold and trigger, the grid, and how far
//! ahead the informed wallets see. It isn't told when or where the price
//! runs through a level, which way, or whether a run is a hunt or a break,
//! and it trades the same market under the same mandate, costs and latency
//! as any bot.
//!
//! Phoenix names traders by their account, a PDA of the wallet, so it maps
//! each wallet to its account and reads their TSLA positions every step.
//!
//! - **A hunt.** A hunter's account taking a clip in one step is a push. It
//!   fades the run with the cap once the book is through the level, and
//!   covers when the book comes back to it.
//! - **A break.** The informed accounts building a position one way is the
//!   break, before or after the book runs: it follows with the cap at once
//!   and holds until the oracle has moved and stalled.

use mtx_bot_hunt_or_breakout::{Bet, LEVEL_STEP_USD, Trade, Watch, go_to, mid, told_number};
use mtx_kit::{log, prelude::*, venue::PhoenixVenue};

/// A hunter's clip in one step, in TSLA.
const PUSH_UNITS: f64 = 1.0;
/// The informed accounts' net change over `FLOW_SLOTS` that is a break.
const FLOW_UNITS: f64 = 1.0;
const FLOW_SLOTS: u64 = 40;
/// How far the book must run past where it was at a push to be through.
const THROUGH: f64 = 0.0008;
/// Slots a push waits for the book to run.
const PUSH_WAIT_SLOTS: u64 = 12;
/// Slots after a trade before following the informed again.
const REST_SLOTS: u64 = 40;

#[derive(Debug, Clone, Copy)]
struct Push {
    dir: f64,
    at: u64,
    mid: f64,
}

/// The strategy.
#[derive(Default)]
pub struct Informed {
    cap: f64,
    step: f64,
    watch: Watch,
    informed: usize,
    /// Each hunter account's open push, by way.
    hunting: Vec<Option<f64>>,
    push: Option<Push>,
    trade: Option<Trade>,
    rest_until: u64,
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.cap;
        let told = session
            .var("MTX_REGIME")
            .ok_or("no MTX_REGIME: the informed bot is told the session's latent state")?;
        let told: serde_json::Value = serde_json::from_str(told)?;
        self.step = told_number(&told, "level_step_usd").unwrap_or(LEVEL_STEP_USD);
        let wallets = |kind: &str| -> BotResult<Vec<String>> {
            Ok(told["wallets"][kind]
                .as_array()
                .ok_or_else(|| format!("MTX_REGIME names no {kind} wallets"))?
                .iter()
                .filter_map(|w| w.as_str().map(str::to_string))
                .collect())
        };
        let (informed, hunters) = (wallets("informed")?, wallets("stop-hunter")?);
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let mut accounts = Watch::of_authorities(&venue, &informed)?;
        self.informed = accounts.len();
        accounts.extend(Watch::of_authorities(&venue, &hunters)?);
        self.hunting = vec![None; accounts.len() - self.informed];
        log(&format!(
            "informed {informed:?}, hunters {hunters:?}, accounts {accounts:?}"
        ));
        self.watch = Watch::new(&venue, &session.symbol, accounts, FLOW_SLOTS + 20)?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let slot = market.slot;
        orders.cancel_all();
        self.watch.read(slot)?;
        let Some(mid) = mid(market) else {
            return Ok(());
        };
        let oracle = market.oracle;

        // The hunter's pushes, and its trades back out.
        let last = self.watch.last_changes();
        for (h, open) in self.hunting.iter_mut().enumerate() {
            let c = last[self.informed + h];
            if c.abs() < PUSH_UNITS {
                continue;
            }
            match *open {
                Some(dir) if dir == -c.signum() => *open = None,
                _ => {
                    *open = Some(c.signum());
                    self.push = Some(Push {
                        dir: c.signum(),
                        at: slot,
                        mid,
                    });
                    log(&format!("slot {slot}: a hunter pushed {c:+.3}"));
                }
            }
        }
        // The informed accounts' flow.
        let flow: f64 = self.watch.changes(FLOW_SLOTS)[..self.informed].iter().sum();

        if let Some(trade) = &mut self.trade {
            if trade.done(slot, mid, oracle) {
                self.trade = None;
                self.rest_until = slot + REST_SLOTS;
            }
        }
        if self.trade.is_none() {
            if let Some(push) = self.push {
                if push.dir * (mid / push.mid - 1.0) >= THROUGH {
                    let level = if push.dir > 0.0 {
                        (push.mid / self.step).ceil() * self.step
                    } else {
                        (push.mid / self.step).floor() * self.step
                    };
                    log(&format!("slot {slot}: fade the hunt {:+} through {level}", push.dir));
                    self.trade = Some(Trade::new(Bet::Fade, push.dir, level, slot, oracle));
                    self.push = None;
                } else if slot > push.at + PUSH_WAIT_SLOTS {
                    self.push = None;
                }
            } else if flow.abs() >= FLOW_UNITS && slot >= self.rest_until {
                let dir = flow.signum();
                // Its stop: well back from where it bought.
                let stop = mid * (1.0 - dir * 0.006);
                log(&format!("slot {slot}: follow the informed {flow:+.3}"));
                self.trade = Some(Trade::new(Bet::Follow, dir, stop, slot, oracle));
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
