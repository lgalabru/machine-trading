//! The informed bot as a process: the strategy in `lib.rs`, on `mtx-kit`'s
//! run loop, told each session's latent state as `MTX_REGIME`.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_toxic_day_make_or_follow_informed::Informed::default())
}

// The component is the library: a wasm build has no process to run.
#[cfg(target_arch = "wasm32")]
fn main() {}
