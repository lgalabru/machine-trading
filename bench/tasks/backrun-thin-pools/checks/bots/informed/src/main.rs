//! The backrun-thin-pools informed bot as a process, for tools that start
//! one. Its anchors run as strategy components (`component_anchors` in
//! task.toml): amm sessions reach only components.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_backrun_thin_pools_informed::Informed::default())
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
