//! A [`Strategy`](crate::Strategy) as a WebAssembly component the grader
//! calls (feature `component`; `docs/rfd/0002-wasm-strategies.md`). The world
//! is `wit/strategy.wit`.
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
        path: "wit",
        pub_export_macro: true,
        default_bindings_module: "mtx_kit::component::bindings",
    });
}

use bindings::mtx::strategy::types as wit;

use crate::{Execution, Intent, Market, Position, Session, Side};

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
        cap: s.cap,
        max_drawdown: s.max_drawdown,
        mandate: serde_json::from_str(&s.mandate).map_err(|e| format!("the mandate: {e}"))?,
        seed: s.seed,
    })
}

/// The market, as the grader hands it to `step`.
#[doc(hidden)]
pub fn market(m: wit::Market) -> Market {
    Market {
        k: m.k,
        slot: m.slot,
        last: m.last,
        steps_left: m.steps_left,
        slots_left: m.slots_left,
        mark: m.mark,
        oracle: m.oracle,
        position: Position {
            size: m.position.size,
            entry_usd: m.position.entry_usd,
        },
        cap: m.cap,
        max_drawdown: m.max_drawdown,
        resting_bids: m.resting_bids,
        resting_asks: m.resting_asks,
        pnl: m.pnl,
        peak_pnl: m.peak_pnl,
        drawdown: m.drawdown,
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
pub fn intent(intent: Intent) -> wit::Intent {
    match intent {
        Intent::Target {
            position,
            execution:
                Execution {
                    max_per_step,
                    slice,
                    slippage_bps,
                    min_trade,
                },
        } => wit::Intent::Target((
            position,
            wit::Execution {
                max_per_step,
                slice,
                slippage_bps,
                min_trade,
            },
        )),
        Intent::Quote { bid, ask, size } => wit::Intent::Quote((bid, ask, size)),
        Intent::Market {
            side: s,
            size,
            limit,
        } => wit::Intent::Market((side(s), size, limit)),
        Intent::Limit {
            side: s,
            price,
            size,
        } => wit::Intent::Limit((side(s), price, size)),
        Intent::CancelAll => wit::Intent::CancelAll,
    }
}

/// A line in the strategy's log, which the grader keeps beside the session.
pub fn log(line: &str) {
    bindings::log(line);
}

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
                Ok(orders.into_intents().into_iter().map($crate::component::intent).collect())
            }

            fn end(last: ::std::option::Option<$crate::component::bindings::mtx::strategy::types::Market>) {
                let last = last.map($crate::component::market);
                MTX_STRATEGY.with(|s| $crate::Strategy::end(&mut *s.borrow_mut(), last.as_ref()))
            }
        }

        $crate::component::bindings::export!(MtxStrategyComponent with_types_in $crate::component::bindings);
    };
}
