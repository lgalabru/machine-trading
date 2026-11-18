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
//! trade (a raw send) the audit catches after it: every taker fill, judged
//! against the position just before it ([`takes`]), and a take the rule
//! forbids is a breach like any other.
//!
//! Whether a limit order crosses is decided against the touch the bot's own
//! orders don't make ([`Touch`]): at each turn's start the session simulates
//! a cancel of the bot's orders and reads Phoenix's best bid and offer
//! after it, its own liquidity included. A bot resting on both sides is
//! still refused when it sends a limit through the other traders' touch, and
//! one that cancels first and then crosses is judged the same. If the session
//! can't read that touch, a bot with orders resting on a side is trusted on
//! that side (the touch may be its own), and the audit judges the fills.
//!
//! A process bot has no sheet to check: the RPC guard simulates each
//! transaction it sends against the paused chain ([`TakeGate`]) and refuses
//! one whose fills would take to open, add to or flip the position.

use mtx_core::{
    Side,
    mandate::{Allowance, InstrumentRef, ObservedTake},
};
use mtx_kit::Action;

use crate::making::MakingFill;

/// The bot's taker fills among `fills` (every fill of its, in the order
/// the venue recorded them), each with the position in `instrument` just
/// before it, counted from `start_position`. A match between the bot's own
/// orders moves nothing and takes nothing ([`MakingFill::self_match`]), and a
/// forced close (the risk desk's) is no taking of the bot's.
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
    for fill in fills {
        if fill.role == "taker" && !fill.self_match && !fill.forced {
            out.push(ObservedTake {
                instrument: instrument.clone(),
                side: fill.side,
                quantity: fill.base,
                position_before: position,
            });
        }
        position += signed(fill);
    }
    out
}

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
            self.position = toward_zero(self.position);
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
        Ok(())
    }
}

/// A process bot's transactions, checked against reduce-only taking by the
/// RPC guard before they go out: each is simulated on the grader's endpoint
/// ahead of Phoenix's view of the bot's position, and one whose fills move
/// the position as a taker may not (the bot's own orders are never a fill, so
/// its transaction moves the position only by what it takes). A transaction
/// that fails its simulation goes through, to fail by itself; so does one the
/// grader couldn't simulate, which the audit then judges.
pub struct TakeGate {
    allowance: Allowance,
    phoenix: mtx_kit::Phoenix,
    symbol: String,
    authority: solana_pubkey::Pubkey,
    rpc_url: String,
    http: reqwest::Client,
    refused: std::sync::atomic::AtomicU64,
}

impl TakeGate {
    /// A gate for `allowance` on `symbol` of the Phoenix `exchange`, for
    /// the bot trading as `authority`, simulating on `rpc_url`, if the
    /// allowance limits taking.
    pub fn new(
        allowance: &Allowance,
        exchange: mtx_kit::phoenix_rise::types::exchange::ExchangeResponse,
        symbol: &str,
        authority: solana_pubkey::Pubkey,
        rpc_url: &str,
    ) -> Option<Self> {
        allowance.taking.map(|_| Self {
            allowance: allowance.clone(),
            phoenix: mtx_kit::Phoenix::new(exchange, rpc_url.to_string()),
            symbol: symbol.to_string(),
            authority,
            rpc_url: rpc_url.to_string(),
            http: reqwest::Client::new(),
            refused: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Transactions refused over the session.
    pub fn refused(&self) -> u64 {
        self.refused.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn position(&self, send: &[String], view: &str) -> Result<Option<f64>, String> {
        let bundle: Vec<String> = send.iter().cloned().chain([view.to_string()]).collect();
        let results =
            crate::raw::simulate_bundle(&self.http, &self.rpc_url, &bundle, &[], 0).await?;
        if results.iter().take(send.len()).any(|r| !r["err"].is_null()) {
            return Ok(None);
        }
        let result = results.get(send.len()).ok_or("no view")?;
        crate::raw::position_of(&self.phoenix, &self.symbol, result).map(Some)
    }

    /// Checks the transactions `wires` the bot sends together: `Err` with
    /// why, if they would take to open, add to or flip the position.
    pub async fn check(&self, wires: &[Vec<u8>]) -> Result<(), String> {
        use base64::Engine as _;
        let infra = |e: String| {
            tracing::warn!("the taking check couldn't simulate the send: {e}");
            Ok(())
        };
        let views = match crate::raw::view_transactions(
            &self.phoenix,
            &self.symbol,
            &self.authority,
            false,
        ) {
            Ok(views) => views,
            Err(e) => return infra(e),
        };
        let Some(view) = views.first() else {
            return Ok(());
        };
        let encoded: Vec<String> = wires
            .iter()
            .map(|wire| base64::engine::general_purpose::STANDARD.encode(wire))
            .collect();
        let before = match self.position(&[], view).await {
            Ok(Some(before)) => before,
            Ok(None) => return Ok(()),
            Err(e) => return infra(e),
        };
        let after = match self.position(&encoded, view).await {
            Ok(Some(after)) => after,
            Ok(None) => return Ok(()),
            Err(e) => return infra(e),
        };
        let judged = judge(&self.allowance, before, after);
        if judged.is_err() {
            self.refused
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        judged
    }
}

/// Whether a transaction that moved the position from `before` to `after`
/// took as `allowance` allows: it moved it only as a taker (the bot's own
/// resting orders fill later, by other traders' orders).
pub(crate) fn judge(allowance: &Allowance, before: f64, after: f64) -> Result<(), String> {
    let moved = after - before;
    if moved.abs() < 1e-9 {
        return Ok(());
    }
    let side = if moved > 0.0 { Side::Buy } else { Side::Sell };
    if allowance.may_take(before, side, moved.abs()) {
        return Ok(());
    }
    Err(format!(
        "refused: it would {} {} {} as a taker from a position of {before}, and the mandate \
         allows taking only to reduce (rest it post-only, or flag it reduce-only)",
        match side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        },
        moved.abs(),
        allowance.instrument,
    ))
}

impl crate::guard::SendGate for TakeGate {
    fn check<'a>(
        &'a self,
        wires: &'a [Vec<u8>],
    ) -> futures::future::BoxFuture<'a, Result<(), String>> {
        Box::pin(TakeGate::check(self, wires))
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
            forced: false,
            self_match: false,
        }
    }

    /// A jump's uncross fills a resting order in an override: no event, so
    /// the position moves with no fill behind it. It is recorded, so the
    /// audit counts the position as the bot held it.
    #[test]
    fn an_uncross_fill_reconciles_fills_with_the_position() {
        use crate::making::{UNCROSS, position_after, uncross_fill};
        let sold = fill(2, Side::Sell, "taker", 1.0);
        let mut all = vec![sold];
        assert_eq!(position_after(0.0, &all), -1.0);
        // The bot is at -0.329 after a bid filled at a jump: 0.671 bought
        // that no event shows.
        let uncross = uncross_fill(0.0, &all, -0.329, 3, 7, 100.0).expect("a fill");
        assert_eq!(uncross.side, Side::Buy);
        assert!((uncross.base - 0.671).abs() < 1e-9);
        assert_eq!((uncross.role, uncross.counterparty), ("maker", UNCROSS));
        all.push(uncross);
        assert!(uncross_fill(0.0, &all, -0.329, 3, 9, 100.0).is_none());
        // Under half a lot is rounding.
        assert!(uncross_fill(0.0, &all, -0.3294, 3, 9, 100.0).is_none());
        // The audit then counts a later take from -0.329, not from -1.
        all.push(fill(10, Side::Sell, "taker", 0.1));
        let takes = takes(&spcx(), 0.0, &all);
        assert_eq!(takes.len(), 2);
        assert!((takes[1].position_before + 0.329).abs() < 1e-9);
    }

    /// A take that lands after the jump in the same step is judged from the
    /// position the uncross left: the fill goes in at the uncross's slot,
    /// before it.
    #[test]
    fn a_take_after_the_jump_in_the_same_step_is_judged_after_the_uncross() {
        use crate::making::{Reconciled, reconcile};
        // Flat; a resting bid fills at the jump (S = 10) for +1; the bot sells
        // 1 as a taker in the next slot to reduce. The meter reads flat.
        let mut fills = vec![fill(11, Side::Sell, "taker", 1.0)];
        let (result, at) = reconcile(0.0, &mut fills, 0, 0.0, 3, Some(10), 100.0);
        assert_eq!((result, at), (Reconciled::Inserted, Some(0)));
        assert_eq!(fills[0].counterparty, crate::making::UNCROSS);
        assert_eq!((fills[0].at, fills[0].side), (10, Side::Buy));
        let takes = takes(&spcx(), 0.0, &fills);
        assert_eq!(takes.len(), 1);
        assert_eq!(takes[0].position_before, 1.0);
        let allowance: Allowance = serde_json::from_str(
            r#"{"instrument":"phoenix:perp:SPCX","max_position":20.0,"taking":"reduce_only"}"#,
        )
        .unwrap();
        assert!(allowance.may_take(takes[0].position_before, takes[0].side, takes[0].quantity));
        // Appended after, as before, the same take is judged from flat: a
        // sell that opens a short.
        let appended = [fills[1].clone(), fills[0].clone()];
        let wrong = super::takes(&spcx(), 0.0, &appended);
        assert_eq!(wrong[0].position_before, 0.0);
        assert!(!allowance.may_take(wrong[0].position_before, wrong[0].side, wrong[0].quantity));
        // A fill of the bot's before the uncross stays before it.
        let mut fills = vec![
            fill(5, Side::Sell, "taker", 1.0),
            fill(11, Side::Sell, "taker", 1.0),
        ];
        let (_, at) = reconcile(1.0, &mut fills, 0, 0.0, 3, Some(10), 100.0);
        assert_eq!(at, Some(1));
    }

    /// A leftover with no jump in the step isn't guessed at.
    #[test]
    fn a_leftover_with_no_jump_is_unexplained() {
        use crate::making::{Reconciled, reconcile};
        let mut fills = vec![fill(11, Side::Sell, "taker", 1.0)];
        let (result, at) = reconcile(0.0, &mut fills, 0, 0.0, 3, None, 100.0);
        assert_eq!((result, at), (Reconciled::Unexplained, None));
        assert_eq!(fills.len(), 1);
        let (clean, _) = reconcile(0.0, &mut fills, 0, -1.0, 3, None, 100.0);
        assert_eq!(clean, Reconciled::Clean);
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
        taker.self_match = true;
        let mut maker = fill(5, Side::Sell, "maker", 2.0);
        maker.counterparty = BOOK;
        maker.self_match = true;
        // A taker fill that merely sits beside a maker fill of the same
        // slot, price and size, but isn't the bot's own match, is a take.
        let beside = [
            fill(9, Side::Buy, "taker", 2.0),
            fill(9, Side::Sell, "maker", 2.0),
        ];
        assert_eq!(super::takes(&spcx(), 0.0, &beside).len(), 1);
        let fills = [taker, maker, fill(7, Side::Sell, "taker", 1.0)];
        let takes = takes(&spcx(), -4.0, &fills);
        assert_eq!(takes.len(), 1);
        assert_eq!(takes[0].side, Side::Sell);
        assert_eq!(takes[0].position_before, -4.0);
        // A maker fill alone is never a take.
        assert!(super::takes(&spcx(), 0.0, &[fill(1, Side::Buy, "maker", 9.0)]).is_empty());
    }

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
    fn a_transactions_fills_are_judged_by_the_position_they_move() {
        let allowance = guard(0.0).allowance;
        // Opens, adds, flips: refused.
        assert!(judge(&allowance, 0.0, 2.0).is_err());
        assert!(judge(&allowance, 3.0, 4.0).is_err());
        assert!(judge(&allowance, -1.0, -2.0).is_err());
        assert!(judge(&allowance, 2.0, -1.0).is_err());
        // Reduces, flattens, or moves nothing (a post-only order, a cancel,
        // a match against the bot's own orders): allowed.
        assert_eq!(judge(&allowance, 3.0, 1.0), Ok(()));
        assert_eq!(judge(&allowance, -3.0, 0.0), Ok(()));
        assert_eq!(judge(&allowance, 2.0, 2.0), Ok(()));
        assert_eq!(judge(&allowance, 0.0, 0.0), Ok(()));
    }

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
