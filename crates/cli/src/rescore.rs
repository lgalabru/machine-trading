//! `mtx rescore` and `mtx bench rescore`: a finished grade scored again from
//! what it wrote ([`mtx_grader::grade::rescore`]), for when a scoring rule
//! changes. Nothing runs: each session's report and scenario come from its
//! detail file (`mtx grade --details`), in the order the report lists them,
//! and the result is compared with the report, field by field.

use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use mtx_grader::{
    grade::{self, GradeReport, StoredSession, Verdict},
    replay::{self, Difference},
    scenario::Scenario,
    scoring_v2::{self, Ceiling, Params},
    session::SessionReport,
};
use serde_json::{Value, json};

use crate::review::Root;

/// Which scores a grade reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Scoring {
    /// `reward` and `skill`, as always.
    #[default]
    V1,
    /// Those, and scoring v2 beside them (`v2` in the report, `score_v2*`
    /// in the rewards): 0 = doing nothing, 0.5 = the reference, 1 = the
    /// task's informed bot, losses counted 1.5 times and a breach scored per
    /// session. Report-only: `reward` and `skill` don't change.
    V2,
}

/// How a grade is scored: `mtx grade`, `mtx rescore` and `mtx bench rescore`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ScoringArgs {
    #[arg(long, value_enum, default_value_t = Scoring::V1)]
    pub scoring: Scoring,
    /// v2's proxy ceiling, for a task whose scenarios carry no informed
    /// bot's PnL: this share of each session's hindsight bound, a constant
    /// frozen with the task (the score says `ceiling: proxy`).
    /// `<task>=<eta>` gives one task's (as named in bench/tasks), for a
    /// batch of several.
    #[arg(long, value_name = "[TASK=]ETA")]
    pub eta: Vec<String>,
    /// v2's ceiling given outright instead: a JSON object from scenario
    /// name to ceiling PnL in USD.
    #[arg(long, conflicts_with = "eta")]
    pub ceiling: Option<PathBuf>,
    /// v2: how many times a loss counts.
    #[arg(long, default_value_t = scoring_v2::LOSS_WEIGHT)]
    pub loss_weight: f64,
    /// v2: the least the reference anchor can be, as a share of the
    /// ceiling; a weaker reference is floored there and flagged.
    #[arg(long, default_value_t = scoring_v2::ANCHOR_FLOOR)]
    pub anchor_floor: f64,
}

impl ScoringArgs {
    /// v2's parameters for a grade of `task`, or `None` under v1.
    pub fn params(&self, task: Option<&str>) -> Result<Option<Params>> {
        if self.scoring == Scoring::V1 {
            if !self.eta.is_empty() || self.ceiling.is_some() {
                bail!("--eta and --ceiling are for --scoring v2");
            }
            return Ok(None);
        }
        let ceiling = match &self.ceiling {
            Some(path) => Ceiling::Given(
                serde_json::from_slice::<BTreeMap<String, f64>>(
                    &std::fs::read(path).with_context(|| path.display().to_string())?,
                )
                .with_context(|| format!("{}: not a map of ceilings", path.display()))?,
            ),
            None => Ceiling::Informed {
                proxy_eta: self.eta(task)?,
            },
        };
        Ok(Some(Params {
            loss_weight: self.loss_weight,
            anchor_floor: self.anchor_floor,
            ..Params::new(ceiling)
        }))
    }

    /// The `--eta` for `task`: its own, else the one without a task.
    fn eta(&self, task: Option<&str>) -> Result<Option<f64>> {
        let mut any = None;
        for given in &self.eta {
            let (name, value) = match given.split_once('=') {
                Some((name, value)) => (Some(name), value),
                None => (None, given.as_str()),
            };
            let eta: f64 = value
                .parse()
                .with_context(|| format!("--eta {given}: not a number"))?;
            if !(eta > 0.0 && eta <= 1.0) {
                bail!("--eta {given}: a share of the bound, in (0, 1]");
            }
            match name {
                Some(name) if Some(name) == task => return Ok(Some(eta)),
                Some(_) => {}
                None => any = Some(eta),
            }
        }
        Ok(any)
    }
}

/// A grade scored again, and where it differs from its report.
pub struct Rescored {
    pub report: GradeReport,
    pub differences: Vec<Difference>,
}

/// Score again the grade whose report is at `report`, its detail files
/// under `details` (`bot/<scenario>.json.gz`). `release` is the published
/// release they're in, for a compact one's scenarios. With `v2`, scoring v2
/// is added; a report that had none isn't compared on it.
pub fn rescore(
    report: &Path,
    details: &Path,
    release: Option<&Path>,
    v2: Option<&Params>,
) -> Result<Rescored> {
    let path = report;
    let original: Value = serde_json::from_slice(
        &std::fs::read(report).with_context(|| report.display().to_string())?,
    )
    .with_context(|| report.display().to_string())?;
    let verdict: Verdict = serde_json::from_value(original.clone())
        .with_context(|| format!("{}: not a grade's report", report.display()))?;
    let sessions = original["scenarios"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|result| {
            let name = result["scenario"]
                .as_str()
                .with_context(|| format!("{}: a session without a name", report.display()))?;
            read_session(&details.join("bot"), name, release)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut report = grade::rescore(sessions, verdict);
    if let Some(params) = v2 {
        report
            .score_v2(params)
            .with_context(|| path.display().to_string())?;
    }
    let mut rescored = serde_json::to_value(&report)?;
    if original.get("v2").is_none()
        && let Value::Object(fields) = &mut rescored
    {
        fields.remove("v2");
    }
    let differences = replay::compare(&named(&original), &named(&rescored), 2);
    Ok(Rescored {
        report,
        differences,
    })
}

/// One session's report and scenario, from its detail file.
fn read_session(
    dir: &Path,
    name: &str,
    release: Option<&Path>,
) -> Result<(SessionReport, Scenario)> {
    let path = dir.join(format!("{name}.json.gz"));
    let mut json = Vec::new();
    flate2::read::GzDecoder::new(
        std::fs::File::open(&path)
            .with_context(|| format!("{}: no detail file", path.display()))?,
    )
    .read_to_end(&mut json)
    .with_context(|| path.display().to_string())?;
    let mut value: Value =
        serde_json::from_slice(&json).with_context(|| path.display().to_string())?;
    crate::compact::expand_session(&mut value, release);
    let stored: StoredSession =
        serde_json::from_value(value).with_context(|| path.display().to_string())?;
    Ok(stored.into_parts())
}

/// A report's parts as the replay check names them: the totals, then each
/// of the bot's sessions. A replay check's own section isn't compared.
fn named(report: &Value) -> Vec<(String, Value)> {
    let mut totals = report.clone();
    if let Value::Object(fields) = &mut totals {
        fields.remove("scenarios");
        fields.remove("replay");
    }
    let mut named = vec![("grade".to_string(), totals)];
    for session in report["scenarios"].as_array().into_iter().flatten() {
        let name = session["scenario"].as_str().unwrap_or_default();
        named.push((format!("bot/{name}"), session.clone()));
    }
    named
}

/// Where a rescored report differs from its original: the part, the first
/// field, and its value in each.
pub fn describe(difference: &Difference) -> String {
    format!(
        "{}: {} was {} and is {}",
        difference.report, difference.field, difference.first, difference.this
    )
}

/// Every trial under `root` scored again: per trial, its reward and skill
/// as reported and as scored now, and the first field that differs. A trial
/// with no report (it never graded) is listed as skipped, and one whose
/// files don't read back (a release from before reports had their present
/// shape) as failed, with why. Under `--scoring v2`, each trial's v2 too,
/// with its task's `--eta`.
pub fn rescore_trials(root: &Root, scoring: &ScoringArgs) -> Result<Value> {
    let trials = crate::review::discover(&root.path);
    if trials.is_empty() {
        bail!("no Harbor trials under {}", root.path.display());
    }
    let mut rows = Vec::new();
    // Each task's v2 grades, by model, for the comparison across runs.
    let mut graded: BTreeMap<String, Vec<Graded>> = BTreeMap::new();
    let (mut same, mut changed, mut skipped, mut failed) = (0, 0, 0, 0);
    for trial in &trials {
        let name = trial
            .strip_prefix(&root.path)
            .unwrap_or(trial)
            .display()
            .to_string();
        let report = trial.join("verifier/report.json");
        if !report.is_file() {
            skipped += 1;
            rows.push(json!({ "trial": name, "skipped": "no verifier/report.json" }));
            continue;
        }
        // A published trial is <release>/<job>/<trial>.
        let release: Option<PathBuf> = trial.parent().and_then(Path::parent).map(Path::to_path_buf);
        let rescored = match scoring.params(Some(&task_of(trial))).and_then(|v2| {
            rescore(
                &report,
                &trial.join("verifier/sessions"),
                release.as_deref(),
                v2.as_ref(),
            )
        }) {
            Ok(rescored) => rescored,
            Err(error) => {
                failed += 1;
                rows.push(json!({ "trial": name, "failed": format!("{error:#}") }));
                continue;
            }
        };
        let original = crate::review::read_json(&report).unwrap_or(Value::Null);
        if rescored.differences.is_empty() {
            same += 1;
        } else {
            changed += 1;
        }
        let mut row = json!({
            "trial": name,
            "reward": [original["reward"], rescored.report.reward],
            "skill": [original["skill"], rescored.report.skill],
            "same": rescored.differences.is_empty(),
            "differences": rescored.differences.len(),
            "first": rescored.differences.first().map(describe),
        });
        if let Some(v2) = &rescored.report.v2 {
            row["v2"] = json!({
                "task": task_of(trial),
                "score": v2.score,
                "raw": v2.raw,
                "raw_unfloored": v2.raw_unfloored,
                "edge": v2.edge,
                "ci": [v2.ci_lo, v2.ci_hi],
                "ci_scope": v2.ci_scope,
                "ci_use": v2.ci_use,
                "within_noise": v2.within_noise,
                "beats_informed": v2.beats_informed,
                "ceiling": v2.ceiling,
                "anchor_floored": v2.anchor_floored,
                "gap": {
                    "wrong_read": v2.gap.wrong_read,
                    "missed": v2.gap.missed,
                    "execution": v2.gap.execution,
                    "risk": v2.gap.risk,
                    "unattributed": v2.gap.unattributed,
                    "noise": v2.gap.noise,
                },
                "breached_sessions": v2.breached_sessions,
                "fatal": v2.fatal,
            });
            graded.entry(task_of(trial)).or_default().push((
                model_of(trial),
                v2.clone(),
                rescored.report.eval_seed_sha256.clone(),
            ));
        }
        rows.push(row);
    }
    let mut summary = json!({
        "root": root.label,
        "trials": trials.len(),
        "same": same,
        "changed": changed,
        "skipped": skipped,
        "failed": failed,
        "rescored": rows,
    });
    if !graded.is_empty() {
        summary["v2_comparison"] = compare_models(&graded, scoring)?;
    }
    Ok(summary)
}

/// One trial's model, v2 score and `eval_seed_sha256`.
type Graded = (String, scoring_v2::ScoreV2, Option<String>);

/// Per task with more than one run: each model's mean v2 `raw` and the
/// paired interval of every difference between models, from a bootstrap of
/// scenarios and runs (see [`scoring_v2::compare`]). A task whose runs
/// can't be compared says why.
fn compare_models(graded: &BTreeMap<String, Vec<Graded>>, scoring: &ScoringArgs) -> Result<Value> {
    let mut tasks = serde_json::Map::new();
    for (task, runs) in graded {
        if runs.len() < 2 {
            continue;
        }
        let Some(params) = scoring.params(Some(task))? else {
            continue;
        };
        let compared = scoring_v2::compare(
            &runs
                .iter()
                .map(|(model, v2, _)| scoring_v2::Run {
                    model: model.clone(),
                    v2,
                })
                .collect::<Vec<_>>(),
            &params,
            runs.iter().find_map(|(_, _, seed)| seed.as_deref()),
        );
        tasks.insert(
            task.clone(),
            match compared {
                Ok(comparison) => serde_json::to_value(comparison)?,
                Err(why) => json!({ "failed": why }),
            },
        );
    }
    Ok(Value::Object(tasks))
}

/// A trial's model: the last part of its agent's model name.
fn model_of(trial: &Path) -> String {
    let config = crate::review::read_json(&trial.join("config.json")).unwrap_or(Value::Null);
    config["agent"]["model_name"]
        .as_str()
        .map(|m| m.rsplit('/').next().unwrap_or(m).to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// A trial's task, as named in bench/tasks: the last part of its config's
/// task path, else the part of the trial's name before `__`.
fn task_of(trial: &Path) -> String {
    let config = crate::review::read_json(&trial.join("config.json")).unwrap_or(Value::Null);
    config["task"]["path"]
        .as_str()
        .and_then(|p| p.trim_end_matches('/').rsplit('/').next())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let name = trial.file_name().unwrap_or_default().to_string_lossy();
            name.split("__").next().unwrap_or_default().to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(scoring: Scoring, eta: &[&str]) -> ScoringArgs {
        ScoringArgs {
            scoring,
            eta: eta.iter().map(|e| e.to_string()).collect(),
            ceiling: None,
            loss_weight: scoring_v2::LOSS_WEIGHT,
            anchor_floor: scoring_v2::ANCHOR_FLOOR,
        }
    }

    #[test]
    fn v2_takes_a_tasks_own_eta_before_the_default() {
        let given = args(Scoring::V2, &["0.3", "flash-crash-spcx=0.37"]);
        let eta = |given: &ScoringArgs, task| match given.params(task).unwrap().unwrap().ceiling {
            Ceiling::Informed { proxy_eta } => proxy_eta,
            Ceiling::Given(_) => unreachable!(),
        };
        assert_eq!(eta(&given, Some("flash-crash-spcx")), Some(0.37));
        assert_eq!(eta(&given, Some("weekend-gap-spcx")), Some(0.3));
        assert_eq!(eta(&given, None), Some(0.3));
        let only = args(Scoring::V2, &["flash-crash-spcx=0.37"]);
        assert_eq!(eta(&only, Some("weekend-gap-spcx")), None);
        // Without η, only an informed bot's PnL can be the ceiling.
        assert_eq!(eta(&args(Scoring::V2, &[]), None), None);
        assert!(args(Scoring::V2, &["2"]).params(None).is_err());
        assert!(args(Scoring::V1, &[]).params(None).unwrap().is_none());
        assert!(args(Scoring::V1, &["0.3"]).params(None).is_err());
    }

    /// A rescore compares the floats it reads back with the ones in the
    /// report, so a float must parse to the value it was written from,
    /// bit for bit, even where its shortest form has 17 digits.
    #[test]
    fn floats_read_back_to_the_bit() {
        for x in [141.89000000000001_f64, 10095.836356485199, 0.1 + 0.2] {
            let text = serde_json::to_string(&x).unwrap();
            let back: f64 = serde_json::from_str(&text).unwrap();
            assert_eq!(back.to_bits(), x.to_bits(), "{text}");
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value.as_f64().unwrap().to_bits(), x.to_bits(), "{text}");
        }
    }
}
