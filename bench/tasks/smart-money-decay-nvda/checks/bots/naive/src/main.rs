//! The ladder's rungs, one picked by `MTX_NAIVE`, on the reference's
//! machinery:
//!
//! - `follow-flow`: the cap the way the market's net taker flow went over
//!   the last 40 slots, past a threshold;
//! - `fade-flow`: the cap against it;
//! - `momentum`: the cap the way the book moved over the last 40 slots, past
//!   8 bp;
//! - `most-active` (a naive reader): follows the wallet that has taken most
//!   often so far.
//!
//! `MTX_FLOW_THRESHOLD`, `MTX_FLOW_LOOK`, `MTX_FLOW_HOLD` and
//! `MTX_FLOW_MOMENTUM` move their knobs.

fn main() -> mtx_kit::BotResult {
    use mtx_bot_smart_money_decay::{Flow, Mode};
    let mode = std::env::var("MTX_NAIVE")
        .ok()
        .and_then(|m| Mode::parse(&m))
        .ok_or("MTX_NAIVE names no rung")?;
    let mut flow = Flow::new(mode);
    let knob = |name: &str| std::env::var(format!("MTX_FLOW_{name}")).ok();
    if let Some(v) = knob("LOOK").and_then(|v| v.parse().ok()) {
        flow.params.look_slots = v;
    }
    if let Some(v) = knob("HOLD").and_then(|v| v.parse().ok()) {
        flow.params.hold_slots = v;
    }
    if let Some(v) = knob("THRESHOLD").and_then(|v| v.parse().ok()) {
        flow.params.threshold = v;
    }
    if let Some(v) = knob("MOMENTUM").and_then(|v| v.parse().ok()) {
        flow.params.momentum_bps = v;
    }
    mtx_kit::run(flow)
}
