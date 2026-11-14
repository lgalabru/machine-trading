//! The informed bot as a process: the strategy in `lib.rs`, on `mtx-kit`'s
//! run loop, told each session's shape as `MTX_REGIME`.

fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_perp_accumulate_informed::Informed::default())
}
