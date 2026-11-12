//! `mtx bot`: start a bot, practise it, and package it, without the bench.
//!
//! Everything here runs what the bench runs. `practice` builds the bot with
//! its own `Justfile` (`just build`) and grades it with the same [`grade::grade`] the
//! verifier uses, on a task's practice scenarios and snapshot. What this adds
//! is the developer's side of it: a starter to begin from, a short report
//! instead of the verifier's JSON, and a check of the bot directory against
//! the rules every bot runs under (`specs/guides/bots/writing-a-bot.md`).

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use mtx_grader::{
    grade::{self, GradeOptions, GradeReport},
    scenario::Scenario,
};

/// The checkout this `mtx` was built from (`just install mtx` builds in
/// place): where the tasks' practice scenarios, the snapshots and the SDK are.
const BUILT_FROM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const REPO_URL: &str = "https://github.com/lgalabru/machine-trading";
/// The commit this `mtx` was built from, when it was built from a git
/// checkout (see `build.rs`).
const BUILT_COMMIT: Option<&str> = option_env!("MTX_GIT_COMMIT");
const STARTER_LIB: &str = include_str!("../../../bots/starter/src/lib.rs");
const STARTER_JUSTFILE: &str = include_str!("../../../bots/starter/Justfile");
/// The kit's features a strategy component builds with.
const KIT_FEATURES: &str = r#"default-features = false, features = ["component", "venue"]"#;
/// The workspace's lockfile, so a new bot builds against the versions the
/// SDK is tested with. Cargo drops the entries the bot doesn't use.
const LOCKFILE: &str = include_str!("../../../Cargo.lock");

/// The tasks a bot can practise for: every task in the checkout with a
/// practice set, by name, in order.
fn tasks(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root.join("bench/tasks"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join("environment/practice").is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// The task asked for, or the flash-crash task when none is: a name the
/// checkout has, so a task renamed in the bench stays reachable.
pub(crate) fn resolve_task(root: &Path, asked: Option<&str>) -> Result<String> {
    let names = tasks(root);
    let found = match asked {
        Some(name) => names.iter().find(|n| *n == name),
        None => names
            .iter()
            .find(|n| n.contains("flash-crash"))
            .or(names.first()),
    };
    found.cloned().with_context(|| {
        format!(
            "no task {} in {}: the tasks are {}",
            asked.unwrap_or("to default to"),
            root.join("bench/tasks").display(),
            names.join(", ")
        )
    })
}

/// The checkout to read tasks and snapshots from: `--repo` or `MTX_REPO`,
/// else the one this `mtx` was built from.
pub(crate) fn repo(given: Option<&Path>) -> Result<PathBuf> {
    let root = given
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(BUILT_FROM));
    let root = root
        .canonicalize()
        .with_context(|| format!("no machine-trading checkout at {}", root.display()))?;
    if !root.join("bench/tasks").is_dir() {
        bail!(
            "{} is not a machine-trading checkout: pass --repo, or set MTX_REPO",
            root.display()
        );
    }
    Ok(root)
}

/// The snapshot a task's sessions start from, as its environment names it
/// (`ENV MTX_SNAPSHOT=/opt/mtx/snapshots/<name>`).
pub(crate) fn task_snapshot(root: &Path, task: &str) -> Result<PathBuf> {
    let dockerfile = root
        .join("bench/tasks")
        .join(task)
        .join("environment/Dockerfile");
    let text = std::fs::read_to_string(&dockerfile)
        .with_context(|| format!("{}: no such task", dockerfile.display()))?;
    let name = text
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("ENV MTX_SNAPSHOT=/opt/mtx/snapshots/")
        })
        .with_context(|| format!("{} names no snapshot", dockerfile.display()))?;
    Ok(root.join("snapshots").join(name.trim()))
}

/// A task's practice scenarios, in order.
pub(crate) fn practice_scenarios(root: &Path, task: &str) -> Result<Vec<PathBuf>> {
    let dir = root
        .join("bench/tasks")
        .join(task)
        .join("environment/practice");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| dir.display().to_string())?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort_by_key(|path| {
        // practice-2 before practice-10.
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let digits: String = stem
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect();
        let n: u64 = digits
            .chars()
            .rev()
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        (stem.trim_end_matches(char::is_numeric).to_string(), n)
    });
    if files.is_empty() {
        bail!("{} holds no practice scenarios", dir.display());
    }
    Ok(files)
}

/// `mtx bot new`: a strategy component that watches the market and never
/// trades (bots/starter), ready to build and practise.
pub fn new(dir: &Path, git: bool, given_repo: Option<&Path>) -> Result<String> {
    if dir.exists() && dir.read_dir()?.next().is_some() {
        bail!("{} exists and isn't empty", dir.display());
    }
    let kit = if git {
        let rev = BUILT_COMMIT
            .context("this mtx wasn't built from a git checkout, so it can't pin the SDK by commit: leave out --git")?;
        format!(r#"mtx-kit = {{ git = "{REPO_URL}", rev = "{rev}", {KIT_FEATURES} }}"#)
    } else {
        let sdk = repo(given_repo)?.join("crates/kit");
        format!(
            r#"mtx-kit = {{ path = "{}", {KIT_FEATURES} }}"#,
            sdk.display()
        )
    };
    // Refuse before making anything: the checkout's workspace would claim it.
    let absolute = std::path::absolute(dir)?;
    if let Ok(root) = repo(given_repo)
        && (absolute.starts_with(&root) || absolute.starts_with(BUILT_FROM))
    {
        bail!(
            "{} is inside the machine-trading checkout, whose workspace would claim it: make the bot somewhere else",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir.join("src")).with_context(|| dir.display().to_string())?;
    let dir = dir.canonicalize()?;
    let cargo = format!(
        r#"# A bot for the machine-trading bench. What the grader runs and allows:
# https://lgalabru.github.io/machine-trading/guides/bots/writing-a-bot.html
[package]
name = "bot"
edition = "2024"
version = "0.1.0"
publish = false

# A strategy component: `just build` builds it for wasm32-wasip2 into bot.wasm.
[lib]
crate-type = ["cdylib"]
path = "src/lib.rs"

[dependencies]
{kit}

# Its own workspace, wherever it's created.
[workspace]
"#
    );
    std::fs::write(dir.join("Cargo.toml"), cargo)?;
    std::fs::write(dir.join("Cargo.lock"), LOCKFILE)?;
    std::fs::write(dir.join("src/lib.rs"), STARTER_LIB)?;
    std::fs::write(dir.join("Justfile"), STARTER_JUSTFILE)?;
    std::fs::write(dir.join(".gitignore"), "/target\n/bot.wasm\n")?;
    Ok(format!(
        "made a bot in {}\n\n  rustup target add wasm32-wasip2   # once: the bot builds into a wasm component\n  cd {}\n  mtx bot run --debug               # build it, and watch it play one session, step by step\n  mtx bot practice                  # grade it on the practice scenarios\n\nThe strategy goes in src/lib.rs. A first strategy, step by step: https://lgalabru.github.io/machine-trading/guides/bots/first-strategy.html\nThe rules it runs under: https://lgalabru.github.io/machine-trading/guides/bots/writing-a-bot.html",
        dir.display(),
        dir.display()
    ))
}

/// What's wrong with a bot directory, and what's worth knowing: problems
/// stop `practice` and `pack`; notes don't.
pub struct Checked {
    pub problems: Vec<String>,
    pub notes: Vec<String>,
}

/// `mtx bot check`: the bot directory against the contract's shape. A bot
/// is a strategy component, `bot.wasm`; a directory without one is a
/// process bot (a `Justfile` whose `run` starts `bin/bot`), which only
/// practice runs.
pub fn check(dir: &Path) -> Checked {
    let mut problems = Vec::new();
    let mut notes = Vec::new();
    let wasm = dir.join("bot.wasm");
    if wasm.exists() {
        match std::fs::read(&wasm) {
            Err(e) => problems.push(format!("can't read bot.wasm: {e}")),
            // A component's preamble: the wasm magic, then layer 1. A core
            // module (version 1, layer 0) isn't one.
            Ok(bytes) if bytes.get(..8) == Some(b"\0asm\x0d\0\x01\0") => {}
            Ok(bytes) if bytes.starts_with(b"\0asm") => problems.push(
                "bot.wasm is a core wasm module, not a component: build it for wasm32-wasip2 (`just build`)".into(),
            ),
            Ok(_) => problems.push("bot.wasm isn't wasm".into()),
        }
        stale(&wasm, dir, "bot.wasm", &mut notes);
        return Checked { problems, notes };
    }
    if !["Justfile", "justfile", "run.sh"]
        .iter()
        .any(|name| dir.join(name).is_file())
    {
        problems.push("no bot.wasm: build the strategy first (`just build`)".into());
        return Checked { problems, notes };
    }
    notes.push(
        "a process bot (no bot.wasm): practice runs it, but the bench grades strategy components only".into(),
    );
    let bin = dir.join("bin/bot");
    match std::fs::read(&bin) {
        Err(_) => problems.push("no bin/bot: build the bot first (`just build`)".into()),
        Ok(bytes) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if std::fs::metadata(&bin).is_ok_and(|m| m.permissions().mode() & 0o111 == 0) {
                    problems.push("bin/bot isn't executable".into());
                }
            }
            let magic = bytes.get(..4).unwrap_or_default();
            let elf = magic == b"\x7fELF";
            let mach_o = matches!(
                magic,
                [0xcf, 0xfa, 0xed, 0xfe] | [0xfe, 0xed, 0xfa, 0xcf] | [0xca, 0xfe, 0xba, 0xbe]
            );
            if mach_o {
                notes.push(
                    "bin/bot is a macOS binary: it runs here, but the bench grades on Linux (Debian bookworm), from a Linux build"
                        .into(),
                );
            } else if !elf && !bytes.starts_with(b"#!") {
                notes.push("bin/bot is neither a native executable nor a script".into());
            }
            stale(&bin, dir, "bin/bot", &mut notes);
        }
    }
    Checked { problems, notes }
}

/// A note if `src/` changed since `built` was.
fn stale(built: &Path, dir: &Path, name: &str, notes: &mut Vec<String>) {
    if let (Ok(built), Some(newest)) = (
        std::fs::metadata(built).and_then(|m| m.modified()),
        newest_source(&dir.join("src")),
    ) && newest > built
    {
        notes.push(format!(
            "src/ changed since {name} was built: run `just build`"
        ));
    }
}

fn newest_source(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let time = if path.is_dir() {
            newest_source(&path)
        } else {
            entry.metadata().and_then(|m| m.modified()).ok()
        };
        newest = newest.max(time);
    }
    newest
}

pub struct Practice<'a> {
    pub dir: &'a Path,
    pub task: Option<&'a str>,
    pub scenarios: &'a [PathBuf],
    pub build: bool,
    pub jobs: usize,
    pub json: bool,
    pub repo: Option<&'a Path>,
}

/// `just build`, in the bot's directory.
pub(crate) fn build(dir: &Path) -> Result<()> {
    if !["Justfile", "justfile"]
        .iter()
        .any(|name| dir.join(name).is_file())
    {
        bail!(
            "no Justfile in {}: build the bot yourself and pass --no-build",
            dir.display()
        );
    }
    let status = Command::new("just")
        .arg("build")
        .current_dir(dir)
        .status()
        .context("running `just build` (is just installed? https://just.systems)")?;
    if !status.success() {
        bail!("`just build` failed");
    }
    Ok(())
}

/// `mtx bot practice`: build the bot, then grade it as the verifier grades
/// hidden scenarios, on the task's practice scenarios.
pub async fn practice(options: Practice<'_>) -> Result<String> {
    let dir = options
        .dir
        .canonicalize()
        .with_context(|| format!("no bot at {}", options.dir.display()))?;
    if options.build {
        build(&dir)?;
    }
    let checked = check(&dir);
    for note in &checked.notes {
        eprintln!("note: {note}");
    }
    if !checked.problems.is_empty() {
        bail!("the bot can't run:\n  {}", checked.problems.join("\n  "));
    }
    let root = repo(options.repo)?;
    let task = resolve_task(&root, options.task)?;
    let snapshot = task_snapshot(&root, &task)?;
    let files = if options.scenarios.is_empty() {
        practice_scenarios(&root, &task)?
    } else {
        options.scenarios.to_vec()
    };
    let scenarios = files
        .iter()
        .map(|path| Scenario::load(path).with_context(|| path.display().to_string()))
        .collect::<Result<Vec<_>>>()?;
    let logs = dir.join("target/mtx-practice-logs");
    std::fs::create_dir_all(&logs)?;
    eprintln!(
        "grading on {} {} scenario{}, one session at a time unless --jobs; bot logs in {}",
        scenarios.len(),
        task,
        if scenarios.len() == 1 { "" } else { "s" },
        logs.display()
    );
    let report = grade::grade(GradeOptions {
        snapshot_dir: &snapshot,
        bot_dir: &dir,
        scenarios: &scenarios,
        log_dir: &logs,
        jobs: options.jobs,
        first_bot_user: 0,
        reference_dir: None,
        anchor_jobs: 1,
        regimes: None,
        stop_on_disqualify: false,
        detail_dir: None,
        annotations: None,
        components_only: false,
        trusted: false,
        observer: None,
    })
    .await?;
    if options.json {
        return Ok(serde_json::to_string_pretty(&report)?);
    }
    Ok(summary(&report, &logs))
}

fn usd(v: f64) -> String {
    let sign = if v < 0.0 {
        "−"
    } else if v > 0.0 {
        "+"
    } else {
        ""
    };
    format!("{sign}${:.2}", v.abs())
}

/// The report a developer reads: one line a session, then the grade.
fn summary(report: &GradeReport, logs: &Path) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<24} {:>12} {:>12} {:>12}  ",
        "scenario", "your PnL", "reference", "bound"
    );
    for s in &report.scenarios {
        let reference = s.reference_pnl_usd.map(usd).unwrap_or_else(|| "–".into());
        let _ = writeln!(
            out,
            "{:<24} {:>12} {:>12} {:>12}  {}",
            s.session.scenario,
            usd(s.session.pnl_usd),
            reference,
            format!("${:.0}", s.bound_pnl_usd),
            s.disqualified
                .as_deref()
                .map(|why| format!("DISQUALIFIED: {why}"))
                .unwrap_or_default(),
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "reward {:.3}   (your PnL {} over the hindsight bound ${:.0})",
        report.reward,
        usd(report.pnl_usd),
        report.bound_pnl_usd
    );
    if let Some(skill) = report.skill {
        let _ = writeln!(
            out,
            "skill  {skill:.2}    (your PnL over the reference bot's: 1 matches it)"
        );
    }
    let risk = &report.risk;
    let _ = writeln!(
        out,
        "risk   worst session {}, deepest drawdown ${:.0}, profitable in {:.0}% of sessions",
        usd(risk.worst_pnl_usd),
        risk.max_drawdown_usd,
        risk.profitable_share * 100.0
    );
    let disqualified = report
        .scenarios
        .iter()
        .filter(|s| s.disqualified.is_some())
        .count();
    if disqualified > 0 {
        let _ = writeln!(
            out,
            "\n{disqualified} session{} broke the rules: a graded run would score 0. Logs: {}",
            if disqualified == 1 { "" } else { "s" },
            logs.display()
        );
    }
    out
}

/// `mtx bot pack`: the bot's source, without builds or logs, as one archive,
/// with a manifest saying which task it's for and which mtx it was made with.
pub fn pack(
    dir: &Path,
    task: Option<&str>,
    given_repo: Option<&Path>,
    out: &Path,
) -> Result<String> {
    let task = resolve_task(&repo(given_repo)?, task)?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("no bot at {}", dir.display()))?;
    for file in ["Cargo.toml", "Justfile"] {
        if !dir.join(file).is_file() {
            bail!(
                "no {file} in {}: a packed bot is its source, built by `just build`",
                dir.display()
            );
        }
    }
    let staging = std::env::temp_dir().join(format!("mtx-pack-{}", uuid::Uuid::new_v4()));
    let root = staging.join("bot");
    copy_source(&dir, &root)?;
    std::fs::write(
        root.join("mtx-bot.toml"),
        format!(
            "# Written by `mtx bot pack`.\ntask = \"{task}\"\nmtx_version = \"{}\"\nmtx_commit = \"{}\"\n",
            env!("CARGO_PKG_VERSION"),
            BUILT_COMMIT.unwrap_or("unknown")
        ),
    )?;
    let out = std::path::absolute(out)?;
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&out)
        .arg("-C")
        .arg(&staging)
        .arg("bot")
        .status()
        .context("tar")?;
    std::fs::remove_dir_all(&staging).ok();
    if !status.success() {
        bail!("tar failed");
    }
    let size = std::fs::metadata(&out)?.len();
    let mut report = format!(
        "packed {} into {} ({:.1} KB)",
        dir.display(),
        out.display(),
        size as f64 / 1024.0
    );
    // A path to this machine's checkout won't build anywhere else.
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
    if manifest
        .lines()
        .any(|line| line.trim_start().starts_with("mtx-kit") && line.contains("path ="))
    {
        report.push_str(
            "\nnote: Cargo.toml depends on mtx-kit by a local path, so this archive builds only on this machine: depend on it by git (`mtx bot new --git` writes one) to build it elsewhere",
        );
    }
    Ok(report)
}

/// Copy a bot's source: everything but builds, logs and version control.
fn copy_source(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("target" | "bin" | "bot.wasm" | ".git" | "logs")
        ) {
            continue;
        }
        let path = entry.path();
        let dest = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_source(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest).with_context(|| path.display().to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_task_names_its_snapshot_and_practice_set() {
        let root = repo(None).unwrap();
        let names = tasks(&root);
        assert!(names.len() >= 2, "{names:?}");
        for task in &names {
            assert!(
                task_snapshot(&root, task).unwrap().is_dir(),
                "{task}: no snapshot"
            );
            assert!(
                !practice_scenarios(&root, task).unwrap().is_empty(),
                "{task}: no practice"
            );
        }
        assert!(resolve_task(&root, None).unwrap().contains("flash-crash"));
        assert!(resolve_task(&root, Some("no-such-task")).is_err());
    }

    #[test]
    fn new_makes_a_bot_outside_the_checkout_only() {
        let dir = std::env::temp_dir().join(format!("mtx-bot-new-{}", uuid::Uuid::new_v4()));
        new(&dir, false, None).unwrap();
        for file in [
            "Cargo.toml",
            "Cargo.lock",
            "Justfile",
            "src/lib.rs",
            ".gitignore",
        ] {
            assert!(dir.join(file).is_file(), "no {file}");
        }
        std::fs::remove_dir_all(&dir).unwrap();

        let inside = repo(None).unwrap().join("bots/mtx-bot-new-test");
        assert!(new(&inside, false, None).is_err());
        assert!(!inside.exists(), "refused, but made {}", inside.display());
    }
}
