//! The rung as a process, for tools that start one; the ladder grades its
//! component (lending sessions reach only components).

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_liquidation_flash_min_tip::MinTip::default())
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
