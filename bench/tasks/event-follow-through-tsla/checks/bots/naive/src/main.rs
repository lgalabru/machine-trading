//! The ladder's rungs that don't read who trades, picked by `MTX_NAIVE`:
//!
//! - `ride`: once the jump is done, the cap the jump's way, until six slots
//!   after the verdict.
//! - `fade`: the cap against it, the same.
//! - `hold`: the cap long from slot 260, before the headline's window,
//!   through the print and the verdict, out six slots after it.
//! - `chase`: once the jump is done, follows the first move of the oracle
//!   past 0.2% from where it settled (a drift, or the verdict), and holds
//!   it 150 slots.
//! - `net-flow`: once the jump is done, follows the net buying of every
//!   account in the session over the last 30 slots, past 2.4 TSLA either
//!   way, until six slots after the verdict.

use mtx_bot_event_follow_through::{FLOW_SLOTS, FLOW_UNITS, Headline, Hold, Watch, go_to, mid};
use mtx_kit::{log, prelude::*, run, venue::PhoenixVenue};

const HOLD_FROM: u64 = 260;
const CHASE_PCT: f64 = 0.002;
const CHASE_SLOTS: u64 = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ride,
    Fade,
    Hold,
    Chase,
    NetFlow,
}

struct Naive {
    mode: Mode,
    start_slot: u64,
    watch: Option<Watch>,
    headline: Headline,
    hold: Hold,
    chased: Option<u64>,
}

impl Strategy for Naive {
    fn start(&mut self, session: &Session) -> BotResult {
        self.start_slot = session.start_slot;
        if self.mode == Mode::NetFlow {
            let venue = PhoenixVenue::from_json(&session.venue)?;
            let own = venue
                .trader_account(&mtx_kit::venue::key(&session.authority)?)
                .to_string();
            let accounts = Watch::every_trader(&own)?;
            self.watch = Some(Watch::new(&venue, &session.symbol, accounts, FLOW_SLOTS + 20)?);
        }
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let at = market.slot.saturating_sub(self.start_slot);
        orders.cancel_all();
        if let Some(watch) = &mut self.watch {
            watch.read(at)?;
        }
        self.headline.update(at, market.oracle);
        let open = self.headline.verdict.is_none();
        match self.mode {
            Mode::Ride | Mode::Fade => {
                if let (Some(_), Some((_, side)), true) =
                    (self.headline.settled, self.headline.jumped, open)
                {
                    let dir = if self.mode == Mode::Ride { side } else { -side };
                    self.hold.want(dir, at);
                }
                self.hold.manage(at, &self.headline);
            }
            Mode::Hold => {
                if at >= HOLD_FROM && open {
                    self.hold.want(1.0, at);
                }
                self.hold.manage(at, &self.headline);
            }
            Mode::Chase => {
                if let (Some((_, level)), None) = (self.headline.settled, self.chased) {
                    let moved = market.oracle / level - 1.0;
                    if moved.abs() > CHASE_PCT {
                        self.hold.want(moved.signum(), at);
                        self.chased = Some(at);
                    }
                }
                if let Some(from) = self.chased
                    && at >= from + CHASE_SLOTS
                    && !self.hold.done
                {
                    log(&format!("slot {at}: out after the chase"));
                    self.hold.dir = 0.0;
                    self.hold.done = true;
                }
            }
            Mode::NetFlow => {
                if self.headline.settled.is_some() && open {
                    let net: f64 = self
                        .watch
                        .as_ref()
                        .map_or(0.0, |w| w.changes(self.headline.flow_slots(at)).iter().sum());
                    if net.abs() >= 2.0 * FLOW_UNITS {
                        self.hold.want(net.signum(), at);
                    }
                }
                self.hold.manage(at, &self.headline);
            }
        }
        if mid(market).is_some() {
            go_to(self.hold.target(market), market, orders);
        }
        Ok(())
    }
}

fn main() -> BotResult {
    let mode = match std::env::var("MTX_NAIVE").as_deref() {
        Ok("ride") => Mode::Ride,
        Ok("fade") => Mode::Fade,
        Ok("hold") => Mode::Hold,
        Ok("chase") => Mode::Chase,
        Ok("net-flow") => Mode::NetFlow,
        other => {
            return Err(format!(
                "MTX_NAIVE must be ride, fade, hold, chase or net-flow, not {other:?}"
            )
            .into());
        }
    };
    run(Naive {
        mode,
        start_slot: 0,
        watch: None,
        headline: Headline::default(),
        hold: Hold::default(),
        chased: None,
    })
}
