//! The ladder's rungs that read nothing of the predator, picked by
//! `MTX_RUNG`: `dump`, `twap`, `random-twap`, `slow`, `tiny`, `patient` and
//! `passive` (see the reference's `Mode`).

use mtx_bot_front_run_aware_unwind::{Mode, Unwind};

fn main() -> mtx_kit::BotResult {
    let name = std::env::var("MTX_RUNG").map_err(|_| "set MTX_RUNG")?;
    let mode = Mode::parse(&name).ok_or_else(|| format!("unknown MTX_RUNG {name}"))?;
    mtx_kit::run(Unwind::new(mode))
}
