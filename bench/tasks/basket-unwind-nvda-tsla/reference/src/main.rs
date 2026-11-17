//! The basket-unwind reference as a process bot: the strategy in `lib.rs`,
//! on `mtx-kit`'s run loop. A process trades the session's primary market
//! only, so the task grades and anchors the component (`bot.wasm`).

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_basket_unwind_nvda::Unwind::default())
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
