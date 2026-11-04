//! `mth bench review`: browse a bench's results in a small web UI.
//!
//! Point it at a batch (`jobs/bench/<batch>`), a Harbor job inside one, or a
//! single trial. It finds every trial below the path and serves a page with
//! a leaderboard, each trial's sessions against the reference's, each
//! session step by step (from the verifier's `sessions/` detail files), the
//! agent's trajectory and the bot it wrote.
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
/// Directories inside a trial the page may read files from, and single
/// files it may read.
const FILE_DIRS: &[&str] = &["artifacts/app/bot/", "verifier/bot/"];
const FILES: &[&str] = &[
    "verifier/test-stdout.txt",
    "agent/claude-code.txt",
    "exception.txt",
    "trial.log",
];
/// What a bot's source listing leaves out: build output.
const SKIPPED_DIRS: &[&str] = &["target", "bin", "node_modules", ".git"];

pub struct ReviewOptions {
    pub port: u16,
    pub open: bool,
}

struct AppState {
    root: PathBuf,
    trials: Vec<PathBuf>,
}

impl AppState {
    fn trial(&self, n: usize) -> Result<&PathBuf, ApiError> {
        self.trials.get(n).ok_or(ApiError::NotFound)
    }
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

pub async fn review(path: PathBuf, opts: ReviewOptions) -> Result<Value> {
    let root = path
        .canonicalize()
        .with_context(|| path.display().to_string())?;
    let trials = discover(&root);
    if trials.is_empty() {
        bail!(
            "no Harbor trials under {} (a trial is a directory with a result.json naming it)",
            root.display()
        );
    }
    let count = trials.len();
    let state = Arc::new(AppState { root, trials });
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/api/index", get(get_index))
        .route("/api/trial/{n}", get(get_trial))
        .route("/api/session/{n}/{role}/{name}", get(get_session))
        .route("/api/trajectory/{n}", get(get_trajectory))
        .route("/api/file/{n}/{*path}", get(get_file))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], opts.port)))
        .await
        .context("binding the review server")?;
    let url = format!("http://{}", listener.local_addr()?);
    info!(%url, trials = count, "review ready; Ctrl-C to stop");
    ui::done(format!(
        "Reviewing {count} trial{} at {}",
        if count == 1 { "" } else { "s" },
        ui::accent(&url)
    ));
    if opts.open {
        crate::play::open_browser(&url);
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    ui::clear();
    Ok(json!({ "root": state.root, "trials": count, "url": url }))
}

/// Trials below `root`, in path order: directories whose `result.json` names
/// a trial.
fn discover(root: &FsPath) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, 0, &mut found);
    found.sort();
    found
}

fn walk(dir: &FsPath, depth: usize, found: &mut Vec<PathBuf>) {
    if read_json(&dir.join("result.json")).is_some_and(|r| r.get("trial_name").is_some()) {
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

fn read_json(path: &FsPath) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A trial at a glance: what the leaderboard and the trial's header show.
fn summary(state: &AppState, n: usize, dir: &FsPath) -> Value {
    let result = read_json(&dir.join("result.json")).unwrap_or(Value::Null);
    let report = read_json(&dir.join("verifier/report.json"));
    let rewards = read_json(&dir.join("verifier/reward.json")).unwrap_or(Value::Null);
    let relative = |path: &FsPath| {
        path.strip_prefix(&state.root)
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
    json!({
        "id": n,
        "path": relative(dir),
        "job": dir.parent().map(relative).unwrap_or_default(),
        "task": result["task_name"],
        "task_ref": result["config"]["task"]["ref"],
        "trial": result["trial_name"],
        "agent": result["agent_info"]["name"],
        "agent_version": result["agent_info"]["version"],
        "model": result["agent_info"]["model_info"]["name"],
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
        "graded": report.is_some() || rewards.is_object(),
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

async fn get_index(State(state): State<Arc<AppState>>) -> Json<Value> {
    let trials: Vec<Value> = state
        .trials
        .iter()
        .enumerate()
        .map(|(n, dir)| summary(&state, n, dir))
        .collect();
    // Jobs: the Harbor runs the trials came from, with their configs.
    let mut jobs: Vec<Value> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for dir in &state.trials {
        let Some(job) = dir.parent() else { continue };
        if !seen.insert(job.to_path_buf()) {
            continue;
        }
        let result = read_json(&job.join("result.json")).unwrap_or(Value::Null);
        let config = read_json(&job.join("config.json")).unwrap_or(Value::Null);
        jobs.push(json!({
            "path": job.strip_prefix(&state.root).unwrap_or(job).display().to_string(),
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
    Json(json!({
        "root": state.root,
        "name": state.root.file_name().map(|n| n.to_string_lossy().to_string()),
        "jobs": jobs,
        "trials": trials,
    }))
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
    list_files(dir, &dir.join("artifacts/app/bot"), &mut files);
    let logs: Vec<String> = FILES
        .iter()
        .filter(|f| dir.join(f).is_file())
        .map(|f| f.to_string())
        .collect();
    let result = read_json(&dir.join("result.json")).unwrap_or(Value::Null);
    let bot_sessions = sessions("bot");
    // Per session, what the report doesn't carry: what the scenario was drawn
    // as, and how the bot's transactions went.
    let session_stats: serde_json::Map<String, Value> = bot_sessions
        .iter()
        .filter_map(|name| {
            let detail = read_session(dir, "bot", name).ok()?;
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
                    "txs": txs,
                    "failed": failed,
                    "duplicates": duplicates,
                }),
            ))
        })
        .collect();
    Ok(Json(json!({
        "summary": summary(&state, n, dir),
        "session_stats": session_stats,
        "report": read_json(&dir.join("verifier/report.json")),
        "rewards": read_json(&dir.join("verifier/reward.json")),
        "sessions": { "bot": bot_sessions, "reference": sessions("reference") },
        "files": files,
        "logs": logs,
        "trajectory": dir.join("agent/trajectory.json").is_file(),
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
    let bytes = read_session(dir, &role, &name)?;
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
    let trajectory = read_json(&dir.join("agent/trajectory.json")).ok_or(ApiError::NotFound)?;
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
                "message": clip(&text_of(&step["message"]), MAX_MESSAGE_CHARS),
                "reasoning": step.get("reasoning_content").map(|r| clip(&text_of(r), MAX_MESSAGE_CHARS)),
                "tools": tools,
                "metrics": step["metrics"],
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
    if !real.starts_with(dir) || !real.is_file() {
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
