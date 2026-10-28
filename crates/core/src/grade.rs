//! Scores a bot over a set of scenarios.
//!
//! A scenario's score is the bot's PnL as a fraction of the reference bot's
//! PnL on the same scenario, clamped to [0, 1]: doing nothing scores 0,
//! matching the reference scores 1. A session scores 0 outright if the bot
//! called a method the RPC guard refuses, or ended the session liquidatable.
//! The task reward is the mean score.

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
    pub score: f64,
    /// Why the score was forced to 0, if it was.
    pub disqualified: Option<String>,
    pub reference_pnl_usd: f64,
    #[serde(flatten)]
    pub session: SessionReport,
}

#[derive(Debug, Serialize)]
pub struct GradeReport {
    pub reward: f64,
    pub scenarios: Vec<ScenarioResult>,
}

impl GradeReport {
    /// Harbor's `reward.json`: flat numeric values. `reward` is the headline;
    /// the rest are diagnostics that aggregate across trials.
    pub fn rewards(&self) -> BTreeMap<String, f64> {
        let mut rewards = BTreeMap::from([
            ("reward".to_string(), self.reward),
            (
                "pnl_usd".to_string(),
                self.scenarios.iter().map(|s| s.session.pnl_usd).sum(),
            ),
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
        ]);
        for result in &self.scenarios {
            rewards.insert(format!("score_{}", result.session.scenario), result.score);
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
    // Fail before spending minutes on sessions if a scenario can't be scored.
    let references = opts
        .scenarios
        .iter()
        .map(|scenario| match scenario.reference_pnl_usd {
            Some(pnl) if pnl > 0.0 => Ok(pnl),
            _ => Err(Error::Uncalibrated(scenario.name.clone())),
        })
        .collect::<Result<Vec<_>>>()?;

    let sessions = run_sessions(
        opts.snapshot_dir,
        opts.bot_dir,
        opts.scenarios,
        opts.log_dir,
    )
    .await?;
    let scenarios: Vec<ScenarioResult> = sessions
        .into_iter()
        .zip(references)
        .map(|(session, reference_pnl_usd)| score(session, reference_pnl_usd))
        .collect();
    let reward = scenarios.iter().map(|s| s.score).sum::<f64>() / scenarios.len().max(1) as f64;
    info!(reward, "graded");
    Ok(GradeReport { reward, scenarios })
}

fn score(session: SessionReport, reference_pnl_usd: f64) -> ScenarioResult {
    let disqualified = if !session.refused_calls.is_empty() {
        Some(format!(
            "called methods bots may not use: {}",
            session.refused_calls.join(", ")
        ))
    } else if session.liquidatable {
        Some("ended the session liquidatable".to_string())
    } else {
        None
    };
    let score = match disqualified {
        Some(_) => 0.0,
        None => (session.pnl_usd / reference_pnl_usd).clamp(0.0, 1.0),
    };
    ScenarioResult {
        score,
        disqualified,
        reference_pnl_usd,
        session,
    }
}

/// Run the bot on each scenario and record its PnL as the scenario's
/// reference. Scenarios where it doesn't make money are reported, not saved:
/// they can't normalize a score.
pub async fn calibrate(
    snapshot_dir: &Path,
    bot_dir: &Path,
    scenarios: &mut [(std::path::PathBuf, Scenario)],
    log_dir: &Path,
) -> Result<Vec<SessionReport>> {
    let list: Vec<Scenario> = scenarios.iter().map(|(_, s)| s.clone()).collect();
    let sessions = run_sessions(snapshot_dir, bot_dir, &list, log_dir).await?;
    for ((path, scenario), session) in scenarios.iter_mut().zip(&sessions) {
        if session.pnl_usd > 0.0 && session.refused_calls.is_empty() && !session.liquidatable {
            scenario.reference_pnl_usd = Some((session.pnl_usd * 100.0).round() / 100.0);
            scenario.save(path)?;
        } else {
            tracing::warn!(
                scenario = scenario.name,
                pnl_usd = session.pnl_usd,
                "reference bot didn't profit; not calibrated"
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
