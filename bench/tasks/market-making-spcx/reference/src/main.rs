//! The market-making reference as a process bot: the strategy in `lib.rs`, on `mtx-kit`'s
//! run loop. The anchor hidden sets are scored against runs this way.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_maker::Maker::default())
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
