//! `mtx rescore` and `mtx bench rescore`: a finished grade scored again from
//! what it wrote ([`mtx_grader::grade::rescore`]), for when a scoring rule
//! changes. Nothing runs: each session's report and scenario come from its
//! detail file (`mtx grade --details`), in the order the report lists them,
//! and the result is compared with the report, field by field.

use std::{
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use mtx_grader::{
    grade::{self, GradeReport, StoredSession, Verdict},
    replay::{self, Difference},
    scenario::Scenario,
    session::SessionReport,
};
use serde_json::{Value, json};

use crate::review::Root;

/// A grade scored again, and where it differs from its report.
pub struct Rescored {
    pub report: GradeReport,
    pub differences: Vec<Difference>,
}

/// Score again the grade whose report is at `report`, its detail files
/// under `details` (`bot/<scenario>.json.gz`). `release` is the published
/// release they're in, for a compact one's scenarios.
pub fn rescore(report: &Path, details: &Path, release: Option<&Path>) -> Result<Rescored> {
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
    let report = grade::rescore(sessions, verdict);
    let differences = replay::compare(
        &named(&original),
        &named(&serde_json::to_value(&report)?),
        2,
    );
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
/// shape) as failed, with why.
pub fn rescore_trials(root: &Root) -> Result<Value> {
    let trials = crate::review::discover(&root.path);
    if trials.is_empty() {
        bail!("no Harbor trials under {}", root.path.display());
    }
    let mut rows = Vec::new();
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
        let rescored = match rescore(
            &report,
            &trial.join("verifier/sessions"),
            release.as_deref(),
        ) {
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
        rows.push(json!({
            "trial": name,
            "reward": [original["reward"], rescored.report.reward],
            "skill": [original["skill"], rescored.report.skill],
            "same": rescored.differences.is_empty(),
            "differences": rescored.differences.len(),
            "first": rescored.differences.first().map(describe),
        }));
    }
    Ok(json!({
        "root": root.label,
        "trials": trials.len(),
        "same": same,
        "changed": changed,
        "skipped": skipped,
        "failed": failed,
        "rescored": rows,
    }))
}

#[cfg(test)]
mod tests {
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
