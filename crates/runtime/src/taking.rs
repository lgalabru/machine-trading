//! Reduce-only taking (`taking: "reduce_only"` on a mandate allowance,
//! `specs/mandate.md` rule 10), checked before a component's order goes
//! out: the host refuses one it can tell would take liquidity to open, add
//! to or flip the position ([`TakingGuard`]): a market or
//! immediate-or-cancel order, or a resting order priced through the touch
//! the strategy was shown. A refusal is counted and handed back to the
//! strategy as the order's status; the order just doesn't happen, so it is
//! no breach. What the guard can't tell before the trade, the mandate's
//! audit judges after it.
//!
//! Whether a limit order crosses is decided against the touch the
//! strategy's own orders don't make ([`Touch`]), when the host can read
//! one. Without it, a strategy with orders resting on a side is trusted on
//! that side (the touch may be its own), and the audit judges the fills.

use mtx_core::{Side, mandate::Allowance};
use mtx_kit::Action;

/// The best bid and ask on Phoenix's book that aren't the bot's own orders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Touch {
    pub bid: Option<f64>,
    pub ask: Option<f64>,
}

/// A component's orders, checked against reduce-only taking before they
/// go out, one turn's sheet at a time.
#[derive(Debug, Clone)]
pub struct TakingGuard {
    allowance: Allowance,
    /// The position as the turn's orders go out: the one the bot was shown,
    /// moved by the orders the guard let take, each counted as filled in
    /// full (a crossing limit may fill less, which only makes the next
    /// check stricter).
    position: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    /// Whether the touch is the other traders' (the session read it with
    /// the bot's orders cancelled); if not, a side the bot has orders
    /// resting on can't be told.
    net: bool,
    /// Whether the bot had orders resting on each side when the turn
    /// started: the touch may then be its own, if it isn't `net`.
    own_bids: bool,
    own_asks: bool,
    /// The touch the next turn is checked against, when the session read
    /// one (see [`TakingGuard::touch`]).
    next: Option<Touch>,
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
            net: false,
            own_bids: false,
            own_asks: false,
            next: None,
            refused: 0,
        })
    }

    /// The touch the session read for the coming turn.
    pub fn touch(&mut self, touch: Option<Touch>) {
        self.next = touch;
    }

    /// A turn starts: the market as the bot was shown it, with the touch
    /// the session read for it without the bot's own orders, if it could.
    pub fn begin(&mut self, market: &mtx_kit::Market) {
        self.position = market.position.size;
        self.net = self.next.is_some();
        let touch = self.next.take();
        self.best_bid = touch.map_or(market.best_bid, |t| t.bid);
        self.best_ask = touch.map_or(market.best_ask, |t| t.ask);
        self.own_bids = market.resting_bids > 0.0;
        self.own_asks = market.resting_asks > 0.0;
    }

    /// Checks the turn's next order on the allowance's market: `Err` with
    /// why, if it would take to open, add to or flip the position. An order
    /// flagged reduce-only goes: the venue applies the flag at execution.
    pub fn check(&mut self, action: &Action, reduce_only: bool) -> Result<(), String> {
        let (side, size) = match *action {
            Action::Market { side, size, .. } => (side, size),
            Action::Limit { side, price, size } => {
                let (crosses, own) = match side {
                    mtx_kit::Side::Bid => {
                        (self.best_ask.is_some_and(|a| price >= a), self.own_asks)
                    }
                    mtx_kit::Side::Ask => {
                        (self.best_bid.is_some_and(|b| price <= b), self.own_bids)
                    }
                };
                if !crosses || (own && !self.net) {
                    return Ok(());
                }
                (side, size)
            }
            // A post-only order never takes, and the rest trade no book the
            // guard watches (a swap's or a reserve's limits are the
            // mandate's audit's). Named, so a new action is decided here.
            Action::PostOnly { .. }
            | Action::CancelAll
            | Action::Cancel { .. }
            | Action::Respond { .. }
            | Action::Swap { .. }
            | Action::AddLiquidity { .. }
            | Action::RemoveLiquidity { .. }
            | Action::Deposit { .. }
            | Action::Withdraw { .. }
            | Action::Borrow { .. }
            | Action::Repay { .. }
            | Action::Liquidate { .. }
            | Action::Flash { .. }
            | Action::PushPrice => return Ok(()),
        };
        let side = match side {
            mtx_kit::Side::Bid => Side::Buy,
            mtx_kit::Side::Ask => Side::Sell,
        };
        let toward_zero = |position: f64| match side {
            Side::Buy => (position + size).min(0.0).max(position),
            Side::Sell => (position - size).max(0.0).min(position),
        };
        let before = self.position;
        if reduce_only {
            self.position = toward_zero(self.position);
            debug_assert!(
                self.position.abs() <= before.abs(),
                "reducing never grows the position"
            );
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
        self.position = toward_zero(self.position);
        debug_assert!(
            self.position.abs() <= before.abs(),
            "a take let through only reduces"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mtx_core::mandate::Taking;

    use super::*;

    fn guard(position: f64) -> TakingGuard {
        let mut allowance =
            mtx_core::Mandate::perp("phoenix", "SPCX", 20.0, 250.0).universe[0].clone();
        allowance.taking = Some(Taking::ReduceOnly);
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
        // A touch that may be the bot's own (its trader id isn't known
        // yet): left to the audit.
        let mut g = guard(0.0);
        g.own_asks = true;
        assert_eq!(g.check(&limit(Bid, 100.1), false), Ok(()));
        assert_eq!(g.check(&Action::CancelAll, false), Ok(()));
    }

    /// A guard for a bot with orders resting on both sides, whose book is
    /// `touch` (the other traders') once its orders are left out.
    fn two_sided(position: f64, touch: Touch) -> TakingGuard {
        let mut g = guard(position);
        g.touch(Some(touch));
        g.begin(&mtx_kit::Market {
            position: mtx_kit::Position {
                size: position,
                entry_usd: 0.0,
            },
            // The bot's own quotes are the book's touch, and rest on both
            // sides: the counters the guard used to trust.
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            resting_bids: 5.0,
            resting_asks: 5.0,
            ..mtx_kit::Market::default()
        });
        g
    }

    fn limit(side: mtx_kit::Side, price: f64) -> Action {
        Action::Limit {
            side,
            price,
            size: 1.0,
        }
    }

    const OTHERS: Touch = Touch {
        bid: Some(99.0),
        ask: Some(101.0),
    };

    #[test]
    fn two_crossing_reduces_that_together_flip_are_caught() {
        use mtx_kit::Side::Ask;
        let sell = |size| Action::Limit {
            side: Ask,
            price: 99.0,
            size,
        };
        let mut g = guard(3.0);
        // Each is a reduce from where the bot stood; together they flip.
        assert_eq!(g.check(&sell(2.0), false), Ok(()));
        assert!(g.check(&sell(2.0), false).is_err());
        assert_eq!(g.refused, 1);
        assert_eq!(g.check(&sell(1.0), false), Ok(()));
    }

    #[test]
    fn a_two_sided_maker_is_refused_a_crossing_limit_that_opens() {
        use mtx_kit::Side::{Ask, Bid};
        let mut g = two_sided(0.0, OTHERS);
        // Through the other traders' ask, however much it has resting.
        assert!(g.check(&limit(Bid, 101.0), false).is_err());
        assert!(g.check(&limit(Ask, 99.0), false).is_err());
        assert_eq!(g.refused, 2);
        // Priced between its own quotes and the others' touch, it rests;
        // and a post-only order never takes.
        assert_eq!(g.check(&limit(Bid, 100.5), false), Ok(()));
        assert_eq!(g.check(&limit(Ask, 100.5), false), Ok(()));
        let post = Action::PostOnly {
            side: Bid,
            price: 105.0,
            size: 1.0,
        };
        assert_eq!(g.check(&post, false), Ok(()));
        assert_eq!(g.refused, 2);
        // A market order is refused as before.
        assert!(g.check(&market(Bid, 1.0), false).is_err());
    }

    #[test]
    fn a_crossing_limit_that_reduces_is_allowed() {
        use mtx_kit::Side::{Ask, Bid};
        let mut g = two_sided(2.0, OTHERS);
        assert_eq!(g.check(&limit(Ask, 99.0), false), Ok(()));
        // The position it was shown moved with the guard's own, the limit
        // counted as filled: long 1 now, so selling 2 would flip, selling 1
        // flattens, and crossing to sell again would open.
        assert!(g.check(&market(Ask, 2.0), false).is_err());
        assert_eq!(g.check(&market(Ask, 1.0), false), Ok(()));
        assert!(g.check(&limit(Ask, 99.0), false).is_err());
        let mut g = two_sided(-2.0, OTHERS);
        assert_eq!(g.check(&limit(Bid, 101.5), false), Ok(()));
        assert_eq!(g.refused, 0);
    }

    #[test]
    fn a_sheet_that_cancels_first_and_then_crosses_is_judged_the_same() {
        use mtx_kit::Side::{Ask, Bid};
        let mut g = two_sided(0.0, OTHERS);
        assert_eq!(g.check(&Action::CancelAll, false), Ok(()));
        assert!(g.check(&limit(Bid, 101.0), false).is_err());
        assert!(g.check(&limit(Ask, 99.0), false).is_err());
        // Its own quotes are no touch to cross: a bid up to its own ask,
        // below the others', rests or matches itself.
        assert_eq!(g.check(&limit(Bid, 100.5), false), Ok(()));
        // No other trader on a side: nothing to cross.
        let mut g = two_sided(
            0.0,
            Touch {
                bid: Some(99.0),
                ask: None,
            },
        );
        assert_eq!(g.check(&Action::CancelAll, false), Ok(()));
        assert_eq!(g.check(&limit(Bid, 150.0), false), Ok(()));
        assert_eq!(g.refused, 0);
    }

    #[test]
    fn a_touch_the_session_read_serves_one_turn() {
        use mtx_kit::Side::Bid;
        let mut g = two_sided(0.0, OTHERS);
        // The next turn, with no touch read: back to the market's, where
        // the bot's resting orders make a crossing unsure.
        g.begin(&mtx_kit::Market {
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            resting_bids: 5.0,
            resting_asks: 5.0,
            ..mtx_kit::Market::default()
        });
        assert_eq!(g.check(&limit(Bid, 100.1), false), Ok(()));
        assert_eq!(g.refused, 0);
    }
}
