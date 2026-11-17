//! The informed bot as a process: the strategy in `lib.rs`, on `mtx-kit`'s
//! run loop. RFQs reach components only, so the grader runs `bot.wasm`.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_rfq_client_tiering_informed::Bot::default())
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
