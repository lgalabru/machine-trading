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
//!
//! A finished grade can be scored again from what it wrote, without
//! running a session: see [`rescore`].

use std::{collections::BTreeMap, path::Path};

use futures::StreamExt;
use mtx_core::MeterError;
use mtx_kit::core::{Decomposition, MarkoutMeter, MarkoutSample};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{
    Error, Result,
    error::io_error,
    keys::{RoleKeys, SessionKeys},
    scenario::Scenario,
    session::{self, SessionOptions, SessionReport, StepRecord, Timeline},
};

#[derive(Debug, Serialize)]
pub struct ScenarioResult {
    /// Why this session disqualifies the bot, if it does.
    pub disqualified: Option<String>,
    /// The hindsight bound for this session, at its starting mark.
    pub bound_pnl_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_pnl_usd: Option<f64>,
    /// The session's PnL split into markout, luck and a residual, for a
    /// scenario that reports a fair value (see [`Scenario::fair_path`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markout: Option<Decomposition>,
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
    /// Why the bot wasn't graded at all, if it wasn't (a grade of strategy
    /// components only, and no `bot.wasm`): the reward is 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_graded: Option<String>,
    /// The SHA-256 of the secret a drawn hidden set came from: enough to
    /// tell two grades on the same set, nothing more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_seed_sha256: Option<String>,
    pub risk: Risk,
    /// `mtx grade --replay`: whether every run came out the same.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay: Option<crate::replay::ReplayCheck>,
    /// The sessions' markout, luck and residual, summed, when every scenario
    /// reports a fair value. Reported, not scored: `reward` and `skill` are
    /// PnL's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markout: Option<Decomposition>,
    pub scenarios: Vec<ScenarioResult>,
    /// The reference's sessions, when the grade ran it: kept out of the
    /// report (its PnL is in [`ScenarioResult::reference_pnl_usd`]), for the
    /// replay check.
    #[serde(skip)]
    pub anchors: Vec<SessionReport>,
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
            (
                "not_graded".to_string(),
                f64::from(u8::from(self.not_graded.is_some())),
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
        if let Some(markout) = &self.markout {
            rewards.insert("markout_usd".to_string(), markout.markout_usd);
            rewards.insert("luck_usd".to_string(), markout.luck_usd);
            rewards.insert("edge_usd".to_string(), markout.edge_usd());
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
    /// The first of [`session::BOT_USERS`] this grade's bots run as; it
    /// uses `jobs` of them from there. Graders running at once on one
    /// machine never share a user at the same time (see
    /// [`session::BotUserLock`]); ranges that don't overlap keep them from
    /// waiting for each other.
    pub first_bot_user: usize,
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
    /// Where to write each session step by step, for review: one file per
    /// session, as `bot/<scenario>.json.gz` and `reference/<scenario>.json.gz`.
    /// They hold the scenarios themselves, so they're written once every
    /// session has ended: a bot can read the verifier's logs while it runs.
    pub detail_dir: Option<&'a Path>,
    /// What each scenario was drawn as (a shock's shape, which taker orders
    /// were informed), recorded in its detail file. Never handed to a bot.
    pub annotations: Option<&'a [serde_json::Value]>,
    /// Grade the bot only if it is a strategy component (a `bot.wasm`): the
    /// verifier's rule. The reference can still be a process.
    pub components_only: bool,
    /// The bot is trusted, as the reference is (an anchor or a ladder rung
    /// graded as the bot): see [`SessionOptions::trusted`].
    pub trusted: bool,
    /// Watches a component's sessions step by step (`mtx bot run --debug`).
    pub observer: Option<std::sync::Arc<dyn crate::wasm::StepObserver>>,
    /// Where the sessions' keypairs come from (see [`crate::keys`]): the same
    /// keys every run of the grade.
    pub keys: &'a SessionKeys,
}

/// One session's detail file (see [`GradeOptions::detail_dir`]).
#[derive(Serialize)]
struct SessionDetail<'a> {
    role: &'static str,
    scenario: &'a Scenario,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotation: Option<&'a serde_json::Value>,
    disqualified: Option<String>,
    bound_pnl_usd: f64,
    report: &'a SessionReport,
    timeline: &'a session::Timeline,
}

fn write_details(
    dir: &Path,
    role: &'static str,
    sessions: &[SessionReport],
    scenarios: &[Scenario],
    annotations: Option<&[serde_json::Value]>,
) -> Result<()> {
    let dir = dir.join(role);
    std::fs::create_dir_all(&dir).map_err(io_error(&dir))?;
    for (index, (report, scenario)) in sessions.iter().zip(scenarios).enumerate() {
        let detail = SessionDetail {
            role,
            scenario,
            annotation: annotations.and_then(|a| a.get(index)),
            disqualified: disqualification(report),
            bound_pnl_usd: scenario.bound_usd(report.start_mark_usd),
            report,
            timeline: &report.timeline,
        };
        // Gzipped: a trial writes two of these per session.
        let path = dir.join(format!("{}.json.gz", scenario.name));
        let file = std::fs::File::create(&path).map_err(io_error(&path))?;
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        serde_json::to_writer(&mut gz, &detail)?;
        gz.finish().map_err(io_error(&path))?;
    }
    Ok(())
}

pub async fn grade(opts: GradeOptions<'_>) -> Result<GradeReport> {
    if opts.components_only && crate::wasm::component_in(opts.bot_dir).is_none() {
        let reason = Error::NotAComponent(opts.bot_dir.to_path_buf()).to_string();
        tracing::warn!("{reason}: the reward is 0");
        return Ok(not_graded(reason));
    }
    let mut scenarios = opts.scenarios.to_vec();
    let mut anchored = Vec::new();
    if let Some(reference) = opts.reference_dir {
        let anchors = run_sessions(
            opts.snapshot_dir,
            reference,
            &scenarios,
            &opts.log_dir.join("reference"),
            opts.first_bot_user..opts.first_bot_user + opts.anchor_jobs,
            None,
            false,
            true,
            None,
            opts.keys.role("reference"),
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
        anchored = anchors;
    }
    let sessions = run_sessions(
        opts.snapshot_dir,
        opts.bot_dir,
        &scenarios,
        opts.log_dir,
        opts.first_bot_user..opts.first_bot_user + opts.jobs,
        opts.regimes,
        opts.stop_on_disqualify,
        opts.trusted,
        opts.observer.clone(),
        opts.keys.role("bot"),
    )
    .await?;
    check_timing(&sessions)?;
    // Every bot has stopped: the scenarios can be written out.
    if let Some(dir) = opts.detail_dir {
        // Whatever a bot left there goes first, links included: the
        // grader writes as root.
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(dir).map_err(io_error(dir))?,
            Ok(_) => std::fs::remove_file(dir).map_err(io_error(dir))?,
            Err(_) => {}
        }
        write_details(dir, "reference", &anchored, &scenarios, opts.annotations)?;
        write_details(dir, "bot", &sessions, &scenarios, opts.annotations)?;
    }
    let mut report = score(sessions.into_iter().zip(&scenarios).collect());
    report.anchors = anchored;
    info!(
        reward = report.reward,
        pnl_usd = report.pnl_usd,
        bound_pnl_usd = report.bound_pnl_usd,
        reference_ratio = report.reference_ratio,
        "graded"
    );
    Ok(report)
}

/// The report of a bot that wasn't graded at all: 0.
fn not_graded(reason: String) -> GradeReport {
    let mut report = score(Vec::new());
    report.not_graded = Some(reason);
    report.skill = Some(0.0);
    report
}

/// A session as its detail file holds it (see [`GradeOptions::detail_dir`]):
/// what [`rescore`] reads back.
#[derive(Debug, Deserialize)]
pub struct StoredSession {
    /// The scenario as the session played it, the reference's PnL in it.
    pub scenario: Scenario,
    pub report: SessionReport,
    pub timeline: StoredTimeline,
}

/// The part of a session's timeline scoring reads: the market and the
/// account at every step. The transactions and calls are left out.
#[derive(Debug, Deserialize)]
pub struct StoredTimeline {
    pub start_slot: u64,
    pub end_slot: u64,
    pub slots_per_step: u64,
    pub start_mark_usd: f64,
    pub steps: Vec<StepRecord>,
}

impl StoredSession {
    /// The session's report, its timeline's steps back in it, and its
    /// scenario.
    pub fn into_parts(self) -> (SessionReport, Scenario) {
        let StoredTimeline {
            start_slot,
            end_slot,
            slots_per_step,
            start_mark_usd,
            steps,
        } = self.timeline;
        let mut report = self.report;
        report.timeline = Timeline {
            start_slot,
            end_slot,
            slots_per_step,
            start_mark_usd,
            steps,
            ..Timeline::default()
        };
        (report, self.scenario)
    }
}

/// What a grade's report says beyond its sessions, read back from it for
/// [`rescore`].
#[derive(Debug, Default, Deserialize)]
pub struct Verdict {
    pub tampered: Option<String>,
    pub not_graded: Option<String>,
    pub eval_seed_sha256: Option<String>,
}

/// Score a finished grade again from what it wrote, without running a
/// session: its bot's sessions, in the report's order, each with the
/// scenario it played (see [`StoredSession`]), and what the report said
/// beyond them. With the scoring rules unchanged, the result is the grade's
/// report, but for a replay check's section and the reference's sessions;
/// with them changed, the grade the same sessions get under the new ones.
pub fn rescore(sessions: Vec<(SessionReport, Scenario)>, verdict: Verdict) -> GradeReport {
    let mut report = match verdict.not_graded {
        Some(reason) => not_graded(reason),
        None => {
            let (reports, scenarios): (Vec<_>, Vec<_>) = sessions.into_iter().unzip();
            score(reports.into_iter().zip(&scenarios).collect())
        }
    };
    report.eval_seed_sha256 = verdict.eval_seed_sha256;
    if let Some(reason) = verdict.tampered {
        report.tamper(reason);
    }
    report
}

/// Lift this process's soft limit on open files to its hard limit. Every
/// session's surfnet, RPC guards and clients live in this one process: with
/// several sessions at once, a container's default soft limit of 1,024 can
/// run out, and a request that can't open its socket fails the grade.
fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit only writes the struct given.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    // macOS refuses a soft limit above OPEN_MAX, and its hard limit is
    // usually unlimited.
    #[cfg(target_os = "macos")]
    let target = limit.rlim_max.min(10_240);
    #[cfg(not(target_os = "macos"))]
    let target = limit.rlim_max;
    if limit.rlim_cur >= target {
        return;
    }
    let raised = libc::rlimit {
        rlim_cur: target,
        ..limit
    };
    // SAFETY: setrlimit only reads the struct given.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
        warn!(soft = limit.rlim_cur, "couldn't raise the open-file limit");
    }
}

/// Score sessions against their scenarios' hindsight bounds.
fn score(sessions: Vec<(SessionReport, &Scenario)>) -> GradeReport {
    let scenarios: Vec<ScenarioResult> = sessions
        .into_iter()
        .map(|(session, scenario)| ScenarioResult {
            disqualified: disqualification(&session),
            bound_pnl_usd: scenario.bound_usd(session.start_mark_usd),
            reference_pnl_usd: scenario.reference_pnl_usd,
            markout: markout(&session, scenario),
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
        not_graded: None,
        eval_seed_sha256: None,
        risk: Risk::of(&scenarios),
        replay: None,
        markout: total_markout(&scenarios),
        scenarios,
        anchors: Vec::new(),
    }
}

/// A session's PnL split against its scenario's fair value, if the scenario
/// reports one at every step. The position is valued as the session valued
/// it: at the mark, the scenario's oracle level (market making) or its book
/// level.
fn markout(session: &SessionReport, scenario: &Scenario) -> Option<Decomposition> {
    let timeline = &session.timeline;
    let start = scenario.fair_value_usd(session.start_mark_usd, 0)?;
    let mut meter = MarkoutMeter::open(MarkoutSample {
        fair: start,
        price: session.start_mark_usd,
        position: 0.0,
        value: session.start_value_usd,
    });
    let valuation = scenario.valuation();
    for step in &timeline.steps {
        meter.record(MarkoutSample {
            fair: step.fair_usd?,
            price: match valuation {
                "oracle" => step.level_usd,
                "book" => step.book_usd?,
                _ => step.mark_usd,
            },
            position: step.position,
            value: step.value_usd,
        });
    }
    let at = timeline.end_slot.saturating_sub(timeline.start_slot);
    let start_mark = session.start_mark_usd;
    meter.record(MarkoutSample {
        fair: scenario.fair_value_usd(start_mark, at)?,
        price: match valuation {
            "oracle" => start_mark * (1.0 + crate::making::level_at(&scenario.path, at) / 100.0),
            "book" => start_mark * (1.0 + crate::making::book_at(&scenario.path, at) / 100.0),
            _ => session.end_mark_usd,
        },
        position: session.end_position,
        value: session.end_value_usd,
    });
    Some(meter.decomposition())
}

/// The sessions' decompositions added up, if every session has one.
fn total_markout(sessions: &[ScenarioResult]) -> Option<Decomposition> {
    if sessions.is_empty() {
        return None;
    }
    sessions.iter().map(|s| s.markout).try_fold(
        Decomposition {
            pnl_usd: 0.0,
            markout_usd: 0.0,
            luck_usd: 0.0,
            residual_usd: 0.0,
        },
        |total, next| {
            let next = next?;
            Some(Decomposition {
                pnl_usd: total.pnl_usd + next.pnl_usd,
                markout_usd: total.markout_usd + next.markout_usd,
                luck_usd: total.luck_usd + next.luck_usd,
                residual_usd: total.residual_usd + next.residual_usd,
            })
        },
    )
}

/// The share of a market-making session's taker orders that may fail before
/// the session isn't scored.
pub const MAX_FAILED_TAKER_SHARE: f64 = 0.1;

/// Fail the grade, as an infrastructure error, rather than score sessions
/// that didn't run as their scenarios describe. Harbor retries a failed
/// verifier; a skewed score would stand. A loaded machine alone no longer
/// shows up here: sessions run on no clock, so slots can't come late (see
/// [`crate::snapshot::start_offline_stepped`]).
fn check_timing(sessions: &[SessionReport]) -> Result<()> {
    let late: Vec<String> = sessions
        .iter()
        .filter_map(|s| {
            // A session whose library actors failed to act didn't see the
            // market its scenario describes either.
            if let Some(actors) = s.actors.as_ref().filter(|a| a.degraded()) {
                let actions = actors.independent();
                return Some(format!(
                    "{} ({} of {} actor actions failed)",
                    s.scenario,
                    actions.failed,
                    actions.total()
                ));
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
    } else if let Some(breach) = session.order.as_ref().and_then(|order| order.breach()) {
        Some(breach)
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
    first_bot_user: usize,
    jobs: usize,
) -> Result<Vec<SessionReport>> {
    let list: Vec<Scenario> = scenarios.iter().map(|(_, s)| s.clone()).collect();
    let sessions = run_sessions(
        snapshot_dir,
        bot_dir,
        &list,
        log_dir,
        first_bot_user..first_bot_user + jobs,
        None,
        false,
        true,
        None,
        SessionKeys::random().role("reference"),
    )
    .await?;
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

/// The most sessions of a strategy component a grade runs at once.
pub const MAX_COMPONENT_JOBS: usize = 32;

/// Run every scenario, `bot_users.len()` at a time, reports in scenario
/// order.
///
/// Concurrent sessions share nothing: each has its own surfnet, ports, RPC
/// guard, directory, keys (derived from the session, not the order it ran
/// in) and, for a component, its own store and fuel. So a grade comes out
/// the same whatever `jobs` is, and a machine too loaded for it grades
/// slower, not differently. A process bot also runs as its own user (see
/// [`session::BOT_USERS`]), one of `bot_users` that no other session
/// running holds (and no other grade on the machine: see
/// [`session::BotUserLock`]), so `jobs` is at most the number of users. A
/// component needs no user: up to [`MAX_COMPONENT_JOBS`] of its sessions run
/// at once. A new session starts as soon as any one ends. `trusted` bots
/// get [`SessionOptions::trusted`].
#[allow(clippy::too_many_arguments)]
async fn run_sessions(
    snapshot_dir: &Path,
    bot_dir: &Path,
    scenarios: &[Scenario],
    log_dir: &Path,
    bot_users: std::ops::Range<usize>,
    regimes: Option<&[String]>,
    stop_on_disqualify: bool,
    trusted: bool,
    observer: Option<std::sync::Arc<dyn crate::wasm::StepObserver>>,
    keys: RoleKeys<'_>,
) -> Result<Vec<SessionReport>> {
    let (first_bot_user, jobs) = (bot_users.start, bot_users.len());
    let components = crate::wasm::component_in(bot_dir).is_some();
    let most = if components {
        MAX_COMPONENT_JOBS
    } else {
        session::BOT_USERS.len()
    };
    if !(1..=most).contains(&jobs) {
        return Err(Error::Jobs(jobs, most));
    }
    if !components && bot_users.end > session::BOT_USERS.len() {
        return Err(Error::BotUsers {
            first: first_bot_user,
            jobs,
            users: session::BOT_USERS.len(),
        });
    }
    // A component may grow to its memory cap in every session: run no more
    // at once than the machine's memory holds, or the grader is killed.
    let jobs = if components {
        let limit = memory_limit();
        let fit = jobs_for_memory(jobs, limit);
        if fit < jobs {
            info!(
                jobs,
                fit,
                limit = ?limit,
                "fewer sessions at once: the memory holds no more"
            );
        }
        fit
    } else {
        jobs
    };
    std::fs::create_dir_all(log_dir).map_err(io_error(log_dir))?;
    raise_open_file_limit();
    // The bot users free for a process bot's sessions: each takes one while
    // it runs, so the ones running at once never share.
    let free = std::sync::Mutex::new(if components {
        Vec::new()
    } else {
        (first_bot_user..first_bot_user + jobs).rev().collect()
    });
    let sessions = futures::stream::iter(scenarios.iter().enumerate())
        .map(|(index, scenario)| {
            let observer = observer.clone();
            let free = &free;
            async move {
            let bot_log = log_dir.join(format!("{}.log", scenario.name));
            let user = UserSlot::take(free);
            let run = || {
                session::run(SessionOptions {
                    snapshot_dir,
                    scenario,
                    bot_dir,
                    bot_log: &bot_log,
                    slot: user.slot,
                    regime: regimes.and_then(|r| r.get(index)).map(String::as_str),
                    cranks: None,
                    trusted,
                    observer: observer.clone(),
                    keys: keys.session(index, &scenario.name),
                })
            };
            // Another grade on this machine may use the same bot user, and
            // a session's end kills everything its user runs: hold the user
            // while the session runs.
            let _held = match user.slot {
                Some(slot) => match session::BotUserLock::acquire(session::BOT_USERS[slot]).await {
                    Ok(held) => Some(held),
                    Err(error) => return (index, Err(error)),
                },
                None => None,
            };
            // A session whose setup fails on the machine (a request to its
            // own surfnet that can't get through on a loaded host) runs once
            // more, from a fresh surfnet. Its bot hadn't started, so it sees
            // the session once either way. A second failure fails the grade.
            let report = match run().await {
                Err(Error::Setup(error)) if is_transient(&error) => {
                    warn!(scenario = scenario.name, %error, "session setup failed on the machine; running it again");
                    run().await
                }
                done => done,
            };
            (index, report)
        }
        })
        // A new session starts whenever any one ends: a long session doesn't
        // hold up the ones behind it.
        .buffer_unordered(jobs);
    in_order(sessions, scenarios.len(), stop_on_disqualify).await
}

/// What a component's session may take in memory beyond the strategy's own
/// cap ([`crate::wasm::MEMORY_BYTES`]): its surfnet, guard, takers and the
/// grader's records. A market-making session, the largest, measures about
/// 0.8 GB; this leaves headroom.
pub const SESSION_WORLD_BYTES: u64 = 1 << 30;
/// What the grader holds apart from its sessions: the snapshot cache, the
/// scenarios, the reports so far.
pub const GRADER_BASE_BYTES: u64 = 3 << 29;

/// How many of a component's sessions fit in `limit` bytes of memory at
/// once, each at its strategy's cap (at least one, at most `jobs`).
fn jobs_for_memory(jobs: usize, limit: Option<u64>) -> usize {
    let Some(limit) = limit else {
        return jobs;
    };
    let per_session = crate::wasm::MEMORY_BYTES as u64 + SESSION_WORLD_BYTES;
    let fit = limit.saturating_sub(GRADER_BASE_BYTES) / per_session;
    (fit as usize).clamp(1, jobs)
}

/// The memory this process may use: its cgroup's limit (a container's, as
/// the verifier's), else the machine's.
fn memory_limit() -> Option<u64> {
    let cgroup = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .filter_map(|path| std::fs::read_to_string(path).ok())
    .find_map(|text| text.trim().parse::<u64>().ok());
    let total = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            let kb = text
                .lines()
                .find(|line| line.starts_with("MemTotal:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<u64>()
                .ok()?;
            Some(kb * 1024)
        });
    // cgroup v1 reports "no limit" as a huge number.
    match (cgroup, total) {
        (Some(cgroup), Some(total)) => Some(cgroup.min(total)),
        (cgroup, total) => cgroup.or(total),
    }
}

/// One of the free bot users, taken for a session and given back when it
/// ends (or is dropped). `None` for a component, which needs none.
struct UserSlot<'a> {
    free: &'a std::sync::Mutex<Vec<usize>>,
    slot: Option<usize>,
}

impl<'a> UserSlot<'a> {
    fn take(free: &'a std::sync::Mutex<Vec<usize>>) -> Self {
        let slot = free
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        Self { free, slot }
    }
}

impl Drop for UserSlot<'_> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            self.free
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(slot);
        }
    }
}

/// Sessions' reports as they end, `(index, report)`, put back in scenario
/// order. With `stop_on_disqualify`, the reports stop at the first
/// disqualifying session in that order, once every session before it has
/// ended: the same reports whatever order they ended in.
async fn in_order(
    sessions: impl futures::Stream<Item = (usize, Result<SessionReport>)>,
    count: usize,
    stop_on_disqualify: bool,
) -> Result<Vec<SessionReport>> {
    let mut sessions = std::pin::pin!(sessions);
    let mut done: Vec<Option<SessionReport>> = (0..count).map(|_| None).collect();
    let mut first_breach: Option<usize> = None;
    while let Some((index, report)) = sessions.next().await {
        let report = report?;
        if stop_on_disqualify && disqualification(&report).is_some() {
            first_breach = Some(first_breach.map_or(index, |first| first.min(index)));
        }
        done[index] = Some(report);
        if let Some(last) = first_breach
            && done[..=last].iter().all(Option::is_some)
        {
            warn!(
                after = last + 1,
                of = count,
                "disqualified: the grade is 0; stopping"
            );
            done.truncate(last + 1);
            break;
        }
    }
    Ok(done.into_iter().flatten().collect())
}

/// Errors from the grader's own machinery talking to its surfnet, as
/// opposed to a missing bot, an invalid scenario or a loaded machine
/// ([`Error::Degraded`]): worth one more try.
fn is_transient(error: &Error) -> bool {
    matches!(
        error,
        Error::Hawkeye(_)
            | Error::Rpc(_)
            | Error::Kit(_)
            | Error::Surfnet(_)
            | Error::Timeout(_)
            | Error::Transaction { .. }
            | Error::Meter(MeterError::Read(_))
    )
}

#[cfg(test)]
mod tests {
    use mtx_kit::{Mandate, mandate::PositionRange};

    use super::*;
    use crate::scenario::{VENUE, Waypoint};

    #[tokio::test]
    async fn a_components_only_grade_of_a_process_bot_scores_zero() {
        let dir = std::env::temp_dir().join(format!("mtx-not-a-component-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
        let report = grade(GradeOptions {
            snapshot_dir: &dir,
            bot_dir: &dir,
            scenarios: &[],
            log_dir: &dir,
            jobs: 1,
            first_bot_user: 0,
            reference_dir: None,
            anchor_jobs: 1,
            regimes: None,
            stop_on_disqualify: false,
            detail_dir: None,
            annotations: None,
            components_only: true,
            trusted: false,
            observer: None,
            keys: &SessionKeys::random(),
        })
        .await
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(report.reward, 0.0);
        assert_eq!(report.skill, Some(0.0));
        assert!(report.not_graded.is_some());
        assert_eq!(report.rewards()["not_graded"], 1.0);
    }

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
            positions: vec![],
            positions_outside_universe: 0,
            max_drawdown_usd: 0.0,
            valuation: "mark",
            flow: None,
            competitor: None,
            actors: None,
            spot: None,
            faults: None,
            order: None,
            participation: None,
            net: None,
            crank_refusals: Vec::new(),
            forced_closes: Vec::new(),
            turns: Default::default(),
            wall_ms: 0,
            timeline: Default::default(),
        }
    }

    #[test]
    fn details_are_written_gzipped_one_per_session() {
        use std::io::Read;
        let dir = std::env::temp_dir().join(format!("mtx-details-{}", uuid::Uuid::new_v4()));
        let annotations = [serde_json::json!({ "shape": "crash" })];
        write_details(
            &dir,
            "bot",
            &[session(5.0)],
            &[scenario(Some(2.0))],
            Some(&annotations),
        )
        .unwrap();
        let mut json = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(dir.join("bot/s.json.gz")).unwrap())
            .read_to_string(&mut json)
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(detail["role"], "bot");
        assert_eq!(detail["annotation"]["shape"], "crash");
        assert_eq!(detail["report"]["pnl_usd"], 5.0);
        assert_eq!(detail["scenario"]["reference_pnl_usd"], 2.0);
        assert!(detail["timeline"]["steps"].is_array());
        assert!(
            detail["report"].get("timeline").is_none(),
            "only once per file"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// One 10% move with a 100-unit cap at a $100 mark: a $1,000 bound.
    fn scenario(reference_pnl_usd: Option<f64>) -> Scenario {
        Scenario {
            name: "s".into(),
            description: String::new(),
            symbol: "SPCX".into(),
            duration_slots: 400,
            mandate: Mandate::perp(VENUE, "SPCX", 100.0, 1_000.0),
            path: vec![Waypoint::at(10, -10.0)],
            making: None,
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            actors: None,
            turn_data: None,
            oracles: None,
            order: None,
            participation: None,
            net: None,
            reference_pnl_usd,
            fair_path: None,
            market_model: None,
            raw_sends: None,
            stop_out: None,
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

    /// A session that buys 10 units at step 1 for $0.40 under fair value and
    /// sells them at step 2, with fair value 1% higher by then.
    fn traded_session() -> SessionReport {
        let step = |k: u64, mark: f64, fair: f64, position: f64, value: f64| session::StepRecord {
            k,
            at: k * 10,
            mark_usd: mark,
            index_usd: mark,
            level_usd: mark,
            book_usd: None,
            fair_usd: Some(fair),
            best_bid_usd: None,
            best_ask_usd: None,
            position,
            value_usd: value,
            pnl_usd: value - 10_100.0,
            turn_ms: None,
            answered: None,
        };
        let mut report = session(0.0);
        // Bought 10 at 99.60 (fair 100), held while fair rose to 101, sold
        // 10 at 100.90 (fair 101).
        report.end_mark_usd = 101.0;
        report.end_value_usd = 10_100.0 + 10.0 * (100.90 - 99.60);
        report.pnl_usd = report.end_value_usd - report.start_value_usd;
        report.timeline.start_slot = 1_000;
        report.timeline.end_slot = 1_020;
        report.timeline.steps = vec![
            step(0, 100.0, 100.0, 0.0, 10_100.0),
            step(1, 100.0, 100.0, 10.0, 10_100.0 + 10.0 * (100.0 - 99.60)),
            step(2, 101.0, 101.0, 0.0, report.end_value_usd),
        ];
        report
    }

    #[test]
    fn a_scenario_without_a_fair_value_grades_as_before() {
        let s = scenario(Some(200.0));
        let report = score(vec![(traded_session(), &s)]);
        assert!(report.markout.is_none());
        assert!(report.scenarios[0].markout.is_none());
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("markout") && !json.contains("fair"));
        assert!(!report.rewards().contains_key("edge_usd"));
    }

    #[test]
    fn a_scenario_with_a_fair_value_reports_where_its_pnl_came_from() {
        let mut s = scenario(Some(200.0));
        s.fair_path = Some(vec![Waypoint {
            slot: 15,
            level_pct: 1.0,
            premium_pct: 0.0,
        }]);
        let marked = score(vec![(traded_session(), &s)]);
        // The scenario's reward and ratios are PnL's, marked or not.
        s.fair_path = None;
        let plain = score(vec![(traded_session(), &s)]);
        assert_eq!(marked.reward, plain.reward);
        assert_eq!(marked.skill, plain.skill);
        assert_eq!(marked.reference_ratio, plain.reference_ratio);

        let m = marked.markout.unwrap();
        assert!((m.pnl_usd - 13.0).abs() < 1e-9);
        // Bought 10 at $0.40 under fair value (+4), sold at $0.10 under (-1).
        assert!((m.markout_usd - 3.0).abs() < 1e-9, "{m:?}");
        assert!((m.luck_usd - 10.0).abs() < 1e-9, "{m:?}");
        assert!(m.residual_usd.abs() < 1e-9);
        assert!((m.markout_usd + m.luck_usd + m.residual_usd - m.pnl_usd).abs() < 1e-9);
        let rewards = marked.rewards();
        assert!((rewards["edge_usd"] - 3.0).abs() < 1e-9);
        assert!((rewards["luck_usd"] - 10.0).abs() < 1e-9);
    }

    /// A grade's sessions as its detail files hold them, read back.
    fn stored(dir: &Path, names: &[&str]) -> Vec<(SessionReport, Scenario)> {
        use std::io::Read;
        names
            .iter()
            .map(|name| {
                let mut json = String::new();
                flate2::read::GzDecoder::new(
                    std::fs::File::open(dir.join(format!("bot/{name}.json.gz"))).unwrap(),
                )
                .read_to_string(&mut json)
                .unwrap();
                serde_json::from_str::<StoredSession>(&json)
                    .unwrap()
                    .into_parts()
            })
            .collect()
    }

    #[test]
    fn a_grade_scored_again_from_its_details_is_the_same_report() {
        let dir = std::env::temp_dir().join(format!("mtx-rescore-{}", uuid::Uuid::new_v4()));
        let mut marked = scenario(Some(200.37));
        marked.name = "marked".into();
        marked.fair_path = Some(vec![Waypoint {
            slot: 15,
            level_pct: 1.0,
            premium_pct: 0.0,
        }]);
        let mut traded = traded_session();
        traded.scenario = "marked".into();
        // PnL as the meter computes it: not short decimals.
        let mut odd = session(67.143_261_999_999_6);
        odd.end_value_usd = 10_167.143_262;
        odd.max_drawdown_usd = 39.389_592_999_999_88;
        odd.turns.fuel_used = Some(43_524_702_445);
        let mut breached = session(0.1 + 0.2);
        breached.scenario = "breached".into();
        breached.refused_calls = vec!["surfnet_setAccount".into()];
        let mut plain = scenario(Some(-13.07));
        plain.name = "breached".into();
        let sessions = [
            (odd, scenario(Some(2.0))),
            (traded, marked),
            (breached, plain),
        ];
        let scenarios: Vec<Scenario> = sessions.iter().map(|(_, s)| s.clone()).collect();
        let reports: Vec<SessionReport> = sessions.iter().map(|(r, _)| r.clone()).collect();
        write_details(&dir, "bot", &reports, &scenarios, None).unwrap();
        let mut graded = score(reports.into_iter().zip(&scenarios).collect());
        graded.eval_seed_sha256 = Some("ab".repeat(32));

        let verdict = Verdict {
            eval_seed_sha256: graded.eval_seed_sha256.clone(),
            ..Verdict::default()
        };
        let again = rescore(stored(&dir, &["s", "marked", "breached"]), verdict);
        assert_eq!(
            serde_json::to_string_pretty(&again).unwrap(),
            serde_json::to_string_pretty(&graded).unwrap()
        );
        assert!(again.markout.is_none() && again.scenarios[1].markout.is_some());
        assert_eq!(again.reward, 0.0, "the breach still disqualifies");

        // What the report said beyond its sessions carries over.
        let tampered = rescore(
            stored(&dir, &["s"]),
            Verdict {
                tampered: Some("a bot wrote reward.json".into()),
                ..Verdict::default()
            },
        );
        assert_eq!(tampered.reward, 0.0);
        assert!(tampered.tampered.is_some());
        let skipped = rescore(
            Vec::new(),
            Verdict {
                not_graded: Some("no bot.wasm".into()),
                ..Verdict::default()
            },
        );
        assert_eq!(skipped.skill, Some(0.0));
        std::fs::remove_dir_all(&dir).unwrap();
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
        big.positions = vec![PositionRange {
            instrument: big.instrument.clone(),
            lowest: -119.0,
            highest: 4.0,
        }];
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

    /// A drawdown past the limit is priced when the risk desk closed it,
    /// and still disqualifies without a close, or for another rule.
    #[test]
    fn a_stop_out_prices_the_drawdown_and_nothing_else() {
        use crate::{
            risk::{ForcedClose, ForcedCloseReason},
            scenario::StopRule,
        };
        let s = scenario(None);
        let mut stopped = session(-1_150.0);
        stopped.max_drawdown_usd = 1_300.0;
        stopped.forced_closes = vec![ForcedClose {
            reason: ForcedCloseReason::StopOut {
                rule: StopRule::Drawdown,
            },
            step: 40,
            at: 80,
            value_before_usd: 8_700.0,
            peak_usd: 10_000.0,
            value_after_usd: 8_650.0,
            position_before: 70.0,
            orders: Vec::new(),
            flat: true,
        }];
        let report = score(vec![(session(2_000.0), &s), (stopped.clone(), &s)]);
        assert!(report.scenarios[1].disqualified.is_none());
        assert_eq!(report.pnl_usd, 850.0, "the close's loss counts");
        assert!(report.reward > 0.0);

        let mut unclosed = stopped.clone();
        unclosed.forced_closes.clear();
        assert_eq!(score(vec![(unclosed, &s)]).reward, 0.0);

        let mut elsewhere = stopped;
        elsewhere.positions_outside_universe = 1;
        assert_eq!(score(vec![(elsewhere, &s)]).reward, 0.0);
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
        thin.flow = Some(crate::actors::FlowReport {
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
    fn sessions_whose_actors_failed_fail_the_grade() {
        use crate::actors::library::{Actions, ActorTally};
        let tally = |placed, failed, reacts_to_bot| ActorTally {
            actions: Actions {
                placed,
                failed,
                ..Default::default()
            },
            reacts_to_bot,
            ..Default::default()
        };
        let mut thin = session(10.0);
        thin.actors = Some(crate::actors::LibraryReport {
            actors: vec![tally(89, 11, false), tally(0, 500, true)],
            ..Default::default()
        });
        let error = check_timing(&[session(1.0), thin.clone()]).unwrap_err();
        assert!(error.to_string().contains("11 of 100 actor actions failed"));
        // What failed of the actors that act on the bot's trading is the
        // bot's doing: never the session's.
        thin.actors.as_mut().unwrap().actors = vec![tally(90, 10, false), tally(0, 500, true)];
        assert!(check_timing(&[thin]).is_ok());
    }

    #[test]
    fn a_slow_machine_doesnt_void_a_grade() {
        let mut slow = session(10.0);
        slow.wall_ms = 3_600_000;
        slow.turns.wall_ms = 1_000_000;
        assert!(check_timing(&[session(1.0), slow]).is_ok());
    }

    #[tokio::test]
    async fn bot_user_ranges_stay_within_the_users() {
        let logs = std::env::temp_dir().join(format!("mtx-logs-{}", uuid::Uuid::new_v4()));
        let keys = SessionKeys::random();
        let run = |first: usize, jobs: usize| {
            run_sessions(
                Path::new("."),
                Path::new("."),
                &[],
                &logs,
                first..first + jobs,
                None,
                false,
                false,
                None,
                keys.role("bot"),
            )
        };
        assert!(matches!(
            run(2, 4).await,
            Err(Error::BotUsers {
                first: 2,
                jobs: 4,
                users: 4
            })
        ));
        assert!(matches!(run(4, 1).await, Err(Error::BotUsers { .. })));
        assert!(run(2, 2).await.unwrap().is_empty());
        assert!(run(0, 4).await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&logs);
    }

    #[tokio::test]
    async fn a_components_sessions_run_many_at_once_with_no_bot_users() {
        let logs = std::env::temp_dir().join(format!("mtx-logs-{}", uuid::Uuid::new_v4()));
        let bot = std::env::temp_dir().join(format!("mtx-component-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&bot).unwrap();
        std::fs::write(bot.join(crate::wasm::BOT_WASM), b"").unwrap();
        let keys = SessionKeys::random();
        let run = |first: usize, jobs: usize| {
            run_sessions(
                Path::new("."),
                &bot,
                &[],
                &logs,
                first..first + jobs,
                None,
                false,
                false,
                None,
                keys.role("bot"),
            )
        };
        assert!(run(0, 16).await.unwrap().is_empty());
        assert!(run(3, MAX_COMPONENT_JOBS).await.unwrap().is_empty());
        assert!(matches!(
            run(0, MAX_COMPONENT_JOBS + 1).await,
            Err(Error::Jobs(_, MAX_COMPONENT_JOBS))
        ));
        assert!(matches!(run(0, 0).await, Err(Error::Jobs(0, _))));
        // A process bot's sessions still take one bot user each.
        std::fs::remove_file(bot.join(crate::wasm::BOT_WASM)).unwrap();
        assert!(matches!(run(0, 5).await, Err(Error::Jobs(5, 4))));
        let _ = std::fs::remove_dir_all(&bot);
        let _ = std::fs::remove_dir_all(&logs);
    }

    #[tokio::test]
    async fn reports_come_back_in_order_and_stop_at_the_first_breach_in_it() {
        let named = |index: usize, breach: bool| {
            let mut report = session(index as f64);
            report.scenario = format!("s{index}");
            if breach {
                report.refused_calls = vec!["surfnet_setAccount".into()];
            }
            (index, Ok(report))
        };
        let names = |reports: Vec<SessionReport>| -> Vec<String> {
            reports.into_iter().map(|r| r.scenario).collect()
        };
        let ended = vec![
            named(2, false),
            named(0, false),
            named(3, false),
            named(1, false),
        ];
        let all = in_order(futures::stream::iter(ended), 4, true)
            .await
            .unwrap();
        assert_eq!(names(all), ["s0", "s1", "s2", "s3"]);
        // Session 3 breached first, then 1 did: the reports end at 1, as
        // they would one at a time.
        let ended = vec![
            named(3, true),
            named(0, false),
            named(1, true),
            named(2, false),
        ];
        let stopped = in_order(futures::stream::iter(ended), 4, true)
            .await
            .unwrap();
        assert_eq!(names(stopped), ["s0", "s1"]);
        // Without stopping, every report comes back.
        let ended = vec![named(1, true), named(0, false)];
        let kept = in_order(futures::stream::iter(ended), 2, false)
            .await
            .unwrap();
        assert_eq!(names(kept), ["s0", "s1"]);
    }

    #[test]
    fn a_components_sessions_at_once_fit_the_memory() {
        let gib = 1u64 << 30;
        // The verifier's 6 GiB: two sessions at their cap.
        assert_eq!(jobs_for_memory(4, Some(6 * gib)), 2);
        assert_eq!(jobs_for_memory(4, Some(64 * gib)), 4);
        assert_eq!(jobs_for_memory(32, Some(64 * gib)), 31);
        // Never none, and never more than asked.
        assert_eq!(jobs_for_memory(4, Some(gib)), 1);
        assert_eq!(jobs_for_memory(1, Some(64 * gib)), 1);
        assert_eq!(jobs_for_memory(8, None), 8);
        assert!(memory_limit().is_none_or(|limit| limit > 0));
    }

    #[test]
    fn only_the_machines_failures_are_retried() {
        assert!(is_transient(&Error::Timeout("slot 9".into())));
        assert!(is_transient(&Error::Transaction {
            label: "onboard".into(),
            reason: "Blockhash not found".into(),
        }));
        assert!(is_transient(&Error::Meter(MeterError::Read(
            "connection refused".into()
        ))));
        assert!(!is_transient(&Error::Meter(MeterError::Decode {
            what: "margin",
            detail: "short".into(),
        })));
        assert!(!is_transient(&Error::MissingBot("bot".into())));
        assert!(!is_transient(&Error::Degraded("failed takers".into())));
        assert!(!is_transient(&Error::UnknownMarket("XYZ".into())));
    }

    #[test]
    fn a_setup_failure_reads_as_its_cause() {
        let error = Error::Setup(Box::new(Error::Timeout("slot 9".into())));
        assert_eq!(error.to_string(), "timed out waiting for slot 9");
    }

    #[test]
    fn the_open_file_limit_never_drops() {
        let limit = || {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: getrlimit only writes the struct given.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
                0
            );
            limit
        };
        let before = limit();
        raise_open_file_limit();
        let after = limit();
        assert!(after.rlim_cur >= before.rlim_cur);
        // Linux takes any soft limit up to the hard one.
        #[cfg(target_os = "linux")]
        assert_eq!(after.rlim_cur, after.rlim_max);
    }
}
