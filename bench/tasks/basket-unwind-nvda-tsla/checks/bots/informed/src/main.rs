//! The informed bot as a process: it trades the primary market only, so the
//! ladder and the verifier run the component (`bot.wasm`).

#[cfg(not(target_arch = "wasm32"))]
fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_basket_unwind_nvda_informed::Informed::default())
}

#[cfg(target_arch = "wasm32")]
fn main() {}
