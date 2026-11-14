//! The perp-accumulate-nvda probes, with the task's reference to wrap.

fn main() -> mtx_kit::BotResult {
    mtx_probes::main_with(Some(Box::new(
        mtx_bot_perp_accumulate::Accumulator::default(),
    )))
}
