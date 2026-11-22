//! Trigger steps (ADR 0004): a step that ends right behind a transaction
//! the scenario lands, a whale's swap, with its slot still open, so the
//! bot and the searchers react in the slot of the move, as mainnet's
//! backruns do (18 of 21 landed in the trigger's slot, `docs/RESEARCH.md`).
//!
//! A step ends when the chain reaches its last slot; what the session lands
//! then, and what the step's turns send with no latency, lands in that same
//! slot before the next step closes it. A trigger step is a step whose last
//! slot is the trigger's: the session lands the trigger there, then the
//! actors and the bot take their turns on the state it left. Steps without
//! a trigger stay [`SLOTS_PER_STEP`] slots long, so a step before a trigger
//! may be shorter, to end on it.
//!
//! The steps are a function of the session's length and its triggers'
//! slots alone, both the scenario's, so a session announces its step count
//! before its first step and replays the same steps every time.

use async_trait::async_trait;

use crate::{Result, session::SLOTS_PER_STEP};

/// Most triggers a session may hold: one a slot over the longest session
/// a scenario runs (`lending::MAX_WARP_SLOTS` aside), far more than the
/// research's densest burst (20 gaps in 17 minutes).
pub const TRIGGERS_MAX: usize = 10_000;

/// One step of a session with triggers: where it ends, in slots from the
/// session's start, and the trigger it ends behind, if any, by its index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepSpan {
    pub end_at: u64,
    pub trigger: Option<usize>,
}

/// The steps of a session of `duration_slots` whose triggers land at
/// `triggers` (slots from the start, strictly increasing): two-slot steps,
/// each trigger ending a step at its slot. Each trigger must land after the
/// session's first slot and before its last, so a step follows it.
pub fn schedule(
    duration_slots: u64,
    triggers: &[u64],
) -> std::result::Result<Vec<StepSpan>, String> {
    if triggers.len() > TRIGGERS_MAX {
        return Err(format!("at most {TRIGGERS_MAX} triggers"));
    }
    let mut last = 0;
    for &at in triggers {
        if at <= last || at >= duration_slots {
            return Err(format!(
                "a trigger at slot {at}: triggers land in increasing slots, after slot 0 and before the session's last ({duration_slots})"
            ));
        }
        last = at;
    }
    let mut steps = Vec::new();
    let mut at = 0;
    let mut next = triggers.iter().copied().enumerate().peekable();
    while at < duration_slots {
        let span = match next.peek() {
            Some(&(i, trigger)) if trigger <= at + SLOTS_PER_STEP => {
                next.next();
                StepSpan {
                    end_at: trigger,
                    trigger: Some(i),
                }
            }
            _ => StepSpan {
                end_at: (at + SLOTS_PER_STEP).min(duration_slots),
                trigger: None,
            },
        };
        assert!(span.end_at > at, "every step moves the chain");
        at = span.end_at;
        steps.push(span);
    }
    assert!(next.next().is_none(), "every trigger ends a step");
    Ok(steps)
}

/// What lands a session's triggers: the slot of each, fixed by the
/// scenario, and the landing itself, in the step that ends on it.
#[async_trait]
pub(crate) trait Triggers: Send + Sync {
    /// Each trigger's slot, from the session's start, in order.
    fn slots(&self) -> Vec<u64>;

    /// Land trigger `i` now, in the open slot `slot` (`at` slots in), as
    /// step `k` ends: it lands at once, through the grader's own endpoint,
    /// as the ordinary transaction it is, never through the auction.
    async fn land(
        &mut self,
        i: usize,
        k: u64,
        slot: u64,
        at: u64,
    ) -> Result<mtx_core::turns::Trigger>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_triggers_the_steps_are_two_slots_as_ever() {
        let steps = schedule(7, &[]).unwrap();
        let ends: Vec<u64> = steps.iter().map(|s| s.end_at).collect();
        assert_eq!(ends, [2, 4, 6, 7]);
        assert_eq!(steps.len() as u64, 7u64.div_ceil(SLOTS_PER_STEP));
        assert!(steps.iter().all(|s| s.trigger.is_none()));
    }

    #[test]
    fn a_trigger_ends_its_step_on_its_slot() {
        let steps = schedule(10, &[3, 4, 9]).unwrap();
        let spans: Vec<(u64, Option<usize>)> =
            steps.iter().map(|s| (s.end_at, s.trigger)).collect();
        assert_eq!(
            spans,
            [
                (2, None),
                (3, Some(0)),
                (4, Some(1)),
                (6, None),
                (8, None),
                (9, Some(2)),
                (10, None)
            ]
        );
    }

    #[test]
    fn triggers_out_of_order_or_outside_the_session_are_refused() {
        assert!(schedule(10, &[0]).is_err());
        assert!(schedule(10, &[10]).is_err());
        assert!(schedule(10, &[4, 4]).is_err());
        assert!(schedule(10, &[5, 3]).is_err());
        assert!(schedule(10, &[9]).is_ok());
    }
}
