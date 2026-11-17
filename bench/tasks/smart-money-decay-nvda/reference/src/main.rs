//! The reference as a process bot, and the ladder's rungs: `MTX_NAIVE`
//! picks the rung (the reference by default), and `MTX_FLOW_*` override its
//! knobs for calibration.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    use mtx_bot_smart_money_decay::{Flow, Mode, Params};
    let mode = std::env::var("MTX_NAIVE")
        .ok()
        .and_then(|m| Mode::parse(&m))
        .unwrap_or(Mode::Reference);
    let mut flow = Flow::new(mode);
    let knob = |name: &str| std::env::var(format!("MTX_FLOW_{name}")).ok();
    let mut p: Params = flow.params;
    if let Some(v) = knob("LOOK").and_then(|v| v.parse().ok()) {
        p.look_slots = v;
    }
    if let Some(v) = knob("HOLD").and_then(|v| v.parse().ok()) {
        p.hold_slots = v;
    }
    if let Some(v) = knob("THRESHOLD").and_then(|v| v.parse().ok()) {
        p.threshold = v;
    }
    if let Some(v) = knob("MIN_SCORE").and_then(|v| v.parse().ok()) {
        p.min_score_bps = v;
    }
    if let Some(v) = knob("MIN_WEIGHT").and_then(|v| v.parse().ok()) {
        p.min_weight = v;
    }
    if let Some(v) = knob("MOMENTUM").and_then(|v| v.parse().ok()) {
        p.momentum_bps = v;
    }
    flow.params = p;
    mtx_kit::run(flow)
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
