//! The ladder's non-reading rungs as a process, picked by `MTX_NAIVE` (see
//! `lib.rs`).

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    let name = std::env::var("MTX_NAIVE").unwrap_or_default();
    let policy = mtx_bot_toxic_day_make_or_follow_naive::policy(&name).ok_or_else(|| {
        format!("MTX_NAIVE must be flat, make, follow, switch or burst, not {name:?}")
    })?;
    mtx_kit::run(mtx_bot_toxic_day_make_or_follow::Desk::new(policy))
}

#[cfg(target_arch = "wasm32")]
fn main() {}
