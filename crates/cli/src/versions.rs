//! Which version produced a score, and the gate that keeps versions apart.
//!
//! Each trial's report carries the task's fingerprint and the scoring
//! spec's hash (`mtx_grader::versions`). `mtx bench publish`, `review`,
//! `compare` and `rescore` read them as [`Stamp`]s and refuse to aggregate
//! or compare trials of one task whose stamps differ (see [`enforce`]),
//! unless `--allow-version-mismatch` is given; the output then shows the
//! mismatch. The task fingerprint itself is [`fingerprint`].

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use mtx_grader::versions::{GRADER_VERSION, hash_parts};
use serde_json::{Value, json};

/// Where the verifier finds its task's fingerprint, written there by
/// `mtx task sync` (from `bench/tasks/<task>/tests/task.fingerprint`).
pub const VERIFIER_FINGERPRINT: &str = "/tests/task.fingerprint";
/// The file `mtx task sync` writes, under a task's folder.
pub const FINGERPRINT_FILE: &str = "tests/task.fingerprint";
/// What a missing stamp reads as.
pub const UNVERSIONED: &str = "unversioned";

/// The fingerprint a grade records: the one given, else the file the
/// verifier ships (blank counts as none).
pub fn recorded_fingerprint(given: Option<String>, file: &Path) -> Option<String> {
    given
        .or_else(|| std::fs::read_to_string(file).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// The seed and size of the scenario draw whose bytes stand for the task's
/// scenario family and its parameters. Public, never a hidden set's.
const FAMILY_SEED: u64 = 0x7461_736b_6670;
const FAMILY_COUNT: usize = 6;

/// Keys of `task.toml` that only describe the task (or point at a build of
/// it): they don't change what it measures.
const COSMETIC_KEYS: &[&str] = &[
    "version",
    "description",
    "keywords",
    "authors",
    "emoji",
    "market",
    "summary",
    "skill",
    "leak_needles",
    "docker_image",
    "build_timeout_sec",
];

/// A task's fingerprint: a hash over everything that changes what it
/// measures. `read` gives a file of the task's folder (`root`-relative, or
/// None), so a caller can overlay files not yet written (see `tasks::render`).
///
/// In: `task.toml` without its cosmetic keys (version, description,
/// authors, README table texts, image digests); `instruction.md` (the
/// mandate and the scoring rules); `tests/test.sh` (the grader's flags);
/// `environment/Dockerfile` without digests; `environment/practice/`; the
/// anchors, `reference/` and `checks/bots/informed/` (sources, not build
/// output); the scenario family and its parameters, as the bytes of a fixed
/// public draw of its generator; the snapshot's manifest; the grader's
/// version. Out: READMEs, DESIGN.md, the checks' ladder and other bots, the
/// verifier's Dockerfile.
pub fn fingerprint(
    root: &Path,
    task: &str,
    read: &dyn Fn(&Path) -> Option<Vec<u8>>,
) -> Result<String> {
    let folder = PathBuf::from("bench/tasks").join(task);
    let get = |rel: &str| read(&folder.join(rel));
    let toml_text = get("task.toml").with_context(|| format!("{task}: no task.toml"))?;
    let mut toml: toml::Value = toml::from_str(&String::from_utf8_lossy(&toml_text))
        .with_context(|| format!("{task}/task.toml"))?;
    strip(&mut toml);
    let generator = toml["metadata"]["mtx"]["generator"]
        .as_str()
        .with_context(|| format!("{task}: no [metadata.mtx] generator"))?
        .to_string();
    let snapshot = toml["metadata"]["snapshot"].as_str().map(str::to_string);

    let mut parts: Vec<(String, Vec<u8>)> = vec![
        ("grader".into(), GRADER_VERSION.to_string().into_bytes()),
        ("task.toml".into(), serde_json::to_vec(&toml)?),
    ];
    for rel in ["instruction.md", "tests/test.sh"] {
        parts.push((rel.into(), get(rel).unwrap_or_default()));
    }
    // A digest changes with every rebuild of an image, not with the task.
    let dockerfile = String::from_utf8_lossy(&get("environment/Dockerfile").unwrap_or_default())
        .lines()
        .map(|l| l.split("@sha256:").next().unwrap_or(l).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    parts.push(("environment/Dockerfile".into(), dockerfile.into_bytes()));
    for dir in ["environment/practice", "reference", "checks/bots/informed"] {
        for rel in files_under(root, &folder.join(dir)) {
            let bytes = read(&rel).unwrap_or_default();
            parts.push((rel.display().to_string(), bytes));
        }
    }
    if let Some(snapshot) = snapshot {
        let manifest = read(
            &PathBuf::from("snapshots")
                .join(&snapshot)
                .join("manifest.json"),
        );
        parts.push((format!("snapshot {snapshot}"), manifest.unwrap_or_default()));
    }
    let family = mtx_grader::generators::generator(&generator)
        .with_context(|| format!("{task}: no generator {generator}"))?;
    let drawn = (family.hidden)(FAMILY_COUNT, FAMILY_SEED, false)
        .with_context(|| format!("{task}: drawing {generator}"))?;
    parts.push((
        format!("family {generator}"),
        serde_json::to_vec(&json!([drawn.scenarios, drawn.annotations]))?,
    ));
    Ok(hash_parts(
        parts.iter().map(|(l, b)| (l.as_str(), b.as_slice())),
    ))
}

/// Drop the cosmetic keys from a parsed `task.toml`, at any depth.
fn strip(value: &mut toml::Value) {
    match value {
        toml::Value::Table(table) => {
            table.retain(|k, _| !COSMETIC_KEYS.iter().any(|c| *c == k));
            table.iter_mut().for_each(|(_, v)| strip(v));
        }
        toml::Value::Array(items) => items.iter_mut().for_each(strip),
        _ => {}
    }
}

/// The source files under `dir` (relative to `root`), sorted: those on
/// disk and none of build output.
fn files_under(root: &Path, dir: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = dir.join(&name);
            if entry.path().is_dir() {
                if !matches!(name.as_str(), "target" | "bin" | "node_modules") {
                    walk(root, &rel, out);
                }
            } else if !matches!(name.as_str(), "bot.wasm" | ".DS_Store") {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, dir, &mut out);
    out.sort();
    out
}

/// What a trial says about its versions.
#[derive(Debug, Clone, PartialEq)]
pub struct Stamp {
    pub trial: String,
    /// The task's short name.
    pub task: String,
    pub fingerprint: Option<String>,
    pub scoring: Option<String>,
}

impl Stamp {
    /// From a grade's report (`versions` in it).
    pub fn of_report(trial: &str, task: &str, report: &Value) -> Self {
        let get = |k: &str| report["versions"][k].as_str().map(str::to_string);
        Self {
            trial: trial.to_string(),
            task: task.to_string(),
            fingerprint: get("task_fingerprint"),
            scoring: get("scoring_hash"),
        }
    }
}

/// Trials of one task whose stamps disagree on one field.
#[derive(Debug, Clone, PartialEq)]
pub struct Mismatch {
    pub task: String,
    /// `task fingerprint` or `scoring spec`.
    pub field: &'static str,
    /// Each value (or `unversioned`) and the trials that carry it.
    pub groups: BTreeMap<String, Vec<String>>,
}

/// The mismatches among `stamps`, per task and field. Trials of different
/// tasks are never compared with each other.
pub fn mismatches(stamps: &[Stamp]) -> Vec<Mismatch> {
    let mut out = Vec::new();
    let mut tasks: Vec<&str> = stamps.iter().map(|s| s.task.as_str()).collect();
    tasks.sort();
    tasks.dedup();
    for task in tasks {
        for (field, pick) in [
            (
                "task fingerprint",
                (|s: &Stamp| s.fingerprint.clone()) as fn(&Stamp) -> _,
            ),
            ("scoring spec", |s: &Stamp| s.scoring.clone()),
        ] {
            let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for s in stamps.iter().filter(|s| s.task == task) {
                groups
                    .entry(pick(s).unwrap_or_else(|| UNVERSIONED.to_string()))
                    .or_default()
                    .push(s.trial.clone());
            }
            if groups.len() > 1 {
                out.push(Mismatch {
                    task: task.to_string(),
                    field,
                    groups,
                });
            }
        }
    }
    out
}

/// The trials with no fingerprint or no spec hash: written before versions,
/// or graded without them.
pub fn unversioned(stamps: &[Stamp]) -> Vec<String> {
    stamps
        .iter()
        .filter(|s| s.fingerprint.is_none() || s.scoring.is_none())
        .map(|s| s.trial.clone())
        .collect()
}

fn short(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

/// The mismatches as lines for a person.
pub fn describe(mismatches: &[Mismatch]) -> String {
    mismatches
        .iter()
        .map(|m| {
            let groups = m
                .groups
                .iter()
                .map(|(value, trials)| {
                    format!(
                        "{} ({} trial{})",
                        short(value),
                        trials.len(),
                        plural(trials.len())
                    )
                })
                .collect::<Vec<_>>()
                .join(" vs ");
            format!("{}: {} differs: {groups}", m.task, m.field)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub fn to_json(mismatches: &[Mismatch]) -> Value {
    Value::Array(
        mismatches
            .iter()
            .map(|m| json!({ "task": m.task, "field": m.field, "groups": m.groups }))
            .collect(),
    )
}

/// The gate: with mismatches, fail naming them and the flag, or (with
/// `allow`) say so on stderr and go on.
pub fn enforce(what: &str, mismatches: &[Mismatch], allow: bool) -> Result<()> {
    if mismatches.is_empty() {
        return Ok(());
    }
    if allow {
        eprintln!(
            "warning: {what} mixes versions (--allow-version-mismatch):\n{}",
            describe(mismatches)
        );
        return Ok(());
    }
    bail!(
        "{what} mixes versions; their scores aren't on one scale:\n{}\npass --allow-version-mismatch to go on anyway",
        describe(mismatches)
    )
}

/// The versions each task's trials carry, for a release's `release.json`:
/// the value when all agree, else `mixed`.
pub fn by_task(stamps: &[Stamp]) -> Value {
    let mut out = serde_json::Map::new();
    let mut tasks: Vec<&str> = stamps.iter().map(|s| s.task.as_str()).collect();
    tasks.sort();
    tasks.dedup();
    for task in tasks {
        let one = |pick: fn(&Stamp) -> &Option<String>| {
            let mut values: Vec<Option<&String>> = stamps
                .iter()
                .filter(|s| s.task == task)
                .map(|s| pick(s).as_ref())
                .collect();
            values.sort();
            values.dedup();
            match values.as_slice() {
                [Some(v)] => json!(v),
                [None] => Value::Null,
                _ => json!("mixed"),
            }
        };
        out.insert(
            task.to_string(),
            json!({
                "task_fingerprint": one(|s| &s.fingerprint),
                "scoring_hash": one(|s| &s.scoring),
            }),
        );
    }
    Value::Object(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn stamp(trial: &str, task: &str, fp: Option<&str>, sc: Option<&str>) -> Stamp {
        Stamp {
            trial: trial.into(),
            task: task.into(),
            fingerprint: fp.map(Into::into),
            scoring: sc.map(Into::into),
        }
    }

    #[test]
    fn the_same_versions_are_no_mismatch_and_other_tasks_are_never_compared() {
        let stamps = [
            stamp("a", "t1", Some("f1"), Some("s")),
            stamp("b", "t1", Some("f1"), Some("s")),
            stamp("c", "t2", Some("f2"), Some("s2")),
        ];
        assert!(mismatches(&stamps).is_empty());
        assert!(enforce("x", &mismatches(&stamps), false).is_ok());
    }

    #[test]
    fn a_different_fingerprint_or_spec_hash_is_refused_unless_allowed() {
        let stamps = [
            stamp("a", "t1", Some("f1"), Some("s1")),
            stamp("b", "t1", Some("f2"), Some("s2")),
            stamp("c", "t1", None, Some("s2")),
        ];
        let found = mismatches(&stamps);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].field, "task fingerprint");
        assert_eq!(found[0].groups.len(), 3);
        assert_eq!(found[1].field, "scoring spec");
        let error = enforce("this batch", &found, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("t1: task fingerprint differs"), "{error}");
        assert!(error.contains("--allow-version-mismatch"), "{error}");
        assert!(enforce("this batch", &found, true).is_ok());
        assert_eq!(to_json(&found)[0]["groups"]["f1"][0], "a");
    }

    #[test]
    fn trials_without_versions_agree_with_each_other_but_not_with_stamped_ones() {
        let old = [stamp("a", "t", None, None), stamp("b", "t", None, None)];
        assert!(mismatches(&old).is_empty());
        assert_eq!(unversioned(&old), ["a", "b"]);
        let mixed = [
            stamp("a", "t", None, None),
            stamp("b", "t", Some("f"), Some("s")),
        ];
        assert_eq!(mismatches(&mixed).len(), 2);
        assert_eq!(unversioned(&mixed), ["a"]);
    }

    #[test]
    fn a_grade_records_the_given_fingerprint_else_the_verifiers_file() {
        let file = std::env::temp_dir().join(format!("mtx-fp-{}", uuid::Uuid::new_v4().simple()));
        assert_eq!(recorded_fingerprint(None, &file), None);
        std::fs::write(&file, "abc\n").unwrap();
        assert_eq!(recorded_fingerprint(None, &file).as_deref(), Some("abc"));
        assert_eq!(
            recorded_fingerprint(Some("x".into()), &file).as_deref(),
            Some("x")
        );
        std::fs::write(&file, "\n").unwrap();
        assert_eq!(recorded_fingerprint(None, &file), None);
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn a_stamp_reads_a_reports_versions_and_a_release_summarises_them() {
        let report = json!({"versions": {"task_fingerprint": "ff", "scoring_hash": "ss"}});
        let s = Stamp::of_report("t", "task", &report);
        assert_eq!(
            (s.fingerprint.as_deref(), s.scoring.as_deref()),
            (Some("ff"), Some("ss"))
        );
        assert_eq!(Stamp::of_report("t", "task", &json!({})).fingerprint, None);
        let v = by_task(&[s.clone(), stamp("u", "task", Some("gg"), Some("ss"))]);
        assert_eq!(v["task"]["task_fingerprint"], "mixed");
        assert_eq!(v["task"]["scoring_hash"], "ss");
    }

    fn disk(root: &Path) -> impl Fn(&Path) -> Option<Vec<u8>> + '_ {
        move |p| std::fs::read(root.join(p)).ok()
    }

    /// A copy of one real task's folder in a scratch repo root.
    fn scratch(task: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mtx-fp-{}", uuid::Uuid::new_v4().simple()));
        let from = crate::authoring::repo_root().join("bench/tasks").join(task);
        let to = root.join("bench/tasks").join(task);
        crate::authoring::copy_dir(&from, &to, &["target", "bin"]).unwrap();
        root
    }

    /// The fingerprint with `rel` replaced by `f` of it, then put back.
    fn with(root: &Path, task: &str, rel: &str, f: &dyn Fn(String) -> String) -> String {
        let path = root.join("bench/tasks").join(task).join(rel);
        let before = std::fs::read_to_string(&path).unwrap_or_default();
        std::fs::write(&path, f(before.clone())).unwrap();
        let fp = fingerprint(root, task, &disk(root)).unwrap();
        std::fs::write(&path, before).unwrap();
        fp
    }

    #[test]
    fn the_fingerprint_moves_with_what_a_task_measures_and_not_with_wording() {
        let task = "flash-crash-spcx";
        let root = scratch(task);
        let base = fingerprint(&root, task, &disk(&root)).unwrap();
        assert_eq!(base.len(), 64);
        assert_eq!(base, fingerprint(&root, task, &disk(&root)).unwrap());
        // Cosmetic: not in it.
        for rel in ["README.md", "DESIGN.md"] {
            assert_eq!(with(&root, task, rel, &|t| t + "\nreworded"), base, "{rel}");
        }
        let version = |t: String| t.replacen("\nversion = \"", "\nversion = \"9", 1);
        assert_eq!(with(&root, task, "task.toml", &version), base, "version");
        let digest = |t: String| t.replace("@sha256:", "@sha256:0");
        assert_eq!(with(&root, task, "task.toml", &digest), base, "digest");
        // What it measures: each one moves it, and no two alike.
        let mut seen = vec![base];
        for (rel, what, f) in [
            (
                "instruction.md",
                "mandate",
                (|t| t + "\nloss limit 1") as fn(String) -> String,
            ),
            ("tests/test.sh", "grader flags", |t| {
                t + "\n# --loss-weight 2"
            }),
            ("environment/practice/practice-1.json", "practice", |t| {
                format!(" {t}")
            }),
            ("reference/Cargo.toml", "anchor", |t| t + "\n# x"),
            ("task.toml", "spec", |t| {
                t.replace("[agent]", "[agent]\nextra = 1")
            }),
        ] {
            let fp = with(&root, task, rel, &f);
            assert!(!seen.contains(&fp), "{what} didn't move the fingerprint");
            seen.push(fp);
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_scenario_family_is_in_the_fingerprint() {
        let task = "flash-crash-spcx";
        let root = scratch(task);
        let a = fingerprint(&root, task, &disk(&root)).unwrap();
        // Another family under the same folder: only the generator differs.
        let b = with(&root, task, "task.toml", &|t| {
            t.replace("generator = \"flash-crash\"", "generator = \"weekend-gap\"")
        });
        assert_ne!(a, b);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A root of finished, graded trials: `(task, fingerprint, scoring)` each.
    pub(crate) fn root_of(trials: &[(&str, Option<&str>, Option<&str>)]) -> crate::review::Root {
        let dir = std::env::temp_dir().join(format!("mtx-ver-{}", uuid::Uuid::new_v4().simple()));
        for (n, (task, fp, sc)) in trials.iter().enumerate() {
            let trial = dir.join(format!("job/{task}__{n}"));
            std::fs::create_dir_all(trial.join("verifier")).unwrap();
            std::fs::write(
                trial.join("result.json"),
                json!({"trial_name": format!("{task}__{n}"), "task_name": format!("o/{task}"),
                    "finished_at": "x", "agent_info": {"model_info": {"name": "m"}}})
                .to_string(),
            )
            .unwrap();
            let mut report = json!({"reward": 0.5, "scenarios": []});
            report["versions"] = json!({"task_fingerprint": fp, "scoring_hash": sc});
            std::fs::write(trial.join("verifier/report.json"), report.to_string()).unwrap();
        }
        crate::review::Root {
            label: dir.file_name().unwrap().to_string_lossy().into_owned(),
            path: dir,
            published: false,
        }
    }

    #[test]
    fn a_root_reads_its_trials_stamps_and_the_gate_reads_the_root() {
        let root = root_of(&[
            ("t", Some("f1"), Some("s")),
            ("t", Some("f2"), Some("s")),
            ("u", Some("g"), Some("s")),
        ]);
        let stamps = crate::review::stamps_of(&root);
        assert_eq!(stamps.len(), 3);
        let found = mismatches(&stamps);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].task.as_str(), found[0].field),
            ("t", "task fingerprint")
        );
        std::fs::remove_dir_all(&root.path).unwrap();
    }

    #[tokio::test]
    async fn review_refuses_a_root_that_mixes_versions() {
        let root = root_of(&[("t", Some("f1"), Some("s")), ("t", Some("f2"), Some("s"))]);
        let options = |allow| crate::review::ReviewOptions {
            allow_version_mismatch: allow,
            port: 0,
            open: false,
            start: None,
        };
        let error = crate::review::review(vec![root.clone()], options(false))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("task fingerprint differs"), "{error}");
        std::fs::remove_dir_all(&root.path).unwrap();
    }
}
