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
use mtx_core::MeterError;
use mtx_kit::core::{Decomposition, MarkoutMeter, MarkoutSample};
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
    /// The SHA-256 of the secret a drawn hidden set came from: enough to
    /// tell two grades on the same set, nothing more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_seed_sha256: Option<String>,
    pub risk: Risk,
    /// The sessions' markout, luck and residual, summed, when every scenario
    /// reports a fair value. Reported, not scored: `reward` and `skill` are
    /// PnL's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markout: Option<Decomposition>,
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
    /// machine need ranges that don't overlap: a session's end kills every
    /// process of its bot user.
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
        return Err(Error::NotAComponent(opts.bot_dir.to_path_buf()));
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
        eval_seed_sha256: None,
        risk: Risk::of(&scenarios),
        markout: total_markout(&scenarios),
        scenarios,
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
    )
    .await?;
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

/// Run every scenario, one per bot user in `bot_users` at a time, reports in
/// scenario order.
///
/// Concurrent sessions each get their own surfnet, and run their bots as
/// different users (see [`session::BOT_USERS`]). Sessions are kept in an
/// ordered window of `jobs` (the range's length), so the ones running at
/// once always have distinct `bot_users.start + index % jobs` slots. A
/// machine too loaded for `jobs` shows up as late slots, which fail the
/// grade rather than skew it.
async fn run_sessions(
    snapshot_dir: &Path,
    bot_dir: &Path,
    scenarios: &[Scenario],
    log_dir: &Path,
    bot_users: std::ops::Range<usize>,
    regimes: Option<&[String]>,
    stop_on_disqualify: bool,
) -> Result<Vec<SessionReport>> {
    let (first_bot_user, jobs) = (bot_users.start, bot_users.len());
    if !(1..=session::BOT_USERS.len()).contains(&jobs) {
        return Err(Error::Jobs(jobs, session::BOT_USERS.len()));
    }
    if bot_users.end > session::BOT_USERS.len() {
        return Err(Error::BotUsers {
            first: first_bot_user,
            jobs,
            users: session::BOT_USERS.len(),
        });
    }
    std::fs::create_dir_all(log_dir).map_err(io_error(log_dir))?;
    raise_open_file_limit();
    let sessions = futures::stream::iter(scenarios.iter().enumerate())
        .map(|(index, scenario)| async move {
            // Stagger the first sessions so their surfnets don't race for
            // the same free ports.
            if index < jobs {
                tokio::time::sleep(STAGGER * index as u32).await;
            }
            let bot_log = log_dir.join(format!("{}.log", scenario.name));
            let run = || {
                session::run(SessionOptions {
                    snapshot_dir,
                    scenario,
                    bot_dir,
                    bot_log: &bot_log,
                    slot: first_bot_user + index % jobs,
                    regime: regimes.and_then(|r| r.get(index)).map(String::as_str),
                })
            };
            // A session whose setup fails on the machine (a request to its
            // own surfnet that can't get through on a loaded host) runs once
            // more, from a fresh surfnet. Its bot hadn't started, so it sees
            // the session once either way. A second failure fails the grade.
            match run().await {
                Err(Error::Setup(error)) if is_transient(&error) => {
                    warn!(scenario = scenario.name, %error, "session setup failed on the machine; running it again");
                    run().await
                }
                done => done,
            }
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
            spot: None,
            faults: None,
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
            reference_pnl_usd,
            fair_path: None,
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

    #[tokio::test]
    async fn bot_user_ranges_stay_within_the_users() {
        let logs = std::env::temp_dir().join(format!("mtx-logs-{}", uuid::Uuid::new_v4()));
        let run = |first: usize, jobs: usize| {
            run_sessions(
                Path::new("."),
                Path::new("."),
                &[],
                &logs,
                first..first + jobs,
                None,
                false,
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
        assert!(!is_transient(&Error::Degraded("late slots".into())));
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
