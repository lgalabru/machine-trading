//! Shared Harbor launcher: one development attempt, then isolated evaluation sessions.
use std::{path::PathBuf, process::Command};

use anyhow::{Context, Result, ensure};
use clap::Args;
#[derive(Args)]
pub struct Evaluate {
    /// Task name under bench/tasks, or a task directory.
    #[arg(long)]
    pub task: PathBuf,
    /// Override the frozen release selected by the atomic-arb task.
    #[arg(long)]
    pub release: Option<PathBuf>,
    #[arg(long, default_value = "codex")]
    pub agent: String,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, default_value = "high")]
    pub effort: String,
    #[arg(long)]
    pub job_name: String,
    #[arg(long, default_value = "jobs")]
    pub jobs_dir: PathBuf,
    /// Validate inputs and print the command without running the agent.
    #[arg(long)]
    pub dry_run: bool,
    /// Follow live progress of an existing scenario-set job. Ctrl-C disconnects.
    #[arg(long, conflicts_with_all = ["resume", "dry_run"])]
    pub follow: bool,
    /// Reuse completed sessions from this same scenario-set job.
    #[arg(long)]
    pub resume: bool,
    /// Explicitly retry only an infrastructure-failed session with no score.
    #[arg(long, requires = "resume")]
    pub retry_infrastructure: bool,
}
#[derive(serde::Deserialize)]
struct AtomicArbTask {
    metadata: FrozenScenarioSet,
}

#[derive(serde::Deserialize)]
struct FrozenScenarioSet {
    default_release: PathBuf,
    fixture_set_hash: String,
}

pub fn run(args: Evaluate) -> Result<String> {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let task_dir = if args.task.is_dir() {
        args.task.clone()
    } else {
        repo.join("bench/tasks").join(&args.task)
    };
    if args.follow {
        ensure!(
            task_dir.file_name().is_some_and(|n| n == "atomic-arb"),
            "--follow requires atomic-arb"
        );
        let status = Command::new("python3")
            .arg(repo.join("bench/backrun/live_evaluate.py"))
            .arg("--follow")
            .arg("--jobs-dir")
            .arg(&args.jobs_dir)
            .arg("--job-name")
            .arg(&args.job_name)
            .status()?;
        ensure!(status.success(), "could not follow evaluation job");
        return Ok(String::new());
    }
    let mut release = args.release;
    let mut expected_set = None;
    if release.is_none() && task_dir.file_name().is_some_and(|n| n == "atomic-arb") {
        let path = task_dir.join("task.toml");
        let task: AtomicArbTask = toml::from_str(
            &std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
        )
        .context("read atomic-arb default release and fixture set from task metadata")?;
        release = Some(repo.join(task.metadata.default_release));
        expected_set = Some(task.metadata.fixture_set_hash);
    }
    let task = if let Some(release) = &release {
        ensure!(
            !release.join("FREEZE_FAILED").exists(),
            "release failed acceptance"
        );
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(release.join("release.json"))
                .with_context(|| format!("cannot read frozen release at {}; provide a validated local release with --release", release.display()))?)?;
        if let Some(expected) = expected_set {
            ensure!(
                metadata["kind"] == "private-scenario-set"
                    && metadata["fixture_set_hash"].as_str() == Some(&expected),
                "default release differs from the scenario set pinned in atomic-arb/task.toml"
            );
        }
        if metadata["kind"] == "private-scenario-set" {
            ensure!(
                args.task.file_name().is_some_and(|n| n == "atomic-arb"),
                "release belongs to atomic-arb"
            );
            let mut command = Command::new("python3");
            command
                .arg(repo.join("bench/backrun/live_evaluate.py"))
                .arg("--release")
                .arg(release)
                .arg("--jobs-dir")
                .arg(&args.jobs_dir)
                .args([
                    "--job-name",
                    &args.job_name,
                    "--agent",
                    &args.agent,
                    "--effort",
                    &args.effort,
                ]);
            if let Some(model) = &args.model {
                command.args(["--model", model]);
            }
            if args.dry_run {
                command.arg("--dry-run");
            }
            if args.resume {
                command.arg("--resume");
            }
            if args.retry_infrastructure {
                command.arg("--retry-infrastructure");
            }
            ensure!(
                command.status()?.success(),
                "scenario-set evaluation failed; inspect the retained report and logs"
            );
            return Ok(serde_json::json!({"report":args.jobs_dir.join(&args.job_name).join("report.json"),"aggregation":null,"dry_run":args.dry_run}).to_string());
        }
        ensure!(
            !args.resume && !args.retry_infrastructure,
            "recovery options require a scenario-set release"
        );
        ensure!(
            metadata["protocol"] == "continuous-arrival-v1",
            "use a validated continuous-arrival release"
        );
        ensure!(
            args.task.file_name().is_some_and(|n| n == "atomic-arb"),
            "release belongs to atomic-arb"
        );
        for image in metadata["images"]
            .as_object()
            .context("missing pinned images")?
            .values()
        {
            ensure!(
                Command::new("docker")
                    .args(["image", "inspect", image.as_str().context("invalid image")?])
                    .stdout(std::process::Stdio::null())
                    .status()?
                    .success(),
                "missing frozen image"
            );
        }
        release.join("task")
    } else {
        task_dir
    };
    ensure!(task.join("task.toml").is_file(), "task.toml missing");
    ensure!(
        !args.jobs_dir.join(&args.job_name).exists(),
        "job already exists; retain previous results and use a new name"
    );
    let mut command = Command::new("harbor");
    command
        .args(["run", "--path"])
        .arg(task.canonicalize()?)
        .args([
            "--agent",
            &args.agent,
            "--effort",
            &args.effort,
            "--n-concurrent",
            "1",
            "--n-attempts",
            "1",
            "--max-retries",
            "0",
            "--jobs-dir",
        ])
        .arg(&args.jobs_dir)
        .args(["--job-name", &args.job_name]);
    if let Some(model) = args.model {
        command.args(["--model", &model]);
    }
    if args.agent == "codex" {
        let auth = std::env::var_os("CODEX_AUTH_JSON_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".codex/auth.json")
            });
        ensure!(
            auth.is_file(),
            "Codex auth missing: run codex login or set CODEX_AUTH_JSON_PATH"
        );
        command.env("CODEX_AUTH_JSON_PATH", auth).args([
            "--agent-kwarg",
            "version=0.166.0",
            "--agent-kwarg",
            "config={}",
        ]);
    }
    if args.dry_run {
        return Ok(format!("{command:?}"));
    }
    ensure!(
        command.status()?.success(),
        "Harbor evaluation failed; inspect the retained job logs"
    );
    Ok(serde_json::json!({"job":args.jobs_dir.join(args.job_name)}).to_string())
}
