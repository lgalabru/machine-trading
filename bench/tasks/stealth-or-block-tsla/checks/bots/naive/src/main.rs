//! The ladder's rungs that don't read the classes, one picked by
//! `MTX_NAIVE`, on the reference's machinery:
//!
//! - `follow-small`, `follow-large`: the cap the way one size class's net
//!   flow went over the last `look_slots`, past a threshold;
//! - `follow-all`: the same on every take, with a high threshold (a burst
//!   filter);
//! - `small-follow-all`: all flow on a low threshold, at a fifth of the cap;
//! - `momentum`: the cap the way the book moved.
//!
//! `MTX_FLOW_LOOK`, `MTX_FLOW_HOLD`, `MTX_FLOW_THRESHOLD` and
//! `MTX_FLOW_MOMENTUM` move their knobs.

fn main() -> mtx_kit::BotResult {
    use mtx_bot_stealth_or_block::{Flow, Mode};
    let mode = std::env::var("MTX_NAIVE")
        .ok()
        .and_then(|m| Mode::parse(&m))
        .ok_or("MTX_NAIVE names no rung")?;
    let mut flow = Flow::new(mode);
    flow.params
        .knobs(|name| std::env::var(format!("MTX_FLOW_{name}")).ok());
    mtx_kit::run(flow)
}
