//! The event-follow-through-tsla probes, with the task's reference to wrap.

fn main() -> mtx_kit::BotResult {
    mtx_probes::main_with(Some(Box::new(
        mtx_bot_event_follow_through::Reference::default(),
    )))
}
