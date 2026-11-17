//! The informed bot: told which of the menu's predators is seated, with its
//! window and threshold (`MTX_REGIME`), it sells at the maker's bid as fast
//! as keeps its selling over the window under the threshold, and leaves the
//! rest to the broker. It isn't told the path or when anyone trades.

use mtx_bot_front_run_aware_unwind::{Mode, Unwind};

fn main() -> mtx_kit::BotResult {
    mtx_kit::run(Unwind::new(Mode::Informed))
}
