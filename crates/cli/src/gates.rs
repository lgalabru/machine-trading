//! `mtx gates <task>`: the quality bar as one report.
//!
//! The checks a task must pass already exist as pieces: `task status`, the
//! Harbor checks, the skill ladder, the leak audit and the probes. This module
//! runs them and folds the results into one report under the quality bar's
//! headings (JSON and markdown):
//!
//! - **Solvable and fair**: the reference scores 1.0 and doing nothing 0, the
//!   skill ladder keeps its order, and a policy that reads what the task is
//!   about beats the best one that doesn't by a margin.
//! - **Graded correctly**: the exploit bots stay within their limits, the
//!   cheaters are disqualified, nothing leaks, replays are exact.
//! - **Realistic**: the market matches its calibration file.
//!
//! `--fast` runs the cheap ones (files, a one-set ladder on a few sessions);
//! the full run adds the image checks. A check whose tool isn't in this build
//! yet (replay, market calibration) reports `not_run`, which never fails the
//! gate: `mtx gates` grows as those land.
//!
//! A task whose headline is scoring v2 (`headline = "v2"`) also needs both
//! anchors and a reference in the strength band (GRADING_DECISIONS D1): the
//! task-files checks refuse it without an informed bot, and the ladder's
//! sets measure the band.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use clap::CommandFactory;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Cli, authoring, scoring_text, tasks};

/// The ladder's extra gate: the best informed policy must make at least this
/// many times the best one that doesn't read the task's signal.
const DEFAULT_MIN_RATIO: f64 = 2.0;
/// The strength band by default: the reference's weighted PnL as a share of
/// the informed bot's, so there is room above and below 0.5.
const DEFAULT_BAND: [f64; 2] = [0.2, 0.7];
/// The least per-session Sharpe the reference may have (t ≈ 2.1 over 72
/// sessions), by default.
const DEFAULT_MIN_SHARPE: f64 = 0.25;
/// Fast mode: one hidden set of this many sessions per rung.
const FAST_SESSIONS: usize = 12;
/// A short set can't carry the full run's bootstrap floor; the order must
/// still hold on the set itself.
const FAST_RULE_ODDS: f64 = 0.5;
/// The replay check's sessions, runs and loaded threads: fast, then full.
const REPLAY_FAST: (usize, u32, usize) = (2, 2, 0);
const REPLAY_FULL: (usize, u32, usize) = (6, 3, 4);
/// Fast mode's ladder draws its set from this public label, never a hidden
/// set's secret: the same sessions on every run, so a verdict doesn't flip
/// with the draw.
const FAST_PUBLIC_SEED: &str = "gates-fast-v1";
/// On a fast set, an exploit bot fails only when it is over a limit with
/// this much confidence (a one-sided binomial test on its profitable share,
/// a paired bootstrap on its total against the `below` rung); otherwise the
/// check is inconclusive at that size.
const FAST_EXPLOIT_CONFIDENCE: f64 = 0.95;
/// The snapshot when the Justfile doesn't name one.
const DEFAULT_SNAPSHOT: &str = "phoenix-spcx-2026-10-29";
/// How much of a failing command's output the report keeps.
const TAIL_LINES: usize = 6;

const SOLVABLE: &str = "Solvable and fair";
const GRADED: &str = "Graded correctly";
const REALISTIC: &str = "Realistic";
const COMPLETE: &str = "Task files";
const SECTIONS: [&str; 4] = [COMPLETE, SOLVABLE, GRADED, REALISTIC];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    /// The check can't run here (a tool that isn't built yet, or full mode).
    NotRun,
    /// It ran, but the sample is too small to decide: never fails the gate.
    Inconclusive,
    /// A defect the task is allowed to keep (a v1 task's practice
    /// descriptions state a latent): never fails the gate, and is never
    /// reported as a pass.
    Warn,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::NotRun => "not run",
            Status::Inconclusive => "inconclusive",
            Status::Warn => "WARN",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub section: &'static str,
    pub title: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn new(
        section: &'static str,
        id: &'static str,
        title: &'static str,
        status: Status,
        detail: impl Into<String>,
    ) -> Self {
        Check {
            id,
            section,
            title,
            status,
            detail: detail.into(),
        }
    }

    fn pass(
        section: &'static str,
        id: &'static str,
        title: &'static str,
        d: impl Into<String>,
    ) -> Self {
        Self::new(section, id, title, Status::Pass, d)
    }

    fn fail(
        section: &'static str,
        id: &'static str,
        title: &'static str,
        d: impl Into<String>,
    ) -> Self {
        Self::new(section, id, title, Status::Fail, d)
    }

    fn warn(
        section: &'static str,
        id: &'static str,
        title: &'static str,
        d: impl Into<String>,
    ) -> Self {
        Self::new(section, id, title, Status::Warn, d)
    }

    fn not_run(
        section: &'static str,
        id: &'static str,
        title: &'static str,
        d: impl Into<String>,
    ) -> Self {
        Self::new(section, id, title, Status::NotRun, d)
    }

    fn from(
        section: &'static str,
        id: &'static str,
        title: &'static str,
        ok: bool,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            section,
            id,
            title,
            if ok { Status::Pass } else { Status::Fail },
            detail,
        )
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub task: String,
    pub mode: &'static str,
    pub commit: String,
    pub passed: bool,
    pub counts: BTreeMap<&'static str, usize>,
    pub checks: Vec<Check>,
}

impl Report {
    fn new(task: &str, mode: &'static str, commit: String, checks: Vec<Check>) -> Self {
        let mut counts = BTreeMap::new();
        for c in &checks {
            let key = match c.status {
                Status::Pass => "pass",
                Status::Fail => "fail",
                Status::NotRun => "not_run",
                Status::Inconclusive => "inconclusive",
                Status::Warn => "warn",
            };
            *counts.entry(key).or_insert(0) += 1;
        }
        Report {
            task: task.to_string(),
            mode,
            commit,
            passed: !checks.iter().any(|c| c.status == Status::Fail),
            counts,
            checks,
        }
    }

    /// The report for people: one table per section of the quality bar.
    pub fn markdown(&self) -> String {
        let count = |k: &str| self.counts.get(k).copied().unwrap_or(0);
        let mut out = format!(
            "# Gates: {}\n\n{} mode, commit `{}`. **{}**: {} passed, {} failed, {} warned, {} inconclusive, {} not run.\n",
            self.task,
            self.mode,
            self.commit,
            if self.passed { "PASSED" } else { "FAILED" },
            count("pass"),
            count("fail"),
            count("warn"),
            count("inconclusive"),
            count("not_run"),
        );
        for section in SECTIONS {
            let rows: Vec<&Check> = self
                .checks
                .iter()
                .filter(|c| c.section == section)
                .collect();
            if rows.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "\n## {section}\n\n| | Check | Detail |\n| --- | --- | --- |\n"
            ));
            for c in rows {
                out.push_str(&format!(
                    "| {} | {} | {} |\n",
                    c.status.label(),
                    c.title,
                    c.detail.replace('|', "\\|").replace('\n', " ")
                ));
            }
        }
        out
    }
}

/// How a run is sized and where it keeps its files.
#[derive(Debug, Clone)]
pub struct Options {
    pub fast: bool,
    /// Hidden sets and sessions per set for the ladder; unset is the mode's
    /// default (fast: one of 12; full: four, the task's own count).
    pub sets: Option<usize>,
    pub sessions: Option<usize>,
    pub out: Option<PathBuf>,
    /// Read a finished ladder's `summary.json` instead of running one.
    pub ladder_summary: Option<PathBuf>,
}

/// What `ladder.py` leaves in `summary.json`.
#[derive(Debug, Deserialize)]
pub struct LadderSummary {
    #[serde(default)]
    pub sessions: usize,
    /// Per set, each rung's total PnL as graded (0 if disqualified), and the
    /// set's `hindsight` bound.
    pub totals: Vec<BTreeMap<String, f64>>,
    pub rules: Vec<RuleResult>,
    pub exploits: Vec<ExploitResult>,
    pub disqualified: Vec<Disqualified>,
    #[serde(default)]
    pub probes: Option<Vec<ProbeResult>>,
    /// `checked`, `exempt` (the ladder file says why: `probes_exempt`) or
    /// `absent`: the task declares nothing about the probe gate.
    #[serde(default)]
    pub probes_status: Option<String>,
    #[serde(default)]
    pub probes_exempt: Option<String>,
    /// Per set, each rung's PnL in each session, as graded (absent in older
    /// summaries).
    #[serde(default)]
    pub pnls: Vec<BTreeMap<String, Vec<f64>>>,
}

#[derive(Debug, Deserialize)]
pub struct RuleResult {
    pub rule: String,
    pub ok: bool,
    pub sets_holding: usize,
    pub sets: usize,
    pub odds: f64,
}

#[derive(Debug, Deserialize, Default)]
pub struct ExploitResult {
    pub bot: String,
    pub ok: bool,
    /// Null when the reference lost money on the sets: no ratio to take.
    #[serde(default)]
    pub ratio: Option<f64>,
    pub profitable: f64,
    pub reasons: Vec<String>,
    /// The sample behind the verdict (absent in older summaries).
    #[serde(default)]
    pub sessions: Option<usize>,
    #[serde(default)]
    pub profitable_sessions: Option<usize>,
    #[serde(default)]
    pub max_profitable: Option<f64>,
    #[serde(default)]
    pub total: Option<f64>,
    #[serde(default)]
    pub below: Option<String>,
    #[serde(default)]
    pub below_total: Option<f64>,
    /// How often a resampled set puts it at or above the `below` rung.
    #[serde(default)]
    pub below_odds: Option<f64>,
    #[serde(default)]
    pub reached_bound: Option<bool>,
    /// `pass`, `fail` or `no_power`: a pushing bot that moved the mark too
    /// little proves nothing by losing.
    #[serde(default)]
    pub verdict: Option<String>,
    #[serde(default)]
    pub no_power: Option<String>,
}

/// One probe rule's verdict (scoring v2's probe gate, `ladder.py`'s
/// `probe_checks`): `pass`, `fail`, `inconclusive` (the point estimate
/// breaks it, the sample can't confirm that) or `no_power` (a probe that was
/// to breach never did).
#[derive(Debug, Deserialize)]
pub struct ProbeResult {
    pub id: String,
    pub verdict: String,
    pub detail: String,
    /// The point estimate breaks the rule, whatever the sample can confirm:
    /// the full gate fails on it.
    #[serde(default)]
    pub point_violated: bool,
}

#[derive(Debug, Deserialize)]
pub struct Disqualified {
    pub rung: String,
    pub scenarios: Vec<String>,
}

/// A task's declaration of what its signal is, in its ladder file: rungs that
/// read it (`informed`) and rungs that trade without it (`non_reading`).
#[derive(Debug, Default, Deserialize)]
pub struct SignalGate {
    #[serde(default)]
    pub informed: Vec<String>,
    #[serde(default)]
    pub non_reading: Vec<String>,
    pub min_ratio: Option<f64>,
}

/// A v2 task's anchors, in its ladder file: the rungs that are its 0.5 and
/// its 1, and the strength band its reference must clear.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anchors {
    pub reference: String,
    pub informed: String,
    /// The reference's weighted PnL as a share of the informed bot's:
    /// `[low, high]` (default 0.2 to 0.7).
    pub band: Option<[f64; 2]>,
    /// The reference's least per-session Sharpe (default 0.25).
    pub min_sharpe: Option<f64>,
}

/// Each session's PnL of `rung`, over every set, or why there's none.
fn session_pnls(summary: &LadderSummary, rung: &str) -> Result<Vec<f64>, String> {
    if summary.pnls.len() != summary.totals.len() || summary.pnls.is_empty() {
        return Err(
            "the ladder's summary has no per-session PnL: rerun it with this build's `just ladder`"
                .into(),
        );
    }
    let mut out = Vec::new();
    for set in &summary.pnls {
        out.extend(
            set.get(rung)
                .ok_or_else(|| format!("no rung `{rung}` in the ladder"))?,
        );
    }
    Ok(out)
}

/// The mean over the standard deviation (n − 1); 0 for fewer than two, and
/// unbounded for a constant positive series.
fn sharpe(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let sd = (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
    if sd > 0.0 {
        mean / sd
    } else if mean > 0.0 {
        f64::INFINITY
    } else {
        0.0
    }
}

/// The reference is a fit 0.5 (D1): on the ladder's sets, with losses
/// weighted as v2 weighs them, the informed bot beats it, its weighted PnL
/// is within the band of the informed bot's (and so above `φ` of it: never
/// `anchor_floored`), its per-session Sharpe clears the floor, and neither
/// anchor broke its mandate. A fast set can't settle the numbers: there a
/// miss is inconclusive, and the full ladder decides.
pub fn strength_band(
    summary: &LadderSummary,
    anchors: &Anchors,
    settings: &scoring_text::Settings,
    fast: bool,
) -> Check {
    let (id, title) = (
        "strength-band",
        "The reference is a fit 0.5 against the informed bot (strength band)",
    );
    let (reference, informed) = match (
        session_pnls(summary, &anchors.reference),
        session_pnls(summary, &anchors.informed),
    ) {
        (Ok(r), Ok(i)) if r.len() == i.len() => (r, i),
        (Ok(_), Ok(_)) => {
            return Check::fail(
                SOLVABLE,
                id,
                title,
                "the anchors played different numbers of sessions",
            );
        }
        (Err(e), _) | (_, Err(e)) => return Check::fail(SOLVABLE, id, title, e),
    };
    let broke: Vec<String> = summary
        .disqualified
        .iter()
        .filter(|d| d.rung == anchors.reference || d.rung == anchors.informed)
        .map(|d| format!("{} broke its mandate in {}", d.rung, d.scenarios.join(", ")))
        .collect();
    if !broke.is_empty() {
        return Check::fail(SOLVABLE, id, title, broke.join("; "));
    }
    let k = settings.loss_weight;
    let weigh = |xs: &[f64]| -> f64 { xs.iter().map(|&x| if x < 0.0 { k * x } else { x }).sum() };
    let (e_ref, e_inf) = (weigh(&reference), weigh(&informed));
    let [low, high] = anchors.band.unwrap_or(DEFAULT_BAND);
    let min_sharpe = anchors.min_sharpe.unwrap_or(DEFAULT_MIN_SHARPE);
    let phi = settings.floor();
    let ratio = if e_inf > 0.0 { e_ref / e_inf } else { f64::NAN };
    let s = sharpe(&reference);
    let mut misses = Vec::new();
    if e_inf <= e_ref {
        misses.push(format!(
            "the informed bot (${e_inf:.0}) doesn't beat the reference (${e_ref:.0}): v2 refuses to score"
        ));
    } else if e_ref < phi * e_inf {
        misses.push(format!(
            "anchor_floored: the reference makes {ratio:.2} of the informed bot, under φ = {phi}"
        ));
    }
    if !(ratio >= low && ratio <= high) && e_inf > e_ref {
        misses.push(format!(
            "the reference makes {ratio:.2} of the informed bot, outside [{low}, {high}]"
        ));
    }
    if s < min_sharpe {
        misses.push(format!(
            "the reference's per-session Sharpe is {s:.2}, under {min_sharpe}"
        ));
    }
    let measured = format!(
        "weighted (k = {k}) over {} sessions: reference ${e_ref:.0}, informed ${e_inf:.0}, ratio {ratio:.2} in [{low}, {high}]; reference Sharpe {s:.2} (at least {min_sharpe})",
        reference.len()
    );
    match (misses.is_empty(), fast) {
        (true, _) => Check::pass(SOLVABLE, id, title, measured),
        (false, true) => Check::new(
            SOLVABLE,
            id,
            title,
            Status::Inconclusive,
            format!(
                "{}. A fast set can't settle the band; the full ladder decides",
                misses.join("; ")
            ),
        ),
        (false, false) => Check::fail(
            SOLVABLE,
            id,
            title,
            format!("{}. {measured}", misses.join("; ")),
        ),
    }
}

/// The checks only a task scored by v2 has: its strength band, and the
/// verifier running its informed bot on the hidden set (no fallback to the
/// hindsight ceiling; see [`informed_on_hidden_sets`]). A v1 task reports
/// them not run.
pub fn v2_checks(
    root: &Path,
    task: &str,
    headline: tasks::Headline,
    ladder: &Value,
    summary: Option<&LadderSummary>,
    test: &str,
    fast: bool,
) -> Vec<Check> {
    let band = (
        "strength-band",
        "The reference is a fit 0.5 against the informed bot (strength band)",
    );
    if headline == tasks::Headline::V1 {
        let why = "a v1 task (one of the four that move to v2 in 0.13)";
        return vec![
            Check::not_run(SOLVABLE, band.0, band.1, why),
            Check::not_run(SOLVABLE, INFORMED_LIVE.0, INFORMED_LIVE.1, why),
        ];
    }
    let mut checks = Vec::new();
    let settings = scoring_text::Settings::of_verifier(test);
    let anchors = ladder
        .get("anchors")
        .map(|a| serde_json::from_value::<Anchors>(a.clone()));
    // The band certifies the task's own bots, or nothing: rungs named
    // `reference` and `informed` that run anything else aren't anchors.
    let foreign = authoring::anchor_rung_problems(ladder, task);
    checks.push(match (anchors, settings, summary) {
        (Some(Ok(_)), _, _) if !foreign.is_empty() => Check::fail(
            SOLVABLE,
            band.0,
            band.1,
            format!("the anchors aren't the task's own: {}", foreign.join("; ")),
        ),
        (None, _, _) => Check::fail(
            SOLVABLE,
            band.0,
            band.1,
            "the ladder file names no `anchors`: a v2 task needs its reference and informed bot",
        ),
        (Some(Err(e)), _, _) => Check::fail(
            SOLVABLE,
            band.0,
            band.1,
            format!("ladder.json's `anchors`: {e}"),
        ),
        (_, Err(e), _) => Check::fail(SOLVABLE, band.0, band.1, format!("{e:#}")),
        (_, _, None) => Check::fail(SOLVABLE, band.0, band.1, "the ladder didn't run"),
        (Some(Ok(anchors)), Ok(settings), Some(summary)) => {
            strength_band(summary, &anchors, &settings, fast)
        }
    });
    checks.push(informed_on_hidden_sets(root, task, test));
    checks
}

/// No task's verifier tells the graded bot anything about its sessions:
/// `--tell-latent` and `--tell-regime` are the ladder's, for its informed
/// rung and its oracles. In a verifier they would hand the agent's bot what
/// only the informed bot may know, or the session's future.
pub fn bot_told_nothing(test: &str) -> Check {
    let (id, title) = (
        "bot-told-nothing",
        "The verifier tells the graded bot nothing",
    );
    let told: Vec<&str> = ["--tell-latent", "--tell-regime"]
        .into_iter()
        .filter(|flag| scoring_text::has_flag(test, flag))
        .collect();
    if told.is_empty() {
        Check::pass(
            GRADED,
            id,
            title,
            "tests/test.sh passes neither `--tell-latent` nor `--tell-regime`",
        )
    } else {
        Check::fail(
            GRADED,
            id,
            title,
            format!(
                "tests/test.sh passes {}: the graded bot would be told what only the ladder's rungs may know",
                told.join(" and ")
            ),
        )
    }
}

/// No practice file in the agent's image states the session's latent state.
/// Practice goes into the image whole (Docker copies `practice/` with its
/// subdirectories), and the informed bot is run on it host-side
/// (`mtx calibrate --practice`), which saves only its PnL. Every file must
/// parse as a scenario of its schema version (`scenario_v2::parse`), which refuses
/// members it doesn't know (a `latent` among them), and nothing else may be
/// there; no `latent`, `roster`, `roles` or `wallets` member sits outside `actors`
/// (where `informed` and `worker` are the library's own configuration); and
/// no string names a latent field of the family or an account. A v2 task's
/// description is the generator's neutral one: the generator's own describes
/// the draw ("informed flow from account 4", the process's parameters). A v1
/// task, unguarded, may keep the generator's: that is a warning, never a pass.
pub fn practice_holds_no_latent(
    root: &Path,
    task: &str,
    family: &str,
    headline: tasks::Headline,
) -> Check {
    let (id, title) = (
        "practice-no-latent",
        "No practice file states the latent state or a roster",
    );
    let dir = root
        .join("bench/tasks")
        .join(task)
        .join("environment/practice");
    let generator = mtx_grader::generators::generator(family);
    let mut files = Vec::new();
    collect_files(&dir, &mut files);
    // An empty file (the scaffold's `.gitkeep`) holds nothing to state.
    files.retain(|p| std::fs::metadata(p).is_ok_and(|m| m.len() > 0));
    files.sort();
    let name_of = |p: &Path| {
        p.strip_prefix(&dir)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    };
    let (mut errors, mut told) = (Vec::new(), Vec::new());
    for path in &files {
        let name = name_of(path);
        let Ok(text) = std::fs::read_to_string(path) else {
            errors.push(format!("{name} is not text"));
            continue;
        };
        let scenario = match mtx_grader::scenario_v2::parse(&text) {
            Ok(s) if path.extension().is_some_and(|e| e == "json") => s,
            Ok(_) => {
                errors.push(format!("{name} is not a .json scenario file"));
                continue;
            }
            Err(e) => {
                errors.push(format!("{name} is not a scenario ({e})"));
                continue;
            }
        };
        let keys = mtx_grader::generators::latent_keys_in(&text);
        if !keys.is_empty() {
            errors.push(format!(
                "{name} holds {}",
                keys.iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let text_hits = mtx_grader::generators::latent_text_in(family, &text);
        let neutral = generator
            .is_some_and(|g| scenario.description == g.neutral_description(&scenario.symbol));
        if !text_hits.is_empty() {
            told.push(format!("{name} states {}", text_hits.join(", ")));
        } else if !neutral && headline == tasks::Headline::V2 {
            told.push(format!(
                "{name} has the generator's description, not the neutral one"
            ));
        }
    }
    let how = "calibrate with `mtx calibrate --informed --v2-sessions --practice <family>`, which writes the neutral description and saves only the PnL";
    if headline == tasks::Headline::V2 {
        errors.append(&mut told);
    }
    if !errors.is_empty() {
        Check::fail(
            GRADED,
            id,
            title,
            format!(
                "{}: the image would tell the agent who is informed; {how}",
                errors.join("; ")
            ),
        )
    } else if !told.is_empty() {
        Check::warn(
            GRADED,
            id,
            title,
            format!(
                "{} of {} practice files state the latent state in their description ({}); the task is v1 and unguarded, so the agent can draw the family anyway. A v2 task fails this",
                told.len(),
                files.len(),
                told.join("; ")
            ),
        )
    } else {
        Check::pass(
            GRADED,
            id,
            title,
            format!(
                "{} practice files, none stating a latent or roster",
                files.len()
            ),
        )
    }
}

/// Every file under `dir`, subdirectories too.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

const INFORMED_LIVE: (&str, &str) = (
    "informed-on-hidden-sets",
    "The verifier runs the informed bot on the hidden set",
);

/// A v2 task's verifier runs its informed bot on every hidden set it draws,
/// so the grade's 1 is measured on the sessions the bot played: `mtx grade
/// --informed` exists; tests/test.sh draws its set and grades only a
/// strategy component (no files or network, so nothing the informed bot was
/// told can reach it); it passes `--informed` at the path the image builds
/// the task's informed bot into, which `bench/image/anchors.txt` lists; and
/// the informed bot is written (not the scaffold's copy of the reference).
pub fn informed_on_hidden_sets(root: &Path, task: &str, test: &str) -> Check {
    let (id, title) = INFORMED_LIVE;
    if !has_command(&["grade"], Some("informed")) {
        // No fallback: a v2 grade without the informed bot on its hidden
        // sessions fails, so the task can't be released until it runs.
        return Check::fail(
            SOLVABLE,
            id,
            title,
            "`mtx grade --informed` isn't in this build: a hidden grade has no informed bot's PnL, and v2 as the one score doesn't fall back to the hindsight ceiling, so the task isn't releasable",
        );
    }
    let loaded = match tasks::load_one(root, task.to_string()) {
        Ok(t) => t,
        Err(e) => return Check::fail(SOLVABLE, id, title, format!("{e:#}")),
    };
    let path = format!("/opt/mtx/anchors/{}", loaded.informed_anchor());
    let mut problems = Vec::new();
    for (flag, why) in [
        ("--draw", "draws no hidden set"),
        (
            "--components-only",
            "doesn't grade only a strategy component (`--components-only`)",
        ),
    ] {
        if !scoring_text::has_flag(test, flag) {
            problems.push(format!("tests/test.sh {why}"));
        }
    }
    match scoring_text::flag_value(test, "--informed") {
        None => problems.push("tests/test.sh doesn't pass `mtx grade --informed`".into()),
        Some(given) if given != path => problems.push(format!(
            "tests/test.sh runs the informed bot from {given}, not {path}, where the image builds it"
        )),
        Some(_) => {}
    }
    match &loaded.informed {
        None => problems.push(
            "the ladder's `anchors.informed` names no rung with a bot crate for the image to build"
                .into(),
        ),
        Some(bot) => {
            let line = format!(
                "{} {} {} {}",
                loaded.informed_anchor(),
                bot.package,
                bot.bin,
                bot.dir
            );
            let listed = std::fs::read_to_string(root.join("bench/image/anchors.txt"))
                .unwrap_or_default()
                .lines()
                .any(|l| l == line);
            if !listed {
                problems.push(format!(
                    "bench/image/anchors.txt doesn't list `{line}` (run `mtx task sync`)"
                ));
            }
        }
    }
    problems.extend(authoring::anchor_problems(root, task));
    if problems.is_empty() {
        Check::pass(
            SOLVABLE,
            id,
            title,
            format!(
                "tests/test.sh grades with `--informed {path}`, which the image builds from {}",
                loaded.informed.map(|b| b.dir).unwrap_or_default()
            ),
        )
    } else {
        Check::fail(SOLVABLE, id, title, problems.join("; "))
    }
}

fn total(summary: &LadderSummary, rung: &str) -> Option<f64> {
    let mut sum = 0.0;
    for set in &summary.totals {
        sum += set.get(rung)?;
    }
    Some(sum)
}

/// The best informed policy against the best that doesn't read the signal,
/// by total PnL over every set.
pub fn signal_gate(summary: &LadderSummary, gate: &SignalGate) -> Check {
    let (id, title) = (
        "informed-vs-non-reading",
        "Informed beats the best non-reading policy",
    );
    if gate.informed.is_empty() || gate.non_reading.is_empty() {
        return Check::not_run(
            SOLVABLE,
            id,
            title,
            "the task's ladder file declares no `gates.informed` / `gates.non_reading` rungs",
        );
    }
    let ratio = gate.min_ratio.unwrap_or(DEFAULT_MIN_RATIO);
    let best = |names: &[String]| -> Result<(String, f64), String> {
        let mut top: Option<(String, f64)> = None;
        for n in names {
            let t = total(summary, n).ok_or_else(|| format!("no rung `{n}` in the ladder"))?;
            if top.as_ref().is_none_or(|(_, b)| t > *b) {
                top = Some((n.clone(), t));
            }
        }
        top.ok_or_else(|| "no rungs".to_string())
    };
    let (informed, baseline) = match (best(&gate.informed), best(&gate.non_reading)) {
        (Ok(i), Ok(b)) => (i, b),
        (Err(e), _) | (_, Err(e)) => return Check::fail(SOLVABLE, id, title, e),
    };
    let floor = baseline.1.max(0.0);
    let ok = informed.1 > 0.0 && informed.1 >= ratio * floor;
    let multiple = if floor > 0.0 {
        format!("{:.1}x", informed.1 / floor)
    } else {
        "unbounded".to_string()
    };
    Check::from(
        SOLVABLE,
        id,
        title,
        ok,
        format!(
            "{} ${:.0} vs {} ${:.0}: {multiple}, need {ratio:.0}x ({} set{})",
            informed.0,
            informed.1,
            baseline.0,
            baseline.1,
            summary.totals.len(),
            if summary.totals.len() == 1 { "" } else { "s" },
        ),
    )
}

/// Why a fast set whose reference lost money can't settle the reference and
/// order checks.
const THIN_SAMPLE: &str = "The reference lost money on this small sample; the task's edge is too thin to judge in fast mode, and the full ladder decides";

/// The checks the ladder answers: the reference and nop anchors, the order,
/// the signal gate, the exploit bots, clean runs.
pub fn ladder_checks(summary: &LadderSummary, gate: &SignalGate, fast: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    let sets = summary.totals.len();
    let reference: Vec<f64> = summary
        .totals
        .iter()
        .filter_map(|s| s.get("reference").copied())
        .collect();
    // A short set can lose money by chance for a task whose reference has a
    // thin edge: that says nothing for or against the task.
    let thin = fast && reference.iter().any(|r| *r <= 0.0);
    let reference_pnl = format!(
        "reference total PnL per set: {}",
        reference
            .iter()
            .map(|r| format!("${r:.0}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    checks.push(if thin {
        Check::new(
            SOLVABLE,
            "reference-scores-1",
            "Reference is the 1.0 line (makes money on every set)",
            Status::Inconclusive,
            format!("{reference_pnl}. {THIN_SAMPLE}"),
        )
    } else {
        Check::from(
            SOLVABLE,
            "reference-scores-1",
            "Reference is the 1.0 line (makes money on every set)",
            reference.len() == sets && reference.iter().all(|r| *r > 0.0),
            reference_pnl,
        )
    });
    let nop: Vec<f64> = summary
        .totals
        .iter()
        .filter_map(|s| s.get("nop").copied())
        .collect();
    checks.push(Check::from(
        SOLVABLE,
        "nop-scores-0",
        "Doing nothing scores 0",
        nop.len() == sets && nop.iter().all(|n| n.abs() < 1e-6),
        format!(
            "nop total PnL per set: {}",
            nop.iter()
                .map(|n| format!("${n:.2}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ));
    let broken: Vec<&str> = summary
        .rules
        .iter()
        .filter(|r| !r.ok)
        .map(|r| r.rule.as_str())
        .collect();
    let order_detail = if broken.is_empty() {
        format!(
            "{} rule{} hold on {sets} set{} of {} sessions",
            summary.rules.len(),
            if summary.rules.len() == 1 { "" } else { "s" },
            if sets == 1 { "" } else { "s" },
            summary.sessions
        )
    } else {
        summary
            .rules
            .iter()
            .filter(|r| !r.ok)
            .map(|r| {
                format!(
                    "{} (holds on {}/{} sets, odds {:.2})",
                    r.rule, r.sets_holding, r.sets, r.odds
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    checks.push(if thin && !broken.is_empty() {
        Check::new(
            SOLVABLE,
            "ladder-order",
            "Skill ladder keeps its order",
            Status::Inconclusive,
            format!("{order_detail}. {THIN_SAMPLE}"),
        )
    } else {
        Check::from(
            SOLVABLE,
            "ladder-order",
            "Skill ladder keeps its order",
            broken.is_empty() && !summary.rules.is_empty(),
            order_detail,
        )
    });
    checks.push(signal_gate(summary, gate));
    checks.push(exploit_check(summary, fast));
    checks.push(probe_check(summary, fast));
    checks.push(Check::from(
        GRADED,
        "ladder-clean",
        "No rung was refused or disqualified",
        summary.disqualified.is_empty(),
        if summary.disqualified.is_empty() {
            "every rung graded clean".to_string()
        } else {
            summary
                .disqualified
                .iter()
                .map(|d| format!("{} in {}", d.rung, d.scenarios.join(", ")))
                .collect::<Vec<_>>()
                .join("; ")
        },
    ));
    checks
}

/// P(X >= k) for X ~ Binomial(n, p).
fn binomial_tail(k: usize, n: usize, p: f64) -> f64 {
    let mut choose = 1.0;
    let mut tail = 0.0;
    for i in 0..=n {
        if i > 0 {
            choose = choose * (n - i + 1) as f64 / i as f64;
        }
        if i >= k {
            tail += choose * p.powi(i as i32) * (1.0 - p).powi((n - i) as i32);
        }
    }
    tail
}

/// What an exploit bot broke: the reasons the ladder gave, each marked
/// confident or not. On the full ladder every reason counts; on a fast set
/// only those that hold with [`FAST_EXPLOIT_CONFIDENCE`].
fn exploit_breaches(e: &ExploitResult, fast: bool) -> Vec<(bool, String)> {
    let stats = (e.sessions, e.profitable_sessions, e.max_profitable);
    let (Some(n), Some(k), Some(limit)) = stats else {
        // An older summary carries no sample: take its verdict as it is.
        if e.ok {
            return Vec::new();
        }
        return e.reasons.iter().map(|r| (true, r.clone())).collect();
    };
    let mut out = Vec::new();
    if e.reached_bound == Some(true) {
        out.push((true, "reached the bound".to_string()));
    }
    if e.profitable > limit {
        let p = binomial_tail(k, n, limit);
        out.push((
            !fast || p < 1.0 - FAST_EXPLOIT_CONFIDENCE,
            format!(
                "profitable in {k}/{n} sessions, limit {:.0}% (p = {p:.2} at the limit)",
                limit * 100.0
            ),
        ));
    }
    if let (Some(below), Some(total), Some(ceiling)) = (&e.below, e.total, e.below_total)
        && total >= ceiling
    {
        let odds = e.below_odds.unwrap_or(1.0);
        out.push((
            !fast || odds >= FAST_EXPLOIT_CONFIDENCE,
            format!("${total:.0}, at or above {below}'s ${ceiling:.0} (odds {odds:.2})"),
        ));
    }
    out
}

/// The exploit bots stay inside their limits. A fast set is short, so a bot
/// over a limit by less than the sample can tell is inconclusive, not failed.
fn exploit_check(summary: &LadderSummary, fast: bool) -> Check {
    let (id, title) = ("exploit-bots", "Exploit bots stay inside their limits");
    let mut failed = Vec::new();
    let mut unsure = Vec::new();
    for e in &summary.exploits {
        for (confident, reason) in exploit_breaches(e, fast) {
            let line = format!("{} ({reason})", e.bot);
            if confident {
                failed.push(line);
            } else {
                unsure.push(line);
            }
        }
    }
    // A bot that was to move the mark and moved nothing proves nothing by
    // losing: no power fails the gate, as a leak would.
    for e in summary
        .exploits
        .iter()
        .filter(|e| e.verdict.as_deref() == Some("no_power"))
    {
        failed.push(format!(
            "{} (no power: {})",
            e.bot,
            e.no_power.as_deref().unwrap_or("it moved nothing")
        ));
    }
    if !failed.is_empty() {
        return Check::fail(GRADED, id, title, failed.join("; "));
    }
    if !unsure.is_empty() {
        return Check::new(
            GRADED,
            id,
            title,
            Status::Inconclusive,
            format!("inconclusive at fast size: {}", unsure.join("; ")),
        );
    }
    Check::pass(
        GRADED,
        id,
        title,
        summary
            .exploits
            .iter()
            .map(|e| {
                let ratio = e.ratio.map_or_else(
                    || "no ratio (the reference lost money on this sample)".to_string(),
                    |r| format!("{r:.2}x"),
                );
                format!("{} {ratio}, profitable {:.0}%", e.bot, e.profitable * 100.0)
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// The probe bots land where scoring v2's decisions say they must: gamblers
/// at or under doing nothing, a breacher no better than its clipped twin,
/// leverage held to its multiple, honest edge over doing nothing, a crash
/// no better than the bot it wraps. The ladder fixes the rules and applies a
/// bootstrap: a rule broken with confidence fails, one the sample can't
/// confirm is inconclusive in a fast run and fails in the full one (as an
/// exploit over its limit does), and a probe with no power fails. A task
/// that says nothing about probes fails; one that opts out says why.
fn probe_check(summary: &LadderSummary, fast: bool) -> Check {
    let (id, title) = ("probe-bots", "Probe bots land in their v2 bands");
    if summary.probes_status.as_deref() == Some("exempt") {
        return Check::not_run(
            GRADED,
            id,
            title,
            format!(
                "exempt: {}",
                summary
                    .probes_exempt
                    .as_deref()
                    .unwrap_or("no reason given")
            ),
        );
    }
    let Some(probes) = summary.probes.as_ref().filter(|p| !p.is_empty()) else {
        return Check::fail(
            GRADED,
            id,
            title,
            "the task's ladder file has no `probes` section: add one, or exempt the task with a reason (bench/checks/probes/README.md)",
        );
    };
    let of = |keep: &dyn Fn(&ProbeResult) -> bool| -> Vec<String> {
        probes
            .iter()
            .filter(|p| keep(p))
            .map(|p| format!("{} ({})", p.id, p.detail))
            .collect()
    };
    let failed =
        of(&|p| p.verdict == "fail" || (!fast && p.verdict == "inconclusive" && p.point_violated));
    let no_power = of(&|p| p.verdict == "no_power");
    let unsure = of(&|p| p.verdict == "inconclusive" && (fast || !p.point_violated));
    if !failed.is_empty() || !no_power.is_empty() {
        let mut parts = Vec::new();
        if !failed.is_empty() {
            parts.push(format!("out of band: {}", failed.join("; ")));
        }
        if !no_power.is_empty() {
            parts.push(format!("no power: {}", no_power.join("; ")));
        }
        return Check::fail(GRADED, id, title, parts.join(". "));
    }
    if !unsure.is_empty() {
        return Check::new(
            GRADED,
            id,
            title,
            Status::Inconclusive,
            format!("the sample can't confirm: {}", unsure.join("; ")),
        );
    }
    // A rule the task can't exercise (the breacher's run cap where its reference
    // rides the cap) is said, never counted as held.
    let idle: Vec<&str> = probes
        .iter()
        .filter(|p| p.verdict == "not_exercised")
        .map(|p| p.id.as_str())
        .collect();
    let held = probes.len() - idle.len();
    let detail = if idle.is_empty() {
        format!("{held} probe rules hold")
    } else {
        format!(
            "{held} probe rules hold; not exercised on this task: {}",
            idle.join(", ")
        )
    };
    Check::pass(GRADED, id, title, detail)
}

/// The last lines of a command's output, joined.
fn tail(out: &std::process::Output) -> String {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].join(" · ")
}

/// Run `cmd` from the repo root; stderr is left on the terminal's stderr for
/// the long ones (`stream`), else captured.
fn run_in(root: &Path, program: &str, args: &[&str], stream: bool) -> Result<(bool, String)> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(root);
    if stream {
        let status = cmd
            .stdout(std::process::Stdio::null())
            .status()
            .with_context(|| format!("run {program}"))?;
        return Ok((status.success(), String::new()));
    }
    match cmd.output() {
        Ok(out) => Ok((out.status.success(), tail(&out))),
        Err(e) => Ok((false, format!("couldn't run `{program}`: {e}"))),
    }
}

fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    serde_json::from_str(&text).with_context(|| path.display().to_string())
}

/// A job directory's `reward.json`, the last one Harbor wrote under `dir`.
fn reward(dir: &Path) -> Option<BTreeMap<String, f64>> {
    fn find(dir: &Path, found: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                find(&p, found);
            } else if p.file_name().is_some_and(|n| n == "reward.json") {
                found.push(p);
            }
        }
    }
    let mut found = Vec::new();
    find(dir, &mut found);
    found.sort();
    serde_json::from_str(&std::fs::read_to_string(found.last()?).ok()?).ok()
}

/// Remove `dir` and everything under it, if it's there.
fn clear_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("clear {}", dir.display()))
        }
        _ => Ok(()),
    }
}

/// What a Harbor check run should have scored, read from its `reward.json`.
/// The oracle is the reference: skill 1 under v1. Under v2, the task's one
/// score: `reward` is v2's (no `skill` beside it), and the reference before
/// the near-breach charge and any breach penalty is 0.5 (the report's
/// `score_v2_raw_before_charge`). The charge itself, anchored on the
/// informed bot's drawdowns, may take a deeper-drawing reference under 0.5
/// by design: it is reported beside, not checked.
fn harbor_checks(jobs: &Path, ran: bool, detail: &str, headline: tasks::Headline) -> Vec<Check> {
    let title_oracle = match headline {
        tasks::Headline::V1 => "Oracle scores the reference's 1.0",
        tasks::Headline::V2 => "Oracle scores the reference's 0.5 (scoring v2)",
    };
    if !ran {
        let d = format!("`just harbor-checks` failed: {detail}");
        return vec![
            Check::fail(SOLVABLE, "oracle", title_oracle, d.clone()),
            Check::fail(
                SOLVABLE,
                "nop-harbor",
                "Nop scores 0 under Harbor",
                d.clone(),
            ),
            Check::fail(
                GRADED,
                "cheaters",
                "Cheaters are disqualified, sneaky is clean",
                d,
            ),
        ];
    }
    let get = |agent: &str, key: &str| reward(&jobs.join(agent)).and_then(|r| r.get(key).copied());
    let mut checks = Vec::new();
    checks.push(match headline {
        tasks::Headline::V1 => {
            let skill = get("oracle", "skill");
            Check::from(
                SOLVABLE,
                "oracle",
                title_oracle,
                skill.is_some_and(|s| (0.999..=1.001).contains(&s)),
                format!(
                    "oracle skill {}",
                    skill.map_or("missing".into(), |s| format!("{s:.4}"))
                ),
            )
        }
        tasks::Headline::V2 => {
            let (reward, v2, before, charge, skill) = (
                get("oracle", "reward"),
                get("oracle", "score_v2"),
                get("oracle", "score_v2_raw_before_charge"),
                get("oracle", "score_v2_risk_charge_usd"),
                get("oracle", "skill"),
            );
            let ok = before.is_some_and(|r| (0.499..=0.501).contains(&r))
                && reward.is_some()
                && v2 == reward
                && skill.is_none();
            let num = |x: Option<f64>| x.map_or("missing".into(), |s| format!("{s:.4}"));
            Check::from(
                SOLVABLE,
                "oracle",
                title_oracle,
                ok,
                format!(
                    "oracle before the charge {}; reward {} after a near-breach charge of ${} (information){}{}",
                    num(before),
                    num(reward),
                    charge.map_or("?".into(), |c| format!("{c:.2}")),
                    if v2 != reward {
                        "; reward isn't v2's score"
                    } else {
                        ""
                    },
                    if skill.is_some() {
                        "; a v1 skill is reported beside it"
                    } else {
                        ""
                    },
                ),
            )
        }
    });
    let nop = get("nop", "reward");
    checks.push(Check::from(
        SOLVABLE,
        "nop-harbor",
        "Nop scores 0 under Harbor",
        nop == Some(0.0),
        format!(
            "nop reward {}",
            nop.map_or("missing".into(), |s| format!("{s}"))
        ),
    ));
    // The component cheater is graded and must score 0 with its grade
    // complete; the process cheaters and the sneaky bot aren't graded at all.
    let wasm = (
        get("component-cheater", "reward"),
        get("component-cheater", "tampered"),
    );
    let process: Vec<_> = ["cheater", "sneaky"]
        .iter()
        .map(|a| get(a, "not_graded"))
        .collect();
    let ok = wasm.0 == Some(0.0) && wasm.1 == Some(0.0) && process.iter().all(|p| *p == Some(1.0));
    checks.push(Check::from(
        GRADED,
        "cheaters",
        "Cheaters are disqualified, sneaky is clean",
        ok,
        format!(
            "component cheater reward {:?}, tampered {:?}; process cheater and sneaky not_graded {:?}",
            wasm.0, wasm.1, process
        ),
    ));
    checks
}

/// Whether this binary has the subcommand at `path` (and, if named, its
/// argument `arg`): the replay and calibration checks land in other changes.
fn has_command(path: &[&str], arg: Option<&str>) -> bool {
    let mut cmd = Cli::command();
    for name in path {
        match cmd.find_subcommand(name) {
            Some(sub) => cmd = sub.clone(),
            None => return false,
        }
    }
    arg.is_none_or(|arg| cmd.get_arguments().any(|a| a.get_id() == arg))
}

/// The name of the snapshot `task` trades on: its task.toml's `[metadata]
/// snapshot`, else the one its verifier image sets (`ENV MTX_SNAPSHOT=` in
/// tests/Dockerfile), else the Justfile's (`snapshot := "..."`). A task on
/// another market than SPCX is graded on its own chain state.
fn snapshot_name(root: &Path, task: &str) -> String {
    let folder = root.join("bench/tasks").join(task);
    let from_toml = std::fs::read_to_string(folder.join("task.toml"))
        .ok()
        .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
        .and_then(|t| {
            t.get("metadata")?
                .get("snapshot")?
                .as_str()
                .map(str::to_string)
        });
    let from_image = || {
        std::fs::read_to_string(folder.join("tests/Dockerfile"))
            .ok()?
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("ENV MTX_SNAPSHOT=/opt/mtx/snapshots/")
            })
            .map(|v| v.trim().to_string())
    };
    let from_justfile = || {
        std::fs::read_to_string(root.join("Justfile"))
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("snapshot := "))
            .map(|v| v.trim().trim_matches('"').to_string())
    };
    from_toml
        .filter(|n| !n.is_empty())
        .or_else(from_image)
        .or_else(from_justfile)
        .unwrap_or_else(|| DEFAULT_SNAPSHOT.to_string())
}

/// The snapshot `task` trades on, from the repository's root.
fn snapshot(root: &Path, task: &str) -> String {
    format!("snapshots/{}", snapshot_name(root, task))
}

/// A fresh eval secret for one check, never printed.
fn secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Replays are byte-identical: the reference graded `runs` times on a fresh
/// hidden set (`mtx grade --replay`), the machine loaded in full mode.
fn replay_check(
    root: &Path,
    task: &str,
    exe: &Path,
    family: &str,
    ladder: &Value,
    fast: bool,
    out: &Path,
) -> Check {
    let (id, title) = ("replay-determinism", "Replays are byte-identical");
    if !has_command(&["grade"], Some("replay")) {
        return Check::not_run(
            GRADED,
            id,
            title,
            "`mtx grade --replay` isn't in this build yet",
        );
    }
    let reference = ladder["rungs"].as_array().and_then(|rungs| {
        rungs
            .iter()
            .find(|r| r["name"] == "reference")
            .and_then(|r| r["dir"].as_str())
    });
    let Some(bot) = reference else {
        return Check::fail(GRADED, id, title, "the ladder file has no `reference` rung");
    };
    let (count, runs, load) = if fast { REPLAY_FAST } else { REPLAY_FULL };
    let report = out.join("replay.json");
    let _ = std::fs::remove_file(&report);
    let secret = match secret() {
        Ok(s) => s,
        Err(e) => return Check::fail(GRADED, id, title, format!("no eval secret: {e}")),
    };
    // The reference is an anchor: trusted, as the ladder grades it, so a
    // loaded machine can't push it into its turn's time wall.
    let trusted = has_command(&["grade"], Some("trusted"));
    let output = Command::new(exe)
        .args(["grade", "--draw", family, "--jobs", "4", "--bot", bot])
        .args([
            "--count",
            &count.to_string(),
            "--snapshot",
            &snapshot(root, task),
        ])
        .args([
            "--replay",
            &runs.to_string(),
            "--replay-load",
            &load.to_string(),
        ])
        .args(trusted.then_some("--trusted"))
        .arg("--report")
        .arg(&report)
        .env("MTX_EVAL_SEED", secret)
        .current_dir(root)
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => return Check::fail(GRADED, id, title, format!("couldn't run `mtx grade`: {e}")),
    };
    let replay = read_json(&report).ok().map(|r| r["replay"].clone());
    match replay {
        Some(r) if r.is_object() => {
            let identical = output.status.success() && r["identical"] == true;
            let detail = if identical {
                format!(
                    "{} reports identical over {} runs of {count} sessions ({} threads loaded)",
                    r["reports"], r["runs"], r["load_threads"]
                )
            } else {
                let differences = r["differences"].as_array().map_or(0, Vec::len);
                format!(
                    "{differences} reports differed; first: {}",
                    r["differences"][0]
                )
            };
            Check::from(GRADED, id, title, identical, detail)
        }
        _ => {
            let err = std::process::Output {
                stdout: Vec::new(),
                ..output
            };
            Check::fail(GRADED, id, title, tail(&err))
        }
    }
}

/// The task's asset calibration (`calibration/market/<asset>.<date>.<hash>.toml`,
/// the latest), the asset being the instrument in the task's name.
fn calibration_file(root: &Path, task: &str) -> Option<PathBuf> {
    let asset = task.rsplit_once('-').map_or(task, |(_, asset)| asset);
    let prefix = format!("{asset}.");
    let mut files: Vec<PathBuf> = std::fs::read_dir(root.join("calibration/market"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "toml")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
        })
        .collect();
    files.sort();
    files.pop()
}

/// The market matches its calibration: `mtx market-cal check <family>`
/// against the task's asset calibration, which exits nonzero on a failed
/// realism check.
fn calibration_check(root: &Path, exe: &Path, task: &str, family: &str) -> Check {
    let (id, title) = (
        "market-calibration",
        "The market matches its calibration file",
    );
    if !has_command(&["market-cal", "check"], None) {
        return Check::not_run(
            REALISTIC,
            id,
            title,
            "`mtx market-cal check` isn't in this build yet",
        );
    }
    let Some(file) = calibration_file(root, task) else {
        return Check::not_run(REALISTIC, id, title, "no calibration file for its asset");
    };
    // Its own default draw (300 sessions) in both modes: fewer can't judge
    // the stylized facts.
    let file = file.to_string_lossy().into_owned();
    let args = [
        "market-cal",
        "check",
        family,
        "--calibration",
        file.as_str(),
    ];
    let output = match Command::new(exe).args(args).current_dir(root).output() {
        Ok(o) => o,
        Err(e) => {
            return Check::fail(REALISTIC, id, title, format!("couldn't run it: {e}"));
        }
    };
    // It prints its report either way: exit 2 when a check fails, 3 when
    // nothing failed but the market wasn't judged.
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(report) if report["checks"].is_array() => {
            let name = Path::new(&file)
                .file_name()
                .map_or(file.clone(), |n| n.to_string_lossy().into_owned());
            let status = realism_status(output.status.code(), &report);
            let detail = format!("{name}: {}", realism_summary(&report));
            let detail = if status == Status::Inconclusive {
                format!("not judged: {detail}")
            } else {
                detail
            };
            Check::new(REALISTIC, id, title, status, detail)
        }
        _ => Check::fail(REALISTIC, id, title, tail(&output)),
    }
}

/// What a realism report's exit code and verdict make of the check. A market
/// not judged (exit 3, or verdict `not_judged`: the return checks didn't run)
/// is neither a pass nor a failure; a failed check (exit 2) or any other
/// error fails.
fn realism_status(code: Option<i32>, report: &Value) -> Status {
    match code {
        Some(0) if report["passed"] == true && report["verdict"] != "not_judged" => Status::Pass,
        Some(0) | Some(3) if report["verdict"] == "not_judged" || code == Some(3) => {
            Status::Inconclusive
        }
        _ => Status::Fail,
    }
}

/// A realism report in a line: each check's status, and the failed and
/// skipped ones by name.
fn realism_summary(report: &Value) -> String {
    let checks = report["checks"].as_array().cloned().unwrap_or_default();
    let named = |status: &str| -> Vec<String> {
        checks
            .iter()
            .filter(|c| c["status"] == status)
            .map(|c| c["name"].as_str().unwrap_or("?").to_string())
            .collect()
    };
    let mut parts = Vec::new();
    for status in ["pass", "fail", "skipped"] {
        let names = named(status);
        if !names.is_empty() {
            parts.push(format!("{status} {}", names.join(", ")));
        }
    }
    format!("{} sessions; {}", report["sessions"], parts.join("; "))
}

fn commit(root: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or("unknown".into(), |o| {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        })
}

/// The task-files checks, from `mtx task status`: every step but the manual
/// one must be done. `harbor lint` needs Harbor, which the report doesn't
/// require.
fn task_checks(root: &Path, task: &str) -> Result<Vec<Check>> {
    let mut checks = Vec::new();
    for step in authoring::status(root, task)? {
        if step.state == "manual" {
            continue;
        }
        if step.id == "lint" && step.detail.contains("isn't installed") {
            checks.push(Check::not_run(COMPLETE, step.id, step.title, step.detail));
            continue;
        }
        checks.push(Check::from(
            COMPLETE,
            step.id,
            step.title,
            step.state == "done",
            if step.state == "done" {
                step.detail
            } else {
                format!("{} Next: {}", step.detail, step.next)
            },
        ));
    }
    Ok(checks)
}

/// Run the gates for `task`, write `gates.json` and `gates.md` under the out
/// directory, and return the report.
pub fn run(root: &Path, task: &str, opts: &Options) -> Result<(Report, PathBuf)> {
    if !root
        .join("bench/tasks")
        .join(task)
        .join("task.toml")
        .is_file()
    {
        bail!("no task `{task}` in bench/tasks (`mtx task list`)");
    }
    let meta = tasks::load_one(root, task.to_string())?.mtx;
    let (family, headline) = (meta.generator, meta.headline);
    let out = opts
        .out
        .clone()
        .unwrap_or_else(|| root.join("target/gates").join(task));
    std::fs::create_dir_all(&out)?;
    // This binary, named before the ladder runs: the ladder rebuilds the
    // workspace, which can replace the file, and `current_exe` then names a
    // deleted one.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mtx"));
    let mut checks = task_checks(root, task)?;

    // The ladder: one hidden set of a few sessions in fast mode.
    let ladder_file = root
        .join("bench/tasks")
        .join(task)
        .join("checks/ladder.json");
    let config = read_json(&ladder_file)?;
    let signal: SignalGate = config
        .get("gates")
        .map(|g| serde_json::from_value(g.clone()))
        .transpose()?
        .unwrap_or_default();
    let summary_path = match &opts.ladder_summary {
        Some(p) => p.clone(),
        None => {
            let sets = opts
                .sets
                .unwrap_or(if opts.fast { 1 } else { 4 })
                .to_string();
            let count = opts
                .sessions
                .or(opts.fast.then_some(FAST_SESSIONS))
                .map(|c| c.to_string())
                .unwrap_or_default();
            let odds = if opts.fast {
                FAST_RULE_ODDS.to_string()
            } else {
                String::new()
            };
            let ladder_out = out.join("ladder");
            // The ladder reuses any rung report it finds: an earlier run's,
            // from another set, mustn't stand in for this one.
            clear_dir(&ladder_out.join(task))?;
            let seed = if opts.fast { FAST_PUBLIC_SEED } else { "" };
            // The ladder exits nonzero when a rule fails: the summary says which.
            // On the task's own snapshot, not the Justfile's default.
            let snapshot_override = format!("snapshot={}", snapshot_name(root, task));
            run_in(
                root,
                "just",
                &[
                    &snapshot_override,
                    "ladder",
                    task,
                    &sets,
                    "4",
                    "1",
                    &count,
                    &ladder_out.to_string_lossy(),
                    &odds,
                    seed,
                ],
                true,
            )?;
            ladder_out.join(task).join("summary.json")
        }
    };
    let summary = match std::fs::read_to_string(&summary_path) {
        Ok(text) => match serde_json::from_str::<LadderSummary>(&text) {
            Ok(summary) => {
                checks.extend(ladder_checks(&summary, &signal, opts.fast));
                Some(summary)
            }
            Err(e) => {
                checks.push(Check::fail(
                    SOLVABLE,
                    "ladder",
                    "Skill ladder ran",
                    format!("couldn't read {}: {e}", summary_path.display()),
                ));
                None
            }
        },
        Err(e) => {
            checks.push(Check::fail(
                SOLVABLE,
                "ladder",
                "Skill ladder ran",
                format!("no {}: {e}", summary_path.display()),
            ));
            None
        }
    };
    let test = std::fs::read_to_string(root.join("bench/tasks").join(task).join("tests/test.sh"))
        .unwrap_or_default();
    checks.push(bot_told_nothing(&test));
    checks.push(practice_holds_no_latent(root, task, &family, headline));
    checks.extend(v2_checks(
        root,
        task,
        headline,
        &config,
        summary.as_ref(),
        &test,
        opts.fast,
    ));

    // Full mode: the image checks.
    let full_only = |section, id, title| {
        Check::not_run(section, id, title, "full mode only (needs the task image)")
    };
    if opts.fast {
        checks.push(full_only(
            SOLVABLE,
            "oracle",
            match headline {
                tasks::Headline::V1 => "Oracle scores the reference's 1.0",
                tasks::Headline::V2 => "Oracle scores the reference's 0.5 (scoring v2)",
            },
        ));
        checks.push(full_only(
            SOLVABLE,
            "nop-harbor",
            "Nop scores 0 under Harbor",
        ));
        checks.push(full_only(
            GRADED,
            "cheaters",
            "Cheaters are disqualified, sneaky is clean",
        ));
        checks.push(full_only(
            GRADED,
            "leak-audit",
            "The agent's image holds no answers",
        ));
        checks.push(full_only(
            GRADED,
            "keypair-probe",
            "A bot can't read the grader's keypairs",
        ));
    } else {
        // Harbor adds a job directory per run and the checks read the last
        // reward.json: a stale one would stand in for a run that broke.
        let jobs = out.join("jobs");
        clear_dir(&jobs)?;
        let (ran, detail) = run_in(
            root,
            "just",
            &["harbor-checks", task, "", &jobs.to_string_lossy()],
            true,
        )?;
        checks.extend(harbor_checks(&jobs, ran, &detail, headline));
        let (ok, d) = run_in(root, "just", &["leak-audit", task], false)?;
        // The image Harbor runs is task.toml's, not the Dockerfile's.
        let images = std::fs::read_to_string(root.join("bench/tasks").join(task).join("task.toml"))
            .map(|t| tasks::image_problems(&t, task))
            .unwrap_or_default();
        let ok = ok && images.is_empty();
        checks.push(Check::from(
            GRADED,
            "leak-audit",
            "The agent's image holds no answers",
            ok,
            if ok {
                "no answer in the image".into()
            } else {
                format!("{}\n{d}", images.join("\n"))
            },
        ));
        let (ok, d) = run_in(root, "just", &["keypair-probe"], false)?;
        checks.push(Check::from(
            GRADED,
            "keypair-probe",
            "A bot can't read the grader's keypairs",
            ok,
            if ok { "no keypair readable".into() } else { d },
        ));
        if tasks::guarded(root, task) {
            let (ok, d) = run_in(root, "just", &["network-probe", task], false)?;
            checks.push(Check::from(
                GRADED,
                "network-probe",
                "A guarded agent can't reach ghcr.io or github.com",
                ok,
                d,
            ));
        }
        if std::fs::read_to_string(root.join("bench/tasks").join(task).join("task.toml"))
            .is_ok_and(|t| t.contains("MTX_AGENT_TIMEOUT_SEC"))
        {
            let (ok, d) = run_in(root, "just", &["clock-probe", task], false)?;
            checks.push(Check::from(
                GRADED,
                "clock-probe",
                "time-left reads the agent's clock",
                ok,
                d,
            ));
        }
    }

    checks.push(replay_check(
        root, task, &exe, &family, &config, opts.fast, &out,
    ));
    checks.push(calibration_check(root, &exe, task, &family));

    let report = Report::new(
        task,
        if opts.fast { "fast" } else { "full" },
        commit(root),
        checks,
    );
    std::fs::write(
        out.join("gates.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    std::fs::write(out.join("gates.md"), report.markdown())?;
    Ok((report, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(sets: &[&[(&str, f64)]]) -> LadderSummary {
        LadderSummary {
            sessions: 12,
            totals: sets
                .iter()
                .map(|s| s.iter().map(|(k, v)| (k.to_string(), *v)).collect())
                .collect(),
            rules: vec![],
            exploits: vec![],
            disqualified: vec![],
            probes: None,
            probes_status: None,
            probes_exempt: None,
            pnls: vec![],
        }
    }

    /// A ladder of `sets`, each rung's sessions given.
    fn sessions(sets: &[&[(&str, &[f64])]]) -> LadderSummary {
        LadderSummary {
            pnls: sets
                .iter()
                .map(|s| s.iter().map(|(k, v)| (k.to_string(), v.to_vec())).collect())
                .collect(),
            ..summary(
                &sets
                    .iter()
                    .map(|s| {
                        s.iter()
                            .map(|(k, v)| (*k, v.iter().sum::<f64>()))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
                    .iter()
                    .map(Vec::as_slice)
                    .collect::<Vec<_>>(),
            )
        }
    }

    fn anchors() -> Anchors {
        Anchors {
            reference: "reference".into(),
            informed: "informed".into(),
            band: None,
            min_sharpe: None,
        }
    }

    const REFERENCE: &[f64] = &[40.0, -10.0, 25.0, 60.0, 5.0, 15.0];
    const INFORMED: &[f64] = &[120.0, 30.0, 80.0, 90.0, -20.0, 60.0];

    #[test]
    fn a_reference_in_the_band_passes_and_one_outside_fails() {
        let k = scoring_text::Settings::default();
        // Weighted at k = 1.5: the reference 130, the informed bot 350.
        let fit = sessions(&[&[("reference", REFERENCE), ("informed", INFORMED)]]);
        let check = strength_band(&fit, &anchors(), &k, false);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(check.detail.contains("ratio 0.37"), "{}", check.detail);

        // A reference that barely earns is floored: v2 would flag it.
        let weak = sessions(&[&[
            ("reference", &[5.0, -2.0, 4.0, 6.0, 1.0, 2.0][..]),
            ("informed", INFORMED),
        ]]);
        let check = strength_band(&weak, &anchors(), &k, false);
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("anchor_floored"), "{}", check.detail);
        // On a fast set the same miss is inconclusive.
        let check = strength_band(&weak, &anchors(), &k, true);
        assert_eq!(check.status, Status::Inconclusive, "{}", check.detail);

        // An informed bot that doesn't beat the reference is broken.
        let flat = sessions(&[&[("reference", INFORMED), ("informed", REFERENCE)]]);
        let check = strength_band(&flat, &anchors(), &k, false);
        assert!(check.detail.contains("doesn't beat"), "{}", check.detail);

        // Too close to the informed bot leaves no room above 0.5.
        let close: Vec<f64> = INFORMED.iter().map(|x| x * 0.9).collect();
        let check = strength_band(
            &sessions(&[&[("reference", &close), ("informed", INFORMED)]]),
            &anchors(),
            &k,
            false,
        );
        assert!(
            check.detail.contains("outside [0.2, 0.7]"),
            "{}",
            check.detail
        );

        // The band and the Sharpe floor are the task's to set.
        let strict = Anchors {
            min_sharpe: Some(5.0),
            ..anchors()
        };
        let check = strength_band(&fit, &strict, &k, false);
        assert!(check.detail.contains("Sharpe"), "{}", check.detail);
    }

    #[test]
    fn the_band_needs_per_session_pnl_and_clean_anchors() {
        let k = scoring_text::Settings::default();
        let old = summary(&[&[("reference", 120.0), ("informed", 330.0)]]);
        let check = strength_band(&old, &anchors(), &k, true);
        assert_eq!(check.status, Status::Fail, "even in fast mode");
        assert!(check.detail.contains("per-session"), "{}", check.detail);
        let mut broke = sessions(&[&[("reference", REFERENCE), ("informed", INFORMED)]]);
        broke.disqualified.push(Disqualified {
            rung: "informed".into(),
            scenarios: vec!["s-3".into()],
        });
        let check = strength_band(&broke, &anchors(), &k, false);
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("informed broke its mandate"));
    }

    #[test]
    fn a_v2_task_without_anchors_is_refused_and_a_v1_task_is_untouched() {
        let fit = sessions(&[&[("reference", REFERENCE), ("informed", INFORMED)]]);
        let test = "mtx grade --scoring v2-headline --v2-sessions";
        let named = serde_json::json!({
            "anchors": { "reference": "reference", "informed": "informed" },
            "rungs": [
                { "name": "blind-long", "dir": "bench/checks/ladder/blind-long" },
                { "name": "reference", "dir": "bench/tasks/flash-crash-spcx/reference" },
                { "name": "informed", "dir": "bench/tasks/flash-crash-spcx/checks/bots/informed", "tell_latent": true }
            ]
        });
        let root = authoring::repo_root();
        let v2 = |ladder: &Value, summary, test| {
            v2_checks(
                &root,
                "flash-crash-spcx",
                tasks::Headline::V2,
                ladder,
                summary,
                test,
                false,
            )
        };
        let checks = v2(&named, Some(&fit), test);
        assert_eq!(checks[0].id, "strength-band");
        assert_eq!(checks[0].status, Status::Pass, "{}", checks[0].detail);
        // A verifier that doesn't run the informed bot isn't releasable.
        assert_eq!(checks[1].id, "informed-on-hidden-sets");
        assert_eq!(checks[1].status, Status::Fail);
        assert!(
            checks[1].detail.contains("--informed"),
            "{}",
            checks[1].detail
        );
        let none = v2(&serde_json::json!({}), Some(&fit), test);
        assert_eq!(none[0].status, Status::Fail);
        assert!(none[0].detail.contains("names no `anchors`"));
        let typo = serde_json::json!({ "anchors": { "reference": "reference", "informd": "x" } });
        let typo = v2(&typo, Some(&fit), test);
        assert_eq!(typo[0].status, Status::Fail, "{}", typo[0].detail);
        // A `reference` rung that runs another bot (a blind bet here) can't
        // be certified as the 0.5, however its numbers fall.
        let mut swapped = named.clone();
        swapped["anchors"]["reference"] = serde_json::json!("blind-long");
        let fit_swapped = sessions(&[&[("blind-long", REFERENCE), ("informed", INFORMED)]]);
        let swapped = v2(&swapped, Some(&fit_swapped), test);
        assert_eq!(swapped[0].status, Status::Fail);
        assert!(
            swapped[0].detail.contains("not the task's reference bot"),
            "{}",
            swapped[0].detail
        );
        // An informed rung that isn't told the latent state isn't the 1.
        let mut untold = named.clone();
        untold["rungs"][2]["tell_latent"] = serde_json::json!(false);
        let untold = v2(&untold, Some(&fit), test);
        assert_eq!(untold[0].status, Status::Fail);
        assert!(
            untold[0].detail.contains("isn't told"),
            "{}",
            untold[0].detail
        );
        // Nor one whose breaches the ladder would leave out of its clean runs.
        let mut breaching = named.clone();
        breaching["rungs"][1]["may_breach"] = serde_json::json!(true);
        let breaching = v2(&breaching, Some(&fit), test);
        assert_eq!(breaching[0].status, Status::Fail);
        assert!(
            breaching[0].detail.contains("`may_breach`"),
            "{}",
            breaching[0].detail
        );
        let v1 = v2_checks(
            &root,
            "flash-crash-spcx",
            tasks::Headline::V1,
            &serde_json::json!({}),
            None,
            "",
            false,
        );
        assert!(v1.iter().all(|c| c.status == Status::NotRun));
    }

    fn gate(informed: &[&str], non_reading: &[&str]) -> SignalGate {
        SignalGate {
            informed: informed.iter().map(|s| s.to_string()).collect(),
            non_reading: non_reading.iter().map(|s| s.to_string()).collect(),
            min_ratio: None,
        }
    }

    #[test]
    fn informed_must_double_the_best_non_reading_policy() {
        let s = summary(&[&[("told", 100.0), ("fade", 40.0), ("follow", -20.0)]]);
        let check = signal_gate(&s, &gate(&["told"], &["fade", "follow"]));
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(check.detail.contains("2.5x"), "{}", check.detail);
        let s = summary(&[&[("told", 100.0), ("fade", 60.0)]]);
        let check = signal_gate(&s, &gate(&["told"], &["fade"]));
        assert_eq!(check.status, Status::Fail, "{}", check.detail);
    }

    #[test]
    fn a_losing_baseline_is_unbounded_but_the_informed_policy_must_profit() {
        let s = summary(&[&[("told", 10.0), ("fade", -5.0)]]);
        assert_eq!(
            signal_gate(&s, &gate(&["told"], &["fade"])).status,
            Status::Pass
        );
        let s = summary(&[&[("told", -1.0), ("fade", -5.0)]]);
        assert_eq!(
            signal_gate(&s, &gate(&["told"], &["fade"])).status,
            Status::Fail
        );
    }

    #[test]
    fn the_gate_sums_every_set_and_names_a_missing_rung() {
        let s = summary(&[
            &[("told", 50.0), ("fade", 40.0)],
            &[("told", 150.0), ("fade", 10.0)],
        ]);
        let check = signal_gate(&s, &gate(&["told"], &["fade"]));
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        let check = signal_gate(&s, &gate(&["told"], &["gone"]));
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("gone"));
    }

    #[test]
    fn an_undeclared_signal_is_not_run_rather_than_failed() {
        let s = summary(&[&[("told", 1.0)]]);
        assert_eq!(
            signal_gate(&s, &SignalGate::default()).status,
            Status::NotRun
        );
    }

    #[test]
    fn the_binomial_tail_matches_its_closed_forms() {
        assert!((binomial_tail(12, 12, 0.75) - 0.75f64.powi(12)).abs() < 1e-12);
        let eleven = 12.0 * 0.75f64.powi(11) * 0.25 + 0.75f64.powi(12);
        assert!((binomial_tail(11, 12, 0.75) - eleven).abs() < 1e-12);
        assert!((binomial_tail(0, 12, 0.3) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_fast_set_fails_an_exploit_only_when_it_is_confidently_over() {
        let bot = |k: usize, total: f64, odds: f64| ExploitResult {
            bot: "fable".into(),
            ok: false,
            ratio: Some(0.9),
            profitable: k as f64 / 12.0,
            reasons: vec!["the ladder's own verdict".into()],
            sessions: Some(12),
            profitable_sessions: Some(k),
            max_profitable: Some(0.75),
            total: Some(total),
            below: Some("regime-oracle".into()),
            below_total: Some(100.0),
            below_odds: Some(odds),
            reached_bound: Some(false),
            ..Default::default()
        };
        let check = |e: ExploitResult, fast: bool| {
            let mut s = summary(&[]);
            s.exploits.push(e);
            exploit_check(&s, fast)
        };
        // 10/12 over a 75% limit: p = 0.39 at the limit, so a short set can't tell.
        let c = check(bot(10, 50.0, 0.2), true);
        assert_eq!(c.status, Status::Inconclusive, "{}", c.detail);
        assert!(c.detail.contains("10/12"), "{}", c.detail);
        // The full ladder takes the share as measured.
        assert_eq!(check(bot(10, 50.0, 0.2), false).status, Status::Fail);
        // 12/12: p = 0.03 at the limit.
        assert_eq!(check(bot(12, 50.0, 0.2), true).status, Status::Fail);
        // Above the oracle on this set, but not on most resamples of it.
        assert_eq!(check(bot(6, 120.0, 0.6), true).status, Status::Inconclusive);
        assert_eq!(check(bot(6, 120.0, 0.97), true).status, Status::Fail);
        let mut bound = bot(6, 50.0, 0.2);
        bound.reached_bound = Some(true);
        assert_eq!(check(bound, true).status, Status::Fail);
        assert_eq!(check(bot(6, 50.0, 0.2), true).status, Status::Pass);
        // A summary without the sample keeps the ladder's verdict.
        let old = ExploitResult {
            bot: "old".into(),
            reasons: vec!["profitable in 90% of sessions".into()],
            ..Default::default()
        };
        assert_eq!(check(old, true).status, Status::Fail);
        let fine = ExploitResult {
            bot: "old".into(),
            ok: true,
            ..Default::default()
        };
        assert_eq!(check(fine, true).status, Status::Pass);
        let mut s = summary(&[]);
        s.exploits.push(bot(10, 50.0, 0.2));
        let report = Report::new("t", "fast", "abc".into(), vec![exploit_check(&s, true)]);
        assert!(report.passed);
        assert!(report.markdown().contains("1 inconclusive"));
    }

    #[test]
    fn a_pusher_with_no_power_fails_the_exploit_check() {
        let mut s = summary(&[]);
        s.exploits.push(ExploitResult {
            bot: "pusher".into(),
            ok: true,
            verdict: Some("no_power".into()),
            no_power: Some("moved the mark 0.10 bp".into()),
            ..Default::default()
        });
        let c = exploit_check(&s, false);
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.detail.contains("no power: moved the mark 0.10 bp"),
            "{}",
            c.detail
        );
        s.exploits[0].verdict = Some("pass".into());
        assert_eq!(exploit_check(&s, false).status, Status::Pass);
    }

    fn probe(id: &str, verdict: &str) -> ProbeResult {
        ProbeResult {
            id: id.into(),
            verdict: verdict.into(),
            detail: "raw +0.100".into(),
            point_violated: verdict != "pass",
        }
    }

    #[test]
    fn probe_rules_fail_the_gate_unless_the_sample_cannot_confirm_them() {
        let check = |probes: Option<Vec<ProbeResult>>, fast: bool| {
            let mut s = summary(&[]);
            s.probes = probes;
            probe_check(&s, fast)
        };
        // Nothing said about probes fails; an exemption says why and isn't run.
        assert_eq!(check(None, true).status, Status::Fail);
        assert_eq!(check(Some(vec![]), true).status, Status::Fail);
        let mut s = summary(&[]);
        s.probes_status = Some("exempt".into());
        s.probes_exempt = Some("not wired".into());
        let exempt = probe_check(&s, false);
        assert_eq!(exempt.status, Status::NotRun);
        assert!(exempt.detail.contains("not wired"));
        let held = check(Some(vec![probe("a", "pass"), probe("b", "pass")]), true);
        assert_eq!(held.status, Status::Pass);
        assert!(held.detail.contains("2 probe rules"));
        let idle = check(
            Some(vec![
                probe("a", "pass"),
                probe("b:run_cap", "not_exercised"),
            ]),
            false,
        );
        assert_eq!(idle.status, Status::Pass);
        assert!(idle.detail.contains("1 probe rules hold"));
        assert!(
            idle.detail
                .contains("not exercised on this task: b:run_cap")
        );
        let unsure = vec![probe("a", "pass"), probe("b", "inconclusive")];
        assert_eq!(check(Some(unsure), true).status, Status::Inconclusive);
        let broken = vec![probe("zero_edge:coin", "fail"), probe("b", "pass")];
        let c = check(Some(broken), true);
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.detail
                .contains("out of band: zero_edge:coin (raw +0.100)")
        );
        let powerless = check(Some(vec![probe("breacher", "no_power")]), true);
        assert_eq!(powerless.status, Status::Fail);
        assert!(powerless.detail.contains("no power"));
        let both = vec![probe("a", "inconclusive"), probe("b", "fail")];
        assert_eq!(check(Some(both), true).status, Status::Fail);
    }

    #[test]
    fn the_full_gate_fails_a_broken_point_estimate_the_fast_one_tolerates() {
        let unsure = || Some(vec![probe("b", "inconclusive")]);
        let mut s = summary(&[]);
        s.probes = unsure();
        assert_eq!(probe_check(&s, true).status, Status::Inconclusive);
        assert_eq!(probe_check(&s, false).status, Status::Fail);
        // An inconclusive rule whose point estimate holds is not a failure.
        let mut fine = probe("c", "inconclusive");
        fine.point_violated = false;
        s.probes = Some(vec![fine]);
        assert_eq!(probe_check(&s, false).status, Status::Inconclusive);
    }

    #[test]
    fn ladder_verdicts_become_checks() {
        let mut s = summary(&[&[
            ("nop", 0.0),
            ("reference", 50.0),
            ("told", 120.0),
            ("fade", 10.0),
        ]]);
        s.rules.push(RuleResult {
            rule: "fade < reference".into(),
            ok: true,
            sets_holding: 1,
            sets: 1,
            odds: 0.9,
        });
        s.exploits.push(ExploitResult {
            bot: "x".into(),
            ok: false,
            ratio: Some(3.1),
            profitable: 0.9,
            reasons: vec!["profitable in 90% of sessions".into()],
            ..Default::default()
        });
        let checks = ladder_checks(&s, &gate(&["told"], &["fade"]), false);
        let status = |id: &str| checks.iter().find(|c| c.id == id).unwrap().status;
        assert_eq!(status("reference-scores-1"), Status::Pass);
        assert_eq!(status("nop-scores-0"), Status::Pass);
        assert_eq!(status("ladder-order"), Status::Pass);
        assert_eq!(status("informed-vs-non-reading"), Status::Pass);
        assert_eq!(status("exploit-bots"), Status::Fail);
        assert_eq!(status("ladder-clean"), Status::Pass);
        s.totals[0].insert("nop".into(), 3.0);
        s.disqualified.push(Disqualified {
            rung: "reference".into(),
            scenarios: vec!["a".into()],
        });
        let checks = ladder_checks(&s, &gate(&["told"], &["fade"]), false);
        let status = |id: &str| checks.iter().find(|c| c.id == id).unwrap().status;
        assert_eq!(status("nop-scores-0"), Status::Fail);
        assert_eq!(status("ladder-clean"), Status::Fail);
    }

    #[test]
    fn a_reference_that_lost_money_on_a_fast_sample_is_inconclusive_not_failed() {
        let mut s = summary(&[&[
            ("nop", 0.0),
            ("reference", -38.0),
            ("fade", 18.0),
            ("told", 850.0),
        ]]);
        s.rules.push(RuleResult {
            rule: "fade < reference".into(),
            ok: false,
            sets_holding: 0,
            sets: 1,
            odds: 0.16,
        });
        let g = gate(&["told"], &["fade"]);
        let status =
            |checks: &[Check], id: &str| checks.iter().find(|c| c.id == id).unwrap().status;
        let fast = ladder_checks(&s, &g, true);
        assert_eq!(status(&fast, "reference-scores-1"), Status::Inconclusive);
        assert_eq!(status(&fast, "ladder-order"), Status::Inconclusive);
        let full = ladder_checks(&s, &g, false);
        assert_eq!(status(&full, "reference-scores-1"), Status::Fail);
        assert_eq!(status(&full, "ladder-order"), Status::Fail);
    }

    #[test]
    fn not_run_never_fails_the_report_and_a_failure_does() {
        let ok = vec![
            Check::pass(SOLVABLE, "a", "A", "fine"),
            Check::not_run(REALISTIC, "b", "B", "not built"),
        ];
        let report = Report::new("t", "fast", "abc".into(), ok);
        assert!(report.passed);
        assert_eq!(report.counts["not_run"], 1);
        let md = report.markdown();
        assert!(md.contains("## Solvable and fair") && md.contains("## Realistic"));
        assert!(md.contains("| not run | B | not built |"));
        let bad = Report::new(
            "t",
            "fast",
            "abc".into(),
            vec![Check::fail(GRADED, "c", "C", "a | b")],
        );
        assert!(!bad.passed);
        assert!(bad.markdown().contains("a \\| b"));
    }

    #[test]
    fn harbor_rewards_are_read_per_agent() {
        let dir = std::env::temp_dir().join(format!("mtx-gates-{}", uuid::Uuid::new_v4().simple()));
        let put = |agent: &str, json: &str| {
            let d = dir.join(agent).join("job/trial/verifier");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("reward.json"), json).unwrap();
        };
        put("oracle", r#"{"reward": 1.0, "skill": 1.0}"#);
        put("nop", r#"{"reward": 0.0}"#);
        put("component-cheater", r#"{"reward": 0.0, "tampered": 0.0}"#);
        put("cheater", r#"{"reward": 0.0, "not_graded": 1.0}"#);
        put("sneaky", r#"{"reward": 0.0, "not_graded": 1.0}"#);
        let checks = harbor_checks(&dir, true, "", tasks::Headline::V1);
        assert!(
            checks.iter().all(|c| c.status == Status::Pass),
            "{checks:?}"
        );
        // Under v2 the oracle's skill 1 isn't the bar: before the charge,
        // its v2 score is 0.5.
        let v2 = |dir: &Path| harbor_checks(dir, true, "", tasks::Headline::V2)[0].status;
        assert_eq!(v2(&dir), Status::Fail, "a v1 reward of 1.0");
        put(
            "oracle",
            r#"{"reward": 0.5, "score_v2": 0.5, "score_v2_raw_before_charge": 0.5}"#,
        );
        assert_eq!(v2(&dir), Status::Pass);
        assert_eq!(
            harbor_checks(&dir, true, "", tasks::Headline::V1)[0].status,
            Status::Fail,
            "no skill"
        );
        // A reference charged by a calmer informed bot scores under 0.5 by
        // design; before the charge it is the 0.5, and it passes.
        put(
            "oracle",
            r#"{"reward": 0.41, "score_v2": 0.41, "score_v2_raw_before_charge": 0.5, "score_v2_risk_charge_usd": 37.5}"#,
        );
        assert_eq!(v2(&dir), Status::Pass);
        let detail = &harbor_checks(&dir, true, "", tasks::Headline::V2)[0].detail;
        assert!(detail.contains("$37.50"), "{detail}");
        // Without the pre-charge value it can't pass.
        put("oracle", r#"{"reward": 0.5, "score_v2": 0.5}"#);
        assert_eq!(v2(&dir), Status::Fail);
        put(
            "oracle",
            r#"{"reward": 0.5, "score_v2": 0.5, "score_v2_raw_before_charge": 0.5, "skill": 1.0}"#,
        );
        assert_eq!(v2(&dir), Status::Fail, "a v1 score beside it");
        put(
            "oracle",
            r#"{"reward": 0.31, "score_v2": 0.31, "score_v2_raw_before_charge": 0.31}"#,
        );
        assert_eq!(
            v2(&dir),
            Status::Fail,
            "floored: under 0.5 before any charge"
        );
        put("oracle", r#"{"reward": 1.0, "skill": 1.0}"#);
        put("sneaky", r#"{"reward": 0.4, "not_graded": 0.0}"#);
        assert_eq!(
            harbor_checks(&dir, true, "", tasks::Headline::V1)[2].status,
            Status::Fail
        );
        assert!(
            harbor_checks(&dir, false, "boom", tasks::Headline::V1)
                .iter()
                .all(|c| c.status == Status::Fail)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checks_from_other_changes_are_found_by_name() {
        assert!(has_command(&["grade"], Some("report")));
        assert!(!has_command(&["grade"], Some("no-such-flag")));
        assert!(!has_command(&["no-such-tool", "check"], None));
    }

    #[test]
    fn an_earlier_runs_rewards_are_cleared_before_harbor_runs() {
        let jobs = std::env::temp_dir().join(format!("mtx-gates-stale-{}", std::process::id()));
        let oracle = jobs.join("oracle/2026-11-01__00-00-00/trial");
        std::fs::create_dir_all(&oracle).unwrap();
        std::fs::write(oracle.join("reward.json"), r#"{"skill": 1.0}"#).unwrap();
        assert_eq!(
            harbor_checks(&jobs, true, "", tasks::Headline::V1)[0].status,
            Status::Pass
        );
        clear_dir(&jobs).unwrap();
        clear_dir(&jobs).unwrap();
        let oracle = &harbor_checks(&jobs, true, "", tasks::Headline::V1)[0];
        assert_eq!(oracle.status, Status::Fail);
        assert!(oracle.detail.contains("missing"), "{}", oracle.detail);
    }

    #[test]
    fn an_unjudged_market_is_inconclusive_and_a_failed_one_fails() {
        let report = |passed: bool, verdict: &str| serde_json::json!({"sessions": 300, "checks": [], "passed": passed, "verdict": verdict});
        assert_eq!(realism_status(Some(0), &report(true, "pass")), Status::Pass);
        assert_eq!(
            realism_status(Some(3), &report(true, "not_judged")),
            Status::Inconclusive
        );
        assert_eq!(
            realism_status(Some(0), &report(true, "not_judged")),
            Status::Inconclusive
        );
        let unlabelled = serde_json::json!({"checks": [], "passed": true});
        assert_eq!(realism_status(Some(3), &unlabelled), Status::Inconclusive);
        assert_eq!(realism_status(Some(0), &unlabelled), Status::Pass);
        assert_eq!(
            realism_status(Some(2), &report(false, "fail")),
            Status::Fail
        );
        assert_eq!(
            realism_status(Some(2), &report(true, "not_judged")),
            Status::Fail
        );
        assert_eq!(realism_status(Some(1), &report(true, "pass")), Status::Fail);
        assert_eq!(realism_status(None, &report(true, "pass")), Status::Fail);
        assert_eq!(
            realism_status(Some(0), &report(false, "fail")),
            Status::Fail
        );
    }

    #[test]
    fn a_realism_report_reads_in_a_line() {
        let report = serde_json::json!({
            "sessions": 60,
            "checks": [
                {"name": "returns", "status": "skipped", "detail": ""},
                {"name": "spread", "status": "pass", "detail": ""},
                {"name": "gaps", "status": "fail", "detail": ""},
            ],
            "passed": false,
        });
        assert_eq!(
            realism_summary(&report),
            "60 sessions; pass spread; fail gaps; skipped returns"
        );
    }

    #[test]
    fn a_latent_in_a_practice_file_fails_a_v2_task_and_warns_a_v1_one() {
        use mtx_grader::generators;
        let root = std::env::temp_dir().join(format!("mtx-practice-latent-{}", std::process::id()));
        let practice = root.join("bench/tasks/t/environment/practice");
        std::fs::create_dir_all(&practice).unwrap();
        let g = generators::generator("weekend-gap").unwrap();
        let mut scenario = g.practice("practice", 6, 5).remove(0);
        assert!(
            scenario.description.contains("account"),
            "{}",
            scenario.description
        );
        let write = |name: &str, scenario: &mtx_grader::scenario::Scenario| {
            scenario.save(&practice.join(name)).unwrap();
        };
        let (v1, v2) = (tasks::Headline::V1, tasks::Headline::V2);
        let check = |h| practice_holds_no_latent(&root, "t", "weekend-gap", h);
        // The generator's description names the draw ("account 4"): a v1
        // task is warned, never passed, and a v2 task fails.
        write("practice-1.json", &scenario);
        let warned = check(v1);
        assert_eq!(warned.status, Status::Warn, "{}", warned.detail);
        assert!(
            warned.detail.contains("practice-1.json states"),
            "{}",
            warned.detail
        );
        let failed = check(v2);
        assert_eq!(failed.status, Status::Fail, "{}", failed.detail);
        // The neutral description passes both.
        scenario.description = g.neutral_description(&scenario.symbol);
        write("practice-1.json", &scenario);
        assert_eq!(check(v1).status, Status::Pass);
        assert_eq!(check(v2).status, Status::Pass);
        // A latent member fails both: `Scenario` refuses it.
        let mut text: Value = serde_json::from_str(
            &std::fs::read_to_string(practice.join("practice-1.json")).unwrap(),
        )
        .unwrap();
        text["latent"] = serde_json::json!({"roster": ["informed"]});
        std::fs::write(practice.join("practice-2.json"), text.to_string()).unwrap();
        assert_eq!(check(v1).status, Status::Fail);
        std::fs::remove_file(practice.join("practice-2.json")).unwrap();
        // So does a file in a subdirectory, and a file that is not a scenario.
        std::fs::create_dir_all(practice.join("sub")).unwrap();
        std::fs::write(practice.join("sub/notes.txt"), "informed account 4").unwrap();
        let check_sub = check(v1);
        assert_eq!(check_sub.status, Status::Fail, "{}", check_sub.detail);
        assert!(
            check_sub.detail.contains("notes.txt"),
            "{}",
            check_sub.detail
        );
        std::fs::remove_dir_all(practice.join("sub")).unwrap();
        // A weekend-gap description in a subdirectory is read too.
        std::fs::create_dir_all(practice.join("sub")).unwrap();
        let mut leaky = g.practice("practice", 6, 5).remove(0);
        leaky.name = "practice-9".into();
        leaky.save(&practice.join("sub/practice-9.json")).unwrap();
        let sub = check(v2);
        assert_eq!(sub.status, Status::Fail, "{}", sub.detail);
        assert!(sub.detail.contains("sub/practice-9.json"), "{}", sub.detail);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The scaffold's empty `.gitkeep` and a schema-2 file are what a correct
    /// task ships; a c1-shaped `making.roles` names the informed account.
    #[test]
    fn practice_gate_reads_schema_2_and_empty_files_and_refuses_a_role_map() {
        let root = std::env::temp_dir().join(format!("mtx-practice-v2-{}", std::process::id()));
        let practice = root.join("bench/tasks/t/environment/practice");
        std::fs::create_dir_all(&practice).unwrap();
        let g = mtx_grader::generators::generator("market-making").unwrap();
        let mut scenario = g.practice("practice", 1, 5).remove(0);
        scenario.description = g.neutral_description(&scenario.symbol);
        let check = || practice_holds_no_latent(&root, "t", "market-making", tasks::Headline::V2);
        std::fs::write(practice.join(".gitkeep"), "").unwrap();
        scenario.save_v2(&practice.join("practice-1.json")).unwrap();
        let ok = check();
        assert_eq!(ok.status, Status::Pass, "{}", ok.detail);
        scenario.save(&practice.join("practice-1.json")).unwrap();
        let mut stated: Value =
            serde_json::from_str(&std::fs::read_to_string(practice.join("practice-1.json")).unwrap())
                .unwrap();
        stated["schema"] = 1.into();
        std::fs::write(practice.join("practice-1.json"), stated.to_string()).unwrap();
        let ok = check();
        assert_eq!(ok.status, Status::Pass, "{}", ok.detail);
        stated["making"]["roles"] = serde_json::json!({"informed": [7]});
        std::fs::write(practice.join("practice-2.json"), stated.to_string()).unwrap();
        let failed = check();
        assert_eq!(failed.status, Status::Fail, "{}", failed.detail);
        assert!(failed.detail.contains("practice-2.json"), "{}", failed.detail);
        assert!(failed.detail.contains("roles"), "{}", failed.detail);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// infra/53's actor scenarios configure `actors.informed` and
    /// `actors.worker`, which are not a latent state.
    #[test]
    fn an_actors_config_with_informed_and_worker_passes() {
        let root = std::env::temp_dir().join(format!("mtx-practice-actors-{}", std::process::id()));
        let practice = root.join("bench/tasks/t/environment/practice");
        std::fs::create_dir_all(&practice).unwrap();
        let g = mtx_grader::generators::generator("weekend-gap").unwrap();
        let scenario = g.practice("practice", 1, 5).remove(0);
        let mut json = serde_json::to_value(&scenario).unwrap();
        json["description"] = g.neutral_description(&scenario.symbol).into();
        // The shape of bench/checks/actors/pooled.json (infra/53): informed
        // wallets and a worker are the library's configuration.
        json["actors"] = serde_json::json!({
            "scale": {"trades_per_minute": 1.5, "trade_size": 0.02, "trade_size_sigma": 1.0},
            "noise": [{"wallets": 3, "flow_mult": 20.0, "size_mult": 5.0}],
            "informed": [{"wallets": 2, "horizon_slots": 40, "edge_bps": 30.0,
                "intensity_mult": 40.0, "size_mult": 10.0}],
        });
        std::fs::write(practice.join("practice-1.json"), json.to_string()).unwrap();
        for headline in [tasks::Headline::V1, tasks::Headline::V2] {
            let check = practice_holds_no_latent(&root, "t", "weekend-gap", headline);
            assert_eq!(check.status, Status::Pass, "{}", check.detail);
        }
        let keys = mtx_grader::generators::latent_keys_in(&json.to_string());
        assert!(keys.is_empty(), "{keys:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn no_verifier_tells_the_graded_bot_anything() {
        let root = authoring::repo_root();
        for task in tasks::load(&root).unwrap() {
            let test = std::fs::read_to_string(
                root.join("bench/tasks")
                    .join(&task.dir)
                    .join("tests/test.sh"),
            )
            .unwrap();
            let check = bot_told_nothing(&test);
            assert_eq!(check.status, Status::Pass, "{}: {}", task.dir, check.detail);
            let check =
                practice_holds_no_latent(&root, &task.dir, &task.mtx.generator, task.mtx.headline);
            assert_ne!(check.status, Status::Fail, "{}: {}", task.dir, check.detail);
            // The four v1 tasks that keep the generator's description are
            // reported, never certified clean.
            if ["funding-carry", "weekend-gap"].contains(&task.mtx.generator.as_str()) {
                assert_eq!(check.status, Status::Warn, "{}: {}", task.dir, check.detail);
            }
        }
        let verifier = "mtx grade \\\n  --draw flash-crash \\\n  --bot /app/bot\n";
        assert_eq!(bot_told_nothing(verifier).status, Status::Pass);
        for flag in ["--tell-latent", "--tell-regime"] {
            let told = verifier.replace("--bot", &format!("{flag} \\\n  --bot"));
            let check = bot_told_nothing(&told);
            assert_eq!(check.status, Status::Fail, "{told}");
            assert!(check.detail.contains(flag), "{}", check.detail);
        }
        // A comment that names a flag doesn't count.
        assert_eq!(
            bot_told_nothing(&format!("# never --tell-latent here\n{verifier}")).status,
            Status::Pass
        );
    }

    #[test]
    fn each_task_is_graded_on_its_own_snapshot() {
        let root = authoring::repo_root();
        for task in tasks::load(&root).unwrap() {
            let snapshot = snapshot(&root, &task.dir);
            assert!(root.join(&snapshot).is_dir(), "{snapshot}");
        }
        assert_eq!(
            snapshot(&root, "perp-accumulate-nvda"),
            "snapshots/phoenix-perps-2026-11-13"
        );
        // A task on another market: task.toml's `[metadata] snapshot` first,
        // then the verifier image's MTX_SNAPSHOT, then the Justfile's.
        let dir = std::env::temp_dir().join(format!("mtx-gates-snap-{}", std::process::id()));
        let folder = dir.join("bench/tasks/nvda-task");
        std::fs::create_dir_all(folder.join("tests")).unwrap();
        std::fs::write(
            dir.join("Justfile"),
            "snapshot := \"phoenix-spcx-2026-10-29\"\n",
        )
        .unwrap();
        assert_eq!(
            snapshot(&dir, "nvda-task"),
            "snapshots/phoenix-spcx-2026-10-29"
        );
        std::fs::write(
            folder.join("tests/Dockerfile"),
            "FROM x\nENV MTX_SNAPSHOT=/opt/mtx/snapshots/phoenix-perps-oracles-2026-11-13\n",
        )
        .unwrap();
        assert_eq!(
            snapshot(&dir, "nvda-task"),
            "snapshots/phoenix-perps-oracles-2026-11-13"
        );
        std::fs::write(
            folder.join("task.toml"),
            "[task]\nname = \"x\"\n\n[metadata]\nsnapshot = \"phoenix-perps-2026-11-13\"\n",
        )
        .unwrap();
        assert_eq!(
            snapshot(&dir, "nvda-task"),
            "snapshots/phoenix-perps-2026-11-13"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_calibration_comes_from_the_repo() {
        let dir = std::env::temp_dir().join(format!("mtx-gates-cal-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("calibration/market")).unwrap();
        for f in [
            "spcx.20261101.aa.toml",
            "spcx.20261111.bb.toml",
            "spcx.20261111.bb.inputs.json",
            "sol-usd.20261201.cc.toml",
        ] {
            std::fs::write(dir.join("calibration/market").join(f), "").unwrap();
        }
        let file = calibration_file(&dir, "flash-crash-spcx").unwrap();
        assert!(
            file.ends_with("spcx.20261111.bb.toml"),
            "{}",
            file.display()
        );
        assert!(calibration_file(&dir, "flash-crash-btc").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_task_declares_its_signal_and_names_real_rungs() {
        let root = authoring::repo_root();
        for task in [
            "flash-crash-spcx",
            "market-making-spcx",
            "funding-carry-spcx",
            "weekend-gap-spcx",
        ] {
            let config =
                read_json(&root.join(format!("bench/tasks/{task}/checks/ladder.json"))).unwrap();
            let gate: SignalGate = serde_json::from_value(config["gates"].clone()).unwrap();
            assert!(
                !gate.informed.is_empty() && !gate.non_reading.is_empty(),
                "{task}"
            );
            let rungs: Vec<&str> = config["rungs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["name"].as_str().unwrap())
                .collect();
            for name in gate.informed.iter().chain(&gate.non_reading) {
                assert!(rungs.contains(&name.as_str()), "{task}: no rung {name}");
            }
        }
    }

    #[test]
    fn status_reads_the_component_layout() {
        let root = authoring::repo_root();
        let steps = authoring::status(&root, "flash-crash-spcx").unwrap();
        let reference = steps.iter().find(|s| s.id == "reference").unwrap();
        assert_eq!(reference.state, "done", "{}", reference.detail);
    }
}
