//! Reference: fade the part of the weekend's move the reopen takes back,
//! and hold it into the reopen.
//!
//! Over a weekend Phoenix's makers re-price SPCX's perp with almost no
//! trade behind them, and the reopen keeps about four fifths of where they
//! took it (the instruction's numbers, from 21 weekends). So the reference
//! reads one thing: the makers' mid against where it started (Friday's last
//! external price). A few slots before the reopen, after the makers' last
//! re-price, it takes a position against the fifth it expects back, sized
//! by how big that is, and holds it through the reopen to the end, where
//! the mark values it. It doesn't read who trades.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution) and a process (`src/main.rs`), which the ladder runs as rungs
//! (`MTX_RUNG`): `reference` (the default); `follow`, the cap the weekend's
//! way into the reopen; `fade`, the cap against it.

pub mod reopen;

use mtx_kit::{log, prelude::*};
use reopen::{Account, Clock, Phase, expected_gap_pct, position_for, trade_toward};

/// Under this weekend move, in percent, the `follow` and `fade` rungs sit
/// out: there's no move to follow or fade.
const RUNG_MOVE_MIN_PCT: f64 = 0.3;

/// Which bot the process runs as.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Rung {
    #[default]
    Reference,
    Follow,
    Fade,
}

#[derive(Default)]
pub struct Weekend {
    clock: Option<Clock>,
    cap: f64,
    rung: Rung,
    /// The makers' mid at the first step: Friday's last external price.
    start_mid: Option<f64>,
    target: f64,
    account: Account,
}

impl Strategy for Weekend {
    fn start(&mut self, session: &Session) -> BotResult {
        assert!(session.cap > 0.0);
        self.clock = Some(Clock::from_session(session)?);
        self.cap = session.cap;
        self.rung = match session.var("MTX_RUNG") {
            Some("follow") => Rung::Follow,
            Some("fade") => Rung::Fade,
            _ => Rung::Reference,
        };
        log(&format!("weekend: rung {:?}", self.rung));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let clock = self.clock.ok_or("step before start")?;
        let (Some(bid), Some(ask)) = (market.best_bid, market.best_ask) else {
            return Ok(());
        };
        assert!(bid > 0.0 && ask >= bid);
        let mid = 0.5 * (bid + ask);
        let start_mid = *self.start_mid.get_or_insert(mid);
        let held = market.position.size;
        if self.account.step(market.mark, held) {
            trade_toward(orders, held, 0.0, mid);
            return Ok(());
        }
        if clock.phase(market.slot) == Phase::Entry {
            let weekend_pct = (mid / start_mid - 1.0) * 100.0;
            self.target = self.decide(weekend_pct);
        }
        assert!(self.target.abs() <= self.cap);
        trade_toward(orders, held, self.target, mid);
        Ok(())
    }
}

impl Weekend {
    /// The position into the reopen, for a weekend that moved the makers'
    /// price `weekend_pct` from where it started.
    fn decide(&self, weekend_pct: f64) -> f64 {
        let way = weekend_pct.signum();
        match self.rung {
            Rung::Reference => position_for(expected_gap_pct(weekend_pct), self.cap),
            Rung::Follow if weekend_pct.abs() >= RUNG_MOVE_MIN_PCT => way * self.cap,
            Rung::Fade if weekend_pct.abs() >= RUNG_MOVE_MIN_PCT => -way * self.cap,
            Rung::Follow | Rung::Fade => 0.0,
        }
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Weekend);

#[cfg(test)]
mod tests {
    use super::*;

    fn bot(rung: Rung) -> Weekend {
        Weekend {
            cap: 70.0,
            rung,
            ..Weekend::default()
        }
    }

    #[test]
    fn the_reference_fades_what_the_reopen_takes_back() {
        let reference = bot(Rung::Reference);
        // Up 1%: the reopen is expected 0.22% lower, most of a full short.
        assert!((reference.decide(1.0) + 70.0 * 0.22 / 0.25).abs() < 1e-9);
        // Down 2%: a full long.
        assert_eq!(reference.decide(-2.0), 70.0);
        // A quiet weekend: nothing worth the entry.
        assert_eq!(reference.decide(0.2), 0.0);
    }

    #[test]
    fn the_rungs_follow_and_fade_at_the_cap() {
        assert_eq!(bot(Rung::Follow).decide(1.0), 70.0);
        assert_eq!(bot(Rung::Fade).decide(1.0), -70.0);
        assert_eq!(bot(Rung::Fade).decide(-0.1), 0.0);
    }
}
