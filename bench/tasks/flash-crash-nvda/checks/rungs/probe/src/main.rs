//! The flash-crash probes, with the task's reference to wrap.

fn main() -> mtx_kit::BotResult {
    mtx_probes::main_with(Some(Box::new(mtx_bot_flash_crash_nvda::Reference::default())))
}
