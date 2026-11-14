//! The generic probes: the ones that need no reference (see the crate docs).
//! A task's reference-wrapping probes start from its `checks/rungs/probe`.

fn main() -> mtx_kit::BotResult {
    mtx_probes::main_with(None)
}
