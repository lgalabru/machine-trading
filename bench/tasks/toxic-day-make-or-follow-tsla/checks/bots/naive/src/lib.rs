//! The ladder's rungs that don't read whether the tape is informed, picked
//! by `MTX_NAIVE` (when the component is built, or when the process starts):
//!
//! - `flat`: never trades.
//! - `make`: makes all session, as the reference does on a quiet morning.
//! - `follow`: follows the takers' net flow all session, as the reference
//!   does on a toxic one.
//! - `switch`: follows the net flow once it is past twice the reference's
//!   bar, and makes between: a fixed threshold, the same every morning.

use mtx_bot_toxic_day_make_or_follow::{Desk, FLOW_UNITS, Policy};
use mtx_kit::prelude::*;

/// The rung named `name`.
pub fn policy(name: &str) -> Option<Policy> {
    match name {
        "flat" => Some(Policy::Flat),
        "make" => Some(Policy::Make),
        "follow" => Some(Policy::Follow(FLOW_UNITS)),
        "switch" => Some(Policy::Switch(2.0 * FLOW_UNITS)),
        _ => None,
    }
}

/// The rung the component was built as.
pub struct Naive(Desk);

impl Default for Naive {
    fn default() -> Self {
        let name = option_env!("MTX_NAIVE").unwrap_or("flat");
        Self(Desk::new(policy(name).unwrap_or(Policy::Flat)))
    }
}

impl Strategy for Naive {
    fn start(&mut self, session: &Session) -> BotResult {
        self.0.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.0.step(market, orders)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Naive);
