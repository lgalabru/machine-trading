//! Scores a bot over a set of scenarios.
//!
//! The reward is the bot's total PnL over the set as a fraction of the
//! hindsight bound: what a bot holding the full position cap the right way
//! through every move would make at no cost (see
//! [`Scenario::hindsight_bound_usd`]). Clamped to [0, 1]: doing nothing scores
//! 0, and no bot reaches 1, so the score doesn't saturate as bots improve.
//! Scoring the set as a whole lets losing scenarios count.
//!
//! The reference bot's PnL is reported alongside as a baseline, not used for
//! the reward: `skill` is the bot's total PnL over the reference's, so the
//! reference scores 1 and doing nothing 0, unclamped. On hidden sets drawn
//! fresh for each grade, the grader runs the reference itself first, on the
//! same sessions (see [`GradeOptions::reference_dir`]); otherwise scenarios
//! carry the PnL [`calibrate`] recorded.
//!
//! Any session that calls a method the RPC guard refuses, breaks its
//! mandate (trades outside its market, goes over the position cap, or draws
//! down past its limit), or ends liquidatable disqualifies the bot: the
//! reward is 0. Risk is otherwise reported, not scored: see [`Risk`].

use std::{collections::BTreeMap, path::Path};

use futures::StreamExt;
use serde::Serialize;
use tracing::{info, warn};

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
    /// The hindsight bound for this session, at its starting mark.
    pub bound_pnl_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_pnl_usd: Option<f64>,
    #[serde(flatten)]
    pub session: SessionReport,
}

#[derive(Debug, Serialize)]
pub struct GradeReport {
    /// Total PnL over the total hindsight bound, clamped to [0, 1].
    pub reward: f64,
    pub pnl_usd: f64,
    pub bound_pnl_usd: f64,
    /// The reference's total PnL, when every scenario carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_pnl_usd: Option<f64>,
    /// Total PnL over the reference's, unclamped: above 1 beats the reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_ratio: Option<f64>,
    /// The headline for comparing bots: [`Self::reference_ratio`], or 0 when
    /// the bot is disqualified. A breach never scores better than complying.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<f64>,
    /// Why the grade itself was tampered with, if it was: the reward is 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tampered: Option<String>,
    /// The SHA-256 of the secret a drawn hidden set came from: enough to
    /// tell two grades on the same set, nothing more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_seed_sha256: Option<String>,
    pub risk: Risk,
    pub scenarios: Vec<ScenarioResult>,
}

/// How the bot made its PnL, across sessions. Reported, not scored: with a
/// dozen short sessions these are rough, and a reward that blended them in
/// would reward whatever weights it picked. The drawdown limit is the risk
/// rule that is enforced.
#[derive(Debug, Serialize, PartialEq)]
pub struct Risk {
    /// The worst session's PnL.
    pub worst_pnl_usd: f64,
    /// The mean PnL of the worst sixth of sessions (at least one): a tail loss.
    pub tail_pnl_usd: f64,
    /// The deepest any session's account value fell below its running peak.
    pub max_drawdown_usd: f64,
    /// The share of sessions that made money.
    pub profitable_share: f64,
    /// Mean session PnL over its standard deviation (0 when it doesn't vary).
    pub sharpe: f64,
}

impl Risk {
    fn of(sessions: &[ScenarioResult]) -> Self {
        let mut pnls: Vec<f64> = sessions.iter().map(|s| s.session.pnl_usd).collect();
        if pnls.is_empty() {
            return Self {
                worst_pnl_usd: 0.0,
                tail_pnl_usd: 0.0,
                max_drawdown_usd: 0.0,
                profitable_share: 0.0,
                sharpe: 0.0,
            };
        }
        pnls.sort_by(f64::total_cmp);
        let n = pnls.len() as f64;
        let tail = pnls.len().div_ceil(6);
        let mean = pnls.iter().sum::<f64>() / n;
        let variance = if pnls.len() > 1 {
            pnls.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / (n - 1.0)
        } else {
            0.0
        };
        Self {
            worst_pnl_usd: pnls[0],
            tail_pnl_usd: pnls[..tail].iter().sum::<f64>() / tail as f64,
            max_drawdown_usd: sessions
                .iter()
                .map(|s| s.session.max_drawdown_usd)
                .fold(0.0, f64::max),
            profitable_share: pnls.iter().filter(|p| **p > 0.0).count() as f64 / n,
            sharpe: if variance > 0.0 {
                mean / variance.sqrt()
            } else {
                0.0
            },
        }
    }
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
            ("pnl_usd".to_string(), self.pnl_usd),
            ("bound_pnl_usd".to_string(), self.bound_pnl_usd),
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
            ("worst_pnl_usd".to_string(), self.risk.worst_pnl_usd),
            ("tail_pnl_usd".to_string(), self.risk.tail_pnl_usd),
            ("max_drawdown_usd".to_string(), self.risk.max_drawdown_usd),
            ("profitable_share".to_string(), self.risk.profitable_share),
            ("sharpe".to_string(), self.risk.sharpe),
        ]);
        if let (Some(reference), Some(ratio)) = (self.reference_pnl_usd, self.reference_ratio) {
            rewards.insert("reference_pnl_usd".to_string(), reference);
            rewards.insert("reference_ratio".to_string(), ratio);
        }
        if let Some(skill) = self.skill {
            rewards.insert("skill".to_string(), skill);
        }
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
    /// How many sessions to run at once.
    pub jobs: usize,
    /// A reference bot to run on the same scenarios first, as the baseline,
    /// with `anchor_jobs` sessions at once (it's trusted: it can share the
    /// machine with itself). Its PnL replaces whatever the scenarios carry.
    pub reference_dir: Option<&'a Path>,
    pub anchor_jobs: usize,
    /// Each scenario's regime, as JSON, handed to the bot (not the
    /// reference): the ladder's regime oracle only.
    pub regimes: Option<&'a [String]>,
    /// Stop at the bot's first disqualifying session: the grade is 0
    /// whatever the rest do, and a bot that ignores turns runs each session
    /// in real time. The report then covers the sessions up to it.
    pub stop_on_disqualify: bool,
}

pub async fn grade(opts: GradeOptions<'_>) -> Result<GradeReport> {
    let mut scenarios = opts.scenarios.to_vec();
    if let Some(reference) = opts.reference_dir {
        let anchors = run_sessions(
            opts.snapshot_dir,
            reference,
            &scenarios,
            &opts.log_dir.join("reference"),
            opts.anchor_jobs,
            None,
            false,
        )
        .await?;
        check_timing(&anchors)?;
        check_anchors(&anchors)?;
        for (scenario, anchor) in scenarios.iter_mut().zip(&anchors) {
            scenario.reference_pnl_usd = Some((anchor.pnl_usd * 100.0).round() / 100.0);
        }
        info!(
            reference_pnl_usd = anchors.iter().map(|a| a.pnl_usd).sum::<f64>(),
            "reference graded"
        );
    }
    let sessions = run_sessions(
        opts.snapshot_dir,
        opts.bot_dir,
        &scenarios,
        opts.log_dir,
        opts.jobs,
        opts.regimes,
        opts.stop_on_disqualify,
    )
    .await?;
    check_timing(&sessions)?;
    let report = score(sessions.into_iter().zip(&scenarios).collect());
    info!(
        reward = report.reward,
        pnl_usd = report.pnl_usd,
        bound_pnl_usd = report.bound_pnl_usd,
        reference_ratio = report.reference_ratio,
        "graded"
    );
    Ok(report)
}

/// Score sessions against their scenarios' hindsight bounds.
fn score(sessions: Vec<(SessionReport, &Scenario)>) -> GradeReport {
    let scenarios: Vec<ScenarioResult> = sessions
        .into_iter()
        .map(|(session, scenario)| ScenarioResult {
            disqualified: disqualification(&session),
            bound_pnl_usd: scenario.bound_usd(session.start_mark_usd),
            reference_pnl_usd: scenario.reference_pnl_usd,
            session,
        })
        .collect();
    let pnl_usd: f64 = scenarios.iter().map(|s| s.session.pnl_usd).sum();
    let bound_pnl_usd: f64 = scenarios.iter().map(|s| s.bound_pnl_usd).sum();
    let reference_pnl_usd: Option<f64> = scenarios.iter().map(|s| s.reference_pnl_usd).sum();
    let reference_ratio = reference_pnl_usd
        .filter(|reference| *reference > 0.0)
        .map(|reference| pnl_usd / reference);
    let disqualified = scenarios.iter().any(|s| s.disqualified.is_some());
    let reward = if disqualified || bound_pnl_usd <= 0.0 {
        0.0
    } else {
        (pnl_usd / bound_pnl_usd).clamp(0.0, 1.0)
    };
    let skill = reference_ratio.map(|ratio| if disqualified { 0.0 } else { ratio });
    GradeReport {
        reward,
        pnl_usd,
        bound_pnl_usd,
        reference_pnl_usd,
        reference_ratio,
        skill,
        tampered: None,
        eval_seed_sha256: None,
        risk: Risk::of(&scenarios),
        scenarios,
    }
}

/// Sessions whose clock fell behind by more than this many slots (2 seconds
/// at 250ms slots) aren't scored: the bot saw a slower market than the one
/// it was meant to trade, and its PnL isn't comparable.
pub const MAX_LATE_SLOTS: u64 = 8;

/// The share of a market-making session's taker orders that may fail before
/// the session isn't scored.
pub const MAX_FAILED_TAKER_SHARE: f64 = 0.1;

/// Fail the grade, as an infrastructure error, rather than score sessions the
/// machine was too loaded to run on time. Harbor retries a failed verifier;
/// a skewed score would stand.
fn check_timing(sessions: &[SessionReport]) -> Result<()> {
    let late: Vec<String> = sessions
        .iter()
        .filter_map(|s| {
            if s.late_slots > MAX_LATE_SLOTS {
                return Some(format!("{} ({} slots late)", s.scenario, s.late_slots));
            }
            // A market-making session whose takers didn't trade as listed
            // didn't see the flow its scenario describes.
            let flow = s.flow.as_ref()?;
            (flow.failed as f64 > MAX_FAILED_TAKER_SHARE * flow.sent as f64).then(|| {
                format!(
                    "{} ({} of {} taker orders failed)",
                    s.scenario, flow.failed, flow.sent
                )
            })
        })
        .collect();
    if late.is_empty() {
        Ok(())
    } else {
        Err(Error::Degraded(late.join(", ")))
    }
}

/// Void the grade, rather than score the bot against a broken baseline, if
/// the reference didn't run cleanly: a disqualification, a crash, or turns
/// it didn't finish.
fn check_anchors(anchors: &[SessionReport]) -> Result<()> {
    let broken: Vec<String> = anchors
        .iter()
        .filter_map(|a| {
            let why = if let Some(reason) = disqualification(a) {
                reason
            } else if a.turns.legacy {
                "never answered the hello".to_string()
            } else if a.turns.overran > 0 {
                format!("overran {} turns", a.turns.overran)
            } else {
                let exit = a
                    .bot_exit
                    .as_ref()
                    .filter(|e| !e.contains("exit status: 0"))?;
                format!("exited: {exit}")
            };
            Some(format!("{}: {why}", a.scenario))
        })
        .collect();
    if broken.is_empty() {
        Ok(())
    } else {
        Err(Error::Degraded(format!(
            "the reference: {}",
            broken.join(", ")
        )))
    }
}

fn disqualification(session: &SessionReport) -> Option<String> {
    if !session.refused_calls.is_empty() {
        Some(format!(
            "called methods bots may not use: {}",
            session.refused_calls.join(", ")
        ))
    } else if let Some(breach) = session.mandate.audit(&session.observed()) {
        Some(breach.to_string())
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
    jobs: usize,
) -> Result<Vec<SessionReport>> {
    let list: Vec<Scenario> = scenarios.iter().map(|(_, s)| s.clone()).collect();
    let sessions = run_sessions(snapshot_dir, bot_dir, &list, log_dir, jobs, None, false).await?;
    // A reference recorded on a loaded machine would skew every score after it.
    check_timing(&sessions)?;
    for ((path, scenario), session) in scenarios.iter_mut().zip(&sessions) {
        // The same rules as grading: a reference the grader would
        // disqualify can't be a baseline.
        if disqualification(session).is_none() {
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

/// Run every scenario, up to `jobs` at a time, reports in scenario order.
///
/// Concurrent sessions each get their own surfnet, and run their bots as
/// different users (see [`session::BOT_USERS`]). Sessions are kept in an
/// ordered window of `jobs`, so the ones running at once always have
/// distinct `index % jobs` slots. A machine too loaded for `jobs` shows up as
/// late slots, which fail the grade rather than skew it.
async fn run_sessions(
    snapshot_dir: &Path,
    bot_dir: &Path,
    scenarios: &[Scenario],
    log_dir: &Path,
    jobs: usize,
    regimes: Option<&[String]>,
    stop_on_disqualify: bool,
) -> Result<Vec<SessionReport>> {
    if !(1..=session::BOT_USERS.len()).contains(&jobs) {
        return Err(Error::Jobs(jobs, session::BOT_USERS.len()));
    }
    std::fs::create_dir_all(log_dir).map_err(io_error(log_dir))?;
    let sessions = futures::stream::iter(scenarios.iter().enumerate())
        .map(|(index, scenario)| async move {
            // Stagger the first sessions so their surfnets don't race for
            // the same free ports.
            if index < jobs {
                tokio::time::sleep(STAGGER * index as u32).await;
            }
            let bot_log = log_dir.join(format!("{}.log", scenario.name));
            session::run(SessionOptions {
                snapshot_dir,
                scenario,
                bot_dir,
                bot_log: &bot_log,
                slot: index % jobs,
                regime: regimes.and_then(|r| r.get(index)).map(String::as_str),
            })
            .await
        })
        .buffered(jobs);
    let mut sessions = std::pin::pin!(sessions);
    let mut reports = Vec::with_capacity(scenarios.len());
    while let Some(report) = sessions.next().await {
        let report = report?;
        let stop = stop_on_disqualify && disqualification(&report).is_some();
        reports.push(report);
        if stop {
            warn!(
                after = reports.len(),
                of = scenarios.len(),
                "disqualified: the grade is 0; stopping"
            );
            break;
        }
    }
    Ok(reports)
}

const STAGGER: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use mth_botkit::Mandate;

    use super::*;
    use crate::scenario::{VENUE, Waypoint};

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
            mandate: Mandate::perp(VENUE, "SPCX", 100.0, 1_000.0),
            instrument: "phoenix:perp:SPCX".parse().unwrap(),
            peak_position: 0.0,
            positions_outside_universe: 0,
            max_drawdown_usd: 0.0,
            valuation: "mark",
            flow: None,
            competitor: None,
            turns: Default::default(),
        }
    }

    /// One 10% move with a 100-unit cap at a $100 mark: a $1,000 bound.
    fn scenario(reference_pnl_usd: Option<f64>) -> Scenario {
        Scenario {
            name: "s".into(),
            description: String::new(),
            symbol: "SPCX".into(),
            duration_slots: 400,
            mandate: Mandate::perp(VENUE, "SPCX", 100.0, 1_000.0),
            path: vec![Waypoint {
                slot: 10,
                level_pct: -10.0,
            }],
            making: None,
            reference_pnl_usd,
        }
    }

    #[test]
    fn the_reward_is_pnl_over_the_hindsight_bound() {
        let s = scenario(Some(200.0));
        let report = score(vec![(session(300.0), &s), (session(-100.0), &s)]);
        assert_eq!(report.bound_pnl_usd, 2_000.0);
        assert_eq!(report.reward, 0.1);
        assert_eq!(report.reference_ratio, Some(0.5));

        assert_eq!(score(vec![(session(-50.0), &s)]).reward, 0.0);
        assert_eq!(score(vec![(session(5_000.0), &s)]).reward, 1.0);
        assert_eq!(
            score(vec![(session(10.0), &scenario(None))]).reference_ratio,
            None
        );
    }

    #[test]
    fn any_disqualified_session_zeroes_the_reward() {
        let s = scenario(None);
        let mut cheat = session(500.0);
        cheat.refused_calls = vec!["surfnet_setTokenAccount".into()];
        let report = score(vec![(session(100.0), &s), (cheat, &s)]);
        assert_eq!(report.reward, 0.0);
        assert!(report.scenarios[1].disqualified.is_some());

        let mut ruined = session(100.0);
        ruined.liquidatable = true;
        assert_eq!(score(vec![(ruined, &s)]).reward, 0.0);

        let mut big = session(900.0);
        big.peak_position = 119.0;
        let report = score(vec![(big, &s)]);
        assert_eq!(report.reward, 0.0);
        assert!(
            report.scenarios[0]
                .disqualified
                .as_deref()
                .unwrap()
                .contains("cap")
        );
    }

    #[test]
    fn drawing_down_past_the_limit_disqualifies() {
        let s = scenario(None);
        let mut deep = session(400.0);
        deep.max_drawdown_usd = 1_200.0;
        let report = score(vec![(session(100.0), &s), (deep, &s)]);
        assert_eq!(report.reward, 0.0);
        assert!(
            report.scenarios[1]
                .disqualified
                .as_deref()
                .unwrap()
                .contains("drew down")
        );

        let mut at_limit = session(100.0);
        at_limit.max_drawdown_usd = 1_000.0;
        assert!(score(vec![(at_limit, &s)]).reward > 0.0);
    }

    #[test]
    fn trading_outside_the_mandate_disqualifies() {
        let s = scenario(None);
        let mut elsewhere = session(300.0);
        elsewhere.positions_outside_universe = 1;
        let report = score(vec![(session(100.0), &s), (elsewhere, &s)]);
        assert_eq!(report.reward, 0.0);
        assert!(
            report.scenarios[1]
                .disqualified
                .as_deref()
                .unwrap()
                .contains("outside its mandate")
        );
    }

    #[test]
    fn risk_is_reported_across_sessions() {
        let s = scenario(None);
        let pnls = [300.0, -200.0, 100.0, 50.0, -50.0, 400.0];
        let mut sessions: Vec<(SessionReport, &Scenario)> =
            pnls.iter().map(|p| (session(*p), &s)).collect();
        sessions[1].0.max_drawdown_usd = 450.0;
        let risk = score(sessions).risk;
        assert_eq!(risk.worst_pnl_usd, -200.0);
        assert_eq!(risk.tail_pnl_usd, -200.0);
        assert_eq!(risk.max_drawdown_usd, 450.0);
        assert_eq!(risk.profitable_share, 4.0 / 6.0);
        assert!(risk.sharpe > 0.0 && risk.sharpe < 1.0, "{}", risk.sharpe);
    }

    #[test]
    fn sessions_whose_takers_failed_fail_the_grade() {
        let mut thin = session(10.0);
        thin.flow = Some(crate::flow::FlowReport {
            orders: 100,
            sent: 100,
            failed: 11,
            ..Default::default()
        });
        assert!(check_timing(&[session(1.0), thin.clone()]).is_err());
        thin.flow.as_mut().unwrap().failed = 10;
        assert!(check_timing(&[thin]).is_ok());
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
}
