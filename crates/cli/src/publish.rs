//! Published bench results: `mtx bench publish` compacts a batch into a
//! release, uploads it to the results dataset ([`crate::store`]) and writes
//! `bench/results/<version>/release.json`, the one file the repo keeps: where
//! the release is and its scores. `mtx bench review <version>` and `mtx bench
//! compare <a>...<b>` fetch it back, once.
//!
//! A published trial keeps what the review page shows, and nothing it
//! doesn't need:
//! - `result.json`, `config.json`, the verifier's report and reward, and the
//!   per-session detail files;
//! - the bot's per-session logs, each cut to its first 8 KiB and last
//!   24 KiB, and the verifier's stdout, gzipped (the reference's logs are
//!   left out: the page never shows them);
//! - each of the reference bot's session files once per task: the trials of
//!   a batch are graded on one hidden set, so the reference plays the same
//!   sessions in each. A trial whose copy matches one already kept, but for
//!   wall-clock fields ([`VOLATILE`]), points to it in
//!   `verifier/sessions/reference-shared.json` instead;
//! - the bot's source, without build output;
//! - the agent's trajectory, gzipped.
//!
//! It leaves out the batch's `.seed`, the agent's raw logs and sessions,
//! anything hidden, links, and any file over 1 MiB. Then it compacts the
//! release ([`crate::compact`]): no transaction signatures, sessions' tables
//! as columns, each scenario and each source file once. Before the release
//! is written, every file is checked for the batch's seed and for API keys.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    review::{self, Root},
    store,
};

/// The largest file a release keeps.
const MAX_FILE_BYTES: u64 = 1 << 20;
/// What a trial's verifier left that a release keeps as is (besides
/// `sessions/`); its text logs are gzipped instead.
const VERIFIER_FILES: &[&str] = &["report.json", "reward.json"];
/// How much of a bot's session log a release keeps: its start and its end,
/// where a bot says what it is and how it failed.
pub(crate) const LOG_HEAD_BYTES: usize = 8 << 10;
pub(crate) const LOG_TAIL_BYTES: usize = 24 << 10;
/// Fields that differ between two runs of the same session by wall clock or
/// chance alone: turn times, compute units (keypairs are random) and
/// signatures. Two reference sessions equal but for these are one.
pub(crate) const VOLATILE: &[&str] = &[
    "turn_ms",
    "wall_ms",
    "compute_units",
    "signature",
    "signatures",
];
/// Where a published trial lists the reference sessions another trial keeps.
pub(crate) const SHARED_REFERENCES: &str = "verifier/sessions/reference-shared.json";

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

/// Why a batch wasn't guarded, if it wasn't: an `.unguarded` marker or
/// `"guarded": false` in a batch.json, in `dir` or the two above it (a job
/// published from inside its batch).
fn marked_unguarded(dir: &Path) -> Option<String> {
    for d in dir.ancestors().take(3) {
        if d.join(".unguarded").exists() {
            return Some(format!("{} is marked .unguarded", d.display()));
        }
        let plan = std::fs::read_to_string(d.join("batch.json")).ok();
        let plan: Option<serde_json::Value> = plan.and_then(|p| serde_json::from_str(&p).ok());
        if plan.is_some_and(|p| p.get("guarded") == Some(&json!(false))) {
            return Some(format!("{}/batch.json says guarded: false", d.display()));
        }
    }
    None
}

/// The repository (where `bench/tasks` is), from the working directory up,
/// else from where this binary was built.
pub(crate) fn repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        if d.join("bench/tasks").is_dir() && d.join("Justfile").is_file() {
            return d;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Whether a task's agent starts from the base's `-agent` image (see
/// `bench/image/Dockerfile`): its network is an allowlist.
fn guarded_task(root: &Path, task: &str) -> bool {
    std::fs::read_to_string(
        root.join("bench/tasks")
            .join(task)
            .join("environment/Dockerfile"),
    )
    .is_ok_and(|text| {
        text.lines().any(|line| {
            line.strip_prefix("FROM ").is_some_and(|image| {
                image
                    .split(['@', ' '])
                    .next()
                    .is_some_and(|name| name.ends_with("-agent"))
            })
        })
    })
}

/// The trials in `trials` that ran a native harness (Claude Code, Codex) on a
/// task whose agent is guarded, or on one `root` doesn't hold (so can't be
/// shown unguarded). Native harnesses can't be guarded: a provider's API
/// must stay open to them, and Harbor opens it itself when asked nothing.
fn native_on_guarded(root: &Path, trials: &[PathBuf]) -> Vec<String> {
    let mut found = Vec::new();
    for trial in trials {
        let Some(result) = std::fs::read_to_string(trial.join("result.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        else {
            continue;
        };
        let agent = result["agent_info"]["name"].as_str().unwrap_or("");
        if !matches!(agent, "codex" | "claude-code") {
            continue;
        }
        let task = result["task_name"]
            .as_str()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("");
        let known = root.join("bench/tasks").join(task).is_dir();
        if !known || guarded_task(root, task) {
            found.push(format!("{} ({agent} on {task})", trial.display()));
        }
    }
    found
}

/// Refuse what ran unguarded: a marked batch, or any trial of a native
/// harness on a guarded task, wherever the trial sits and whatever it was
/// copied with. Checked by `publish` and `export`.
pub(crate) fn refuse_unguarded(root: &Path, dir: &Path, trials: &[PathBuf]) -> Result<()> {
    if let Some(why) = marked_unguarded(dir) {
        bail!("{why}: it was not guarded and can't be published");
    }
    let native = native_on_guarded(root, trials);
    if !native.is_empty() {
        bail!(
            "{} trial{} ran a native harness on a guarded task, which can't be guarded (its provider's API is open to it): {}",
            native.len(),
            if native.len() == 1 { "" } else { "s" },
            native.into_iter().take(3).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

/// Published releases, newest first. A release in the results dataset is
/// fetched, once, and read from its cache; one that fails to fetch is left
/// out, with a warning. A slim release whose full copy is in
/// [`crate::slim::ARTIFACTS_ENV`] is read from there.
pub fn releases(results: &Path) -> Vec<Root> {
    let mut found: Vec<Root> = std::fs::read_dir(results)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let label = e.file_name().to_string_lossy().to_string();
            if label.starts_with('.') || !e.path().join("release.json").is_file() {
                return None;
            }
            let path = match stored(&e.path()) {
                Some(store) => match fetched(&store) {
                    Ok(path) => path,
                    Err(error) => {
                        tracing::warn!(
                            "{label} couldn't be fetched from {}: {error:#}",
                            store.url()
                        );
                        return None;
                    }
                },
                None => crate::slim::full_copy(&e.path())
                    .unwrap_or_else(|| e.path().canonicalize().unwrap_or_else(|_| e.path())),
            };
            Some(Root {
                label,
                path,
                published: true,
            })
        })
        .collect();
    found.sort_by_key(|r| std::cmp::Reverse(version_key(&r.label)));
    found
}

/// Where a release in `bench/results` keeps its files, if in the dataset.
pub fn stored(release: &Path) -> Option<store::Store> {
    store::Store::of(&review::read_json(&release.join("release.json"))?)
}

/// The store's files, fetched if they aren't yet, saying so.
fn fetched(store: &store::Store) -> Result<PathBuf> {
    if !store.cache().join(store::COMPLETE).is_file() {
        crate::ui::note(format!(
            "fetching {} ({:.1} MB)",
            store.url(),
            store.bytes as f64 / 1e6
        ));
    }
    store::fetch(store)
}

/// `0.10.0` after `0.9.0`.
pub(crate) fn version_key(label: &str) -> Vec<(u64, String)> {
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
        if let Some(release) = releases(results).into_iter().find(|r| {
            r.path == path
                || results
                    .join(&r.label)
                    .canonicalize()
                    .is_ok_and(|p| p == path)
        }) {
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

/// Compact `batch` into a release, upload it to the results dataset and
/// write its `release.json` to `<results>/<version>`. With `local`, the
/// whole release is written there instead, and nothing is uploaded. With
/// `artifacts`, nothing is uploaded either: the release goes there whole (as
/// `<artifacts>/<version>`) and `<results>/<version>` is its slim release
/// (see [`crate::slim`]).
pub fn publish(
    batch: &Path,
    version: &str,
    results: &Path,
    force: bool,
    artifacts: Option<&Path>,
    local: bool,
    allow_missing_trajectories: bool,
) -> Result<Value> {
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
    // A batch run with MTX_UNGUARDED, or a native harness's trial on a
    // guarded task, whichever way it got here.
    refuse_unguarded(&repo_root(), &batch, &trials)?;
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
    // Every run's agent trajectory, unless asked to publish without: a run
    // without one can't be reviewed step by step (0.12.0 lost 67 of 120 to
    // an empty session export).
    let untraced: Vec<String> = trials
        .iter()
        .filter(|t| !t.join("agent/trajectory.json").is_file())
        .map(|t| t.strip_prefix(&batch).unwrap_or(t).display().to_string())
        .collect();
    if !untraced.is_empty() && !allow_missing_trajectories {
        bail!(
            "{} of {} trials have no agent trajectory (agent/trajectory.json), as {}; \
             pass --allow-missing-trajectories to publish them without",
            untraced.len(),
            trials.len(),
            untraced
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
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
    let written = copy_release(&batch, &trials, &staging, version).and_then(|mut summary| {
        let compacted = crate::compact::compact_release(&staging)?;
        summary["compacted"] = json!(compacted);
        check_secrets(&batch, &trials, &staging)?;
        Ok(summary)
    });
    let mut summary = match written {
        Ok(summary) => summary,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    let bytes = dir_bytes(&staging);
    if let Some(artifacts) = artifacts {
        let full = artifacts.join(version);
        if full.exists() {
            if !force {
                let _ = std::fs::remove_dir_all(&staging);
                bail!("{} exists; pass --force to replace it", full.display());
            }
            std::fs::remove_dir_all(&full)?;
        }
        crate::slim::move_dir(&staging, &full)?;
        let slim = results.join(format!(".{version}.slim"));
        if slim.exists() {
            std::fs::remove_dir_all(&slim)?;
        }
        summary["slim"] = crate::slim::slim(&full, &slim)?;
        summary["full"] = json!(full);
        if out.exists() {
            std::fs::remove_dir_all(&out)?;
        }
        std::fs::rename(&slim, &out)?;
        return Ok(json!({
            "release": version,
            "path": out,
            "bytes": dir_bytes(&out),
            "summary": summary,
        }));
    }
    if local {
        if out.exists() {
            std::fs::remove_dir_all(&out)?;
        }
        std::fs::rename(&staging, &out)?;
        return Ok(json!({
            "release": version,
            "path": out,
            "bytes": bytes,
            "summary": summary,
        }));
    }
    let store = match offload(&staging, &out, version) {
        Ok(store) => store,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    Ok(json!({
        "release": version,
        "path": out,
        "bytes": bytes,
        "store": store.url(),
        "summary": summary,
    }))
}

/// Upload the release a full directory holds (`dir`, maybe `out` itself) to
/// the results dataset, and leave `out` with its `release.json` alone: the
/// release as before, its scores, and where its files are. `dir` becomes the
/// store's cache.
pub fn offload(dir: &Path, out: &Path, version: &str) -> Result<store::Store> {
    let mut release = review::read_json(&dir.join("release.json"))
        .with_context(|| format!("{} has no release.json", dir.display()))?;
    ensure!(
        store::Store::of(&release).is_none(),
        "{} is in the dataset already",
        dir.display()
    );
    let scores = review::scores_of(&Root {
        label: version.to_string(),
        path: dir.to_path_buf(),
        published: true,
    });
    let (trials, sessions) = crate::tables::tables(dir, version);
    let store = store::upload(
        dir,
        version,
        &[
            (
                "trials.jsonl",
                crate::tables::jsonl(&trials, crate::tables::TRIALS),
            ),
            (
                "sessions.jsonl",
                crate::tables::jsonl(&sessions, crate::tables::SESSIONS),
            ),
        ],
    )?;
    release["store"] = store.to_json();
    release["scores"] = scores;
    // The full directory moves to the cache before `out` is rewritten: `dir`
    // may be `out`.
    let cache = store::adopt(&store, dir)?;
    if out.exists() {
        std::fs::remove_dir_all(out)?;
    }
    std::fs::create_dir_all(out)?;
    std::fs::write(
        out.join("release.json"),
        serde_json::to_string_pretty(&release)? + "\n",
    )?;
    tracing::debug!("{version} is cached at {}", cache.display());
    let results = out.parent().context("the release has no parent")?;
    store::upload_beside(None, &[], Some(&crate::tables::card(results)?))?;
    Ok(store)
}

/// A release already in the dataset: its tables made again from its
/// archive, and uploaded, with the dataset's card.
pub fn retable(release: &Path, version: &str) -> Result<()> {
    let store = stored(release).with_context(|| format!("{version} isn't in the dataset"))?;
    let dir = fetched(&store)?;
    let (trials, sessions) = crate::tables::tables(&dir, version);
    let results = release.parent().context("the release has no parent")?;
    store::upload_beside(
        Some(version),
        &[
            (
                "trials.jsonl",
                crate::tables::jsonl(&trials, crate::tables::TRIALS),
            ),
            (
                "sessions.jsonl",
                crate::tables::jsonl(&sessions, crate::tables::SESSIONS),
            ),
        ],
        Some(&crate::tables::card(results)?),
    )
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
        gzip_logs(
            &trial.join("verifier/bot"),
            &dest.join("verifier/bot"),
            &mut skipped,
        )?;
        let stdout = trial.join("verifier/test-stdout.txt");
        if stdout.is_file() {
            gzip_small(
                &stdout,
                &dest.join("verifier/test-stdout.txt.gz"),
                &mut skipped,
            )?;
        }
        copy_tree(
            &trial.join("artifacts/app/bot"),
            &dest.join("artifacts/app/bot"),
            &mut skipped,
        )?;
        // Hermes's own transcript of the run (its tool calls, edits and final
        // report), gzipped: what's left of a run whose session export came
        // out empty, and readable beside the trajectory otherwise.
        let transcript = trial.join("agent/hermes.txt");
        if transcript.is_file() {
            gzip_small(&transcript, &dest.join("agent/hermes.txt.gz"), &mut skipped)?;
        }
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
    let shared = share_references(out)?;
    copy_file(
        &batch.join("batch.json"),
        &out.join("batch.json"),
        &mut skipped,
    )?;
    started.sort();
    // Skipped files by their path in the batch: an absolute path names the
    // machine and its user, and the release is public.
    let prefix = format!("{}/", batch.display());
    let skipped: Vec<String> = skipped
        .iter()
        .map(|s| s.strip_prefix(&prefix).unwrap_or(s).to_string())
        .collect();
    let release = json!({
        "version": version,
        "source": batch.file_name().map(|n| n.to_string_lossy().to_string()),
        "ran_at": started.first(),
        "trials": trials.len(),
        "tasks": tasks,
        "shared_reference_sessions": shared,
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

/// The bot's `<session>.log` files, each gzipped; the reference's logs (in
/// `reference/`) and anything else are left out.
fn gzip_logs(from: &Path, to: &Path, skipped: &mut Vec<String>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_log = entry.file_type().is_ok_and(|t| t.is_file())
            && name.ends_with(".log")
            && !name.starts_with('.');
        if is_log {
            let bytes = std::fs::read(entry.path())?;
            let out = to.join(format!("{name}.gz"));
            write_gz(&out, &cut_log(&bytes))?;
            if std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
                std::fs::remove_file(&out)?;
                skipped.push(format!(
                    "{} (over the limit gzipped)",
                    entry.path().display()
                ));
            }
        }
    }
    Ok(())
}

/// A log cut to its first [`LOG_HEAD_BYTES`] and last [`LOG_TAIL_BYTES`],
/// with a line saying how much was left out.
pub(crate) fn cut_log(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() <= LOG_HEAD_BYTES + LOG_TAIL_BYTES {
        return bytes.to_vec();
    }
    let tail = bytes.len() - LOG_TAIL_BYTES;
    let mut out = bytes[..LOG_HEAD_BYTES].to_vec();
    out.extend_from_slice(
        format!(
            "\n[… {} bytes left out of the published log …]\n",
            tail - LOG_HEAD_BYTES
        )
        .as_bytes(),
    );
    out.extend_from_slice(&bytes[tail..]);
    out
}

fn write_gz(to: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut encoder =
        flate2::write::GzEncoder::new(std::fs::File::create(to)?, flate2::Compression::best());
    encoder.write_all(bytes)?;
    encoder.finish()?.flush()?;
    Ok(())
}

/// Keeps each reference session once per task across a release's trials:
/// a trial whose copy matches one kept (see [`VOLATILE`]) drops it and
/// points to the kept one in [`SHARED_REFERENCES`]. Returns how many it
/// dropped.
fn share_references(out: &Path) -> Result<usize> {
    let mut trials: Vec<PathBuf> = review::discover(out)
        .into_iter()
        .filter(|t| t.join("verifier/sessions/reference").is_dir())
        .collect();
    trials.sort();
    let mut kept: std::collections::HashMap<(String, String, String), PathBuf> =
        std::collections::HashMap::new();
    let mut dropped = 0;
    for trial in &trials {
        let task = trial
            .file_name()
            .map(|n| {
                n.to_string_lossy()
                    .split("__")
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .unwrap_or_default();
        let dir = trial.join("verifier/sessions/reference");
        let mut names: Vec<String> = std::fs::read_dir(&dir)?
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .strip_suffix(".json.gz")
                    .map(str::to_string)
            })
            .collect();
        names.sort();
        let mut pointers = serde_json::Map::new();
        for name in names {
            let file = dir.join(format!("{name}.json.gz"));
            let Some(key) = session_key(&file) else {
                continue;
            };
            match kept.entry((task.clone(), name.clone(), key)) {
                std::collections::hash_map::Entry::Occupied(owner) => {
                    let owner = owner.get().strip_prefix(out)?;
                    pointers.insert(
                        name.clone(),
                        json!(format!(
                            "{}/verifier/sessions/reference/{name}.json.gz",
                            owner.display()
                        )),
                    );
                    std::fs::remove_file(&file)?;
                    dropped += 1;
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(trial.clone());
                }
            }
        }
        if !pointers.is_empty() {
            std::fs::write(
                trial.join(SHARED_REFERENCES),
                serde_json::to_string_pretty(&Value::Object(pointers))? + "\n",
            )?;
        }
    }
    Ok(dropped)
}

/// A session file's content with its [`VOLATILE`] fields taken out, as a
/// string to compare by: `None` if it doesn't read.
fn session_key(file: &Path) -> Option<String> {
    let mut json = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(file).ok()?)
        .read_to_end(&mut json)
        .ok()?;
    let mut value: Value = serde_json::from_slice(&json).ok()?;
    strip_volatile(&mut value);
    serde_json::to_string(&value).ok()
}

fn strip_volatile(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for key in VOLATILE {
                map.remove(*key);
            }
            map.values_mut().for_each(strip_volatile);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_volatile),
        _ => {}
    }
}

/// Gzip a file, unless even compressed it's over the release's limit.
fn gzip_small(from: &Path, to: &Path, skipped: &mut Vec<String>) -> Result<()> {
    gzip(from, to)?;
    let size = std::fs::metadata(to).map(|m| m.len()).unwrap_or(0);
    if size > MAX_FILE_BYTES {
        std::fs::remove_file(to)?;
        skipped.push(format!("{} ({size} bytes gzipped)", from.display()));
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

pub(crate) fn dir_bytes(dir: &Path) -> u64 {
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

/// Each task's models on both sides: the mean of the task's one score (v2's
/// reward for a task headlined by v2, else skill), and the change from `b`
/// to `a`. A task whose score differs between the sides (it moved to v2)
/// has no change: the two aren't on one scale. `skill_delta` is only ever
/// a v1 task's.
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
            let headline = |t: &Value| t["headline"].as_str().unwrap_or("v1").to_string();
            let (ha, hb) = (headline(ta), headline(tb));
            let one = (ta.is_null() || tb.is_null() || ha == hb)
                .then(|| if ta.is_null() { hb.clone() } else { ha.clone() });
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
                    let score = |m: &Value| m["score"].as_f64().or(m["skill"].as_f64());
                    let delta = match (&one, score(ma), score(mb)) {
                        (Some(_), Some(x), Some(y)) => json!(x - y),
                        _ => Value::Null,
                    };
                    let mut row = json!({ "model": model, "a": ma, "b": mb, "score_delta": delta });
                    if one.as_deref() == Some("v1") {
                        row["skill_delta"] = delta;
                    }
                    row
                })
                .collect();
            json!({
                "task": task,
                "versions": { "a": ta["version"], "b": tb["version"] },
                "headline": { "a": ha, "b": hb },
                "models": models,
            })
        })
        .collect();
    json!({ "a": a.label, "b": b.label, "tasks": rows })
}

/// The comparison as a table on stderr.
pub fn print_compare(cmp: &Value) {
    // Skill is a multiple of the reference (1×); v2 is a score (0.5 the
    // reference).
    let show = |v: &Value, v2: bool| {
        v.as_f64()
            .map(|x| {
                if v2 {
                    format!("{x:.2}")
                } else {
                    format!("{x:.2}×")
                }
            })
            .unwrap_or_else(|| "–".to_string())
    };
    let (a, b) = (
        cmp["a"].as_str().unwrap_or("a"),
        cmp["b"].as_str().unwrap_or("b"),
    );
    for task in cmp["tasks"].as_array().into_iter().flatten() {
        let (va, vb) = (task["headline"]["a"] == "v2", task["headline"]["b"] == "v2");
        eprintln!(
            "\n{}  {}",
            crate::ui::bold(task["task"].as_str().unwrap_or("?")),
            crate::ui::dim(format!(
                "task {} vs {}{}",
                task["versions"]["a"].as_str().unwrap_or("–"),
                task["versions"]["b"].as_str().unwrap_or("–"),
                match (va, vb) {
                    (true, true) => " · scoring v2",
                    (false, false) => "",
                    _ => " · scored differently: no change shown",
                }
            ))
        );
        eprintln!("  {:<22} {:>10} {:>10} {:>9}", "model", a, b, "change");
        for m in task["models"].as_array().into_iter().flatten() {
            let delta = m["score_delta"]
                .as_f64()
                .map(|d| {
                    let text = if va {
                        format!("{d:+.2}")
                    } else {
                        format!("{d:+.2}×")
                    };
                    if d >= 0.0 {
                        crate::ui::accent(text)
                    } else {
                        text
                    }
                })
                .unwrap_or_else(|| "–".to_string());
            let score = |side: &Value| {
                if side["score"].is_null() {
                    side["skill"].clone()
                } else {
                    side["score"].clone()
                }
            };
            eprintln!(
                "  {:<22} {:>10} {:>10} {:>9}",
                m["model"]
                    .as_str()
                    .unwrap_or("?")
                    .trim_start_matches("claude-"),
                show(&score(&m["a"]), va),
                show(&score(&m["b"]), vb),
                delta
            );
        }
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A root with one finished trial of `task` by `model`, its report given.
    fn root_with(report: &str) -> Root {
        let dir = std::env::temp_dir().join(format!("mtx-cmp-{}", uuid::Uuid::new_v4().simple()));
        let trial = dir.join("job/t-1");
        std::fs::create_dir_all(trial.join("verifier")).unwrap();
        std::fs::write(
            trial.join("result.json"),
            r#"{"trial_name": "t-1", "task_name": "solana-foundation/new-thing-spcx", "finished_at": "x", "agent_info": {"model_info": {"name": "m"}}}"#,
        )
        .unwrap();
        std::fs::write(trial.join("verifier/report.json"), report).unwrap();
        Root {
            label: dir.file_name().unwrap().to_string_lossy().into_owned(),
            path: dir,
            published: false,
        }
    }

    #[test]
    fn a_v2_task_is_compared_on_its_reward_never_on_skill() {
        let a = root_with(r#"{"headline": "v2", "reward": 0.6, "scenarios": []}"#);
        let b = root_with(r#"{"headline": "v2", "reward": 0.4, "scenarios": []}"#);
        let v1 = root_with(r#"{"reward": 0.1, "skill": 1.2, "scenarios": []}"#);
        let cmp = compare(&a, &b);
        let row = &cmp["tasks"][0]["models"][0];
        assert!(
            (row["score_delta"].as_f64().unwrap() - 0.2).abs() < 1e-12,
            "{row}"
        );
        assert!(row.get("skill_delta").is_none());
        assert_eq!(cmp["tasks"][0]["headline"]["a"], "v2");
        // A task that moved to v2 isn't compared across the switch.
        let moved = compare(&a, &v1);
        assert!(moved["tasks"][0]["models"][0]["score_delta"].is_null());
        // A v1 task keeps its skill comparison.
        let both_v1 = compare(&v1, &v1);
        assert_eq!(both_v1["tasks"][0]["models"][0]["skill_delta"], 0.0);
        // The leaderboard's scores rank a v2 task on reward.
        let scores = review::scores_of(&a);
        let task = &scores["tasks"]["new-thing"];
        assert_eq!(task["headline"], "v2");
        assert_eq!(task["models"]["m"]["score"], 0.6);
        assert!(task["models"]["m"]["skill"].is_null());
        for r in [a, b, v1] {
            std::fs::remove_dir_all(&r.path).unwrap();
        }
    }

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
        std::fs::write(trial.join("agent/hermes.txt"), "the agent's transcript").unwrap();
        std::fs::write(trial.join("agent/claude-code.txt"), "raw").unwrap();
        // Over the size cap: left out, and named by its path in the batch.
        std::fs::write(
            trial.join("artifacts/app/bot/model.bin"),
            vec![7u8; 2 << 20],
        )
        .unwrap();
        std::fs::create_dir_all(trial.join("verifier/bot/reference")).unwrap();
        std::fs::write(trial.join("verifier/bot/hidden-1.log"), "bot log").unwrap();
        std::fs::write(trial.join("verifier/bot/reference/hidden-1.log"), "ref log").unwrap();
        let results = tmp.join("results");
        std::fs::create_dir_all(&results).unwrap();

        publish(&batch, "1.0.0", &results, false, None, true, false).unwrap();
        let out = results.join("1.0.0/job/task__a");
        assert!(out.join("artifacts/app/bot/src/main.rs").is_file());
        assert!(out.join("verifier/sessions/bot/hidden-1.json.gz").is_file());
        assert!(out.join("agent/trajectory.json.gz").is_file());
        assert!(out.join("agent/hermes.txt.gz").is_file());
        assert!(!out.join("agent/trajectory.json").exists());
        assert!(!out.join("agent/claude-code.txt").exists());
        assert!(out.join("verifier/bot/hidden-1.log.gz").is_file());
        assert!(!out.join("verifier/bot/hidden-1.log").exists());
        assert!(!out.join("verifier/bot/reference").exists());
        assert!(!out.join("artifacts/app/bot/.env").exists());
        assert!(!out.join("artifacts/app/bot/target").exists());
        assert!(!results.join("1.0.0/.seed").exists());
        assert!(!out.join("artifacts/app/bot/model.bin").exists());
        let release: Value = serde_json::from_str(
            &std::fs::read_to_string(results.join("1.0.0/release.json")).unwrap(),
        )
        .unwrap();
        let skipped: Vec<&str> = release["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s.as_str())
            .collect();
        assert_eq!(
            skipped,
            ["job/task__a/artifacts/app/bot/model.bin (2097152 bytes)"]
        );
        assert_eq!(releases(&results).len(), 1);
        assert!(
            publish(&batch, "1.0.0", &results, false, None, true, false).is_err(),
            "exists"
        );

        // The seed in a bot's source: nothing is written.
        std::fs::write(
            trial.join("artifacts/app/bot/src/seed.rs"),
            "0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let error = publish(&batch, "1.0.1", &results, false, None, true, false).unwrap_err();
        assert!(error.to_string().contains("seed"), "{error}");
        assert!(!results.join("1.0.1").exists());
        assert!(!results.join(".1.0.1.staging").exists());

        // With --artifacts: the full release outside, the slim one here.
        std::fs::remove_file(trial.join("artifacts/app/bot/src/seed.rs")).unwrap();
        let artifacts = tmp.join("artifacts");
        publish(
            &batch,
            "1.0.2",
            &results,
            false,
            Some(&artifacts),
            false,
            false,
        )
        .unwrap();
        let full = artifacts.join("1.0.2/job/task__a");
        let slim = results.join("1.0.2/job/task__a");
        assert!(
            full.join("verifier/sessions/bot/hidden-1.json.gz")
                .is_file()
        );
        assert!(full.join("verifier/bot/hidden-1.log.gz").is_file());
        assert!(!slim.join("verifier/sessions").exists());
        assert!(!slim.join("verifier/bot").exists());
        assert!(!slim.join("agent").exists());
        assert!(slim.join("verifier/report.json").is_file());
        assert!(slim.join("artifacts/app/bot/src/main.rs").is_file());
        let release = review::read_json(&results.join("1.0.2/release.json")).unwrap();
        assert_eq!(release["artifacts"]["files"], 4);
        assert!(!results.join(".1.0.2.slim").exists());
        assert!(
            publish(
                &batch,
                "1.0.2",
                &results,
                false,
                Some(&artifacts),
                false,
                false
            )
            .is_err(),
            "exists"
        );

        // A batch whose native harnesses ran on guarded tasks is refused,
        // by its marker or by its batch.json, whatever else is allowed.
        std::fs::write(batch.join(".unguarded"), "").unwrap();
        let error = publish(&batch, "1.0.9", &results, false, None, true, true).unwrap_err();
        assert!(error.to_string().contains("not guarded"), "{error}");
        std::fs::remove_file(batch.join(".unguarded")).unwrap();
        let plan = batch.join("batch.json");
        let kept = std::fs::read_to_string(&plan).ok();
        std::fs::write(&plan, r#"{"guarded": false}"#).unwrap();
        let error = publish(&batch, "1.0.9", &results, false, None, true, true).unwrap_err();
        assert!(error.to_string().contains("not guarded"), "{error}");
        match kept {
            Some(text) => std::fs::write(&plan, text).unwrap(),
            None => std::fs::remove_file(&plan).unwrap(),
        }
        assert!(!results.join("1.0.9").exists());

        // A run without its agent's trajectory: refused, unless allowed.
        std::fs::remove_file(trial.join("agent/trajectory.json")).unwrap();
        let error = publish(&batch, "1.0.3", &results, false, None, true, false).unwrap_err();
        assert!(error.to_string().contains("no agent trajectory"), "{error}");
        assert!(!results.join("1.0.3").exists());
        publish(&batch, "1.0.3", &results, false, None, true, true).unwrap();
        assert!(
            results
                .join("1.0.3/job/task__a/agent/hermes.txt.gz")
                .is_file()
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn a_native_harness_on_a_guarded_task_is_refused_wherever_the_trial_sits() {
        let tmp = std::env::temp_dir().join(format!("mtx-native-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let task = |name: &str, from: &str| {
            let env = tmp.join("bench/tasks").join(name).join("environment");
            std::fs::create_dir_all(&env).unwrap();
            std::fs::write(
                env.join("Dockerfile"),
                format!("FROM ghcr.io/x/machine-trading:{from}\n"),
            )
            .unwrap();
        };
        task("guarded-spcx", "0.12.0-agent");
        task("plain-spcx", "0.12.0");
        let trial = |dir: &str, agent: &str, task: &str| {
            let t = tmp.join("runs").join(dir);
            std::fs::create_dir_all(&t).unwrap();
            std::fs::write(
                t.join("result.json"),
                json!({"trial_name": dir, "task_name": format!("solana-foundation/{task}"), "agent_info": {"name": agent}}).to_string(),
            )
            .unwrap();
            t
        };
        let hermes = trial("a", "hermes", "guarded-spcx");
        let codex = trial("b", "codex", "guarded-spcx");
        let claude = trial("c", "claude-code", "guarded-spcx");
        let plain = trial("d", "codex", "plain-spcx");
        let unknown = trial("e", "codex", "elsewhere-spcx");
        let run = tmp.join("runs");
        assert!(refuse_unguarded(&tmp, &run, &[hermes.clone(), plain]).is_ok());
        for bad in [codex, claude, unknown] {
            let error = refuse_unguarded(&tmp, &bad, &[hermes.clone(), bad.clone()]).unwrap_err();
            assert!(error.to_string().contains("native harness"), "{error}");
        }
        // A marker above a job is found from inside it.
        let job = tmp.join("batch/job");
        std::fs::create_dir_all(&job).unwrap();
        assert!(refuse_unguarded(&tmp, &job, &[]).is_ok());
        std::fs::write(tmp.join("batch/.unguarded"), "").unwrap();
        let error = refuse_unguarded(&tmp, &job, &[]).unwrap_err();
        assert!(error.to_string().contains("not guarded"), "{error}");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    fn session(dir: &Path, name: &str, pnl: f64, turn_ms: u64) {
        std::fs::create_dir_all(dir).unwrap();
        let body = json!({ "report": { "pnl_usd": pnl }, "timeline": { "steps": [{ "k": 0, "turn_ms": turn_ms }] } });
        write_gz(
            &dir.join(format!("{name}.json.gz")),
            body.to_string().as_bytes(),
        )
        .unwrap();
    }

    #[test]
    fn a_release_keeps_each_reference_session_once() {
        let out = std::env::temp_dir().join(format!("mtx-share-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let trial = |name: &str| {
            let dir = out.join("job").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("result.json"),
                json!({ "trial_name": name }).to_string(),
            )
            .unwrap();
            dir
        };
        let (a, b, c) = (trial("mm__a"), trial("mm__b"), trial("fc__c"));
        let refs = |t: &Path| t.join("verifier/sessions/reference");
        // b plays hidden-1 like a, but for its turn times; hidden-2 differs.
        session(&refs(&a), "hidden-1", 10.0, 3);
        session(&refs(&b), "hidden-1", 10.0, 9);
        session(&refs(&a), "hidden-2", 5.0, 1);
        session(&refs(&b), "hidden-2", 6.0, 1);
        // Another task's session of the same name is its own.
        session(&refs(&c), "hidden-1", 10.0, 3);

        assert_eq!(share_references(&out).unwrap(), 1);
        assert!(refs(&a).join("hidden-1.json.gz").is_file());
        assert!(!refs(&b).join("hidden-1.json.gz").exists());
        assert!(refs(&b).join("hidden-2.json.gz").is_file());
        assert!(refs(&c).join("hidden-1.json.gz").is_file());
        let pointers = review::read_json(&b.join(SHARED_REFERENCES)).unwrap();
        assert_eq!(
            pointers["hidden-1"],
            "job/mm__a/verifier/sessions/reference/hidden-1.json.gz"
        );
        assert!(!c.join(SHARED_REFERENCES).exists());
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn a_long_log_keeps_its_start_and_end() {
        assert_eq!(cut_log(b"short"), b"short");
        let mut log = vec![b'a'; LOG_HEAD_BYTES];
        log.extend(vec![b'b'; 100_000]);
        log.extend(vec![b'c'; LOG_TAIL_BYTES]);
        let cut = String::from_utf8(cut_log(&log)).unwrap();
        assert!(cut.starts_with('a') && cut.ends_with('c'));
        assert!(cut.contains("[… 100000 bytes left out of the published log …]"));
        assert!(!cut.contains("bb"));
    }
}
