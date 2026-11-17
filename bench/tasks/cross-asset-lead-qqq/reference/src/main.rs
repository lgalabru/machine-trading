//! The reference as a process bot, and the ladder's rungs: `MTX_NAIVE`
//! picks the rung (the reference by default), and `MTX_FLOW_*` override its
//! knobs for calibration.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_bot_cross_asset_lead::main_with(None)
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
