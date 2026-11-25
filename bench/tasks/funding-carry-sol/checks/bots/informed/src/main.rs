//! The funding-carry-sol informed bot as a process bot: the strategy in
//! `lib.rs`, on `mtx-kit`'s run loop, for the ladder's informed rung.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_carry_sol_informed::Informed::default())
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
