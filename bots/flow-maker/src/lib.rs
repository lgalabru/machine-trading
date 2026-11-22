//! The book maker, minding the takers: it quotes off the book like
//! `bots/book-maker`, and stops quoting the side one-sided takers are
//! hitting.
//!
//! On mainnet's SOL perp, the book maker's paper fills earn the half-spread
//! (+0.7 bp) and lose about 1.6 bp to the move after them: takers who know
//! the next tick fill it just before the price moves. Their orders come
//! one-sided first. So this maker reads the market's trades each step
//! (`market.tape`), and when the takers' recent volume is mostly one way
//! ([`GUARD`]) it pulls the side they are hitting (the asks when they buy)
//! and holds it off for a few slots. The other side keeps quoting, and
//! sheds inventory into the move.
//!
//! The same idea won GPT-6 Astra's market-making bot its score on the
//! bench (0.12.0), where a trained model read the takers' flow before a
//! jump. That model learned the bench's generator; this guard takes only
//! the mechanism, with thresholds from mainnet's own history (`luthor
//! study`, see [`GUARD`]): it helps the few fills that follow one-sided
//! flow, which on SOL are few.
//!
//! Everything it does is `mtx_kit::making`'s, composed here: each block is
//! tested on its own there.

use mtx_kit::{
    log,
    making::{
        Guard, GuardConfig, Momentum, Pull, Requote, Sizing, TouchConfig, View, place, taker_flow,
        touch_targets,
    },
    prelude::*,
    venue::PhoenixVenue,
};

/// The quote's size, as a share of the position cap: two fills the same
/// way reach it.
const QUOTE_SHARE_OF_CAP: f64 = 0.5;

/// The momentum window, in steps (2 slots each: about 2.4 s).
const MOMENTUM_STEPS: usize = 3;

/// Requote at least this often even if nothing moved.
const REQUOTE_SLOTS: u64 = 20;

/// Phoenix's SOL base lot.
const SIZE_MIN: f64 = 0.001;

/// The takers' window, in slots: about 3 seconds.
const FLOW_WINDOW_SLOTS: u64 = 8;

/// When to pull a side: four fifths of at least 1 SOL of taker volume one
/// way, held 8 slots (about 3 seconds, a 10-second markout's third).
///
/// Measured with `luthor study` on 6,938 of the book maker's maker fills
/// on mainnet (2026-11-21/22, overnight): over 8 slots, fills after flow
/// against them marked out at -0.85 bp [-1.16, -0.54] against -0.55 bp
/// [-0.62, -0.48] for fills after no trade. The window separates them best
/// (3 slots has too few, 16 blurs them), the share is nearly always all
/// one way (SOL's trades are few and large, a median of 4.9 SOL), so the
/// threshold matters little and 1 SOL is a trade's size. The catch: only
/// 6% of fills follow any trade, so the guard moves the mean by about
/// 0.02 bp. Most bad fills are hit by the very trade that moves the price.
const GUARD: GuardConfig = GuardConfig {
    threshold: 0.6,
    volume_min: 1.0,
    hold_slots: 8,
};

/// The strategy.
#[derive(Default)]
pub struct FlowMaker {
    tick: f64,
    sizing: Option<Sizing>,
    momentum: Momentum,
    requote: Option<Requote>,
    guard: Option<Guard>,
    pulled: Option<Pull>,
}

impl Strategy for FlowMaker {
    fn start(&mut self, session: &Session) -> BotResult {
        let cap = session.mandate.universe[0].max_position;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?;
        self.tick = venue.ticks_to_usd(market, 1);
        if !(self.tick > 0.0 && cap > 0.0) {
            return Err(format!("a tick of {} and a cap of {cap}", self.tick).into());
        }
        if !session.reports.tape {
            log("this runtime reports no tape: quoting as the book maker, with no guard");
        }
        let sizing = Sizing {
            size: cap * QUOTE_SHARE_OF_CAP,
            cap,
            size_min: SIZE_MIN,
        };
        self.sizing = Some(sizing);
        self.momentum = Momentum::new(MOMENTUM_STEPS);
        self.requote = Some(Requote::new(REQUOTE_SLOTS));
        self.guard = Some(Guard::new(GUARD));
        log(&format!(
            "quoting off the book, minding the takers: tick {}, cap {cap}, size {}, \
             pull past {} of {} over {} slots, for {}",
            self.tick,
            sizing.size,
            GUARD.threshold,
            GUARD.volume_min,
            FLOW_WINDOW_SLOTS,
            GUARD.hold_slots
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (Some(sizing), Some(requote), Some(guard)) =
            (self.sizing, self.requote.as_mut(), self.guard.as_mut())
        else {
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
        let flow = taker_flow(&market.tape, market.slot, FLOW_WINDOW_SLOTS);
        let pull = guard.update(market.slot, flow);
        if self.pulled != Some(pull) {
            log(&format!(
                "slot {}: {} (takers {})",
                market.slot,
                match pull {
                    Pull::Neither => "quoting both sides",
                    Pull::Bids => "pulling the bids",
                    Pull::Asks => "pulling the asks",
                },
                flow.map_or("quiet".into(), |f| format!(
                    "{:+.2} of {:.2}",
                    f.share, f.volume
                )),
            ));
            self.pulled = Some(pull);
        }
        let targets = touch_targets(&view, self.tick, &TouchConfig::default()).without(pull);
        if requote.due(market.slot, targets) {
            place(orders, targets, self.tick, held, sizing);
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(FlowMaker);

#[cfg(test)]
mod tests {
    use mtx_kit::{
        Intent, Side as OrderSide,
        making::{TapeSide as Side, Trade},
        prelude::Strategy as _,
    };
    use proptest::prelude::*;

    use super::*;

    const TICK: f64 = 0.01;

    fn maker() -> FlowMaker {
        FlowMaker {
            tick: TICK,
            sizing: Some(Sizing {
                size: 0.05,
                cap: 0.1,
                size_min: SIZE_MIN,
            }),
            momentum: Momentum::new(MOMENTUM_STEPS),
            requote: Some(Requote::new(REQUOTE_SLOTS)),
            guard: Some(Guard::new(GUARD)),
            pulled: None,
        }
    }

    fn trade(slot: u64, side: Side, base: f64) -> Trade {
        Trade {
            venue: "phoenix".into(),
            market: Some("phoenix:perp:SOL".into()),
            side,
            price: 70.8,
            base,
            slot,
            taker: None,
            maker: None,
            own: None,
        }
    }

    fn market(slot: u64, tape: Vec<Trade>) -> Market {
        Market {
            slot,
            best_bid: Some(70.80),
            best_ask: Some(70.81),
            oracle: 70.805,
            tape,
            ..Market::default()
        }
    }

    /// The limit orders a step sent: (side, price, size).
    fn limits(maker: &mut FlowMaker, market: &Market) -> Vec<(OrderSide, f64, f64)> {
        let mut orders = Orders::default();
        maker.step(market, &mut orders).unwrap();
        orders
            .intents()
            .iter()
            .filter_map(|i| match i {
                Intent::Limit { side, price, size } => Some((*side, *price, *size)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_quiet_tape_quotes_both_sides_at_the_touch() {
        let sent = limits(&mut maker(), &market(100, Vec::new()));
        assert_eq!(sent.len(), 2);
        assert!(
            sent.iter()
                .any(|(s, p, _)| *s == OrderSide::Bid && (p - 70.80).abs() < 1e-9)
        );
        assert!(
            sent.iter()
                .any(|(s, p, _)| *s == OrderSide::Ask && (p - 70.81).abs() < 1e-9)
        );
    }

    #[test]
    fn takers_buying_pull_the_asks_and_selling_the_bids() {
        let buying = vec![trade(98, Side::Buy, 2.0), trade(99, Side::Buy, 1.0)];
        let sent = limits(&mut maker(), &market(100, buying));
        assert_eq!(
            sent.iter().map(|s| s.0).collect::<Vec<_>>(),
            vec![OrderSide::Bid]
        );
        let selling = vec![trade(99, Side::Sell, 3.0)];
        let sent = limits(&mut maker(), &market(100, selling));
        assert_eq!(
            sent.iter().map(|s| s.0).collect::<Vec<_>>(),
            vec![OrderSide::Ask]
        );
    }

    #[test]
    fn a_pull_holds_then_both_sides_come_back() {
        let mut m = maker();
        limits(&mut m, &market(100, vec![trade(99, Side::Buy, 3.0)]));
        // Still held, nothing new on the tape: no requote (targets unchanged).
        assert!(limits(&mut m, &market(102, Vec::new())).is_empty());
        let back = limits(&mut m, &market(100 + GUARD.hold_slots, Vec::new()));
        assert_eq!(back.len(), 2);
    }

    #[test]
    fn a_trickle_or_mixed_flow_pulls_nothing() {
        let trickle = vec![trade(99, Side::Buy, 0.2)];
        assert_eq!(limits(&mut maker(), &market(100, trickle)).len(), 2);
        let mixed = vec![trade(98, Side::Buy, 2.0), trade(99, Side::Sell, 1.5)];
        assert_eq!(limits(&mut maker(), &market(100, mixed)).len(), 2);
    }

    proptest! {
        /// Whatever the takers did and whatever it holds, it never quotes
        /// across the book, never adds past the cap, and never sends a
        /// market order.
        #[test]
        fn it_never_crosses_takes_or_overfills(
            trades in prop::collection::vec((90u64..101, any::<bool>(), 0.001f64..20.0), 0..30),
            held in -0.12f64..0.12,
        ) {
            let tape = trades.into_iter().map(|(slot, buy, base)| trade(slot, if buy { Side::Buy } else { Side::Sell }, base)).collect();
            let mut m = maker();
            let mut market = market(100, tape);
            market.position.size = held;
            let mut orders = Orders::default();
            m.step(&market, &mut orders).unwrap();
            for intent in orders.intents() {
                match intent {
                    Intent::Limit { side: OrderSide::Bid, price, size } => {
                        prop_assert!(*price < 70.81 - 1e-9);
                        prop_assert!(held + size <= 0.1 + 1e-9);
                    }
                    Intent::Limit { side: OrderSide::Ask, price, size } => {
                        prop_assert!(*price > 70.80 + 1e-9);
                        prop_assert!(held - size >= -0.1 - 1e-9);
                    }
                    Intent::CancelAll => {}
                    other => prop_assert!(false, "it sent {other:?}"),
                }
            }
        }
    }
}
