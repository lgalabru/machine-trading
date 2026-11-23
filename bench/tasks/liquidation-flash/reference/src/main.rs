//! The liquidation-flash reference as a process, for tools that start one.
//! Its anchors run as strategy components (`component_anchors` in
//! task.toml): lending sessions reach only components.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_liquidation_flash::Reference::default())
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
