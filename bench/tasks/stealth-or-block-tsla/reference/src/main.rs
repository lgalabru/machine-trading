//! The reference as a process bot, and the ladder's rungs: `MTX_NAIVE`
//! picks the rung (the reference by default), and `MTX_FLOW_*` override its
//! knobs for calibration.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    use mtx_bot_stealth_or_block::{Flow, Mode};
    let mode = std::env::var("MTX_NAIVE")
        .ok()
        .and_then(|m| Mode::parse(&m))
        .unwrap_or(Mode::Reference);
    let mut flow = Flow::new(mode);
    flow.params
        .knobs(|name| std::env::var(format!("MTX_FLOW_{name}")).ok());
    mtx_kit::run(flow)
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
