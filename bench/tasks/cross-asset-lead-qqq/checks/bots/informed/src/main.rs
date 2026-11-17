//! The informed bot, scoring v2's 1: the reference's machinery, told which
//! NVDA wallets are informed and which are decoys (`MTX_REGIME`), and
//! nothing of the session's draws: not the moves, nor QQQ's share of them.
//! It follows the informed wallets' takes from the first one, into QQQ.

fn main() -> mtx_kit::BotResult {
    mtx_bot_cross_asset_lead::main_with(Some(mtx_bot_cross_asset_lead::Mode::Informed))
}
