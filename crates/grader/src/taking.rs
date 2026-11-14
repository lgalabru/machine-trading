//! Reduce-only taking (`taking: "reduce_only"` on a mandate allowance,
//! `specs/mandate.md` rule 10) in a market-making session, which tells the
//! bot's taker fills from its maker fills ([`MakingFill`]).
//!
//! Two layers. Before a component's order goes out, the grader refuses one
//! it can tell would take liquidity to open, add to or flip the position
//! ([`TakingGuard`]): a market or immediate-or-cancel order, or a resting
//! order priced through the touch the bot was shown. A refusal is logged,
//! counted and handed back to the bot as the order's status; the order just
//! doesn't happen, so it is no breach. What the guard can't tell before the
//! trade (a process bot's transactions, a raw send, a cross against a touch
//! that may have been the bot's own) the audit catches after it: every taker
//! fill, judged against the position just before it ([`takes`]), and a take
//! the rule forbids is a breach like any other.

use mtx_core::{
    Side,
    mandate::{Allowance, InstrumentRef, ObservedTake},
};
use mtx_kit::Action;

use crate::making::MakingFill;

/// The bot's taker fills among `fills` (every fill of its, in the order
/// the venue recorded them), each with the position in `instrument` just
/// before it, counted from `start_position`. A match between the bot's own
/// orders moves nothing and takes nothing.
pub fn takes(
    instrument: &InstrumentRef,
    start_position: f64,
    fills: &[MakingFill],
) -> Vec<ObservedTake> {
    let signed = |fill: &MakingFill| match fill.side {
        Side::Buy => fill.base,
        Side::Sell => -fill.base,
    };
    let mut position = start_position;
    let mut out = Vec::new();
    let mut i = 0;
    while i < fills.len() {
        let fill = &fills[i];
        if fill.role == "taker"
            && let Some(next) = fills.get(i + 1)
            && is_self_match(fill, next)
        {
            i += 2;
            continue;
        }
        if fill.role == "taker" {
            out.push(ObservedTake {
                instrument: instrument.clone(),
                side: fill.side,
                quantity: fill.base,
                position_before: position,
            });
        }
        position += signed(fill);
        i += 1;
    }
    out
}

/// Whether `maker` is the other half of `taker`: the bot on both sides of
/// one match, which [`MakingFill::of`] records taker first.
fn is_self_match(taker: &MakingFill, maker: &MakingFill) -> bool {
    maker.role == "maker"
        && maker.at == taker.at
        && maker.side != taker.side
        && maker.price == taker.price
        && maker.base == taker.base
}

/// A component's orders, checked against reduce-only taking before they
/// go out, one turn's sheet at a time.
#[derive(Debug, Clone)]
pub struct TakingGuard {
    allowance: Allowance,
    /// The position as the turn's orders go out: the one the bot was shown,
    /// moved by the orders the guard let take.
    position: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    /// Whether the bot had orders resting on each side when the turn
    /// started: the touch may then be its own.
    own_bids: bool,
    own_asks: bool,
    /// Orders refused over the session.
    pub refused: u64,
}

impl TakingGuard {
    /// A guard for `allowance`, if it limits taking.
    pub fn new(allowance: &Allowance) -> Option<Self> {
        allowance.taking.map(|_| Self {
            allowance: allowance.clone(),
            position: 0.0,
            best_bid: None,
            best_ask: None,
            own_bids: false,
            own_asks: false,
            refused: 0,
        })
    }

    /// A turn starts: the market as the bot was shown it.
    pub fn begin(&mut self, market: &mtx_kit::Market) {
        self.position = market.position.size;
        self.best_bid = market.best_bid;
        self.best_ask = market.best_ask;
        self.own_bids = market.resting_bids > 0.0;
        self.own_asks = market.resting_asks > 0.0;
    }

    /// Checks the turn's next order on the allowance's market: `Err` with
    /// why, if it would take to open, add to or flip the position. An order
    /// flagged reduce-only goes: the venue applies the flag at execution.
    pub fn check(&mut self, action: &Action, reduce_only: bool) -> Result<(), String> {
        let (side, size, sure) = match *action {
            Action::Market { side, size, .. } => (side, size, true),
            Action::Limit { side, price, size } => {
                let (crosses, own) = match side {
                    mtx_kit::Side::Bid => {
                        (self.best_ask.is_some_and(|a| price >= a), self.own_asks)
                    }
                    mtx_kit::Side::Ask => {
                        (self.best_bid.is_some_and(|b| price <= b), self.own_bids)
                    }
                };
                if !crosses || own {
                    return Ok(());
                }
                (side, size, false)
            }
            _ => return Ok(()),
        };
        let side = match side {
            mtx_kit::Side::Bid => Side::Buy,
            mtx_kit::Side::Ask => Side::Sell,
        };
        let toward_zero = |position: f64| match side {
            Side::Buy => (position + size).min(0.0).max(position),
            Side::Sell => (position - size).max(0.0).min(position),
        };
        if reduce_only {
            if sure {
                self.position = toward_zero(self.position);
            }
            return Ok(());
        }
        if !self.allowance.may_take(self.position, side, size) {
            self.refused += 1;
            return Err(format!(
                "refused: it would {} {size} {} as a taker from a position of {}, and the mandate \
                 allows taking only to reduce (rest it post-only, or flag it reduce-only)",
                match side {
                    Side::Buy => "buy",
                    Side::Sell => "sell",
                },
                self.allowance.instrument,
                self.position,
            ));
        }
        if sure {
            self.position = toward_zero(self.position);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mtx_core::mandate::Taking;

    use super::*;
    use crate::making::{BOOK, COMPETITOR, TAKER};

    fn spcx() -> InstrumentRef {
        "phoenix:perp:SPCX".parse().unwrap()
    }

    fn fill(at: u64, side: Side, role: &'static str, base: f64) -> MakingFill {
        MakingFill {
            at,
            side,
            role,
            counterparty: if role == "taker" { COMPETITOR } else { TAKER },
            price: 100.0,
            base,
            fee_usd: 0.0,
        }
    }

    #[test]
    fn takes_carry_the_position_each_found() {
        // Rests and is hit: long 3. Takes 2 to sell (reducing), then buys
        // 4 as a taker from 1 (adding).
        let fills = [
            fill(2, Side::Buy, "maker", 3.0),
            fill(4, Side::Sell, "taker", 2.0),
            fill(6, Side::Buy, "taker", 4.0),
            fill(8, Side::Sell, "maker", 1.0),
        ];
        let takes = takes(&spcx(), 0.0, &fills);
        let seen: Vec<_> = takes
            .iter()
            .map(|t| (t.side, t.quantity, t.position_before))
            .collect();
        assert_eq!(seen, [(Side::Sell, 2.0, 3.0), (Side::Buy, 4.0, 1.0)]);
        // The audit's rule: the first reduces, the second adds.
        let allowance: Allowance = serde_json::from_str(
            r#"{"instrument":"phoenix:perp:SPCX","max_position":20.0,"taking":"reduce_only"}"#,
        )
        .unwrap();
        assert!(allowance.may_take(3.0, Side::Sell, 2.0));
        assert!(!allowance.may_take(1.0, Side::Buy, 4.0));
    }

    #[test]
    fn takes_count_from_the_start_position_and_skip_self_matches() {
        let mut taker = fill(5, Side::Buy, "taker", 2.0);
        taker.counterparty = BOOK;
        let mut maker = fill(5, Side::Sell, "maker", 2.0);
        maker.counterparty = BOOK;
        let fills = [taker, maker, fill(7, Side::Sell, "taker", 1.0)];
        let takes = takes(&spcx(), -4.0, &fills);
        assert_eq!(takes.len(), 1);
        assert_eq!(takes[0].side, Side::Sell);
        assert_eq!(takes[0].position_before, -4.0);
        // A maker fill alone is never a take.
        assert!(super::takes(&spcx(), 0.0, &[fill(1, Side::Buy, "maker", 9.0)]).is_empty());
    }

    fn guard(position: f64) -> TakingGuard {
        let allowance = Allowance {
            taking: Some(Taking::ReduceOnly),
            ..mtx_core::Mandate::perp("phoenix", "SPCX", 20.0, 250.0).universe[0].clone()
        };
        let mut guard = TakingGuard::new(&allowance).unwrap();
        guard.position = position;
        guard.best_bid = Some(99.9);
        guard.best_ask = Some(100.1);
        guard
    }

    fn market(side: mtx_kit::Side, size: f64) -> Action {
        Action::Market {
            side,
            size,
            limit: 0.0,
        }
    }

    #[test]
    fn the_guard_refuses_a_market_order_that_opens_adds_or_flips() {
        use mtx_kit::Side::{Ask, Bid};
        let mut g = guard(0.0);
        assert!(g.check(&market(Bid, 1.0), false).is_err());
        let mut g = guard(3.0);
        assert!(g.check(&market(Bid, 1.0), false).is_err());
        assert!(g.check(&market(Ask, 4.0), false).is_err());
        assert_eq!(g.refused, 2);
        // Reducing is allowed, and moves the position the next order sees:
        // selling 3 flattens, so selling 1 more would open.
        assert_eq!(g.check(&market(Ask, 3.0), false), Ok(()));
        assert!(g.check(&market(Ask, 1.0), false).is_err());
        // Flagged reduce-only, the venue clips it: it goes.
        assert_eq!(g.check(&market(Ask, 5.0), true), Ok(()));
        assert_eq!(g.refused, 3);
        // Without a taking rule there's no guard.
        let free = mtx_core::Mandate::perp("phoenix", "SPCX", 20.0, 250.0);
        assert!(TakingGuard::new(&free.universe[0]).is_none());
    }

    #[test]
    fn the_guard_refuses_a_resting_order_priced_through_the_touch() {
        use mtx_kit::Side::{Ask, Bid};
        let limit = |side, price| Action::Limit {
            side,
            price,
            size: 1.0,
        };
        let mut g = guard(0.0);
        assert!(g.check(&limit(Bid, 100.1), false).is_err());
        assert!(g.check(&limit(Ask, 99.0), false).is_err());
        // Inside the spread, or post-only: it rests.
        assert_eq!(g.check(&limit(Bid, 100.0), false), Ok(()));
        let post = Action::PostOnly {
            side: Bid,
            price: 101.0,
            size: 1.0,
        };
        assert_eq!(g.check(&post, false), Ok(()));
        // Crossing to reduce a short is allowed.
        let mut g = guard(-2.0);
        assert_eq!(g.check(&limit(Bid, 100.2), false), Ok(()));
        // A touch that may be the bot's own: left to the audit.
        let mut g = guard(0.0);
        g.own_asks = true;
        assert_eq!(g.check(&limit(Bid, 100.1), false), Ok(()));
        assert_eq!(g.check(&Action::CancelAll, false), Ok(()));
    }
}
