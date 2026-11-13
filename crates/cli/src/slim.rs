//! Slim releases: what a published release keeps in the repository, with
//! its full copy kept outside it.
//!
//! Most of a release is its sessions step by step, the bots' logs and the
//! agents' trajectories (0.11.0: 69 of its 75 MB). A slim release keeps
//! what the leaderboard, comparisons and readers of the bots need: the
//! release's and batch's records, each task's `task.toml`, and per trial its
//! Harbor result and config, the verifier's report and reward, and the
//! bot's source. The full release goes to a directory outside the
//! repository (`mtx bench publish --artifacts <dir>`, as `<dir>/<version>`),
//! with a [`MANIFEST`] of every file the slim one leaves out and its
//! SHA-256; the slim `release.json` records the manifest's own SHA-256, so a
//! full copy can be checked against the release it belongs to.
//!
//! Readers (`mtx bench review`, `export`, `compare` and `rescore`) read a
//! release's full copy when [`ARTIFACTS_ENV`] names the directory it is in
//! and it matches, and the slim release otherwise: everything but the
//! sessions step by step, the logs and the trajectories.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The directory full copies of slim releases are in, for readers.
pub const ARTIFACTS_ENV: &str = "MTX_ARTIFACTS";
/// A full copy's list of the files its slim release leaves out: one
/// `<sha256>  <path>` line per file, sorted by path (`sha256sum -c` reads it).
pub const MANIFEST: &str = "artifacts.sha256";

/// Whether a slim release keeps the file at `rel`, a path in the release
/// with `/` between its parts.
pub fn keeps(rel: &str) -> bool {
    const TRIAL_FILES: &[&str] = &[
        "result.json",
        "config.json",
        "exception.txt",
        crate::compact::SHARED_FILES,
        "verifier/report.json",
        "verifier/reward.json",
    ];
    rel == "release.json"
        || rel == "batch.json"
        || rel.starts_with("tasks/")
        || rel.contains("/artifacts/app/bot/")
        || TRIAL_FILES
            .iter()
            .any(|file| rel.strip_suffix(file).is_some_and(|dir| dir.ends_with('/')))
}

/// Write the slim release of the full release `full` to `out`, a new
/// directory, and `full`'s [`MANIFEST`]. Both `release.json`s gain an
/// `artifacts` section: what was left out, and the manifest's SHA-256.
pub fn slim(full: &Path, out: &Path) -> Result<Value> {
    if !full.join("release.json").is_file() {
        bail!("{} isn't a published release", full.display());
    }
    if out.exists() {
        bail!("{} exists", out.display());
    }
    let mut files = Vec::new();
    walk_files(full, &mut files);
    files.sort();
    let mut manifest = String::new();
    let (mut kept, mut left, mut left_bytes) = (0usize, 0usize, 0u64);
    for path in &files {
        let rel = relative(full, path)?;
        if rel == MANIFEST || rel == "release.json" {
            continue;
        }
        if keeps(&rel) {
            let to = out.join(&rel);
            if let Some(dir) = to.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::copy(path, &to).with_context(|| path.display().to_string())?;
            kept += 1;
        } else {
            let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
            manifest.push_str(&format!("{}  {rel}\n", hex(&Sha256::digest(&bytes))));
            left += 1;
            left_bytes += bytes.len() as u64;
        }
    }
    std::fs::write(full.join(MANIFEST), &manifest)?;
    let artifacts = json!({
        "files": left,
        "bytes": left_bytes,
        "sha256": hex(&Sha256::digest(manifest.as_bytes())),
    });
    let mut release: Value = serde_json::from_slice(&std::fs::read(full.join("release.json"))?)?;
    release["artifacts"] = artifacts.clone();
    let release = serde_json::to_string_pretty(&release)? + "\n";
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join("release.json"), &release)?;
    std::fs::write(full.join("release.json"), &release)?;
    Ok(json!({ "kept": kept + 1, "artifacts": artifacts }))
}

/// Move the release `release` (in the repository's results) out to
/// `<artifacts>/<its name>`, and leave its slim release in its place.
pub fn slim_in_place(release: &Path, artifacts: &Path) -> Result<Value> {
    let name = release
        .file_name()
        .with_context(|| release.display().to_string())?;
    if crate::review::read_json(&release.join("release.json"))
        .is_some_and(|r| r["artifacts"].is_object())
    {
        bail!("{} is slim already", release.display());
    }
    let full = artifacts.join(name);
    if full.exists() {
        bail!("{} exists", full.display());
    }
    copy_dir(release, &full)?;
    let staging = release.with_file_name(format!(".{}.slim", name.to_string_lossy()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    let summary = slim(&full, &staging);
    if summary.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        return summary;
    }
    std::fs::remove_dir_all(release)?;
    std::fs::rename(&staging, release)?;
    summary
}

/// The full copy of the slim release at `release`, if [`ARTIFACTS_ENV`]
/// names a directory that holds one matching it. `None` for a release
/// that isn't slim.
pub fn full_copy(release: &Path) -> Option<PathBuf> {
    full_copy_in(release, Path::new(&std::env::var_os(ARTIFACTS_ENV)?))
}

/// [`full_copy`], in `artifacts`.
fn full_copy_in(release: &Path, artifacts: &Path) -> Option<PathBuf> {
    let digest = crate::review::read_json(&release.join("release.json"))?["artifacts"]["sha256"]
        .as_str()?
        .to_string();
    let full = artifacts.join(release.file_name()?);
    let manifest = std::fs::read(full.join(MANIFEST)).ok()?;
    if hex(&Sha256::digest(&manifest)) == digest {
        Some(full.canonicalize().unwrap_or(full))
    } else {
        tracing::warn!(
            full = %full.display(),
            "this full copy's manifest doesn't match its release's; reading the slim release"
        );
        None
    }
}

/// Move `from` to `to`, a new path, even on another file system.
pub fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_dir(from, to)?;
    std::fs::remove_dir_all(from)?;
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    let mut files = Vec::new();
    walk_files(from, &mut files);
    for path in files {
        let dest = to.join(path.strip_prefix(from)?);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::copy(&path, &dest).with_context(|| path.display().to_string())?;
    }
    Ok(())
}

/// Regular files under `dir`, links not followed.
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk_files(&entry.path(), out),
            Ok(kind) if kind.is_file() => out.push(entry.path()),
            _ => {}
        }
    }
}

fn relative(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)?
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slim_release_keeps_the_records_and_the_bots_not_the_sessions() {
        for kept in [
            "release.json",
            "batch.json",
            "tasks/flash-crash-spcx/task.toml",
            "2026-11-10__05-21-34/result.json",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/result.json",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/config.json",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/shared-files.json",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/verifier/report.json",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/verifier/reward.json",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/artifacts/app/bot/src/lib.rs",
        ] {
            assert!(keeps(kept), "{kept}");
        }
        for left in [
            "scenarios/7e22cbfdf8224d4c.json.gz",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/verifier/sessions/bot/hidden-1.json.gz",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/verifier/sessions/reference-shared.json",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/verifier/bot/hidden-1.log.gz",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/verifier/test-stdout.txt.gz",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/agent/trajectory.json.gz",
            "2026-11-10__05-21-34/flash-crash-spcx__abc/verifier/my-report.json",
        ] {
            assert!(!keeps(left), "{left}");
        }
    }

    #[test]
    fn a_full_copy_matches_its_slim_release_and_nothing_else() {
        let root = std::env::temp_dir().join(format!("mtx-slim-{}", uuid::Uuid::new_v4()));
        let release = root.join("results/0.1.0");
        let trial = release.join("job/task__a");
        std::fs::create_dir_all(trial.join("verifier/sessions/bot")).unwrap();
        std::fs::write(release.join("release.json"), r#"{"version": "0.1.0"}"#).unwrap();
        std::fs::write(trial.join("result.json"), "{}").unwrap();
        std::fs::write(trial.join("verifier/report.json"), r#"{"reward": 0.5}"#).unwrap();
        std::fs::write(
            trial.join("verifier/sessions/bot/hidden-1.json.gz"),
            b"steps",
        )
        .unwrap();

        let artifacts = root.join("artifacts");
        let summary = slim_in_place(&release, &artifacts).unwrap();
        assert_eq!(summary["artifacts"]["files"], 1);
        assert!(trial.join("verifier/report.json").is_file());
        assert!(!trial.join("verifier/sessions").exists());
        let full = artifacts.join("0.1.0");
        assert!(
            full.join("job/task__a/verifier/sessions/bot/hidden-1.json.gz")
                .is_file()
        );
        let manifest = std::fs::read_to_string(full.join(MANIFEST)).unwrap();
        assert!(manifest.ends_with("  job/task__a/verifier/sessions/bot/hidden-1.json.gz\n"));
        assert!(slim_in_place(&release, &artifacts).is_err(), "slim already");

        assert_eq!(
            full_copy_in(&release, &artifacts),
            Some(full.canonicalize().unwrap())
        );
        // A full copy whose files changed isn't this release's.
        std::fs::write(full.join(MANIFEST), "").unwrap();
        assert_eq!(full_copy_in(&release, &artifacts), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
