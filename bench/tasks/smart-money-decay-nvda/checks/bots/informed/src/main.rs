//! The informed bot, scoring v2's 1: the reference's machinery, told which
//! wallets are informed and which are decoys (`MTX_REGIME`), and nothing of
//! the session's draws. It votes with the informed wallets (two that agree,
//! or the one if only one is told), holds without flips, and leaves the
//! decoys alone.

fn main() -> mtx_kit::BotResult {
    use mtx_bot_smart_money_decay::{Flow, Mode};
    mtx_kit::run(Flow::new(Mode::Informed))
}
