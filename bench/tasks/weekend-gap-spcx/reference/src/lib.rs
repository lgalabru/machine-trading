//! Reference: fade the part of the weekend's move the reopen takes back,
//! leaning the way the late positioners trade, and hold it into the reopen.
//!
//! Over a weekend Phoenix's makers re-price SPCX's perp with almost no
//! trade behind them, and the reopen keeps about four fifths of where they
//! took it (the instruction's numbers, from 21 weekends). So the reference
//! reads two things:
//!
//! - **The weekend's move:** the makers' mid against where it started
//!   (Friday's last external price). The fifth it expects back is the
//!   prior.
//! - **The positioners' net flow:** every transaction on SPCX's market
//!   account and its signer (`tape.rs`). The wallets that first trade in
//!   the last 40 slots before the reopen are the ones positioning into it;
//!   the weekend's prints come from wallets it has seen all weekend. Their
//!   positions, summed, are one vote on which way the reopen goes. It can't
//!   tell the informed from the decoys, so it trusts the vote as one right
//!   65% of the time: a hedge, not a fit.
//!
//! A few slots before the reopen, after the makers' last re-price, it takes
//! the gap it then expects, sized by how big that is, and holds it through
//! the reopen to the end, where the mark values it.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution) and a process (`src/main.rs`), which the ladder runs as rungs
//! (`MTX_RUNG`): `reference` (the default); `follow`, the cap the weekend's
//! way into the reopen; `fade`, the cap against it.

pub mod reopen;
pub mod tape;

use mtx_kit::{log, prelude::*, venue::PhoenixVenue};
use reopen::{
    ACCOUNTS_MAX, Account, Clock, OPEN_RESIDUAL_SD_PCT, Phase, Positions, REOPEN_KEEP,
    expected_gap_given_votes, position_for, trade_toward,
};
use tape::Tape;

/// The positioners trade from this many slots before the reopen: the
/// instruction's number.
const POSITIONING_SLOTS: u64 = 40;
/// How often it takes the positioners' net flow to be right about the
/// reopen's direction: the informed are right 80% of the time, but half the
/// late wallets are decoys, and it can't tell which.
const FLOW_HIT_RATE: f64 = 0.65;
/// A net position smaller than this, in SPCX, is no vote.
const FLOW_MIN: f64 = 0.0005;

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
    symbol: String,
    venue: Option<PhoenixVenue>,
    positions: Positions,
    tape: Tape,
}

impl Strategy for Weekend {
    fn start(&mut self, session: &Session) -> BotResult {
        assert!(session.cap > 0.0);
        self.clock = Some(Clock::from_session(session)?);
        self.cap = session.cap;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        self.positions = Positions::new(&venue)?;
        self.venue = Some(venue);
        self.symbol = session.symbol.clone();
        let own: mtx_kit::Pubkey = session
            .authority
            .parse()
            .map_err(|e| format!("the session's authority: {e}"))?;
        self.tape = Tape::new(own);
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
        let phase = clock.phase(market.slot);
        if self.rung == Rung::Reference && phase != Phase::Hold {
            self.read_tape()?;
        }
        if phase == Phase::Entry {
            let weekend_pct = (mid / start_mid - 1.0) * 100.0;
            let vote = match self.rung {
                Rung::Reference => self.positioners_vote(clock)?,
                Rung::Follow | Rung::Fade => 0.0,
            };
            self.target = self.decide(weekend_pct, vote);
        }
        assert!(self.target.abs() <= self.cap);
        trade_toward(orders, held, self.target, mid);
        Ok(())
    }
}

impl Weekend {
    fn read_tape(&mut self) -> Result<(), String> {
        let venue = self.venue.as_ref().ok_or("step before start")?;
        let market = venue.market(&self.symbol)?.market_pubkey.clone();
        self.tape.read(&market)
    }

    /// The late positioners' summed position: +1 net long, -1 net short, 0
    /// none.
    fn positioners_vote(&self, clock: Clock) -> Result<f64, String> {
        let venue = self.venue.as_ref().ok_or("step before start")?;
        let from = clock.reopen_slot.saturating_sub(POSITIONING_SLOTS);
        let late = self.tape.late(from, ACCOUNTS_MAX);
        if late.is_empty() {
            return Ok(0.0);
        }
        let net: f64 = self
            .positions
            .read(venue, &self.symbol, &late)?
            .iter()
            .sum();
        Ok(if net.abs() < FLOW_MIN {
            0.0
        } else {
            net.signum()
        })
    }

    /// The position into the reopen, for a weekend that moved the makers'
    /// price `weekend_pct` from where it started, and the positioners' vote.
    fn decide(&self, weekend_pct: f64, vote: f64) -> f64 {
        assert!(vote.abs() <= 1.0);
        let way = weekend_pct.signum();
        match self.rung {
            Rung::Reference => position_for(
                expected_gap_given_votes(
                    weekend_pct,
                    REOPEN_KEEP,
                    OPEN_RESIDUAL_SD_PCT,
                    vote,
                    FLOW_HIT_RATE,
                ),
                self.cap,
            ),
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
        // Up 1%, no positioners: the reopen is expected 0.22% lower, most
        // of a full short.
        assert!((reference.decide(1.0, 0.0) + 70.0 * 0.22 / 0.25).abs() < 1e-6);
        // Down 2%: a full long.
        assert_eq!(reference.decide(-2.0, 0.0), 70.0);
        // A quiet weekend: nothing worth the entry.
        assert_eq!(reference.decide(0.2, 0.0), 0.0);
        // The positioners buying a flat weekend: a long.
        assert!(reference.decide(0.0, 1.0) > 0.0);
        // Selling into a fade: a bigger short.
        assert!(reference.decide(0.5, -1.0) < reference.decide(0.5, 0.0));
    }

    #[test]
    fn the_rungs_follow_and_fade_at_the_cap() {
        assert_eq!(bot(Rung::Follow).decide(1.0, 0.0), 70.0);
        assert_eq!(bot(Rung::Fade).decide(1.0, 0.0), -70.0);
        assert_eq!(bot(Rung::Fade).decide(-0.1, 0.0), 0.0);
    }
}
