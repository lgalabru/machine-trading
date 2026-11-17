//! The informed bot, scoring v2's 1: the reference's machinery, told the
//! session's informed size class (`MTX_REGIME`'s `informed_class`) and
//! nothing of its draws. It follows that class's net flow from the first
//! take.

fn main() -> mtx_kit::BotResult {
    use mtx_bot_stealth_or_block::{Flow, Mode};
    mtx_kit::run(Flow::new(Mode::Informed))
}
