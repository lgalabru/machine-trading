//! The strategy as a process bot: `MTX_RUNG` picks the rung, the reference
//! by default.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    use mtx_bot_copy_or_bait::{Copier, Mode};
    let mode = std::env::var("MTX_RUNG")
        .ok()
        .and_then(|m| Mode::parse(&m))
        .unwrap_or_else(Mode::built);
    mtx_kit::run(Copier::new(mode))
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
