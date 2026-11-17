//! What the engine does to a task's anchors: `tests/anchor.digest`.
//!
//! A task's fingerprint can't hash the engine's source: most engine changes
//! (a new venue, a test, a comment) leave every task's scores where they
//! were, and a fingerprint that moves every time is one nobody reads. So it
//! hashes the engine's *behaviour* on the task instead. The digest is the
//! SHA-256 of the sessions the task's reference (and its informed bot, when
//! the task has one) plays on a small fixed public draw of its scenario
//! family, without the fields that differ from run to run by the machine or
//! by chance (`publish::VOLATILE`: wall and turn times, compute units,
//! signatures). It is committed beside the fingerprint and is one of its
//! inputs: an engine, kit or dependency change that alters what the anchors
//! do moves it, and so moves the fingerprint; one that doesn't, doesn't.
//!
//! - `mtx task sync --anchors` builds the anchors and plays the sessions
//!   (Linux, `just` and Surfpool installed, as the verifier has them) and
//!   writes it. Plain `mtx task sync` keeps a digest whose inputs are
//!   unchanged and otherwise writes `pending`;
//! - `mtx task check` fails when the anchors' inputs changed since it was
//!   written (the `inputs` line) and on a `pending` digest;
//! - `mtx task anchor-digest --check`, which CI runs on every change to
//!   `crates/`, `Cargo.lock`, `rust-toolchain` or `snapshots/`, and on a
//!   task's own changes, plays the sessions again. It fails on a `pending`
//!   digest (printing the one to commit) and when the digest differs: the
//!   engine changed what the task does. Then run `mtx task sync --anchors`
//!   (its fingerprint moves with it; bump `GRADER_VERSION` only for changes
//!   the digests can't see).

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use mtx_grader::versions::hash_parts;
use serde_json::Value;

/// Under a task's folder.
pub const DIGEST_FILE: &str = "tests/anchor.digest";

/// The secret the public draw comes from: public on purpose, never a
/// hidden set's.
const SECRET: &str = "mtx-anchor-digest/public-draw/2026-11/xxxxxxxx";
/// Sessions in the draw: small enough for a CI job, and enough that each
/// of a family's strata is in it.
pub const SESSIONS: usize = 12;
/// The digest file's format.
const FORMAT: &str = "1";

/// The anchors the digest covers, as the file names them.
const REFERENCE_ONLY: &str = "reference (this task has no informed bot)";
const REFERENCE_AND_INFORMED: &str = "reference and informed bot";

/// A digest file's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    /// A hash of what the sessions are played from (see [`inputs`]).
    pub inputs: String,
    /// A hash of the sessions' results.
    pub digest: String,
    pub informed: bool,
}

impl Digest {
    pub fn text(&self) -> String {
        format!(
            "# Written by `mtx task sync`; do not edit.\n\
             # What the engine does to this task's anchors on a fixed public draw\n\
             # ({SESSIONS} sessions of the task's family): the SHA-256 of their results,\n\
             # without wall times, compute units and signatures. CI recomputes it when\n\
             # the engine changes (`mtx task anchor-digest --check`); a difference\n\
             # means the engine changed what this task does: run\n\
             # `mtx task sync --anchors` and commit the result.\n\
             format: {FORMAT}\n\
             anchors: {}\n\
             inputs: {}\n\
             digest: {}\n",
            if self.informed {
                REFERENCE_AND_INFORMED
            } else {
                REFERENCE_ONLY
            },
            self.inputs,
            self.digest
        )
    }

    pub fn parse(text: &str) -> Option<Self> {
        let field = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix(": "))
                .map(|v| v.trim().to_string())
        };
        Some(Self {
            inputs: field("inputs")?,
            digest: field("digest")?,
            informed: field("anchors")?.starts_with(REFERENCE_AND_INFORMED),
        })
    }

    /// Not played yet: the file says so instead of a digest.
    pub fn is_pending(&self) -> bool {
        self.digest.starts_with("pending")
    }

    /// What the file holds for a task whose digest hasn't been played yet.
    pub fn pending(inputs: &str, informed: bool) -> Self {
        Self {
            inputs: inputs.to_string(),
            digest: "pending: run `mtx task sync --anchors` on Linux".to_string(),
            informed,
        }
    }
}

/// What a task's digest is played from: the anchors' sources, the snapshot,
/// the family's parameters and the draw's settings. Not the engine: that is
/// what the digest measures.
pub fn inputs(
    root: &Path,
    task: &str,
    read: &dyn Fn(&Path) -> Option<Vec<u8>>,
) -> Result<(String, bool)> {
    let meta = Meta::of(root, task, read)?;
    let folder = PathBuf::from("bench/tasks").join(task);
    let mut dirs = vec![folder.join("reference")];
    if let Some(informed) = &meta.informed {
        dirs.push(PathBuf::from(informed));
    }
    let mut parts: Vec<(String, Vec<u8>)> = vec![
        ("format".into(), FORMAT.into()),
        ("secret".into(), SECRET.into()),
        ("sessions".into(), SESSIONS.to_string().into_bytes()),
        ("v2 sessions".into(), vec![u8::from(meta.v2_sessions)]),
        ("generator".into(), meta.generator.clone().into_bytes()),
        (
            "parameters".into(),
            (mtx_grader::generators::generator(&meta.generator)
                .with_context(|| format!("{task}: no generator {}", meta.generator))?
                .params)()
            .into_bytes(),
        ),
    ];
    let snapshot = PathBuf::from("snapshots").join(&meta.snapshot);
    dirs.push(snapshot);
    for dir in &dirs {
        for rel in crate::versions::tracked_files(root, dir, &["target", "bin", "node_modules"]) {
            parts.push((rel.display().to_string(), read(&rel).unwrap_or_default()));
        }
    }
    Ok((
        hash_parts(parts.iter().map(|(l, b)| (l.as_str(), b.as_slice()))),
        meta.informed.is_some(),
    ))
}

struct Meta {
    generator: String,
    snapshot: String,
    /// The informed bot's crate, from the repository's root.
    informed: Option<String>,
    v2_sessions: bool,
}

impl Meta {
    fn of(root: &Path, task: &str, read: &dyn Fn(&Path) -> Option<Vec<u8>>) -> Result<Self> {
        let folder = PathBuf::from("bench/tasks").join(task);
        let toml_text =
            read(&folder.join("task.toml")).with_context(|| format!("{task}: no task.toml"))?;
        let toml: toml::Value = toml::from_str(&String::from_utf8_lossy(&toml_text))
            .with_context(|| format!("{task}/task.toml"))?;
        let generator = toml["metadata"]["mtx"]["generator"]
            .as_str()
            .with_context(|| format!("{task}: no [metadata.mtx] generator"))?
            .to_string();
        let snapshot = toml["metadata"]["snapshot"]
            .as_str()
            .with_context(|| format!("{task}: no [metadata] snapshot"))?
            .to_string();
        let script = read(&folder.join("tests/test.sh")).unwrap_or_default();
        let v2_sessions = String::from_utf8_lossy(&script).contains("--v2-sessions");
        let informed = crate::tasks::load_one(root, task.to_string())
            .ok()
            .and_then(|t| t.informed)
            .map(|i| i.dir);
        Ok(Self {
            generator,
            snapshot,
            informed,
            v2_sessions,
        })
    }
}

/// Build the anchors and play the sessions, then digest them. Needs what the
/// verifier has: `just`, the wasm target, Surfpool, and Linux (a process bot
/// runs as its own user). The graded bot is the reference itself: only the
/// anchors' sessions count. The anchors are built here, never taken from a
/// `bin/bot` already on disk, which may be older than its sources.
pub fn compute(root: &Path, task: &str) -> Result<Digest> {
    let read = |p: &Path| std::fs::read(root.join(p)).ok();
    let (inputs, informed) = inputs(root, task, &read)?;
    let meta = Meta::of(root, task, &read)?;
    let folder = root.join("bench/tasks").join(task);

    let mut built = vec![folder.join("reference")];
    if let Some(informed) = &meta.informed {
        built.push(root.join(informed));
    }
    for dir in &built {
        build_anchor(dir)?;
    }

    let work = std::env::temp_dir().join(format!("mtx-anchors-{}", uuid::Uuid::new_v4().simple()));
    let result = play(root, &folder, &meta, &work);
    let digest = result.and_then(|()| digest_of(&work.join("details"), informed));
    let _ = std::fs::remove_dir_all(&work);
    Ok(Digest {
        inputs,
        digest: digest.with_context(|| format!("{task}: anchor digest"))?,
        informed,
    })
}

/// `just build` in an anchor's folder: its `bot.wasm` and `bin/bot`.
fn build_anchor(dir: &Path) -> Result<()> {
    let status = Command::new("just")
        .arg("--justfile")
        .arg(dir.join("Justfile"))
        .arg("build")
        .stdout(std::process::Stdio::null())
        .status()
        .context("running `just build` (is `just` installed?)")?;
    if !status.success() {
        bail!("`just build` failed in {} ({status})", dir.display());
    }
    Ok(())
}

/// An anchor as the image ships it: the Justfile and the built process.
fn stage_anchor(from: &Path, to: &Path) -> Result<()> {
    let bin = from.join("bin/bot");
    if !bin.is_file() {
        bail!(
            "{} isn't built: run `just --justfile {} build`",
            from.display(),
            from.join("Justfile").display()
        );
    }
    std::fs::create_dir_all(to.join("bin"))?;
    std::fs::copy(from.join("Justfile"), to.join("Justfile"))?;
    std::fs::copy(&bin, to.join("bin/bot"))?;
    Ok(())
}

fn play(root: &Path, folder: &Path, meta: &Meta, work: &Path) -> Result<()> {
    let reference = work.join("reference");
    stage_anchor(&folder.join("reference"), &reference)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .current_dir(root)
        .env("MTX_EVAL_SEED", SECRET)
        .env_remove("MTX_JOBS")
        .args(["grade", "--draw", &meta.generator])
        .args(["--count", &SESSIONS.to_string()])
        .arg("--reference")
        .arg(&reference);
    if let Some(informed) = &meta.informed {
        let staged = work.join("informed");
        stage_anchor(&root.join(informed), &staged)?;
        command.arg("--informed").arg(&staged);
    }
    command
        .args(["--anchor-jobs", "4", "--jobs", "4", "--trusted"])
        .arg("--snapshot")
        .arg(root.join("snapshots").join(&meta.snapshot))
        .arg("--bot")
        .arg(&reference)
        .arg("--logs")
        .arg(work.join("logs"))
        .arg("--details")
        .arg(work.join("details"));
    if meta.v2_sessions {
        command.arg("--v2-sessions");
    }
    let out = command
        .stdout(std::process::Stdio::null())
        .output()
        .context("running `mtx grade`")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(15).collect();
        bail!(
            "`mtx grade` failed ({}); the anchors play on Linux, as root, with just and surfpool installed:\n{}",
            out.status,
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(())
}

/// The hash of each anchor's session files, in order, without their
/// volatile fields.
fn digest_of(details: &Path, informed: bool) -> Result<String> {
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    for role in ["reference", "informed"] {
        if role == "informed" && !informed {
            continue;
        }
        let dir = details.join(role);
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| dir.display().to_string())?
            .flatten()
            .map(|e| e.path())
            .collect();
        files.sort();
        if files.len() < SESSIONS {
            bail!("{role}: {} sessions, not {SESSIONS}", files.len());
        }
        for file in files {
            let mut json = Vec::new();
            flate2::read::GzDecoder::new(std::fs::File::open(&file)?)
                .read_to_end(&mut json)
                .with_context(|| file.display().to_string())?;
            let mut value: Value = serde_json::from_slice(&json)?;
            crate::publish::strip_volatile(&mut value);
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            parts.push((format!("{role}/{name}"), serde_json::to_vec(&value)?));
        }
    }
    Ok(hash_parts(
        parts.iter().map(|(l, b)| (l.as_str(), b.as_slice())),
    ))
}

/// The committed digest file of a task, if it parses.
pub fn committed(root: &Path, task: &str) -> Option<Digest> {
    let text =
        std::fs::read_to_string(root.join("bench/tasks").join(task).join(DIGEST_FILE)).ok()?;
    Digest::parse(&text)
}

/// Write a task's digest file.
pub fn write(root: &Path, task: &str, digest: &Digest) -> Result<()> {
    let path = root.join("bench/tasks").join(task).join(DIGEST_FILE);
    std::fs::create_dir_all(path.parent().context("no parent")?)?;
    std::fs::write(&path, digest.text()).with_context(|| path.display().to_string())
}

/// `mtx task anchor-digest --check`: play the sessions and compare with the
/// committed digest. Returns why the task fails, if it does.
pub fn check(root: &Path, task: &str) -> Result<Option<String>> {
    let read = |p: &Path| std::fs::read(root.join(p)).ok();
    let (now, _) = inputs(root, task, &read)?;
    judge(task, committed(root, task), &now, || compute(root, task))
}

/// The verdict on a committed digest, given the inputs now and a way to play
/// the sessions (only asked for when the file isn't already out of date).
fn judge(
    task: &str,
    committed: Option<Digest>,
    now: &str,
    play: impl FnOnce() -> Result<Digest>,
) -> Result<Option<String>> {
    let Some(committed) = committed else {
        return Ok(Some(format!(
            "{task}: no {DIGEST_FILE}: run `mtx task sync --anchors`"
        )));
    };
    if committed.is_pending() {
        // Say what to commit, so nobody needs a Linux box of their own.
        let played = play()?;
        return Ok(Some(format!(
            "{task}: {DIGEST_FILE} is pending: its anchors were never played. Run `mtx task \
             sync --anchors` on Linux, or commit this as bench/tasks/{task}/{DIGEST_FILE} (then \
             `mtx task sync` for the fingerprint):\n{}",
            Digest {
                inputs: now.to_string(),
                ..played
            }
            .text()
        )));
    }
    if committed.inputs != now {
        return Ok(Some(format!(
            "{task}: {DIGEST_FILE} was written for other anchors, snapshot or draw settings: run `mtx task sync --anchors`"
        )));
    }
    let played = play()?;
    if played.digest != committed.digest {
        return Ok(Some(format!(
            "{task}: the anchors play differently: digest {} was {}. An engine, kit or dependency \
             change altered what this task does. Run `mtx task sync --anchors` and commit the \
             digest (it moves this task's fingerprint only)",
            short(&played.digest),
            short(&committed.digest)
        )));
    }
    Ok(None)
}

fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_round_trips_and_names_what_it_covers() {
        let d = Digest {
            inputs: "aa".into(),
            digest: "bb".into(),
            informed: false,
        };
        assert!(d.text().contains("no informed bot"));
        assert_eq!(Digest::parse(&d.text()), Some(d.clone()));
        let both = Digest {
            informed: true,
            ..d
        };
        assert_eq!(Digest::parse(&both.text()), Some(both));
        assert_eq!(Digest::parse("nothing"), None);
    }

    #[test]
    fn a_pending_file_never_matches_a_played_digest() {
        let pending = Digest::pending("aa", false);
        assert!(pending.is_pending());
        assert_ne!(pending.digest.len(), 64);
        let parsed = Digest::parse(&pending.text()).unwrap();
        assert!(parsed.is_pending());
        let played = Digest {
            digest: "ab".repeat(32),
            ..pending
        };
        assert!(!Digest::parse(&played.text()).unwrap().is_pending());
    }

    fn played(digest: &str) -> Result<Digest> {
        Ok(Digest {
            inputs: "ignored".into(),
            digest: digest.into(),
            informed: false,
        })
    }

    #[test]
    fn the_check_refuses_a_pending_digest_and_prints_the_one_to_commit() {
        let pending = Digest::pending("aa", false);
        let why = judge("t", Some(pending), "aa", || played("cc")).unwrap();
        let why = why.expect("a pending digest fails");
        assert!(why.contains("pending"), "{why}");
        assert!(!why.contains("GRADER_VERSION"), "{why}");
        let printed = Digest::parse(why.split_once(":\n").unwrap().1).unwrap();
        assert_eq!(
            printed,
            Digest {
                inputs: "aa".into(),
                digest: "cc".into(),
                informed: false
            }
        );
        assert!(
            judge("t", None, "aa", || played("cc"))
                .unwrap()
                .unwrap()
                .contains("no tests/anchor.digest")
        );
    }

    #[test]
    fn the_check_plays_only_a_digest_that_is_current() {
        let file = |inputs: &str, digest: &str| Digest {
            inputs: inputs.into(),
            digest: digest.into(),
            informed: false,
        };
        let never = || -> Result<Digest> { panic!("played a digest written for other inputs") };
        assert!(
            judge("t", Some(file("old", "cc")), "aa", never)
                .unwrap()
                .unwrap()
                .contains("other anchors")
        );
        assert_eq!(
            judge("t", Some(file("aa", "cc")), "aa", || played("cc")).unwrap(),
            None
        );
        let why = judge("t", Some(file("aa", "cc")), "aa", || played("dd"))
            .unwrap()
            .unwrap();
        assert!(why.contains("play differently"), "{why}");
        assert!(!why.contains("GRADER_VERSION"), "{why}");
    }
}
