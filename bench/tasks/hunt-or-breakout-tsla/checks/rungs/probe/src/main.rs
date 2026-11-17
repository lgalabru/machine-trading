//! The hunt-or-breakout-tsla probes, with the task's reference to wrap.

fn main() -> mtx_kit::BotResult {
    mtx_probes::main_with(Some(Box::new(
        mtx_bot_hunt_or_breakout::Reference::default(),
    )))
}
