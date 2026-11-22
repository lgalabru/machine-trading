//! The flow maker, watching an exchange: it quotes off Phoenix's book like
//! `bots/book-maker`, pulls the side one-sided takers hit like
//! `bots/flow-maker`, and pulls the side an exchange's move runs into
//! before Phoenix's book follows.
//!
//! SOL's price is made on the exchanges. On mtx-alpha, Binance's 1-second
//! returns lined up best with Phoenix's half a second later, and the book
//! maker's fills that came right after Binance moved against them (43% of
//! them) marked out at -1.38 bp [-2.09, -0.81], against -0.63 for fills
//! after no move and -0.12 for fills after a move their way. Those fills
//! are arbitrageurs taking quotes the exchange has already left behind.
//!
//! So when the reference venue ([`REFERENCE`], an exchange's book the live
//! runtime shows in `market.venues`, configured as `control.references`)
//! has moved [`LEAD_BPS`] or more over the last [`LEAD_WINDOW_SLOTS`], the
//! side the move runs into is pulled (the asks when the exchange rose) and
//! held [`LEAD_HOLD_SLOTS`]: about the time Phoenix takes to catch up.
//! Without a reference (none configured, or its stream stale) it quotes as
//! the flow maker.
//!
//! Everything it does is `mtx_kit::making`'s, composed here.

use mtx_kit::{
    log,
    making::{
        Guard, GuardConfig, Lead, Momentum, Pull, Requote, Sizing, TouchConfig, View, lead_call,
        place, quotes, taker_flow, touch_targets, venue_mid,
    },
    prelude::*,
    venue::PhoenixVenue,
};

/// The quote's size, as a share of the position cap.
const QUOTE_SHARE_OF_CAP: f64 = 0.5;

/// The momentum window, in steps (2 slots each: about 2.4 s).
const MOMENTUM_STEPS: usize = 3;

/// Requote at least this often even if nothing moved.
const REQUOTE_SLOTS: u64 = 20;

/// Phoenix's SOL base lot.
const SIZE_MIN: f64 = 0.001;

/// The takers' guard, as the flow maker's (`bots/flow-maker`).
const FLOW_WINDOW_SLOTS: u64 = 8;
const FLOW_GUARD: GuardConfig = GuardConfig {
    threshold: 0.6,
    volume_min: 1.0,
    hold_slots: 8,
};

/// The reference: Coinbase's SOL-USD book, as the runtime names it.
const REFERENCE: &str = "coinbase:spot:SOL-USD";

/// The exchange's move that pulls a side: a tick of SOL ($0.01 at $69 is
/// 1.45 bp) over 2 seconds (5 slots), the window the measurement used.
const LEAD_WINDOW_SLOTS: u64 = 5;
const LEAD_BPS: f64 = 1.0;

/// How long a pull holds after the exchange last moved: 3 slots, about
/// 1.2 s, past the half second Phoenix takes to follow.
const LEAD_HOLD_SLOTS: u64 = 3;

/// The strategy.
#[derive(Default)]
pub struct LeadMaker {
    tick: f64,
    sizing: Option<Sizing>,
    momentum: Momentum,
    requote: Option<Requote>,
    flow: Option<Guard>,
    lead: Lead,
    leading: Option<Guard>,
    pulled: Option<(Pull, Pull)>,
}

impl Strategy for LeadMaker {
    fn start(&mut self, session: &Session) -> BotResult {
        let cap = session.mandate.universe[0].max_position;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?;
        self.tick = venue.ticks_to_usd(market, 1);
        if !(self.tick > 0.0 && cap > 0.0) {
            return Err(format!("a tick of {} and a cap of {cap}", self.tick).into());
        }
        let sizing = Sizing {
            size: cap * QUOTE_SHARE_OF_CAP,
            cap,
            size_min: SIZE_MIN,
        };
        self.sizing = Some(sizing);
        self.momentum = Momentum::new(MOMENTUM_STEPS);
        self.requote = Some(Requote::new(REQUOTE_SLOTS));
        self.flow = Some(Guard::new(FLOW_GUARD));
        self.lead = Lead::new(LEAD_WINDOW_SLOTS);
        self.leading = Some(Guard::new(GuardConfig {
            threshold: 1.0,
            volume_min: 0.0,
            hold_slots: LEAD_HOLD_SLOTS,
        }));
        log(&format!(
            "quoting off the book, minding the takers and {REFERENCE}: tick {}, cap {cap}, \
             pull on a {LEAD_BPS} bp move over {LEAD_WINDOW_SLOTS} slots, for {LEAD_HOLD_SLOTS}",
            self.tick
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (Some(sizing), Some(requote), Some(flow), Some(leading)) = (
            self.sizing,
            self.requote.as_mut(),
            self.flow.as_mut(),
            self.leading.as_mut(),
        ) else {
            return Err("stepped before it started".into());
        };
        let (Some(best_bid), Some(best_ask)) = (market.best_bid, market.best_ask) else {
            return Ok(());
        };
        if !(best_bid > 0.0 && best_ask > best_bid) {
            return Ok(());
        }
        let held = market.position.size;
        let view = View {
            best_bid,
            best_ask,
            oracle: market.oracle,
            inventory: (held / sizing.cap).clamp(-1.0, 1.0),
            momentum_bps: self.momentum.push((best_bid + best_ask) / 2.0),
        };
        let by_flow = flow.update(
            market.slot,
            taker_flow(&market.tape, market.slot, FLOW_WINDOW_SLOTS),
        );
        let moved = self
            .lead
            .push(market.slot, venue_mid(&market.venues, REFERENCE));
        let by_lead = leading.hold(market.slot, lead_call(moved, LEAD_BPS));
        if self.pulled != Some((by_flow, by_lead)) {
            log(&format!(
                "slot {}: takers {by_flow:?}, {REFERENCE} {by_lead:?} ({})",
                market.slot,
                moved.map_or("no reference".into(), |m| format!("{m:+.2} bp")),
            ));
            self.pulled = Some((by_flow, by_lead));
        }
        let targets = touch_targets(&view, self.tick, &TouchConfig::default())
            .without(by_flow)
            .without(by_lead);
        let quotes = quotes(targets, held, sizing);
        if requote.due(market.slot, quotes) {
            place(orders, quotes, self.tick);
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(LeadMaker);

#[cfg(test)]
mod tests {
    use mtx_kit::{Intent, Side as OrderSide, VenueView, prelude::Strategy as _};
    use proptest::prelude::*;

    use super::*;

    const TICK: f64 = 0.01;

    fn maker() -> LeadMaker {
        LeadMaker {
            tick: TICK,
            sizing: Some(Sizing {
                size: 0.05,
                cap: 0.1,
                size_min: SIZE_MIN,
            }),
            momentum: Momentum::new(MOMENTUM_STEPS),
            requote: Some(Requote::new(REQUOTE_SLOTS)),
            flow: Some(Guard::new(FLOW_GUARD)),
            lead: Lead::new(LEAD_WINDOW_SLOTS),
            leading: Some(Guard::new(GuardConfig {
                threshold: 1.0,
                volume_min: 0.0,
                hold_slots: LEAD_HOLD_SLOTS,
            })),
            pulled: None,
        }
    }

    fn market(slot: u64, reference: Option<f64>) -> Market {
        Market {
            slot,
            best_bid: Some(69.00),
            best_ask: Some(69.01),
            oracle: 69.005,
            venues: reference
                .map(|mid| VenueView {
                    id: REFERENCE.into(),
                    best_bid: Some(mid - 0.005),
                    best_ask: Some(mid + 0.005),
                    ..Default::default()
                })
                .into_iter()
                .collect(),
            ..Market::default()
        }
    }

    fn sides(maker: &mut LeadMaker, market: &Market) -> Vec<OrderSide> {
        let mut orders = Orders::default();
        maker.step(market, &mut orders).unwrap();
        orders
            .intents()
            .iter()
            .filter_map(|i| match i {
                Intent::Limit { side, .. } => Some(*side),
                _ => None,
            })
            .collect()
    }

    /// The exchange rising a tick pulls the asks, falling pulls the bids,
    /// and both come back once the hold is over.
    #[test]
    fn an_exchange_move_pulls_the_side_it_runs_into() {
        let mut m = maker();
        for slot in 100..106 {
            sides(&mut m, &market(slot, Some(69.00)));
        }
        assert_eq!(
            sides(&mut m, &market(106, Some(69.02))),
            vec![OrderSide::Bid]
        );
        let mut m = maker();
        for slot in 100..106 {
            sides(&mut m, &market(slot, Some(69.00)));
        }
        assert_eq!(
            sides(&mut m, &market(106, Some(68.98))),
            vec![OrderSide::Ask]
        );
        // Flat after: the pull holds 3 slots, then both sides come back.
        for slot in 107..109 {
            sides(&mut m, &market(slot, Some(68.98)));
        }
        let back = sides(&mut m, &market(115, Some(68.98)));
        assert_eq!(back.len(), 2, "{back:?}");
    }

    /// No reference: it quotes as the flow maker, both sides.
    #[test]
    fn without_a_reference_it_quotes_both_sides() {
        assert_eq!(sides(&mut maker(), &market(100, None)).len(), 2);
    }

    proptest! {
        /// Whatever the exchange does, it never quotes across Phoenix's
        /// book and never sends anything but post-only limits and cancels.
        #[test]
        fn it_never_crosses_whatever_the_exchange_does(path in prop::collection::vec(prop::option::of(68.5f64..69.5), 1..30)) {
            let mut m = maker();
            for (i, mid) in path.iter().enumerate() {
                let mut orders = Orders::default();
                m.step(&market(100 + i as u64, *mid), &mut orders).unwrap();
                for intent in orders.intents() {
                    match intent {
                        Intent::Limit { side: OrderSide::Bid, price, .. } => prop_assert!(*price < 69.01 - 1e-9),
                        Intent::Limit { side: OrderSide::Ask, price, .. } => prop_assert!(*price > 69.00 + 1e-9),
                        Intent::CancelAll => {}
                        other => prop_assert!(false, "it sent {other:?}"),
                    }
                }
            }
        }
    }
}
