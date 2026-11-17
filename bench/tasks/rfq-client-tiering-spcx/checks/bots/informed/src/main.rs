//! The informed bot as a process: the strategy in `lib.rs`, on `mtx-kit`'s
//! run loop, told each session's latent state as `MTX_REGIME`.

fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_rfq_client_tiering_informed::Bot::default())
}
