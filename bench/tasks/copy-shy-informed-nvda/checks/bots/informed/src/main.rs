//! The informed bot, scoring v2's 1: the reference's machinery, told which
//! wallet is the fund and which are decoys, and the fund's shy rule
//! (`MTX_REGIME`), nothing of the session's draws. It copies the fund alone
//! from its first take, at a share just under the rule's.

fn main() -> mtx_kit::BotResult {
    use mtx_bot_copy_shy_informed::{Copier, Mode};
    let mut copier = Copier::new(Mode::Informed);
    if let Some(v) = std::env::var("MTX_COPY_SHARE")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        copier.params.share = v;
    }
    if let Some(v) = std::env::var("MTX_COPY_LAG")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        copier.params.lag_slots = v;
    }
    if let Some(v) = std::env::var("MTX_COPY_HOLD")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        copier.params.hold_slots = v;
    }
    mtx_kit::run(copier)
}
