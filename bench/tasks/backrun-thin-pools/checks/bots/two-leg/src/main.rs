//! The backrun-thin-pools two-leg rung as a process, for tools that start one.
//! It runs as a strategy component: amm sessions reach only components.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_backrun_thin_pools_two_leg::TwoLeg::default())
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
