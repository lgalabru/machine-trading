//! `mtx bench review`: browse a bench's results in a small web UI.
//!
//! Point it at a batch (`jobs/bench/<batch>`), a Harbor job inside one, or a
//! single trial. It finds every trial below the path and serves a page with
//! a leaderboard, each trial's sessions against the reference's, each
//! session step by step (from the verifier's `sessions/` detail files), the
//! agent's trajectory and the bot it wrote.
//!
//! It also serves every release published under `bench/results/<version>`
//! (`mtx bench publish`), so the page can switch to one, or compare them.
//!
//! The server only reads, and only inside the trials it found: never a
//! batch's `.seed`, nor anything else whose name starts with a dot.

use std::{
    collections::HashMap,
    io::Read,
    net::SocketAddr,
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use tracing::info;

use crate::ui;

const PAGE: &str = include_str!("review.html");
/// How deep below the given path trials are looked for: batch, job, trial.
const MAX_DEPTH: usize = 4;
/// The largest file the page may open.
const MAX_FILE_BYTES: u64 = 4 << 20;
/// How much of a trajectory step's message, tool input and tool output the
/// page gets.
const MAX_MESSAGE_CHARS: usize = 6_000;
const MAX_TOOL_CHARS: usize = 3_000;
/// How much of a user message (the task's instruction) the page gets.
const MAX_USER_CHARS: usize = 40_000;
/// Directories inside a trial the page may read files from, and single
/// files it may read.
pub(crate) const FILE_DIRS: &[&str] = &["artifacts/app/bot/", "verifier/bot/"];
pub(crate) const FILES: &[&str] = &[
    "verifier/test-stdout.txt",
    "agent/claude-code.txt",
    "exception.txt",
    "trial.log",
];
/// What a bot's source listing leaves out: build output.
pub(crate) const SKIPPED_DIRS: &[&str] = &["target", "bin", "node_modules", ".git"];

pub struct ReviewOptions {
    pub port: u16,
    pub open: bool,
    /// Where the page opens, after the `#`.
    pub start: Option<String>,
}

/// A directory of trials the page can show: the one asked for, or a
/// published release.
#[derive(Clone, Debug)]
pub struct Root {
    /// The release's version, or the directory's name.
    pub label: String,
    pub path: PathBuf,
    /// Under `bench/results`.
    pub published: bool,
}

struct AppState {
    /// The first is the one asked for.
    roots: Vec<Root>,
    /// Every trial found so far, with its root. A trial keeps its index for
    /// the server's life: new ones (a bench still running) are appended.
    trials: std::sync::Mutex<Vec<(usize, PathBuf)>>,
    /// Task metadata by name and digest, read once.
    tasks: std::sync::Mutex<HashMap<(String, String), Value>>,
}

impl AppState {
    fn trial(&self, n: usize) -> Result<PathBuf, ApiError> {
        self.trials
            .lock()
            .expect("review lock")
            .get(n)
            .map(|(_, dir)| dir.clone())
            .ok_or(ApiError::NotFound)
    }

    /// The root a trial was found under.
    fn root_of(&self, n: usize) -> &Root {
        let i = self
            .trials
            .lock()
            .expect("review lock")
            .get(n)
            .map(|(i, _)| *i)
            .unwrap_or(0);
        &self.roots[i]
    }

    /// Look for trials again, keeping the ones known at their indices.
    fn refresh(&self) -> Vec<(usize, PathBuf)> {
        let mut trials = self.trials.lock().expect("review lock");
        for (i, root) in self.roots.iter().enumerate() {
            for dir in discover(&root.path) {
                if !trials.iter().any(|(_, known)| known == &dir) {
                    trials.push((i, dir));
                }
            }
        }
        trials.clone()
    }

    /// A task's `[task]` table (name, version, description) as its
    /// `task.toml` has it: from Harbor's cache by digest, the trial's local
    /// task path, or this checkout's tasks.
    fn task_meta(&self, root: &FsPath, name: &str, digest: &str, local: Option<&str>) -> Value {
        let key = (name.to_string(), digest.to_string());
        if let Some(meta) = self.tasks.lock().expect("review lock").get(&key) {
            return meta.clone();
        }
        let meta = task_toml(root, name, digest, local)
            .map(|toml| task_table(&toml))
            .unwrap_or_else(|| json!({}));
        self.tasks
            .lock()
            .expect("review lock")
            .insert(key, meta.clone());
        meta
    }
}

/// A task's `task.toml`: a published release's copy, Harbor's cache by
/// digest, the trial's local task path, or this checkout's tasks.
pub(crate) fn task_toml(
    root: &FsPath,
    name: &str,
    digest: &str,
    local: Option<&str>,
) -> Option<String> {
    {
        let short = name.rsplit('/').next().unwrap_or(name);
        let mut candidates = vec![root.join("tasks").join(short).join("task.toml")];
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(
                PathBuf::from(home)
                    .join(".cache/harbor/tasks/packages")
                    .join(name)
                    .join(digest.trim_start_matches("sha256:"))
                    .join("task.toml"),
            );
        }
        if let Some(local) = local {
            candidates.push(PathBuf::from(local).join("task.toml"));
        }
        candidates.push(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../bench/tasks")
                .join(short)
                .join("task.toml"),
        );
        candidates
            .iter()
            .find_map(|path| std::fs::read_to_string(path).ok())
    }
}

/// `version` and `description` from a `task.toml`'s `[task]` table: enough
/// for a header, without a TOML parser.
fn task_table(toml: &str) -> Value {
    let mut in_task = false;
    let mut meta = serde_json::Map::new();
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_task = line == "[task]";
            continue;
        }
        if !in_task {
            continue;
        }
        for key in ["name", "version", "description"] {
            if let Some(rest) = line.strip_prefix(key)
                && let Some(value) = rest.trim_start().strip_prefix('=')
                && let Ok(text) = serde_json::from_str::<String>(value.trim())
            {
                meta.insert(key.to_string(), json!(text));
            }
        }
    }
    Value::Object(meta)
}

enum ApiError {
    NotFound,
    Bad(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            Self::Bad(message) => (StatusCode::BAD_REQUEST, message),
            Self::Internal(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

pub async fn review(roots: Vec<Root>, opts: ReviewOptions) -> Result<Value> {
    let primary = roots.first().context("nothing to review")?;
    if discover(&primary.path).is_empty() {
        bail!(
            "no Harbor trials under {} (a trial is a directory whose config.json names it)",
            primary.path.display()
        );
    }
    let state = Arc::new(AppState {
        roots,
        trials: std::sync::Mutex::default(),
        tasks: std::sync::Mutex::default(),
    });
    let count = state.refresh().iter().filter(|(i, _)| *i == 0).count();
    let app = Router::new()
        .route(
            "/",
            // A new build serves a new page: never let a browser keep the old one.
            get(|| async { ([(header::CACHE_CONTROL, "no-store")], Html(PAGE)) }),
        )
        .route("/api/index", get(get_index))
        .route("/api/index/{release}", get(get_release_index))
        .route("/api/releases", get(get_releases))
        .route("/api/trial/{n}", get(get_trial))
        .route("/api/session/{n}/{role}/{name}", get(get_session))
        .route("/api/trajectory/{n}", get(get_trajectory))
        .route("/api/curves/{n}", get(get_curves))
        .route("/api/file/{n}/{*path}", get(get_file))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], opts.port)))
        .await
        .context("binding the review server")?;
    let url = format!("http://{}", listener.local_addr()?);
    let page = match &opts.start {
        Some(start) => format!("{url}/#/{start}"),
        None => url.clone(),
    };
    info!(%url, trials = count, "review ready; Ctrl-C to stop");
    ui::done(format!(
        "Reviewing {count} trial{} at {}",
        if count == 1 { "" } else { "s" },
        ui::accent(&page)
    ));
    let releases = state.roots.iter().filter(|r| r.published).count();
    if releases > 0 {
        ui::note(format!(
            "{releases} published release{} to switch to or compare",
            if releases == 1 { "" } else { "s" }
        ));
    }
    if opts.open {
        crate::play::open_browser(&page);
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    ui::clear();
    Ok(json!({ "root": state.roots[0].path, "trials": count, "url": page }))
}

/// Trials below `root`, in path order: directories whose `config.json` (or,
/// once done, `result.json`) names a trial. A trial still running has no
/// `result.json` yet.
pub(crate) fn discover(root: &FsPath) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, 0, &mut found);
    found.sort();
    found
}

fn walk(dir: &FsPath, depth: usize, found: &mut Vec<PathBuf>) {
    let names_a_trial =
        |file: &str| read_json(&dir.join(file)).is_some_and(|r| r.get("trial_name").is_some());
    if names_a_trial("result.json") || names_a_trial("config.json") {
        found.push(dir.to_path_buf());
        return;
    }
    if depth >= MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let hidden = name.to_string_lossy().starts_with('.');
        if !hidden && entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&entry.path(), depth + 1, found);
        }
    }
}

pub(crate) fn read_json(path: &FsPath) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A trial at a glance: what the leaderboard and the trial's header show.
fn summary(root: &Root, n: usize, dir: &FsPath) -> Value {
    let result = read_json(&dir.join("result.json")).unwrap_or(Value::Null);
    let config = read_json(&dir.join("config.json")).unwrap_or(Value::Null);
    let report = read_json(&dir.join("verifier/report.json"));
    let rewards = read_json(&dir.join("verifier/reward.json")).unwrap_or(Value::Null);
    let relative = |path: &FsPath| {
        path.strip_prefix(&root.path)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let scenarios = report
        .as_ref()
        .and_then(|r| r["scenarios"].as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    // Before 0.9.0 reports carried no skill: the reference ratio is the same
    // number, zeroed for a disqualified bot.
    let disqualified = scenarios
        .iter()
        .filter(|s| !s["disqualified"].is_null())
        .count();
    let pick = |key: &str| {
        report
            .as_ref()
            .map(|r| r[key].clone())
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| rewards[key].clone())
    };
    let mut skill = pick("skill");
    if skill.is_null() {
        skill = pick("reference_ratio");
        if disqualified > 0 && skill.is_number() {
            skill = json!(0.0);
        }
    }
    let agent = &result["agent_result"];
    let graded = report.is_some() || rewards.is_object();
    let finished = !result["finished_at"].is_null();
    // success: graded (a timed-out agent's bot is still graded); error:
    // finished without a grade; pending: still running.
    let status = match (finished, graded) {
        (_, true) => "success",
        (true, false) => "error",
        (false, false) => "pending",
    };
    // A running trial has only its config.
    let task_config = if result["config"]["task"].is_object() {
        &result["config"]["task"]
    } else {
        &config["task"]
    };
    let task = task_config["name"].as_str().unwrap_or_default().to_string();
    let model = result["agent_info"]["model_info"]["name"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            config["agent"]["model_name"]
                .as_str()
                .map(|m| m.trim_start_matches("anthropic/").to_string())
        });
    let pnls: serde_json::Map<String, Value> = scenarios
        .iter()
        .filter_map(|s| Some((s["scenario"].as_str()?.to_string(), s["pnl_usd"].clone())))
        .collect();
    let reference_pnls: serde_json::Map<String, Value> = scenarios
        .iter()
        .filter_map(|s| {
            Some((
                s["scenario"].as_str()?.to_string(),
                s["reference_pnl_usd"].clone(),
            ))
        })
        .collect();
    json!({
        "id": n,
        "release": root.label,
        "path": relative(dir),
        "job": dir.parent().map(relative).unwrap_or_default(),
        "task": task,
        "task_ref": task_config["ref"],
        "task_path": task_config["path"],
        "trial": config["trial_name"].as_str().or(result["trial_name"].as_str()),
        "status": status,
        "agent": result["agent_info"]["name"],
        "agent_version": result["agent_info"]["version"],
        "model": model,
        "started_at": result["started_at"],
        "finished_at": result["finished_at"],
        "agent_execution": result["agent_execution"],
        "verifier_execution": result["verifier"],
        "exception": result["exception_info"].as_object().map(|e| json!({
            "type": e.get("exception_type"),
            "message": e.get("exception_message"),
        })),
        "cost_usd": agent["cost_usd"],
        "tokens": {
            "input": agent["n_input_tokens"],
            "cache": agent["n_cache_tokens"],
            "output": agent["n_output_tokens"],
        },
        "graded": graded,
        "pnls": pnls,
        "reference_pnls": reference_pnls,
        "reward": pick("reward"),
        "skill": skill,
        "pnl_usd": pick("pnl_usd"),
        "reference_pnl_usd": pick("reference_pnl_usd"),
        "bound_pnl_usd": pick("bound_pnl_usd"),
        "tampered": report.as_ref().map(|r| r["tampered"].clone()),
        "eval_seed_sha256": report.as_ref().map(|r| r["eval_seed_sha256"].clone()),
        "risk": report.as_ref().map(|r| r["risk"].clone()),
        "sessions": scenarios.len(),
        "disqualified": disqualified,
        "details": dir.join("verifier/sessions/bot").is_dir(),
    })
}

async fn get_index(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    index_of(&state, None)
}

async fn get_release_index(
    State(state): State<Arc<AppState>>,
    Path(release): Path<String>,
) -> Result<Json<Value>, ApiError> {
    index_of(&state, Some(release))
}

/// The page's index for one root: the one asked for, or a release.
fn index_of(state: &AppState, release: Option<String>) -> Result<Json<Value>, ApiError> {
    let at = match &release {
        None => 0,
        Some(label) => state
            .roots
            .iter()
            .position(|r| &r.label == label)
            .ok_or(ApiError::NotFound)?,
    };
    let root = &state.roots[at];
    let all = state.refresh();
    let picked: Vec<(usize, &PathBuf)> = all
        .iter()
        .enumerate()
        .filter(|(_, (i, _))| *i == at)
        .map(|(n, (_, dir))| (n, dir))
        .collect();
    let dirs: Vec<PathBuf> = picked.iter().map(|(_, dir)| (*dir).clone()).collect();
    let trials: Vec<Value> = picked
        .iter()
        .map(|(n, dir)| summary(root, *n, dir))
        .collect();
    // Jobs: the Harbor runs the trials came from, with their configs.
    let mut jobs: Vec<Value> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for dir in &dirs {
        let Some(job) = dir.parent() else { continue };
        if !seen.insert(job.to_path_buf()) {
            continue;
        }
        let result = read_json(&job.join("result.json")).unwrap_or(Value::Null);
        let config = read_json(&job.join("config.json")).unwrap_or(Value::Null);
        jobs.push(json!({
            "path": job.strip_prefix(&root.path).unwrap_or(job).display().to_string(),
            "started_at": result["started_at"],
            "finished_at": result["finished_at"],
            "n_total_trials": result["n_total_trials"],
            "agents": config["agents"],
            "datasets": config["datasets"],
            "n_attempts": config["n_attempts"],
            "n_concurrent_trials": config["orchestrator"]["n_concurrent_trials"],
            "timeout_multiplier": config["agent_timeout_multiplier"],
        }));
    }
    // The batch's plan, if `just bench` wrote one: which models, how many
    // attempts each, so trials not started yet show as queued.
    // A batch is the root or the directory above a job.
    let mut batches: Vec<Value> = Vec::new();
    let mut batch_dirs: Vec<PathBuf> = vec![root.path.clone()];
    batch_dirs.extend(
        dirs.iter()
            .filter_map(|d| Some(d.parent()?.parent()?.to_path_buf())),
    );
    batch_dirs.sort();
    batch_dirs.dedup();
    for dir in &batch_dirs {
        if let Some(plan) = read_json(&dir.join("batch.json")) {
            batches.push(plan);
        }
    }
    let batch = batches.first().cloned();
    // Tasks: every task a trial ran, with its metadata and how its trials
    // stand.
    let mut tasks: Vec<Value> = Vec::new();
    let mut names: Vec<String> = trials
        .iter()
        .filter_map(|t| t["task"].as_str().map(str::to_string))
        .filter(|t| !t.is_empty())
        .collect();
    names.sort();
    names.dedup();
    for name in &names {
        let mine: Vec<&Value> = trials.iter().filter(|t| t["task"] == json!(name)).collect();
        let digest = mine
            .iter()
            .find_map(|t| t["task_ref"].as_str())
            .unwrap_or_default();
        let local = mine.iter().find_map(|t| t["task_path"].as_str());
        let meta = state.task_meta(&root.path, name, digest, local);
        let count = |status: &str| mine.iter().filter(|t| t["status"] == json!(status)).count();
        // Planned: the batch's models times its attempts; otherwise each
        // job's agents times its attempts.
        let planned: usize = match batches.is_empty() {
            false => batches
                .iter()
                .map(|batch| {
                    batch["models"].as_array().map(Vec::len).unwrap_or(0)
                        * batch["attempts"].as_u64().unwrap_or(0) as usize
                })
                .sum(),
            true => jobs
                .iter()
                .map(|j| {
                    j["agents"].as_array().map(Vec::len).unwrap_or(1)
                        * j["n_attempts"].as_u64().unwrap_or(0) as usize
                })
                .sum(),
        };
        tasks.push(json!({
            "name": name,
            "short": name.rsplit('/').next(),
            "version": meta["version"],
            "description": meta["description"],
            "ref": digest,
            "trials": mine.iter().map(|t| t["id"].clone()).collect::<Vec<_>>(),
            "counts": {
                "success": count("success"),
                "error": count("error"),
                "pending": count("pending"),
                "queued": planned.saturating_sub(mine.len()),
            },
        }));
    }
    let version = batch
        .as_ref()
        .and_then(|b| {
            b["version"]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            tasks
                .iter()
                .find_map(|t| t["version"].as_str().map(str::to_string))
        });
    let dataset = jobs
        .iter()
        .find_map(|j| j["datasets"][0]["name"].as_str().map(str::to_string));
    let releases: Vec<Value> = state
        .roots
        .iter()
        .enumerate()
        .map(|(i, r)| json!({ "label": r.label, "published": r.published, "primary": i == 0 }))
        .collect();
    Ok(Json(json!({
        "root": root.path,
        "release": root.label,
        "published": root.published,
        "releases": releases,
        "name": if root.published { Some(root.label.clone()) } else { root.path.file_name().map(|n| n.to_string_lossy().to_string()) },
        "dataset": dataset,
        "version": version,
        "batch": batch,
        "tasks": tasks,
        "jobs": jobs,
        "trials": trials,
    })))
}

/// Every root's scores, for switching and comparing: per task, each model's
/// mean skill over its graded trials.
async fn get_releases(State(state): State<Arc<AppState>>) -> Json<Value> {
    let all = state.refresh();
    let releases: Vec<Value> = state
        .roots
        .iter()
        .enumerate()
        .map(|(i, root)| {
            let trials: Vec<Value> = all
                .iter()
                .enumerate()
                .filter(|(_, (r, _))| *r == i)
                .map(|(n, (_, dir))| summary(root, n, dir))
                .collect();
            let mut out = scores(root, &trials);
            out["primary"] = json!(i == 0);
            out
        })
        .collect();
    Json(json!({ "releases": releases }))
}

/// A root's scores from its trials' summaries: per task (by short name),
/// per model, the mean skill, reward and PnL of its graded trials.
pub(crate) fn scores_of(root: &Root) -> Value {
    let trials: Vec<Value> = discover(&root.path)
        .iter()
        .enumerate()
        .map(|(n, dir)| summary(root, n, dir))
        .collect();
    scores(root, &trials)
}

fn scores(root: &Root, trials: &[Value]) -> Value {
    let mean = |xs: Vec<f64>| (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64);
    let mut tasks = serde_json::Map::new();
    let mut names: Vec<&str> = trials.iter().filter_map(|t| t["task"].as_str()).collect();
    names.sort();
    names.dedup();
    for name in names {
        let mine: Vec<&Value> = trials.iter().filter(|t| t["task"] == json!(name)).collect();
        let mut models: Vec<&str> = mine.iter().filter_map(|t| t["model"].as_str()).collect();
        models.sort();
        models.dedup();
        let per_model: serde_json::Map<String, Value> = models
            .iter()
            .map(|model| {
                let ts: Vec<&&Value> = mine.iter().filter(|t| t["model"] == json!(model)).collect();
                let graded: Vec<&&Value> = ts
                    .iter()
                    .copied()
                    .filter(|t| t["graded"] == json!(true))
                    .collect();
                let pick =
                    |key: &str| mean(graded.iter().filter_map(|t| t[key].as_f64()).collect());
                (
                    model.to_string(),
                    json!({
                        "trials": ts.len(),
                        "graded": graded.len(),
                        "skill": pick("skill"),
                        "skills": graded.iter().map(|t| t["skill"].clone()).collect::<Vec<_>>(),
                        "reward": pick("reward"),
                        "pnl_usd": pick("pnl_usd"),
                        "sessions": graded.iter().filter_map(|t| t["sessions"].as_u64()).max(),
                    }),
                )
            })
            .collect();
        let digest = mine
            .iter()
            .find_map(|t| t["task_ref"].as_str())
            .unwrap_or_default();
        let local = mine.iter().find_map(|t| t["task_path"].as_str());
        let version = task_toml(&root.path, name, digest, local)
            .map(|toml| task_table(&toml)["version"].clone())
            .unwrap_or(Value::Null);
        tasks.insert(
            name.rsplit('/').next().unwrap_or(name).to_string(),
            json!({ "name": name, "version": version, "models": per_model }),
        );
    }
    json!({
        "label": root.label,
        "published": root.published,
        "trials": trials.len(),
        "tasks": tasks,
    })
}

async fn get_trial(
    State(state): State<Arc<AppState>>,
    Path(n): Path<usize>,
) -> Result<Json<Value>, ApiError> {
    let dir = state.trial(n)?;
    let sessions = |role: &str| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.join("verifier/sessions").join(role))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.strip_suffix(".json.gz")
                    .or_else(|| name.strip_suffix(".json"))
                    .map(str::to_string)
            })
            .collect();
        names.sort_by_key(|name| natural_key(name));
        names
    };
    let mut files = Vec::new();
    list_files(&dir, &dir.join("artifacts/app/bot"), &mut files);
    let logs: Vec<String> = FILES
        .iter()
        .filter(|f| dir.join(f).is_file())
        .map(|f| f.to_string())
        .collect();
    let result = read_json(&dir.join("result.json")).unwrap_or(Value::Null);
    let bot_sessions = sessions("bot");
    let root = state.root_of(n);
    // Per session, what the report doesn't carry: what the scenario was drawn
    // as, and how the bot's transactions went.
    let session_stats: serde_json::Map<String, Value> = bot_sessions
        .iter()
        .filter_map(|name| {
            let detail = read_session(&dir, "bot", name).ok()?;
            let detail: Value = serde_json::from_slice(&detail).ok()?;
            let txs = detail["timeline"]["transactions"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0);
            let count = |pick: &dyn Fn(&Value) -> bool| {
                detail["timeline"]["transactions"]
                    .as_array()
                    .map(|t| t.iter().filter(|x| pick(x)).count())
                    .unwrap_or(0)
            };
            let duplicates = count(&|x| x["duplicate"] == json!(true));
            let failed = count(&|x| x["ok"] != json!(true)) - duplicates;
            Some((
                name.clone(),
                json!({
                    "annotation": detail["annotation"],
                    "description": detail["scenario"]["description"],
                    "txs": txs,
                    "failed": failed,
                    "duplicates": duplicates,
                }),
            ))
        })
        .collect();
    Ok(Json(json!({
        "summary": summary(root, n, &dir),
        "session_stats": session_stats,
        "report": read_json(&dir.join("verifier/report.json")),
        "rewards": read_json(&dir.join("verifier/reward.json")),
        "sessions": { "bot": bot_sessions, "reference": sessions("reference") },
        "files": files,
        "logs": logs,
        "trajectory": dir.join("agent/trajectory.json").is_file()
            || dir.join("agent/trajectory.json.gz").is_file(),
        "exception": result["exception_info"],
        "config": result["config"],
        "timings": {
            "environment_setup": result["environment_setup"],
            "agent_setup": result["agent_setup"],
            "agent_execution": result["agent_execution"],
            "verifier": result["verifier"],
        },
    })))
}

/// `hidden-10` after `hidden-9`.
fn natural_key(name: &str) -> (String, u64) {
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (head, tail) = name.split_at(name.len() - digits);
    (head.to_string(), tail.parse().unwrap_or(0))
}

/// Source files under `dir`, relative to the trial, build output left out.
fn list_files(trial: &FsPath, dir: &FsPath, out: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if name.starts_with('.') || kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        if kind.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_str()) {
                list_files(trial, &path, out);
            }
        } else if let Ok(meta) = entry.metadata() {
            out.push(json!({
                "path": path.strip_prefix(trial).unwrap_or(&path).display().to_string(),
                "bytes": meta.len(),
            }));
        }
    }
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.starts_with('.')
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path((n, role, name)): Path<(usize, String, String)>,
) -> Result<Response, ApiError> {
    let dir = state.trial(n)?;
    if !["bot", "reference"].contains(&role.as_str()) || !safe_name(&name) {
        return Err(ApiError::Bad("no such session".into()));
    }
    let bytes = read_session(&dir, &role, &name)?;
    Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response())
}

/// A session's detail file, gzipped or not, as JSON bytes.
fn read_session(dir: &FsPath, role: &str, name: &str) -> Result<Vec<u8>, ApiError> {
    let base = dir.join("verifier/sessions").join(role);
    let gz = base.join(format!("{name}.json.gz"));
    if gz.is_file() {
        let file =
            std::fs::File::open(&gz).map_err(|e| anyhow::anyhow!("{}: {e}", gz.display()))?;
        let mut json = Vec::new();
        flate2::read::GzDecoder::new(file)
            .read_to_end(&mut json)
            .map_err(|e| anyhow::anyhow!("{}: {e}", gz.display()))?;
        Ok(json)
    } else {
        std::fs::read(base.join(format!("{name}.json"))).map_err(|_| ApiError::NotFound)
    }
}

/// The most points a session's curve carries in `/api/curves`.
const CURVE_POINTS: usize = 80;

/// A session over its steps, thinned to at most [`CURVE_POINTS`]
/// `[slot offset, PnL, mark]` triples, the last step always kept.
fn curve(detail: &Value) -> Vec<Value> {
    let steps = detail["timeline"]["steps"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let every = steps.len().div_ceil(CURVE_POINTS).max(1);
    steps
        .iter()
        .enumerate()
        .filter(|(i, _)| i % every == 0 || *i + 1 == steps.len())
        .map(|(_, step)| json!([step["at"], step["pnl_usd"], step["mark_usd"]]))
        .collect()
}

/// Every session of a trial at a glance, for the run's session list: what it
/// was drawn as, the bot's and the reference's PnL, and both curves.
async fn get_curves(
    State(state): State<Arc<AppState>>,
    Path(n): Path<usize>,
) -> Result<Json<Value>, ApiError> {
    let dir = state.trial(n)?;
    let mut names: Vec<String> = std::fs::read_dir(dir.join("verifier/sessions/bot"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".json.gz")
                .or_else(|| name.strip_suffix(".json"))
                .map(str::to_string)
        })
        .collect();
    names.sort_by_key(|name| natural_key(name));
    let parse = |role: &str, name: &str| -> Option<Value> {
        serde_json::from_slice(&read_session(&dir, role, name).ok()?).ok()
    };
    let sessions: Vec<Value> = names
        .iter()
        .filter_map(|name| {
            let bot = parse("bot", name)?;
            let reference = parse("reference", name);
            Some(json!({
                "name": name,
                "description": bot["scenario"]["description"],
                "annotation": bot["annotation"],
                "disqualified": bot["disqualified"],
                "pnl_usd": bot["report"]["pnl_usd"],
                "reference_pnl_usd": reference.as_ref().map(|r| r["report"]["pnl_usd"].clone()),
                "bot": curve(&bot),
                "reference": reference.as_ref().map(curve),
            }))
        })
        .collect();
    Ok(Json(json!({ "sessions": sessions })))
}

fn clip(text: &str, max: usize) -> Value {
    if text.chars().count() <= max {
        json!(text)
    } else {
        let head: String = text.chars().take(max).collect();
        json!(format!(
            "{head}\n… ({} more characters)",
            text.chars().count() - max
        ))
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Array(parts) => parts.iter().map(text_of).collect::<Vec<_>>().join("\n"),
        Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .map(text_of)
            .unwrap_or_else(|| value.to_string()),
        other => other.to_string(),
    }
}

/// The agent's trajectory (Harbor's ATIF), condensed: each step's message,
/// tool calls and their output, clipped.
async fn get_trajectory(
    State(state): State<Arc<AppState>>,
    Path(n): Path<usize>,
) -> Result<Json<Value>, ApiError> {
    let dir = state.trial(n)?;
    // Published releases keep it gzipped.
    let trajectory = read_json(&dir.join("agent/trajectory.json"))
        .or_else(|| {
            let file = std::fs::File::open(dir.join("agent/trajectory.json.gz")).ok()?;
            serde_json::from_reader(flate2::read::GzDecoder::new(file)).ok()
        })
        .ok_or(ApiError::NotFound)?;
    let steps: Vec<Value> = trajectory["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|step| {
            let outputs: HashMap<String, String> = step["observation"]["results"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| {
                    (
                        r["source_call_id"].as_str().unwrap_or_default().to_string(),
                        text_of(&r["content"]),
                    )
                })
                .collect();
            let tools: Vec<Value> = step["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|call| {
                    let id = call["tool_call_id"].as_str().unwrap_or_default();
                    let args = &call["arguments"];
                    let input = match args.get("command") {
                        Some(command) => text_of(command),
                        None => serde_json::to_string_pretty(args).unwrap_or_default(),
                    };
                    json!({
                        "name": call["function_name"],
                        "description": args.get("description"),
                        "input": clip(&input, MAX_TOOL_CHARS),
                        "output": outputs.get(id).map(|o| clip(o, MAX_TOOL_CHARS)),
                    })
                })
                .collect();
            json!({
                "id": step["step_id"],
                "timestamp": step["timestamp"],
                "source": step["source"],
                // The task's instruction and other user messages, whole.
                "message": clip(
                    &text_of(&step["message"]),
                    if step["source"] == json!("user") { MAX_USER_CHARS } else { MAX_MESSAGE_CHARS },
                ),
                "reasoning": step.get("reasoning_content").map(|r| clip(&text_of(r), MAX_MESSAGE_CHARS)),
                "tools": tools,
                "metrics": step["metrics"],
                // What Harbor records and its viewer doesn't show: why the
                // turn ended, subagent and compacted-context steps, LLM calls.
                "stop_reason": step["extra"]["stop_reason"],
                "sidechain": step["extra"]["is_sidechain"],
                "copied": step["is_copied_context"],
                "llm_calls": step["llm_call_count"],
                "model": step["model_name"],
            })
        })
        .collect();
    Ok(Json(json!({
        "agent": trajectory["agent"],
        "final_metrics": trajectory["final_metrics"],
        "steps": steps,
    })))
}

/// A path inside the trial the page may read: a listed file, or one under a
/// listed directory, with no `..` and nothing hidden on the way.
fn allowed(path: &str) -> bool {
    let clean = FsPath::new(path).components().all(|c| match c {
        Component::Normal(part) => !part.to_string_lossy().starts_with('.'),
        _ => false,
    });
    clean && (FILES.contains(&path) || FILE_DIRS.iter().any(|d| path.starts_with(d)))
}

async fn get_file(
    State(state): State<Arc<AppState>>,
    Path((n, path)): Path<(usize, String)>,
) -> Result<Response, ApiError> {
    let dir = state.trial(n)?;
    let path = path.as_str();
    if !allowed(path) {
        return Err(ApiError::Bad(format!(
            "{path} isn't a file the review serves"
        )));
    }
    let full = dir.join(path);
    // A link inside the trial could point anywhere.
    let real = full.canonicalize().map_err(|_| ApiError::NotFound)?;
    if !real.starts_with(&dir) || !real.is_file() {
        return Err(ApiError::NotFound);
    }
    let size = real.metadata().map(|m| m.len()).unwrap_or(0);
    let mut file = std::fs::File::open(&real).map_err(|_| ApiError::NotFound)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let mut text = String::from_utf8_lossy(&bytes).to_string();
    if size > MAX_FILE_BYTES {
        text.push_str(&format!(
            "\n… ({} more bytes not shown)",
            size - MAX_FILE_BYTES
        ));
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_trial_files_are_served() {
        assert!(allowed("artifacts/app/bot/src/main.rs"));
        assert!(allowed("verifier/bot/hidden-3.log"));
        assert!(allowed("verifier/test-stdout.txt"));
        assert!(!allowed("verifier/report.json"), "not listed");
        assert!(!allowed("artifacts/app/bot/../../../.seed"));
        assert!(!allowed("artifacts/app/bot/.env"));
        assert!(!allowed("/etc/passwd"));
        assert!(!allowed("../.seed"));
    }

    #[test]
    fn curves_thin_to_their_limit_and_keep_the_end() {
        let steps: Vec<Value> = (0..1000)
            .map(|k| json!({ "at": k * 2, "pnl_usd": k as f64 }))
            .collect();
        let points = curve(&json!({ "timeline": { "steps": steps } }));
        assert!(points.len() <= CURVE_POINTS + 1, "{}", points.len());
        assert_eq!(points.last().unwrap(), &json!([1998, 999.0, null]));
        assert!(curve(&json!({})).is_empty());
    }

    #[test]
    fn sessions_sort_naturally() {
        let mut names = vec!["hidden-10", "hidden-9", "hidden-1"];
        names.sort_by_key(|n| natural_key(n));
        assert_eq!(names, ["hidden-1", "hidden-9", "hidden-10"]);
    }

    #[test]
    fn session_names_are_plain() {
        assert!(safe_name("hidden-12"));
        assert!(!safe_name("../x"));
        assert!(!safe_name(".seed"));
        assert!(!safe_name(""));
    }
}
