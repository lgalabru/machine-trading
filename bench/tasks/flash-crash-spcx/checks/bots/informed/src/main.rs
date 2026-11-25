//! The flash-crash-spcx informed bot as a process: the strategy in `lib.rs`, on
//! `mtx-kit`'s run loop, for the ladder and tools that start one.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_flash_crash_spcx_informed::Informed::default())
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
