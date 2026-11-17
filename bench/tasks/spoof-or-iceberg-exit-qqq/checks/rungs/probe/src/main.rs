//! The spoof-or-iceberg-exit-qqq probes, with the task's reference to wrap.

fn main() -> mtx_kit::BotResult {
    mtx_probes::main_with(Some(Box::new(
        mtx_bot_spoof_or_iceberg_exit::Reference::default(),
    )))
}
