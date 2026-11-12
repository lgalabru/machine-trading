//! A [`Strategy`](crate::Strategy) as a WebAssembly component the grader
//! calls (feature `component`; `docs/rfd/0002-wasm-strategies.md`). The world
//! is `wit/strategy.wit`, `mtx:strategy@0.2.0`; the grader still loads a
//! component built on 0.1.0 (`wit/0.1.0/strategy.wit`).
//!
//! Build the strategy's crate as a `cdylib` for `wasm32-wasip2`, with
//! `mtx-kit` at `default-features = false, features = ["component"]`, and
//! export the strategy with [`export!`](crate::export). The output is a
//! component; name it `bot.wasm` in the bot's directory.

#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub mod bindings {
    wit_bindgen::generate!({
        world: "strategy",
        path: "wit/strategy.wit",
        pub_export_macro: true,
        default_bindings_module: "mtx_kit::component::bindings",
    });
}

use bindings::mtx::strategy::types as wit;
use mtx_core::{mandate::Kind, turns};

use crate::{
    Action, Book, Execution, Intent, LineOptions, Market, OwnFill, Position, Rfq, Role, SendState,
    SendStatus, Session, Side, TimeInForce, TxOptions, VenueInfo, VenueView,
};

/// The session, as the grader hands it to `start`.
#[doc(hidden)]
pub fn session(s: wit::Session) -> Result<Session, String> {
    Ok(Session {
        steps: s.steps,
        slots_per_step: s.slots_per_step,
        start_slot: s.start_slot,
        end_slot: s.end_slot,
        symbol: s.symbol,
        start_mark: s.start_mark,
        start_oracle: s.start_oracle,
        cap: s.cap,
        max_drawdown: s.max_drawdown,
        mandate: serde_json::from_str(&s.mandate).map_err(|e| format!("the mandate: {e}"))?,
        venue: s.venue,
        authority: s.authority,
        vars: s.vars,
        seed: s.seed,
        venues: s
            .venues
            .into_iter()
            .map(|v| VenueInfo {
                id: v.id,
                kind: match v.kind {
                    wit::MarketKind::Perp => Kind::Perp,
                    wit::MarketKind::Spot => Kind::Spot,
                },
                tradable: v.tradable,
            })
            .collect(),
        applies_tx_options: s.applies_tx_options,
    })
}

/// The market, as the grader hands it to `step`. The venues' tapes and
/// books are also [`Market::tape`] and [`Market::depth`], as a process gets
/// them.
#[doc(hidden)]
pub fn market(m: wit::Market) -> Market {
    let venues: Vec<VenueView> = m.venues.into_iter().map(venue_view).collect();
    let name = |id: &str| id.split(':').next().unwrap_or_default().to_string();
    Market {
        k: m.k,
        slot: m.slot,
        last: m.last,
        steps_left: m.steps_left,
        slots_left: m.slots_left,
        mark: m.mark,
        oracle: m.oracle,
        index: m.index,
        best_bid: m.best_bid,
        best_ask: m.best_ask,
        position: position(m.position),
        cap: m.cap,
        max_drawdown: m.max_drawdown,
        resting_bids: m.resting_bids,
        resting_asks: m.resting_asks,
        pnl: m.pnl,
        peak_pnl: m.peak_pnl,
        drawdown: m.drawdown,
        tape: venues
            .iter()
            .flat_map(|v| v.tape.iter().flatten().cloned())
            .collect(),
        depth: venues
            .iter()
            .filter_map(|v| {
                let book = v.depth.as_ref()?;
                Some(turns::BookLevels {
                    venue: name(&v.id),
                    bids: book.bids.clone(),
                    asks: book.asks.clone(),
                })
            })
            .collect(),
        unix_timestamp: m.unix_timestamp,
        venues,
        sends: m.sends.into_iter().map(send_status).collect(),
        fills: m
            .fills
            .into_iter()
            .map(|f| OwnFill {
                venue: f.venue,
                slot: f.slot,
                side: kit_side(f.side),
                price: f.price,
                size: f.size,
                role: role(f.role),
            })
            .collect(),
        rfqs: m
            .rfqs
            .into_iter()
            .map(|r| Rfq {
                id: r.id,
                venue: r.venue,
                side: kit_side(r.side),
                size: r.size,
                deadline_slot: r.deadline_slot,
            })
            .collect(),
        notes: m.notes,
    }
}

fn position(p: wit::Position) -> Position {
    Position {
        size: p.size,
        entry_usd: p.entry_usd,
    }
}

fn role(role: wit::Role) -> Role {
    match role {
        wit::Role::Taker => Role::Taker,
        wit::Role::Maker => Role::Maker,
    }
}

fn venue_view(v: wit::VenueView) -> VenueView {
    let name = v.id.split(':').next().unwrap_or_default().to_string();
    let levels = |levels: Vec<wit::Level>| {
        levels
            .into_iter()
            .map(|l| turns::Level {
                price: l.price,
                base: l.size,
            })
            .collect()
    };
    VenueView {
        best_bid: v.best_bid,
        best_ask: v.best_ask,
        depth: v.depth.map(|book| Book {
            bids: levels(book.bids),
            asks: levels(book.asks),
        }),
        tape: v.tape.map(|tape| {
            tape.into_iter()
                .map(|t| turns::Trade {
                    venue: name.clone(),
                    side: match t.side {
                        wit::Side::Bid => mtx_core::Side::Buy,
                        wit::Side::Ask => mtx_core::Side::Sell,
                    },
                    price: t.price,
                    base: t.size,
                    slot: t.slot,
                    taker: t.taker,
                    maker: t.maker,
                    own: t.role.map(role),
                })
                .collect()
        }),
        position: v.position.map(position),
        id: v.id,
    }
}

fn send_status(s: wit::SendStatus) -> SendStatus {
    SendStatus {
        step: s.step,
        line: s.line,
        venue: s.venue,
        order: match s.order {
            wit::Order::Market(o) => Action::Market {
                side: kit_side(o.side),
                size: o.size,
                limit: o.limit,
            },
            wit::Order::Limit(o) => match o.time_in_force {
                wit::TimeInForce::Gtc => Action::Limit {
                    side: kit_side(o.side),
                    price: o.price,
                    size: o.size,
                },
                wit::TimeInForce::PostOnly => Action::PostOnly {
                    side: kit_side(o.side),
                    price: o.price,
                    size: o.size,
                },
                wit::TimeInForce::Ioc => Action::Market {
                    side: kit_side(o.side),
                    size: o.size,
                    limit: o.price,
                },
            },
            wit::Order::CancelAll => Action::CancelAll,
            wit::Order::Respond(r) => Action::Respond {
                id: r.id,
                price: r.price,
                size: r.size,
            },
        },
        state: match s.state {
            wit::SendState::Landed(signature) => SendState::Landed(signature),
            wit::SendState::Failed(why) => SendState::Failed(why),
            wit::SendState::NotSent(why) => SendState::NotSent(why),
            wit::SendState::Delayed => SendState::Delayed,
            wit::SendState::Expired => SendState::Expired,
        },
    }
}

fn kit_side(side: wit::Side) -> Side {
    match side {
        wit::Side::Bid => Side::Bid,
        wit::Side::Ask => Side::Ask,
    }
}

fn side(side: Side) -> wit::Side {
    match side {
        Side::Bid => wit::Side::Bid,
        Side::Ask => wit::Side::Ask,
    }
}

/// A line of the sheet, as the grader takes it from `step`.
#[doc(hidden)]
pub fn intent((intent, options): (Intent, LineOptions)) -> wit::Intent {
    let time_in_force = match options.time_in_force {
        TimeInForce::Gtc => wit::TimeInForce::Gtc,
        TimeInForce::PostOnly => wit::TimeInForce::PostOnly,
        TimeInForce::Ioc => wit::TimeInForce::Ioc,
    };
    let action = match intent {
        Intent::Target {
            position,
            execution:
                Execution {
                    max_per_step,
                    slice,
                    slippage_bps,
                    min_trade,
                },
        } => wit::Action::Target(wit::Target {
            position,
            execution: wit::Execution {
                max_per_step,
                slice,
                slippage_bps,
                min_trade,
                participation: options.participation,
            },
        }),
        Intent::Quote { bid, ask, size } => wit::Action::Quote(wit::Quote {
            bid,
            ask,
            size,
            post_only: options.time_in_force == TimeInForce::PostOnly,
        }),
        Intent::Market {
            side: s,
            size,
            limit,
        } => wit::Action::Market(wit::MarketOrder {
            side: side(s),
            size,
            limit,
        }),
        Intent::Limit {
            side: s,
            price,
            size,
        } => wit::Action::Limit(wit::LimitOrder {
            side: side(s),
            price,
            size,
            time_in_force,
        }),
        Intent::CancelAll => wit::Action::CancelAll,
        Intent::Respond { id, price, size } => {
            wit::Action::Respond(wit::RfqResponse { id, price, size })
        }
    };
    wit::Intent {
        action,
        venue: options.venue,
        reduce_only: options.reduce_only,
        tx: options.tx.map(
            |TxOptions {
                 cu_limit,
                 cu_price,
                 tip,
                 bundle,
             }| wit::TxOptions {
                cu_limit,
                cu_price,
                tip,
                bundle,
            },
        ),
    }
}

pub use crate::log;

/// Exports a [`Strategy`](crate::Strategy) that is `Default` as the
/// component's `start`, `step` and `end`.
#[macro_export]
macro_rules! export {
    ($strategy:ty) => {
        struct MtxStrategyComponent;

        thread_local! {
            static MTX_STRATEGY: ::std::cell::RefCell<$strategy> =
                ::std::cell::RefCell::new(<$strategy as ::std::default::Default>::default());
        }

        impl $crate::component::bindings::Guest for MtxStrategyComponent {
            fn start(
                session: $crate::component::bindings::mtx::strategy::types::Session,
            ) -> ::std::result::Result<(), ::std::string::String> {
                let session = $crate::component::session(session)?;
                MTX_STRATEGY.with(|s| {
                    $crate::Strategy::start(&mut *s.borrow_mut(), &session).map_err(|e| e.to_string())
                })
            }

            fn step(
                market: $crate::component::bindings::mtx::strategy::types::Market,
            ) -> ::std::result::Result<
                ::std::vec::Vec<$crate::component::bindings::mtx::strategy::types::Intent>,
                ::std::string::String,
            > {
                let market = $crate::component::market(market);
                let mut orders = $crate::Orders::new();
                MTX_STRATEGY.with(|s| {
                    $crate::Strategy::step(&mut *s.borrow_mut(), &market, &mut orders)
                        .map_err(|e| e.to_string())
                })?;
                Ok(orders.into_lines().into_iter().map($crate::component::intent).collect())
            }

            fn end(last: ::std::option::Option<$crate::component::bindings::mtx::strategy::types::Market>) {
                let last = last.map($crate::component::market);
                MTX_STRATEGY.with(|s| $crate::Strategy::end(&mut *s.borrow_mut(), last.as_ref()))
            }
        }

        $crate::component::bindings::export!(MtxStrategyComponent with_types_in $crate::component::bindings);
    };
}
