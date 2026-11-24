//! A strategy's life in a runtime (`docs/rfd/0006-the-strategy-lifecycle.md`):
//! whether it steps, and if not, who holds it and what would let it go.
//!
//! A strategy steps unless someone holds it or it can't. Several may hold
//! it at once, each for a reason of its own: a person paused it, the kill
//! switch was thrown, the enforcer found its mandate broken, the feed went
//! stale. Each [`Hold`] is lifted by what cures it, and by nothing else: a
//! `resume` gives back a person's will, and never talks the enforcer out
//! of a breach. So [`Lifecycle`] keeps every hold, and the state a sidecar
//! reads, [`StrategyState`], is derived from them: it cannot say `running`
//! while one stands, and lifting one never forgets another.
//!
//! This module is the rules, and pure: the runtime tells it what happened
//! and when, and does what the answer says.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Who holds a strategy paused. In order of gravity: the gravest hold is
/// the one a paused strategy's `why` tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hold {
    /// A `pause` command, or an add without `run`. Lifted by `resume`.
    Operator,
    /// The kill switch. Lifted by `resume`.
    Kill,
    /// The enforcer: the feed was behind or dropped events. Lifted by
    /// `resume`, once the feed is fresh again.
    Feed,
    /// The enforcer: the mandate was broken. Lifted by `resume` once what
    /// was observed no longer breaks it, or by a new mandate.
    Mandate,
}

impl Hold {
    /// Every hold, least grave first.
    pub const ALL: [Hold; 4] = [Hold::Operator, Hold::Kill, Hold::Feed, Hold::Mandate];

    /// Whether a person's (or a sidecar's) will set it, rather than the
    /// enforcer's finding: what `resume` takes back without a question.
    pub fn is_will(self) -> bool {
        match self {
            Hold::Operator | Hold::Kill => true,
            Hold::Feed | Hold::Mandate => false,
        }
    }

    /// Whether it outlives the runtime's restart. A stale feed doesn't:
    /// the new process judges its own feed.
    pub fn is_kept(self) -> bool {
        match self {
            Hold::Operator | Hold::Kill | Hold::Mandate => true,
            Hold::Feed => false,
        }
    }
}

/// Why a strategy can't step again as it is. Only a new component
/// (`upgrade`), a new mandate, or `remove` gets it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopCause {
    /// It didn't start: its component didn't load, its accounts couldn't
    /// be read, or its `start` failed.
    Start,
    /// Its component's bytes changed since it was added: a restart found
    /// another file under the same path.
    Changed,
    /// It trapped, or ran out of fuel or time, in a step.
    Trap,
}

/// Where a strategy stands, as `status` and the event stream tell it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StrategyState {
    /// Kept by the runtime and not started yet: it restarted, and starts
    /// its strategies once its node has caught up with the chain.
    Starting {
        why: String,
    },
    Running,
    /// Not stepping, and able to: `holds` says who keeps it, least grave
    /// first, and `why` is the gravest one's reason.
    Paused {
        /// Absent from a runtime older than the holds.
        #[serde(default)]
        holds: Vec<Hold>,
        why: String,
    },
    /// It can't step again as it is.
    Stopped {
        cause: StopCause,
        why: String,
    },
}

/// What a `resume` finds: whether the enforcer's holds are cured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cures {
    /// Why the feed is still stale, if it is.
    pub feed_stale: Option<String>,
    /// How the mandate is still broken by what was observed, if it is.
    pub breach: Option<String>,
}

/// A `resume` that left the strategy held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotResumed {
    /// The enforcer still holds it: `why` is what isn't cured.
    Held { hold: Hold, why: String },
    /// It is stopped: nothing to resume.
    Stopped { cause: StopCause, why: String },
}

impl std::fmt::Display for NotResumed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotResumed::Held {
                hold: Hold::Feed,
                why,
            } => write!(f, "the enforcer still holds it: {why}"),
            NotResumed::Held { why, .. } => write!(
                f,
                "the enforcer still holds it: {why}. It runs again under a new mandate"
            ),
            NotResumed::Stopped { why, .. } => write!(f, "it is stopped: {why}"),
        }
    }
}

/// A strategy's life: every hold on it with its reason, and whether it
/// stopped. The state is derived, never set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lifecycle {
    holds: BTreeMap<Hold, String>,
    stop: Option<(StopCause, String)>,
    /// When the state last changed.
    since_ms: u64,
}

impl Lifecycle {
    /// A strategy just added: held by the operator unless it was added to
    /// run.
    pub fn added(run: bool, now_ms: u64) -> Self {
        Self::restored(
            match run {
                true => BTreeMap::new(),
                false => BTreeMap::from([(Hold::Operator, "added paused".to_string())]),
            },
            now_ms,
        )
    }

    /// A strategy coming back after a restart, under the holds that were
    /// kept ([`Lifecycle::kept`]). One that doesn't outlive a restart is
    /// dropped: a file written by hand may name it.
    pub fn restored(holds: BTreeMap<Hold, String>, now_ms: u64) -> Self {
        Self {
            holds: holds
                .into_iter()
                .filter(|(hold, _)| hold.is_kept())
                .collect(),
            stop: None,
            since_ms: now_ms,
        }
    }

    pub fn is_running(&self) -> bool {
        self.stop.is_none() && self.holds.is_empty()
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.is_some()
    }

    pub fn is_held_by(&self, hold: Hold) -> bool {
        self.holds.contains_key(&hold)
    }

    /// When the state last changed.
    pub fn since_ms(&self) -> u64 {
        self.since_ms
    }

    /// The holds a restart brings back, with their reasons.
    pub fn kept(&self) -> BTreeMap<Hold, String> {
        self.holds
            .iter()
            .filter(|(hold, _)| hold.is_kept())
            .map(|(hold, why)| (*hold, why.clone()))
            .collect()
    }

    /// The state a sidecar reads. Stopped wins over held: a strategy that
    /// can't step is told so, and its holds wait under it.
    pub fn state(&self) -> StrategyState {
        if let Some((cause, why)) = &self.stop {
            return StrategyState::Stopped {
                cause: *cause,
                why: why.clone(),
            };
        }
        match self.holds.last_key_value() {
            None => StrategyState::Running,
            Some((_, why)) => StrategyState::Paused {
                holds: self.holds.keys().copied().collect(),
                why: why.clone(),
            },
        }
    }

    /// Do `change`, and tell the new state if it differs from the old.
    fn changing(&mut self, now_ms: u64, change: impl FnOnce(&mut Self)) -> Option<StrategyState> {
        let before = self.state();
        change(self);
        let after = self.state();
        if after == before {
            return None;
        }
        self.since_ms = now_ms;
        Some(after)
    }

    /// `hold` takes it, for `why`. A hold already there keeps its first
    /// reason: the breach that paused it, not the one after.
    pub fn hold(
        &mut self,
        hold: Hold,
        why: impl Into<String>,
        now_ms: u64,
    ) -> Option<StrategyState> {
        self.changing(now_ms, |life| {
            life.holds.entry(hold).or_insert_with(|| why.into());
        })
    }

    /// `hold` lets it go, if it held it.
    pub fn lift(&mut self, hold: Hold, now_ms: u64) -> Option<StrategyState> {
        self.changing(now_ms, |life| {
            life.holds.remove(&hold);
        })
    }

    /// It can't step again. The first cause stands.
    pub fn stop(
        &mut self,
        cause: StopCause,
        why: impl Into<String>,
        now_ms: u64,
    ) -> Option<StrategyState> {
        self.changing(now_ms, |life| {
            life.stop.get_or_insert_with(|| (cause, why.into()));
        })
    }

    /// Its component started again (an upgrade, a new mandate): it is no
    /// longer stopped, and whoever held it still does.
    pub fn started(&mut self, now_ms: u64) -> Option<StrategyState> {
        self.changing(now_ms, |life| life.stop = None)
    }

    /// A `resume`: a person's will to run it. Their own holds go; the
    /// enforcer's go only if `cures` says they are cured. A strategy still
    /// held or stopped after it is told why. The change, if any, is told
    /// either way: a resume may lift one hold and leave another.
    pub fn resume(
        &mut self,
        cures: &Cures,
        now_ms: u64,
    ) -> (Option<StrategyState>, Result<(), NotResumed>) {
        let changed = self.changing(now_ms, |life| {
            life.holds.retain(|hold, _| match hold {
                Hold::Operator | Hold::Kill => false,
                Hold::Feed => cures.feed_stale.is_some(),
                Hold::Mandate => cures.breach.is_some(),
            });
        });
        let left = if let Some((cause, why)) = &self.stop {
            Err(NotResumed::Stopped {
                cause: *cause,
                why: why.clone(),
            })
        } else if self.is_held_by(Hold::Mandate) {
            Err(NotResumed::Held {
                hold: Hold::Mandate,
                why: cures.breach.clone().expect("kept because still breached"),
            })
        } else if self.is_held_by(Hold::Feed) {
            Err(NotResumed::Held {
                hold: Hold::Feed,
                why: cures.feed_stale.clone().expect("kept because still stale"),
            })
        } else {
            Ok(())
        };
        assert_eq!(
            left.is_ok(),
            self.is_running(),
            "a resume that is done runs it"
        );
        (changed, left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURED: Cures = Cures {
        feed_stale: None,
        breach: None,
    };

    fn holds(life: &Lifecycle) -> Vec<Hold> {
        match life.state() {
            StrategyState::Paused { holds, .. } => holds,
            StrategyState::Starting { .. }
            | StrategyState::Running
            | StrategyState::Stopped { .. } => vec![],
        }
    }

    #[test]
    fn a_strategy_runs_unless_it_was_added_paused() {
        assert_eq!(Lifecycle::added(true, 5).state(), StrategyState::Running);
        let paused = Lifecycle::added(false, 5);
        assert_eq!(
            paused.state(),
            StrategyState::Paused {
                holds: vec![Hold::Operator],
                why: "added paused".into()
            }
        );
        assert_eq!(paused.since_ms(), 5);
    }

    #[test]
    fn a_resume_gives_back_a_person_s_will_and_not_the_enforcer_s_finding() {
        let mut life = Lifecycle::added(true, 0);
        life.hold(Hold::Mandate, "drawdown 21 over 20", 1);
        life.hold(Hold::Operator, "paused", 2);
        life.hold(Hold::Kill, "the kill switch", 3);
        // The gravest hold is the reason told.
        assert!(
            matches!(life.state(), StrategyState::Paused { why, .. } if why.starts_with("drawdown"))
        );
        let still = Cures {
            breach: Some("drawdown 21 over 20".into()),
            ..CURED
        };
        let (changed, left) = life.resume(&still, 4);
        assert_eq!(holds(&life), [Hold::Mandate]);
        assert!(changed.is_some(), "two holds went");
        assert_eq!(
            left,
            Err(NotResumed::Held {
                hold: Hold::Mandate,
                why: "drawdown 21 over 20".into()
            })
        );
        // Asked again, nothing changes and the answer is the same.
        let (changed, left) = life.resume(&still, 5);
        assert_eq!((changed, left.is_err(), life.since_ms()), (None, true, 4));
        // Cured, it runs.
        let (changed, left) = life.resume(&CURED, 6);
        assert_eq!((changed, left), (Some(StrategyState::Running), Ok(())));
        assert_eq!(life.since_ms(), 6);
    }

    #[test]
    fn a_stale_feed_holds_until_it_is_fresh() {
        let mut life = Lifecycle::added(true, 0);
        life.hold(Hold::Feed, "the feed is 40 slots behind the chain", 1);
        let stale = Cures {
            feed_stale: Some("the feed is 12 slots behind the chain".into()),
            ..CURED
        };
        let (_, left) = life.resume(&stale, 2);
        assert!(
            matches!(left, Err(NotResumed::Held { hold: Hold::Feed, why }) if why.contains("12 slots"))
        );
        assert_eq!(life.resume(&CURED, 3).1, Ok(()));
    }

    #[test]
    fn a_hold_keeps_its_first_reason_and_a_lift_forgets_only_its_own() {
        let mut life = Lifecycle::added(true, 0);
        assert!(life.hold(Hold::Mandate, "the first breach", 1).is_some());
        assert_eq!(life.hold(Hold::Mandate, "a later breach", 2), None);
        life.hold(Hold::Kill, "the kill switch", 3);
        // A new mandate lifts the enforcer's hold; the kill switch stands.
        let state = life.lift(Hold::Mandate, 4).expect("a hold went");
        assert_eq!(
            state,
            StrategyState::Paused {
                holds: vec![Hold::Kill],
                why: "the kill switch".into()
            }
        );
        assert_eq!(life.lift(Hold::Mandate, 5), None);
        assert_eq!(life.since_ms(), 4);
    }

    #[test]
    fn a_stopped_strategy_is_told_stopped_and_comes_back_under_its_holds() {
        let mut life = Lifecycle::added(true, 0);
        life.hold(Hold::Kill, "the kill switch", 1);
        life.stop(StopCause::Trap, "out of fuel", 2);
        assert_eq!(life.stop(StopCause::Start, "a later reason", 3), None);
        assert_eq!(
            life.state(),
            StrategyState::Stopped {
                cause: StopCause::Trap,
                why: "out of fuel".into()
            }
        );
        // A resume takes the kill switch off, and it is still stopped.
        let (changed, left) = life.resume(&CURED, 4);
        assert_eq!(changed, None, "the state told is the same");
        assert!(matches!(
            left,
            Err(NotResumed::Stopped {
                cause: StopCause::Trap,
                ..
            })
        ));
        assert!(!life.is_running() && life.is_stopped());
        // Held again, then upgraded: it comes back paused, not running.
        life.hold(Hold::Operator, "paused", 5);
        let state = life.started(6).expect("no longer stopped");
        assert!(matches!(state, StrategyState::Paused { holds, .. } if holds == [Hold::Operator]));
    }

    #[test]
    fn what_a_restart_brings_back_is_every_hold_but_the_feed_s() {
        let mut life = Lifecycle::added(false, 0);
        life.hold(Hold::Feed, "behind", 1);
        life.hold(Hold::Mandate, "drawdown", 2);
        let kept = life.kept();
        assert_eq!(
            kept.keys().copied().collect::<Vec<_>>(),
            [Hold::Operator, Hold::Mandate]
        );
        let back = Lifecycle::restored(kept, 9);
        assert_eq!(holds(&back), [Hold::Operator, Hold::Mandate]);
        assert_eq!(back.since_ms(), 9);
        // A file that names the feed's hold doesn't bring it back.
        let by_hand = BTreeMap::from([(Hold::Feed, "behind".to_string())]);
        assert!(Lifecycle::restored(by_hand, 9).is_running());
    }

    /// Every set of holds, stopped or not, under every cure: the state
    /// never says running while a hold stands, and a resume never lifts
    /// what isn't cured.
    #[test]
    fn no_state_runs_under_a_hold_and_no_resume_lifts_what_isn_t_cured() {
        for set in 0u8..16 {
            for stopped in [false, true] {
                for cure in 0u8..4 {
                    let mut life = Lifecycle::added(true, 0);
                    for (bit, hold) in Hold::ALL.into_iter().enumerate() {
                        if set & (1 << bit) != 0 {
                            life.hold(hold, format!("{hold:?}"), 1);
                        }
                    }
                    if stopped {
                        life.stop(StopCause::Trap, "trapped", 2);
                    }
                    assert_eq!(life.is_running(), set == 0 && !stopped);
                    assert_eq!(life.state() == StrategyState::Running, life.is_running());
                    let cures = Cures {
                        feed_stale: (cure & 1 != 0).then(|| "stale".to_string()),
                        breach: (cure & 2 != 0).then(|| "breached".to_string()),
                    };
                    let (_, left) = life.resume(&cures, 3);
                    assert!(!life.is_held_by(Hold::Operator) && !life.is_held_by(Hold::Kill));
                    let feed_stays = set & 4 != 0 && cures.feed_stale.is_some();
                    let mandate_stays = set & 8 != 0 && cures.breach.is_some();
                    assert_eq!(life.is_held_by(Hold::Feed), feed_stays);
                    assert_eq!(life.is_held_by(Hold::Mandate), mandate_stays);
                    assert_eq!(left.is_ok(), !stopped && !feed_stays && !mandate_stays);
                }
            }
        }
    }

    #[test]
    fn a_state_is_the_line_a_sidecar_reads() {
        let line = |state: &StrategyState| serde_json::to_string(state).unwrap();
        assert_eq!(line(&StrategyState::Running), r#"{"state":"running"}"#);
        let paused = StrategyState::Paused {
            holds: vec![Hold::Kill, Hold::Mandate],
            why: "drawdown".into(),
        };
        assert_eq!(
            line(&paused),
            r#"{"state":"paused","holds":["kill","mandate"],"why":"drawdown"}"#
        );
        let stopped = StrategyState::Stopped {
            cause: StopCause::Trap,
            why: "out of fuel".into(),
        };
        assert_eq!(
            line(&stopped),
            r#"{"state":"stopped","cause":"trap","why":"out of fuel"}"#
        );
        // A runtime older than the holds: paused, by no one it names.
        let old: StrategyState =
            serde_json::from_str(r#"{"state":"paused","why":"paused"}"#).unwrap();
        assert_eq!(
            old,
            StrategyState::Paused {
                holds: vec![],
                why: "paused".into()
            }
        );
    }
}
