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

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use clap::CommandFactory;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Cli, authoring};

/// The ladder's extra gate: the best informed policy must make at least this
/// many times the best one that doesn't read the task's signal.
const DEFAULT_MIN_RATIO: f64 = 2.0;
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
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::NotRun => "not run",
            Status::Inconclusive => "inconclusive",
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
            "# Gates: {}\n\n{} mode, commit `{}`. **{}**: {} passed, {} failed, {} inconclusive, {} not run.\n",
            self.task,
            self.mode,
            self.commit,
            if self.passed { "PASSED" } else { "FAILED" },
            count("pass"),
            count("fail"),
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
    pub ratio: f64,
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
    checks.push(Check::from(
        SOLVABLE,
        "reference-scores-1",
        "Reference is the 1.0 line (makes money on every set)",
        reference.len() == sets && reference.iter().all(|r| *r > 0.0),
        format!(
            "reference total PnL per set: {}",
            reference
                .iter()
                .map(|r| format!("${r:.0}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ));
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
    checks.push(Check::from(
        SOLVABLE,
        "ladder-order",
        "Skill ladder keeps its order",
        broken.is_empty() && !summary.rules.is_empty(),
        if broken.is_empty() {
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
        },
    ));
    checks.push(signal_gate(summary, gate));
    checks.push(exploit_check(summary, fast));
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
                format!(
                    "{} {:.2}x, profitable {:.0}%",
                    e.bot,
                    e.ratio,
                    e.profitable * 100.0
                )
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
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
fn harbor_checks(jobs: &Path, ran: bool, detail: &str) -> Vec<Check> {
    let title_oracle = "Oracle scores the reference's 1.0";
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
    let skill = get("oracle", "skill");
    checks.push(Check::from(
        SOLVABLE,
        "oracle",
        title_oracle,
        skill.is_some_and(|s| (0.999..=1.001).contains(&s)),
        format!(
            "oracle skill {}",
            skill.map_or("missing".into(), |s| format!("{s:.4}"))
        ),
    ));
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

/// The snapshot the Justfile grades on (`snapshot := "..."`).
fn snapshot(root: &Path) -> String {
    let justfile = std::fs::read_to_string(root.join("Justfile")).unwrap_or_default();
    let name = justfile
        .lines()
        .find_map(|l| l.strip_prefix("snapshot := "))
        .map_or(DEFAULT_SNAPSHOT, |v| v.trim().trim_matches('"'));
    format!("snapshots/{name}")
}

/// A fresh eval secret for one check, never printed.
fn secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Replays are byte-identical: the reference graded `runs` times on a fresh
/// hidden set (`mtx grade --replay`), the machine loaded in full mode.
fn replay_check(root: &Path, family: &str, ladder: &Value, fast: bool, out: &Path) -> Check {
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
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mtx"));
    let output = Command::new(exe)
        .args(["grade", "--draw", family, "--jobs", "4", "--bot", bot])
        .args(["--count", &count.to_string(), "--snapshot", &snapshot(root)])
        .args([
            "--replay",
            &runs.to_string(),
            "--replay-load",
            &load.to_string(),
        ])
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
fn calibration_check(root: &Path, task: &str, family: &str) -> Check {
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
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mtx"));
    let output = match Command::new(exe).args(&args).current_dir(root).output() {
        Ok(o) => o,
        Err(e) => {
            return Check::fail(REALISTIC, id, title, format!("couldn't run it: {e}"));
        }
    };
    // It prints its report either way and exits 2 when a check fails.
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(report) if report["checks"].is_array() => {
            let name = Path::new(&file)
                .file_name()
                .map_or(file.clone(), |n| n.to_string_lossy().into_owned());
            Check::from(
                REALISTIC,
                id,
                title,
                output.status.success() && report["passed"] == true,
                format!("{name}: {}", realism_summary(&report)),
            )
        }
        _ => Check::fail(REALISTIC, id, title, tail(&output)),
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
    let family = authoring::family_of(task);
    let out = opts
        .out
        .clone()
        .unwrap_or_else(|| root.join("target/gates").join(task));
    std::fs::create_dir_all(&out)?;
    let mut checks = task_checks(root, task)?;

    // The ladder: one hidden set of a few sessions in fast mode.
    let ladder_file = root
        .join("bench/checks/ladder")
        .join(format!("{family}.json"));
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
            clear_dir(&ladder_out.join(&family))?;
            let seed = if opts.fast { FAST_PUBLIC_SEED } else { "" };
            // The ladder exits nonzero when a rule fails: the summary says which.
            run_in(
                root,
                "just",
                &[
                    "ladder",
                    &family,
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
            ladder_out.join(&family).join("summary.json")
        }
    };
    match std::fs::read_to_string(&summary_path) {
        Ok(text) => {
            let summary: LadderSummary =
                serde_json::from_str(&text).with_context(|| summary_path.display().to_string())?;
            checks.extend(ladder_checks(&summary, &signal, opts.fast));
        }
        Err(e) => checks.push(Check::fail(
            SOLVABLE,
            "ladder",
            "Skill ladder ran",
            format!("no {}: {e}", summary_path.display()),
        )),
    }

    // Full mode: the image checks.
    let full_only = |section, id, title| {
        Check::not_run(section, id, title, "full mode only (needs the task image)")
    };
    if opts.fast {
        checks.push(full_only(
            SOLVABLE,
            "oracle",
            "Oracle scores the reference's 1.0",
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
        checks.extend(harbor_checks(&jobs, ran, &detail));
        let (ok, d) = run_in(root, "just", &["leak-audit"], false)?;
        checks.push(Check::from(
            GRADED,
            "leak-audit",
            "The agent's image holds no answers",
            ok,
            if ok {
                "no answer in the image".into()
            } else {
                d
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

    checks.push(replay_check(root, &family, &config, opts.fast, &out));
    checks.push(calibration_check(root, task, &family));

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
        }
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
            ratio: 0.9,
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
        let mut s = summary(&[]);
        s.exploits.push(bot(10, 50.0, 0.2));
        let report = Report::new("t", "fast", "abc".into(), vec![exploit_check(&s, true)]);
        assert!(report.passed);
        assert!(report.markdown().contains("1 inconclusive"));
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
            ratio: 3.1,
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
        let checks = harbor_checks(&dir, true, "");
        assert!(
            checks.iter().all(|c| c.status == Status::Pass),
            "{checks:?}"
        );
        put("sneaky", r#"{"reward": 0.4, "not_graded": 0.0}"#);
        assert_eq!(harbor_checks(&dir, true, "")[2].status, Status::Fail);
        assert!(
            harbor_checks(&dir, false, "boom")
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
        assert_eq!(harbor_checks(&jobs, true, "")[0].status, Status::Pass);
        clear_dir(&jobs).unwrap();
        clear_dir(&jobs).unwrap();
        let oracle = &harbor_checks(&jobs, true, "")[0];
        assert_eq!(oracle.status, Status::Fail);
        assert!(oracle.detail.contains("missing"), "{}", oracle.detail);
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
    fn the_snapshot_and_calibration_come_from_the_repo() {
        let root = authoring::repo_root();
        assert!(root.join(snapshot(&root)).is_dir(), "{}", snapshot(&root));
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
        for family in [
            "flash-crash",
            "market-making",
            "funding-carry",
            "weekend-gap",
        ] {
            let config =
                read_json(&root.join(format!("bench/checks/ladder/{family}.json"))).unwrap();
            let gate: SignalGate = serde_json::from_value(config["gates"].clone()).unwrap();
            assert!(
                !gate.informed.is_empty() && !gate.non_reading.is_empty(),
                "{family}"
            );
            let rungs: Vec<&str> = config["rungs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["name"].as_str().unwrap())
                .collect();
            for name in gate.informed.iter().chain(&gate.non_reading) {
                assert!(rungs.contains(&name.as_str()), "{family}: no rung {name}");
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
