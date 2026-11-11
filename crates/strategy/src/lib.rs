//! A trading strategy as a WebAssembly component the mtx grader calls
//! (`docs/rfd/0002-wasm-strategies.md`).
//!
//! Write a [`Strategy`], export it with [`export!`], and build the crate as a
//! `cdylib` for `wasm32-wasip2`: the output is a component the grader loads.
//!
//! ```ignore
//! use mtx_strategy::{Intent, Market, Strategy};
//!
//! #[derive(Default)]
//! struct AlwaysFlat;
//!
//! impl Strategy for AlwaysFlat {
//!     fn step(&mut self, _market: &Market) -> Result<Vec<Intent>, String> {
//!         Ok(vec![])
//!     }
//! }
//!
//! mtx_strategy::export!(AlwaysFlat);
//! ```
//!
//! The grader calls [`Strategy::start`] once, [`Strategy::step`] every step,
//! and [`Strategy::end`] after the last. The world is paused during each
//! call: [`rpc`] reads the chain as it is at that step, and the call is
//! synchronous. What `step` returns is the step's order sheet, which the
//! grader cuts to the mandate's cap and sends, signed with the bot's key.

#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub mod bindings {
    wit_bindgen::generate!({
        world: "strategy",
        path: "wit",
        pub_export_macro: true,
        default_bindings_module: "mtx_strategy::bindings",
    });
}

pub use bindings::mtx::strategy::types::{Execution, Intent, Market, Position, Session, Side};

/// A line in the strategy's log, which the grader keeps beside the session.
pub fn log(line: &str) {
    bindings::log(line);
}

/// A read-only JSON-RPC call against the session's chain: `params` and the
/// result are JSON text. The grader answers it before it returns, from the
/// chain as it is at this step.
pub fn rpc(method: &str, params: &str) -> Result<String, String> {
    bindings::rpc(method, params)
}

impl Default for Execution {
    /// As `mtx_kit::Execution::default()`: slices of 10, 200 bps of
    /// slippage, trades of at least 1, no limit per step.
    fn default() -> Self {
        Self {
            max_per_step: f64::INFINITY,
            slice: 10.0,
            slippage_bps: 200.0,
            min_trade: 1.0,
        }
    }
}

/// A strategy: what the grader calls. Synchronous: it reads the chain
/// through [`rpc`], whose calls return before the world moves.
pub trait Strategy: Default {
    /// Once, before the first step.
    fn start(&mut self, session: &Session) -> Result<(), String> {
        let _ = session;
        Ok(())
    }

    /// Every step: the order sheet, from the market as it is now. An error
    /// sends nothing this step, and the session goes on.
    fn step(&mut self, market: &Market) -> Result<Vec<Intent>, String>;

    /// After the last step.
    fn end(&mut self, last: Option<&Market>) {
        let _ = last;
    }
}

/// Exports a [`Strategy`] (which must be `Default`) as the component's
/// `start`, `step` and `end`.
#[macro_export]
macro_rules! export {
    ($strategy:ty) => {
        struct MtxStrategyComponent;

        thread_local! {
            static MTX_STRATEGY: ::std::cell::RefCell<$strategy> =
                ::std::cell::RefCell::new(<$strategy as ::std::default::Default>::default());
        }

        impl $crate::bindings::Guest for MtxStrategyComponent {
            fn start(session: $crate::Session) -> ::std::result::Result<(), ::std::string::String> {
                MTX_STRATEGY.with(|s| $crate::Strategy::start(&mut *s.borrow_mut(), &session))
            }

            fn step(
                market: $crate::Market,
            ) -> ::std::result::Result<::std::vec::Vec<$crate::Intent>, ::std::string::String> {
                MTX_STRATEGY.with(|s| $crate::Strategy::step(&mut *s.borrow_mut(), &market))
            }

            fn end(last: ::std::option::Option<$crate::Market>) {
                MTX_STRATEGY.with(|s| $crate::Strategy::end(&mut *s.borrow_mut(), last.as_ref()))
            }
        }

        $crate::bindings::export!(MtxStrategyComponent with_types_in $crate::bindings);
    };
}
