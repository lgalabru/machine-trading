//! Published bench results: `mtx bench publish` copies a batch into
//! `bench/results/<version>/`, small enough to commit, and `mtx bench review
//! <version>` and `mtx bench compare <a>...<b>` read them back.
//!
//! A published trial keeps what the review page shows, and nothing it
//! doesn't need:
//! - `result.json`, `config.json`, the verifier's report, reward, stdout,
//!   per-session detail files and the bot's logs;
//! - the bot's source, without build output;
//! - the agent's trajectory, gzipped.
//!
//! It leaves out the batch's `.seed`, the agent's raw logs and sessions,
//! anything hidden, links, and any file over 1 MiB. Before the release is
//! written, every file is checked for the batch's seed and for API keys.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::review::{self, Root};

/// The largest file a release keeps.
const MAX_FILE_BYTES: u64 = 1 << 20;
/// What a trial's verifier left that a release keeps (besides `sessions/`).
const VERIFIER_FILES: &[&str] = &["report.json", "reward.json", "test-stdout.txt"];

/// `bench/results` in this checkout: found from the working directory up,
/// else from where this binary was built.
pub fn results_dir() -> PathBuf {
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        if d.join("bench/tasks").is_dir() {
            return d.join("bench/results");
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/results")
}

/// Published releases, newest first.
pub fn releases(results: &Path) -> Vec<Root> {
    let mut found: Vec<Root> = std::fs::read_dir(results)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let label = e.file_name().to_string_lossy().to_string();
            (!label.starts_with('.') && e.path().join("release.json").is_file()).then(|| Root {
                label,
                path: e.path().canonicalize().unwrap_or_else(|_| e.path()),
                published: true,
            })
        })
        .collect();
    found.sort_by_key(|r| std::cmp::Reverse(version_key(&r.label)));
    found
}

/// `0.10.0` after `0.9.0`.
fn version_key(label: &str) -> Vec<(u64, String)> {
    label
        .split(['.', '-'])
        .map(|part| match part.parse() {
            Ok(n) => (n, String::new()),
            Err(_) => (u64::MAX, part.to_string()),
        })
        .collect()
}

/// A path to review, or a published version.
pub fn resolve(arg: &str, results: &Path) -> Result<Root> {
    let path = Path::new(arg);
    if path.exists() {
        let path = path.canonicalize().with_context(|| arg.to_string())?;
        // A release reviewed by its path is still that release.
        if let Some(release) = releases(results).into_iter().find(|r| r.path == path) {
            return Ok(release);
        }
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| arg.to_string());
        return Ok(Root {
            label,
            path,
            published: false,
        });
    }
    if let Some(release) = releases(results).into_iter().find(|r| r.label == arg) {
        return Ok(release);
    }
    let known: Vec<String> = releases(results).into_iter().map(|r| r.label).collect();
    bail!(
        "{arg} is neither a path nor a release in {} (published: {})",
        results.display(),
        if known.is_empty() {
            "none".to_string()
        } else {
            known.join(", ")
        }
    )
}

/// What to serve: the root asked for first, then every other release.
pub fn roots(first: Root, results: &Path) -> Vec<Root> {
    let mut roots = vec![first];
    for release in releases(results) {
        if !roots.iter().any(|r| r.path == release.path) {
            roots.push(release);
        }
    }
    roots
}

/// Copy `batch` into `<results>/<version>`.
pub fn publish(batch: &Path, version: &str, results: &Path, force: bool) -> Result<Value> {
    if version.is_empty()
        || version.starts_with('.')
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_+".contains(c))
    {
        bail!("{version:?} isn't a version name");
    }
    let batch = batch
        .canonicalize()
        .with_context(|| batch.display().to_string())?;
    let trials = review::discover(&batch);
    if trials.is_empty() {
        bail!("no Harbor trials under {}", batch.display());
    }
    let pending: Vec<String> = trials
        .iter()
        .filter(|t| !t.join("result.json").is_file())
        .map(|t| t.display().to_string())
        .collect();
    if !pending.is_empty() {
        bail!(
            "{} trial{} still running, or never finished: {}",
            pending.len(),
            if pending.len() == 1 { " is" } else { "s are" },
            pending.join(", ")
        );
    }
    let out = results.join(version);
    if out.exists() && !force {
        bail!("{} exists; pass --force to replace it", out.display());
    }
    // Written beside it, then swapped in: a failed check leaves nothing.
    let staging = results.join(format!(".{version}.staging"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let written = copy_release(&batch, &trials, &staging, version).and_then(|summary| {
        check_secrets(&batch, &trials, &staging)?;
        Ok(summary)
    });
    let summary = match written {
        Ok(summary) => summary,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if out.exists() {
        std::fs::remove_dir_all(&out)?;
    }
    std::fs::rename(&staging, &out)?;
    let bytes = dir_bytes(&out);
    Ok(json!({
        "release": version,
        "path": out,
        "bytes": bytes,
        "summary": summary,
    }))
}

fn copy_release(batch: &Path, trials: &[PathBuf], out: &Path, version: &str) -> Result<Value> {
    let mut skipped: Vec<String> = Vec::new();
    let mut jobs: Vec<PathBuf> = Vec::new();
    let mut tasks = serde_json::Map::new();
    let mut started: Vec<String> = Vec::new();
    for trial in trials {
        let rel = trial.strip_prefix(batch).unwrap_or(Path::new(""));
        // One trial reviewed alone: keep it under its own name.
        let rel = if rel.as_os_str().is_empty() {
            PathBuf::from(trial.file_name().unwrap_or_default())
        } else {
            rel.to_path_buf()
        };
        let dest = out.join(&rel);
        std::fs::create_dir_all(&dest)?;
        for file in ["result.json", "config.json", "exception.txt"] {
            copy_file(&trial.join(file), &dest.join(file), &mut skipped)?;
        }
        for file in VERIFIER_FILES {
            copy_file(
                &trial.join("verifier").join(file),
                &dest.join("verifier").join(file),
                &mut skipped,
            )?;
        }
        for role in ["bot", "reference"] {
            copy_tree(
                &trial.join("verifier/sessions").join(role),
                &dest.join("verifier/sessions").join(role),
                &mut skipped,
            )?;
        }
        copy_tree(
            &trial.join("verifier/bot"),
            &dest.join("verifier/bot"),
            &mut skipped,
        )?;
        copy_tree(
            &trial.join("artifacts/app/bot"),
            &dest.join("artifacts/app/bot"),
            &mut skipped,
        )?;
        let trajectory = trial.join("agent/trajectory.json");
        if trajectory.is_file() {
            gzip(&trajectory, &dest.join("agent/trajectory.json.gz"))?;
        }
        // The Harbor job the trial ran in: its config and result.
        if let Some(job) = trial
            .parent()
            .filter(|j| j.starts_with(batch) && *j != batch)
            && !jobs.iter().any(|j| j == job)
        {
            jobs.push(job.to_path_buf());
            let dest = out.join(job.strip_prefix(batch)?);
            for file in ["result.json", "config.json"] {
                copy_file(&job.join(file), &dest.join(file), &mut skipped)?;
            }
            if let Some(at) = review::read_json(&job.join("result.json"))
                .and_then(|r| r["started_at"].as_str().map(str::to_string))
            {
                started.push(at);
            }
        }
        // The task, as it ran: its task.toml.
        let result = review::read_json(&trial.join("result.json")).unwrap_or(Value::Null);
        let task = &result["config"]["task"];
        if let Some(name) = task["name"].as_str() {
            let short = name.rsplit('/').next().unwrap_or(name).to_string();
            if !tasks.contains_key(&short) {
                let toml = review::task_toml(
                    batch,
                    name,
                    task["ref"].as_str().unwrap_or_default(),
                    task["path"].as_str(),
                );
                if let Some(toml) = &toml {
                    let dir = out.join("tasks").join(&short);
                    std::fs::create_dir_all(&dir)?;
                    std::fs::write(dir.join("task.toml"), toml)?;
                }
                tasks.insert(short, json!({ "name": name, "ref": task["ref"] }));
            }
        }
    }
    copy_file(
        &batch.join("batch.json"),
        &out.join("batch.json"),
        &mut skipped,
    )?;
    started.sort();
    let release = json!({
        "version": version,
        "source": batch.file_name().map(|n| n.to_string_lossy().to_string()),
        "ran_at": started.first(),
        "trials": trials.len(),
        "tasks": tasks,
        "skipped": skipped,
    });
    std::fs::write(
        out.join("release.json"),
        serde_json::to_string_pretty(&release)? + "\n",
    )?;
    Ok(release)
}

/// Copy one file, if it's there, small and not a link.
fn copy_file(from: &Path, to: &Path, skipped: &mut Vec<String>) -> Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(from) else {
        return Ok(());
    };
    if !meta.is_file() {
        return Ok(());
    }
    if meta.len() > MAX_FILE_BYTES {
        skipped.push(format!("{} ({} bytes)", from.display(), meta.len()));
        return Ok(());
    }
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::copy(from, to).with_context(|| from.display().to_string())?;
    Ok(())
}

/// Copy a directory, leaving out hidden files, links and build output.
fn copy_tree(from: &Path, to: &Path, skipped: &mut Vec<String>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if name.starts_with('.') || kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if !review::SKIPPED_DIRS.contains(&name.as_str()) {
                copy_tree(&entry.path(), &to.join(&name), skipped)?;
            }
        } else {
            copy_file(&entry.path(), &to.join(&name), skipped)?;
        }
    }
    Ok(())
}

fn gzip(from: &Path, to: &Path) -> Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut input = std::fs::File::open(from).with_context(|| from.display().to_string())?;
    let mut encoder =
        flate2::write::GzEncoder::new(std::fs::File::create(to)?, flate2::Compression::best());
    std::io::copy(&mut input, &mut encoder)?;
    encoder.finish()?.flush()?;
    Ok(())
}

/// Refuse a release that holds a secret: the batch's seed (which draws its
/// hidden sets), an Anthropic key, or this shell's `ANTHROPIC_API_KEY`.
fn check_secrets(batch: &Path, trials: &[PathBuf], out: &Path) -> Result<()> {
    let mut needles: Vec<(String, &str)> = vec![("sk-ant-".to_string(), "an Anthropic API key")];
    let mut seeds: Vec<PathBuf> = vec![batch.join(".seed")];
    seeds.extend(
        trials
            .iter()
            .flat_map(|t| t.ancestors().skip(1).take(2).map(|d| d.join(".seed"))),
    );
    for seed in seeds {
        if let Ok(seed) = std::fs::read_to_string(&seed) {
            let seed = seed.trim().to_string();
            if seed.len() >= 16 && !needles.iter().any(|(n, _)| *n == seed) {
                needles.push((seed, "the batch's seed"));
            }
        }
    }
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && key.len() >= 16
    {
        needles.push((key, "ANTHROPIC_API_KEY"));
    }
    let mut hits: Vec<String> = Vec::new();
    scan(out, out, &needles, &mut hits)?;
    if !hits.is_empty() {
        bail!(
            "not published: the release would hold a secret\n{}",
            hits.join("\n")
        );
    }
    Ok(())
}

fn scan(root: &Path, dir: &Path, needles: &[(String, &str)], hits: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            scan(root, &path, needles, hits)?;
            continue;
        }
        let raw = std::fs::read(&path)?;
        // A gzipped file is checked inflated; one that won't inflate, as is.
        let mut bytes = Vec::new();
        let inflated = path.extension().is_some_and(|e| e == "gz")
            && flate2::read::GzDecoder::new(raw.as_slice())
                .read_to_end(&mut bytes)
                .is_ok();
        let bytes = if inflated { bytes } else { raw };
        for (needle, what) in needles {
            if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                hits.push(format!(
                    "  {}: {what}",
                    path.strip_prefix(root).unwrap_or(&path).display()
                ));
            }
        }
    }
    Ok(())
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

/// `0.9.0...0.8.0` (or `..`): the two sides.
pub fn split_range(range: &str) -> Result<(String, String)> {
    let (a, b) = range
        .split_once("...")
        .or_else(|| range.split_once(".."))
        .context("compare takes <a>...<b>, as in 0.9.0...0.8.0")?;
    if a.is_empty() || b.is_empty() {
        bail!("compare takes <a>...<b>, as in 0.9.0...0.8.0");
    }
    Ok((a.to_string(), b.to_string()))
}

/// Each task's models on both sides: mean skill, and the change from `b`
/// to `a`.
pub fn compare(a: &Root, b: &Root) -> Value {
    let (sa, sb) = (review::scores_of(a), review::scores_of(b));
    let mut tasks: Vec<String> = Vec::new();
    for s in [&sa, &sb] {
        if let Some(t) = s["tasks"].as_object() {
            tasks.extend(t.keys().cloned());
        }
    }
    tasks.sort();
    tasks.dedup();
    let rows: Vec<Value> = tasks
        .iter()
        .map(|task| {
            let (ta, tb) = (&sa["tasks"][task], &sb["tasks"][task]);
            let mut models: Vec<String> = Vec::new();
            for t in [ta, tb] {
                if let Some(m) = t["models"].as_object() {
                    models.extend(m.keys().cloned());
                }
            }
            models.sort();
            models.dedup();
            let models: Vec<Value> = models
                .iter()
                .map(|model| {
                    let (ma, mb) = (&ta["models"][model], &tb["models"][model]);
                    let delta = match (ma["skill"].as_f64(), mb["skill"].as_f64()) {
                        (Some(x), Some(y)) => json!(x - y),
                        _ => Value::Null,
                    };
                    json!({ "model": model, "a": ma, "b": mb, "skill_delta": delta })
                })
                .collect();
            json!({
                "task": task,
                "versions": { "a": ta["version"], "b": tb["version"] },
                "models": models,
            })
        })
        .collect();
    json!({ "a": a.label, "b": b.label, "tasks": rows })
}

/// The comparison as a table on stderr.
pub fn print_compare(cmp: &Value) {
    let skill = |v: &Value| {
        v.as_f64()
            .map(|x| format!("{x:.2}×"))
            .unwrap_or_else(|| "–".to_string())
    };
    let (a, b) = (
        cmp["a"].as_str().unwrap_or("a"),
        cmp["b"].as_str().unwrap_or("b"),
    );
    for task in cmp["tasks"].as_array().into_iter().flatten() {
        eprintln!(
            "\n{}  {}",
            crate::ui::bold(task["task"].as_str().unwrap_or("?")),
            crate::ui::dim(format!(
                "task {} vs {}",
                task["versions"]["a"].as_str().unwrap_or("–"),
                task["versions"]["b"].as_str().unwrap_or("–")
            ))
        );
        eprintln!("  {:<22} {:>10} {:>10} {:>9}", "model", a, b, "change");
        for m in task["models"].as_array().into_iter().flatten() {
            let delta = m["skill_delta"]
                .as_f64()
                .map(|d| {
                    let text = format!("{d:+.2}×");
                    if d >= 0.0 {
                        crate::ui::accent(text)
                    } else {
                        text
                    }
                })
                .unwrap_or_else(|| "–".to_string());
            eprintln!(
                "  {:<22} {:>10} {:>10} {:>9}",
                m["model"]
                    .as_str()
                    .unwrap_or("?")
                    .trim_start_matches("claude-"),
                skill(&m["a"]["skill"]),
                skill(&m["b"]["skill"]),
                delta
            );
        }
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_sort_numerically() {
        let mut v = vec!["0.9.0", "0.10.0", "0.8.1"];
        v.sort_by_key(|x| std::cmp::Reverse(version_key(x)));
        assert_eq!(v, ["0.10.0", "0.9.0", "0.8.1"]);
    }

    #[test]
    fn ranges_split() {
        assert_eq!(
            split_range("0.9.0...0.8.0").unwrap(),
            ("0.9.0".into(), "0.8.0".into())
        );
        assert_eq!(
            split_range("0.9.0..0.8.0").unwrap(),
            ("0.9.0".into(), "0.8.0".into())
        );
        assert!(split_range("0.9.0").is_err());
        assert!(split_range("...0.8.0").is_err());
    }

    #[test]
    fn a_release_leaves_secrets_and_build_output_out() {
        let tmp = std::env::temp_dir().join(format!("mtx-publish-{}", uuid::Uuid::new_v4()));
        let batch = tmp.join("batch");
        let trial = batch.join("job/task__a");
        std::fs::create_dir_all(trial.join("verifier/sessions/bot")).unwrap();
        std::fs::create_dir_all(trial.join("artifacts/app/bot/src")).unwrap();
        std::fs::create_dir_all(trial.join("artifacts/app/bot/target")).unwrap();
        std::fs::create_dir_all(trial.join("agent")).unwrap();
        std::fs::write(batch.join(".seed"), "0123456789abcdef0123456789abcdef").unwrap();
        std::fs::write(trial.join("config.json"), r#"{"trial_name":"task__a"}"#).unwrap();
        std::fs::write(
            trial.join("result.json"),
            r#"{"trial_name":"task__a","config":{"task":{"name":"o/task"}}}"#,
        )
        .unwrap();
        std::fs::write(trial.join("verifier/report.json"), "{}").unwrap();
        std::fs::write(trial.join("verifier/sessions/bot/hidden-1.json.gz"), "x").unwrap();
        std::fs::write(trial.join("artifacts/app/bot/src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(trial.join("artifacts/app/bot/.env"), "KEY=1").unwrap();
        std::fs::write(trial.join("artifacts/app/bot/target/big"), "built").unwrap();
        std::fs::write(trial.join("agent/trajectory.json"), r#"{"steps":[]}"#).unwrap();
        std::fs::write(trial.join("agent/claude-code.txt"), "raw").unwrap();
        let results = tmp.join("results");
        std::fs::create_dir_all(&results).unwrap();

        publish(&batch, "1.0.0", &results, false).unwrap();
        let out = results.join("1.0.0/job/task__a");
        assert!(out.join("artifacts/app/bot/src/main.rs").is_file());
        assert!(out.join("verifier/sessions/bot/hidden-1.json.gz").is_file());
        assert!(out.join("agent/trajectory.json.gz").is_file());
        assert!(!out.join("agent/trajectory.json").exists());
        assert!(!out.join("agent/claude-code.txt").exists());
        assert!(!out.join("artifacts/app/bot/.env").exists());
        assert!(!out.join("artifacts/app/bot/target").exists());
        assert!(!results.join("1.0.0/.seed").exists());
        assert_eq!(releases(&results).len(), 1);
        assert!(publish(&batch, "1.0.0", &results, false).is_err(), "exists");

        // The seed in a bot's source: nothing is written.
        std::fs::write(
            trial.join("artifacts/app/bot/src/seed.rs"),
            "0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let error = publish(&batch, "1.0.1", &results, false).unwrap_err();
        assert!(error.to_string().contains("seed"), "{error}");
        assert!(!results.join("1.0.1").exists());
        assert!(!results.join(".1.0.1.staging").exists());
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
