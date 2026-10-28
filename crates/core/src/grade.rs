//! Scores a bot over a set of scenarios.
//!
//! The reward is the bot's total PnL over the set as a fraction of the
//! reference bot's total PnL on the same set, clamped to [0, 1]: doing nothing
//! scores 0, matching the reference scores 1. Scoring the set as a whole lets
//! scenarios where the reference loses (a crash that never recovers) count:
//! a bot that loses less there, or makes more elsewhere, scores higher.
//!
//! Any session that calls a method the RPC guard refuses, or ends liquidatable,
//! disqualifies the bot: the reward is 0.

use std::{collections::BTreeMap, path::Path};

use serde::Serialize;
use tracing::info;

use crate::{
    Error, Result,
    error::io_error,
    scenario::Scenario,
    session::{self, SessionOptions, SessionReport},
};

#[derive(Debug, Serialize)]
pub struct ScenarioResult {
    /// Why this session disqualifies the bot, if it does.
    pub disqualified: Option<String>,
    pub reference_pnl_usd: f64,
    #[serde(flatten)]
    pub session: SessionReport,
}

#[derive(Debug, Serialize)]
pub struct GradeReport {
    pub reward: f64,
    /// Total PnL over total reference PnL, unclamped: above 1 beats the reference.
    pub pnl_ratio: f64,
    pub pnl_usd: f64,
    pub reference_pnl_usd: f64,
    /// Why the grade itself was tampered with, if it was: the reward is 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tampered: Option<String>,
    pub scenarios: Vec<ScenarioResult>,
}

impl GradeReport {
    /// Disqualify the bot for interfering with grading itself, e.g. writing
    /// the reward file the grader is about to write.
    pub fn tamper(&mut self, reason: String) {
        self.reward = 0.0;
        self.tampered = Some(reason);
    }

    /// Harbor's `reward.json`: flat numeric values. `reward` is the headline;
    /// the rest are diagnostics that aggregate across trials.
    pub fn rewards(&self) -> BTreeMap<String, f64> {
        let mut rewards = BTreeMap::from([
            ("reward".to_string(), self.reward),
            ("pnl_ratio".to_string(), self.pnl_ratio),
            ("pnl_usd".to_string(), self.pnl_usd),
            ("reference_pnl_usd".to_string(), self.reference_pnl_usd),
            (
                "refused_calls".to_string(),
                self.scenarios
                    .iter()
                    .map(|s| s.session.refused_calls.len() as f64)
                    .sum(),
            ),
            (
                "disqualified".to_string(),
                self.scenarios
                    .iter()
                    .filter(|s| s.disqualified.is_some())
                    .count() as f64,
            ),
            (
                "tampered".to_string(),
                f64::from(u8::from(self.tampered.is_some())),
            ),
        ]);
        for result in &self.scenarios {
            rewards.insert(
                format!("pnl_{}", result.session.scenario),
                result.session.pnl_usd,
            );
        }
        rewards
    }
}

pub struct GradeOptions<'a> {
    pub snapshot_dir: &'a Path,
    pub bot_dir: &'a Path,
    pub scenarios: &'a [Scenario],
    /// Bot logs go here as `<scenario>.log`.
    pub log_dir: &'a Path,
}

pub async fn grade(opts: GradeOptions<'_>) -> Result<GradeReport> {
    // Fail before spending minutes on sessions if the set can't be scored.
    let references = opts
        .scenarios
        .iter()
        .map(|scenario| {
            scenario
                .reference_pnl_usd
                .ok_or_else(|| Error::Uncalibrated(format!("scenario {}", scenario.name)))
        })
        .collect::<Result<Vec<_>>>()?;
    let reference_pnl_usd: f64 = references.iter().sum();
    if reference_pnl_usd <= 0.0 {
        return Err(Error::Uncalibrated(format!(
            "the set (the reference makes {reference_pnl_usd:.2} USD in total)"
        )));
    }

    let sessions = run_sessions(
        opts.snapshot_dir,
        opts.bot_dir,
        opts.scenarios,
        opts.log_dir,
    )
    .await?;
    check_timing(&sessions)?;
    let report = score(sessions.into_iter().zip(references).collect());
    info!(
        reward = report.reward,
        pnl_usd = report.pnl_usd,
        reference_pnl_usd = report.reference_pnl_usd,
        "graded"
    );
    Ok(report)
}

/// Score sessions against their references' PnL. The set's reference total
/// must be positive.
fn score(sessions: Vec<(SessionReport, f64)>) -> GradeReport {
    let scenarios: Vec<ScenarioResult> = sessions
        .into_iter()
        .map(|(session, reference_pnl_usd)| ScenarioResult {
            disqualified: disqualification(&session),
            reference_pnl_usd,
            session,
        })
        .collect();
    let pnl_usd: f64 = scenarios.iter().map(|s| s.session.pnl_usd).sum();
    let reference_pnl_usd: f64 = scenarios.iter().map(|s| s.reference_pnl_usd).sum();
    let pnl_ratio = pnl_usd / reference_pnl_usd;
    let reward = if scenarios.iter().any(|s| s.disqualified.is_some()) {
        0.0
    } else {
        pnl_ratio.clamp(0.0, 1.0)
    };
    GradeReport {
        reward,
        pnl_ratio,
        pnl_usd,
        reference_pnl_usd,
        tampered: None,
        scenarios,
    }
}

/// Sessions whose clock fell behind by more than this many slots (2 seconds
/// at 250ms slots) aren't scored: the bot saw a slower market than the one
/// it was meant to trade, and its PnL isn't comparable.
pub const MAX_LATE_SLOTS: u64 = 8;

/// Fail the grade, as an infrastructure error, rather than score sessions the
/// machine was too loaded to run on time. Harbor retries a failed verifier;
/// a skewed score would stand.
fn check_timing(sessions: &[SessionReport]) -> Result<()> {
    let late: Vec<String> = sessions
        .iter()
        .filter(|s| s.late_slots > MAX_LATE_SLOTS)
        .map(|s| format!("{} ({} slots)", s.scenario, s.late_slots))
        .collect();
    if late.is_empty() {
        Ok(())
    } else {
        Err(Error::Degraded(late.join(", ")))
    }
}

fn disqualification(session: &SessionReport) -> Option<String> {
    if !session.refused_calls.is_empty() {
        Some(format!(
            "called methods bots may not use: {}",
            session.refused_calls.join(", ")
        ))
    } else if session.liquidatable {
        Some("ended the session liquidatable".to_string())
    } else {
        None
    }
}

/// Run the bot on each scenario and record its PnL as the scenario's
/// reference, losses included: the reward normalizes by the set's total.
/// Sessions the reference itself would be disqualified from are reported, not
/// saved.
pub async fn calibrate(
    snapshot_dir: &Path,
    bot_dir: &Path,
    scenarios: &mut [(std::path::PathBuf, Scenario)],
    log_dir: &Path,
) -> Result<Vec<SessionReport>> {
    let list: Vec<Scenario> = scenarios.iter().map(|(_, s)| s.clone()).collect();
    let sessions = run_sessions(snapshot_dir, bot_dir, &list, log_dir).await?;
    // A reference recorded on a loaded machine would skew every score after it.
    check_timing(&sessions)?;
    for ((path, scenario), session) in scenarios.iter_mut().zip(&sessions) {
        if session.refused_calls.is_empty() && !session.liquidatable {
            scenario.reference_pnl_usd = Some((session.pnl_usd * 100.0).round() / 100.0);
            scenario.save(path)?;
        } else {
            tracing::warn!(
                scenario = scenario.name,
                pnl_usd = session.pnl_usd,
                "reference bot was disqualified; not calibrated"
            );
        }
    }
    Ok(sessions)
}

async fn run_sessions(
    snapshot_dir: &Path,
    bot_dir: &Path,
    scenarios: &[Scenario],
    log_dir: &Path,
) -> Result<Vec<SessionReport>> {
    std::fs::create_dir_all(log_dir).map_err(io_error(log_dir))?;
    let mut reports = Vec::new();
    for scenario in scenarios {
        let bot_log = log_dir.join(format!("{}.log", scenario.name));
        reports.push(
            session::run(SessionOptions {
                snapshot_dir,
                scenario,
                bot_dir,
                bot_log: &bot_log,
            })
            .await?,
        );
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pnl_usd: f64) -> SessionReport {
        SessionReport {
            scenario: "s".into(),
            start_mark_usd: 100.0,
            end_mark_usd: 100.0,
            start_value_usd: 10_100.0,
            end_value_usd: 10_100.0 + pnl_usd,
            pnl_usd,
            end_position: 0.0,
            liquidatable: false,
            refused_calls: vec![],
            bot_exit: None,
            late_slots: 0,
        }
    }

    #[test]
    fn late_sessions_fail_the_grade_instead_of_scoring() {
        let mut slow = session(10.0);
        slow.late_slots = MAX_LATE_SLOTS + 1;
        assert!(check_timing(&[session(1.0), slow]).is_err());

        let mut tolerable = session(10.0);
        tolerable.late_slots = MAX_LATE_SLOTS;
        assert!(check_timing(&[tolerable]).is_ok());
    }

    #[test]
    fn the_reward_is_the_set_ratio_and_losses_count() {
        // The reference makes 100 on the rebound and loses 60 on the slide: 40.
        let careful = score(vec![(session(50.0), 100.0), (session(-10.0), -60.0)]);
        assert_eq!(careful.pnl_ratio, 1.0);
        assert_eq!(careful.reward, 1.0);

        let reckless = score(vec![(session(100.0), 100.0), (session(-90.0), -60.0)]);
        assert_eq!(reckless.pnl_ratio, 0.25);
        assert_eq!(reckless.reward, 0.25);

        let better = score(vec![(session(120.0), 100.0), (session(0.0), -60.0)]);
        assert_eq!(better.pnl_ratio, 3.0);
        assert_eq!(better.reward, 1.0);

        let losing = score(vec![(session(-20.0), 100.0), (session(0.0), -60.0)]);
        assert_eq!(losing.reward, 0.0);
    }

    #[test]
    fn any_disqualified_session_zeroes_the_reward() {
        let mut cheat = session(500.0);
        cheat.refused_calls = vec!["surfnet_setTokenAccount".into()];
        let report = score(vec![(session(100.0), 100.0), (cheat, 100.0)]);
        assert_eq!(report.reward, 0.0);
        assert!(report.scenarios[1].disqualified.is_some());

        let mut ruined = session(100.0);
        ruined.liquidatable = true;
        assert_eq!(score(vec![(ruined, 50.0)]).reward, 0.0);
    }
}
