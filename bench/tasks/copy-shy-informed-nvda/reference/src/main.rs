//! The reference as a process bot, and the ladder's rungs: `MTX_RUNG` picks
//! the rung (the reference by default), and `MTX_COPY_*` override its knobs
//! for calibration.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    use mtx_bot_copy_shy_informed::{Copier, Mode};
    let mode = std::env::var("MTX_RUNG")
        .ok()
        .and_then(|m| Mode::parse(&m))
        .unwrap_or(Mode::Reference);
    let mut copier = Copier::new(mode);
    let knob = |name: &str| std::env::var(format!("MTX_COPY_{name}")).ok();
    let p = &mut copier.params;
    if let Some(v) = knob("SHARE").and_then(|v| v.parse().ok()) {
        p.share = v;
    }
    if let Some(v) = knob("LAG").and_then(|v| v.parse().ok()) {
        p.lag_slots = v;
    }
    if let Some(v) = knob("HOLD").and_then(|v| v.parse().ok()) {
        p.hold_slots = v;
    }
    if let Some(v) = knob("MIN_SCORE").and_then(|v| v.parse().ok()) {
        p.min_score_bps = v;
    }
    if let Some(v) = knob("THRESHOLD").and_then(|v| v.parse().ok()) {
        p.threshold = v;
    }
    mtx_kit::run(copier)
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
