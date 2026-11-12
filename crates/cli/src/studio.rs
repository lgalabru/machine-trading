//! `mtx studio`: a workbench for a strategy and its scenarios
//! (`docs/rfd/0003-studio.md`). This is the server: the page
//! (`studio.html`) talks to it over one WebSocket, `/ws`, in messages
//! shaped like the Debug Adapter Protocol's: requests and their responses,
//! and events.
//!
//! The server owns the workspace (a strategy crate and its scenarios), the
//! build, the debug session and the coding agent. A session is the grader's,
//! as `mtx bot run` plays it; it stops and resumes because the debugger is
//! the grader's [`StepObserver`], which blocks until the page says go. The
//! world waits for a component, so stopping changes nothing in the session.

use std::{
    collections::HashMap,
    io::BufRead,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::{Html, IntoResponse},
    routing::get,
};
use futures::{SinkExt, StreamExt};
use mtx_grader::{
    grade::{self, GradeOptions},
    scenario::Scenario,
    wasm::{StepObserver, StepTrace},
};
use mtx_kit::{Action, Intent, Session, Side};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::{
    acp::{AgentClient, PermissionPolicy, TurnEvent},
    author::Agent,
    notice::{self, NoticeLevel},
};

/// The page. `MTX_STUDIO_PAGE` serves a file from disk instead, read on
/// every load: for working on the page without rebuilding `mtx`.
const PAGE: &str = include_str!("studio.html");
/// The protocol's test page, at `/dev`: every request a button, every
/// event in a log.
const DEV_PAGE: &str = include_str!("studio/dev.html");
/// The page's own files (bundled scripts), served at `/studio/<name>`; from
/// disk, beside `MTX_STUDIO_PAGE`, when that is set.
static ASSETS: include_dir::Dir<'static> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/src/studio");

/// A request the server answers with `success: false`, a `message`, and a
/// body the page acts on (a conflict's current file, a scenario's
/// diagnostics).
#[derive(Debug)]
struct Rejected {
    message: String,
    body: Value,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Rejected {}

/// A file's version, as `file/read`, `file/changed` and `file/write` carry
/// it: its content's hash.
fn version(text: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// Where the workspace's scenarios are.
const SCENARIOS: &str = "scenarios";
/// The studio's own state in the workspace: the agent picked.
const SETTINGS: &str = ".mtx/studio.json";
/// Files the server pushes to the page when they change on disk.
const WATCHED: &[&str] = &["src", SCENARIOS, "Cargo.toml"];

pub struct StudioOptions<'a> {
    pub dir: &'a Path,
    pub task: Option<&'a str>,
    pub repo: Option<&'a Path>,
    pub port: u16,
    pub open: bool,
}

/// `mtx studio`.
pub async fn studio(options: StudioOptions<'_>) -> Result<()> {
    // The bot's directory, or its Cargo.toml.
    let given = if options.dir.file_name().is_some_and(|n| n == "Cargo.toml") {
        options.dir.parent().unwrap_or(Path::new("."))
    } else {
        options.dir
    };
    let given = if given.as_os_str().is_empty() {
        Path::new(".")
    } else {
        given
    };
    let dir = given
        .canonicalize()
        .with_context(|| format!("no bot at {}", options.dir.display()))?;
    if !dir.join("Cargo.toml").is_file() || !dir.join("src/lib.rs").is_file() {
        bail!(
            "{} isn't a strategy: mtx studio opens a crate with src/lib.rs, as `mtx bot new` makes. \
             Run it from the bot's directory, or pass --bot-path",
            dir.display()
        );
    }
    let root = crate::bot::repo(options.repo)?;
    let task = crate::bot::resolve_task(&root, options.task)?;
    let snapshot = crate::bot::task_snapshot(&root, &task)?;
    seed_scenarios(&dir)?;
    let (events, _) = broadcast::channel(4096);
    let studio = Arc::new(Studio {
        dir: dir.clone(),
        task,
        snapshot,
        events,
        seq: AtomicU64::new(1),
        debug: Mutex::new(None),
        agent: Mutex::new(AgentState::default()),
        written: Mutex::new(HashMap::new()),
        start_mark: Mutex::new(None),
    });
    studio.load_settings();
    tokio::spawn(watch(studio.clone()));

    let app = Router::new()
        .route("/", get(page))
        .route("/dev", get(|| async { Html(DEV_PAGE) }))
        .route("/studio/{*path}", get(asset))
        .route("/ws", get(ws))
        .with_state(studio.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", options.port)).await?;
    let url = format!("http://{}", listener.local_addr()?);
    notice::print(
        NoticeLevel::Success,
        "mtx studio",
        &format!(
            "{} · {}\n{url}",
            dir.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            studio.task
        ),
    );
    if options.open {
        crate::play::open_browser(&url);
    }
    axum::serve(listener, app).await?;
    Ok(())
}

/// The studio's own scenarios, a few steps long and plain to read: the
/// first to play in a new workspace.
pub(crate) const FIRST_SCENARIO: &str = "practice-101";

/// The studio's own scenarios' names (`studio/scenarios/`).
pub(crate) fn builtin_scenarios() -> Vec<String> {
    let mut names: Vec<String> = ASSETS
        .get_dir("scenarios")
        .into_iter()
        .flat_map(|d| d.files())
        .filter_map(|f| {
            f.path()
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .collect();
    names.sort();
    names
}

/// One of the studio's own scenarios, by name.
pub(crate) fn builtin_scenario(name: &str) -> Option<&'static [u8]> {
    ASSETS
        .get_file(format!("scenarios/{name}.json"))
        .map(|f| f.contents())
}

/// Every workspace gets the studio's own scenarios (`studio/scenarios/`)
/// that it lacks.
fn seed_scenarios(dir: &Path) -> Result<()> {
    let scenarios = dir.join(SCENARIOS);
    std::fs::create_dir_all(&scenarios)?;
    for name in builtin_scenarios() {
        let to = scenarios.join(format!("{name}.json"));
        if !to.exists()
            && let Some(bytes) = builtin_scenario(&name)
        {
            std::fs::write(&to, bytes)?;
        }
    }
    Ok(())
}

struct Studio {
    dir: PathBuf,
    task: String,
    snapshot: PathBuf,
    events: broadcast::Sender<String>,
    seq: AtomicU64,
    debug: Mutex<Option<Arc<Control>>>,
    agent: Mutex<AgentState>,
    /// What the server wrote itself, and when: the watcher doesn't echo it.
    written: Mutex<HashMap<PathBuf, SystemTime>>,
    /// The mark the last session started at; before one, the snapshot's.
    start_mark: Mutex<Option<f64>>,
}

impl Studio {
    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Send an event to every page.
    fn event(&self, event: &str, body: Value) {
        let message =
            json!({ "seq": self.next_seq(), "type": "event", "event": event, "body": body });
        let _ = self.events.send(message.to_string());
    }

    fn output(&self, category: &str, text: &str) {
        self.event("output", json!({ "category": category, "text": text }));
    }

    /// A path inside the workspace, or an error.
    fn path(&self, rel: &str) -> Result<PathBuf> {
        let clean = Path::new(rel)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !clean || rel.is_empty() {
            bail!("{rel} isn't a path in the workspace");
        }
        Ok(self.dir.join(rel))
    }

    fn load_settings(&self) {
        if let Some(settings) = crate::review::read_json(&self.dir.join(SETTINGS)) {
            let mut agent = self.agent.lock().unwrap_or_else(|e| e.into_inner());
            agent.pick = agent_named(settings["agent"].as_str().unwrap_or_default());
            agent.model = settings["model"].as_str().map(str::to_string);
        }
    }

    fn save_settings(&self) -> Result<()> {
        let agent = self.agent.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.dir.join(SETTINGS);
        std::fs::create_dir_all(path.parent().unwrap_or(&self.dir))?;
        std::fs::write(
            path,
            serde_json::to_string_pretty(&json!({
                "agent": agent.pick.map(agent_id),
                "model": agent.model,
            }))? + "\n",
        )?;
        Ok(())
    }
}

async fn page() -> impl IntoResponse {
    match std::env::var_os("MTX_STUDIO_PAGE") {
        Some(path) => {
            Html(std::fs::read_to_string(path).unwrap_or_else(|e| format!("MTX_STUDIO_PAGE: {e}")))
        }
        None => Html(PAGE.to_string()),
    }
}

async fn asset(axum::extract::Path(path): axum::extract::Path<String>) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    let kind = match path.rsplit('.').next() {
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    };
    let clean = Path::new(&path)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    let bytes = if !clean {
        None
    } else if let Some(page) = std::env::var_os("MTX_STUDIO_PAGE") {
        Path::new(&page)
            .parent()
            .and_then(|dir| std::fs::read(dir.join("studio").join(&path)).ok())
    } else {
        ASSETS.get_file(&path).map(|f| f.contents().to_vec())
    };
    match bytes {
        Some(bytes) => ([(header::CONTENT_TYPE, kind)], bytes).into_response(),
        None => (StatusCode::NOT_FOUND, format!("no {path}")).into_response(),
    }
}

async fn ws(upgrade: WebSocketUpgrade, State(studio): State<Arc<Studio>>) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| connection(socket, studio))
}

/// One page: its requests in, every event out.
async fn connection(socket: WebSocket, studio: Arc<Studio>) {
    let (mut sink, mut stream) = socket.split();
    let mut events = studio.events.subscribe();
    let (replies, mut replies_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let send = tokio::spawn(async move {
        loop {
            let text = tokio::select! {
                Some(reply) = replies_rx.recv() => reply,
                event = events.recv() => match event {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                else => break,
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(message)) = stream.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(request) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let studio = studio.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let seq = request["seq"].as_u64().unwrap_or(0);
            let command = request["command"].as_str().unwrap_or_default().to_string();
            let result = handle(&studio, &command, &request["arguments"]).await;
            let response = match result {
                Ok(body) => json!({
                    "seq": studio.next_seq(), "type": "response", "request_seq": seq,
                    "command": command, "success": true, "body": body,
                }),
                Err(error) => json!({
                    "seq": studio.next_seq(), "type": "response", "request_seq": seq,
                    "command": command, "success": false, "message": format!("{error:#}"),
                    "body": error.downcast_ref::<Rejected>().map(|r| r.body.clone()).unwrap_or(json!({})),
                }),
            };
            let _ = replies.send(response.to_string());
        });
    }
    send.abort();
}

async fn handle(studio: &Arc<Studio>, command: &str, args: &Value) -> Result<Value> {
    let str_arg = |name: &str| -> Result<String> {
        args[name]
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("{command} needs `{name}`"))
    };
    match command {
        "initialize" => Ok(json!({
            "workspace": studio.dir,
            "task": studio.task,
            "files": workspace_files(&studio.dir),
            "scenarios": scenario_names(&studio.dir),
            // What a new page opens: the studio's five-step scenario.
            "defaultScenario": scenario_names(&studio.dir)
                .into_iter()
                .find(|n| n == FIRST_SCENARIO)
                .or_else(|| scenario_names(&studio.dir).into_iter().next()),
            "agent": agent_pick(studio),
            "session": session_state(studio),
        })),
        "session/history" => Ok(json!({
            "events": studio
                .debug
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|c| c.history())
                .unwrap_or_default(),
        })),
        "composer/history" => Ok(json!({
            "messages": studio.agent.lock().unwrap_or_else(|e| e.into_inner()).history.clone(),
        })),
        "file/read" => {
            let path = studio.path(&str_arg("path")?)?;
            let text =
                std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
            Ok(json!({ "version": version(&text), "text": text }))
        }
        "file/write" => {
            let rel = str_arg("path")?;
            let path = studio.path(&rel)?;
            let text = str_arg("text")?;
            // Saved over a version the page didn't load: someone else wrote it.
            if let (Some(base), Ok(current)) =
                (args["baseVersion"].as_str(), std::fs::read_to_string(&path))
                && version(&current) != base
            {
                return Err(Rejected {
                    message: "conflict".into(),
                    body: json!({ "conflict": true, "version": version(&current), "text": current }),
                }
                .into());
            }
            if rel.starts_with(&format!("{SCENARIOS}/"))
                && rel.ends_with(".json")
                && let Err(error) = serde_json::from_str::<Scenario>(&text)
            {
                return Err(Rejected {
                    message: format!("{rel} isn't a scenario: {error}"),
                    body: json!({ "diagnostics": [{
                        "file": rel, "line": error.line(), "column": error.column(),
                        "endLine": error.line(), "endColumn": error.column(),
                        "severity": "error", "message": error.to_string(),
                    }] }),
                }
                .into());
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, &text)?;
            if let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) {
                studio
                    .written
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(path, modified);
            }
            Ok(json!({ "version": version(&text) }))
        }
        "scenario/read" => {
            let name = str_arg("name")?;
            let path = studio.dir.join(SCENARIOS).join(format!("{name}.json"));
            let scenario = Scenario::load(&path).with_context(|| path.display().to_string())?;
            plan_json(studio, &scenario)
        }
        "build" => {
            let studio = studio.clone();
            let built = tokio::task::spawn_blocking(move || build(&studio)).await??;
            Ok(built)
        }
        "launch" => launch(studio, args).await,
        "continue" | "pause" | "next" | "terminate" | "setBreakpoints" | "setSpeed" => {
            let control = studio
                .debug
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .context("no session: launch one")?;
            control.command(command, args);
            if command == "continue" {
                studio.event("continued", json!({}));
            }
            Ok(control.status())
        }
        "restart" => {
            let last = {
                let debug = studio.debug.lock().unwrap_or_else(|e| e.into_inner());
                debug.as_ref().map(|c| c.launch.clone())
            };
            let launch_args = last.context("no session to restart")?;
            if let Some(control) = studio
                .debug
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                control.command("terminate", &Value::Null);
            }
            Box::pin(launch(studio, &launch_args)).await
        }
        "agents/list" => Ok(json!({ "selected": agent_pick(studio), "agents": discover_agents() })),
        "agents/select" => {
            let agent = agent_named(&str_arg("agent")?).context("no such agent")?;
            {
                let mut state = studio.agent.lock().unwrap_or_else(|e| e.into_inner());
                state.pick = Some(agent);
                state.model = args["model"]
                    .as_str()
                    .map(str::to_string)
                    .filter(|m| !m.is_empty());
                // The next prompt starts the new agent.
                state.client = None;
            }
            studio.save_settings()?;
            Ok(agent_pick(studio))
        }
        "composer/send" => {
            let text = str_arg("text")?;
            let context = args["context"].clone();
            let studio = studio.clone();
            let images: Vec<Value> = args["images"].as_array().cloned().unwrap_or_default();
            tokio::task::spawn_blocking(move || compose(&studio, &text, &context, &images))
                .await??;
            Ok(json!({}))
        }
        "composer/cancel" => {
            let state = studio.agent.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((client, session)) = &state.client {
                client.cancel(session)?;
            }
            Ok(json!({}))
        }
        other => bail!("unknown command {other}"),
    }
}

/// The workspace's files the page may open: sources and scenarios.
fn workspace_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for top in WATCHED {
        walk(dir, &dir.join(top), &mut files);
    }
    files.sort();
    files
}

fn walk(root: &Path, path: &Path, out: &mut Vec<String>) {
    if path.is_file() {
        if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.display().to_string());
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        walk(root, &entry.path(), out);
    }
}

fn scenario_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join(SCENARIOS))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_suffix(".json")
                .map(str::to_string)
        })
        .collect();
    names.sort_by_key(|n| {
        let digits = n.len() - n.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        (
            n[..n.len() - digits].to_string(),
            n[n.len() - digits..].parse::<u64>().unwrap_or(0),
        )
    });
    names
}

/// Push the page any watched file that changed on disk, by the agent or by
/// another editor; not the server's own writes.
async fn watch(studio: Arc<Studio>) {
    let mut seen: HashMap<PathBuf, SystemTime> = HashMap::new();
    let mut first = true;
    loop {
        for rel in workspace_files(&studio.dir) {
            let path = studio.dir.join(&rel);
            let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
                continue;
            };
            let before = seen.insert(path.clone(), modified);
            if first || before == Some(modified) {
                continue;
            }
            let ours = studio
                .written
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&path)
                .is_some_and(|at| *at == modified);
            if ours {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                let by = if studio.agent.lock().unwrap_or_else(|e| e.into_inner()).busy {
                    "agent"
                } else {
                    "disk"
                };
                if by == "agent" {
                    notice::print(NoticeLevel::Info, "The agent edited", &rel);
                }
                studio.event(
                    "file/changed",
                    json!({ "path": rel, "version": version(&text), "text": text, "by": by }),
                );
            }
        }
        first = false;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Build the strategy into `bot.wasm`, streaming the compiler's output and
/// returning its diagnostics, by file and line.
fn build(studio: &Studio) -> Result<Value> {
    studio.output("build", "building the strategy for wasm32-wasip2…");
    let mut child = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--lib",
            "--target",
            "wasm32-wasip2",
            "--message-format=json-diagnostic-rendered-ansi",
        ])
        .current_dir(&studio.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running cargo")?;
    let stderr = child.stderr.take();
    let progress = std::thread::scope(|scope| -> Result<(Vec<Value>, Option<PathBuf>)> {
        if let Some(stderr) = stderr {
            scope.spawn(|| {
                for line in std::io::BufReader::new(stderr)
                    .lines()
                    .map_while(Result::ok)
                {
                    studio.output("build", &line);
                }
            });
        }
        let mut diagnostics = Vec::new();
        let mut wasm = None;
        let stdout = child.stdout.take().context("cargo's output")?;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match message["reason"].as_str() {
                Some("compiler-message") => {
                    let m = &message["message"];
                    if let Some(rendered) = m["rendered"].as_str() {
                        studio.output("build", rendered);
                    }
                    if let Some(span) = m["spans"]
                        .as_array()
                        .and_then(|s| s.iter().find(|s| s["is_primary"] == json!(true)))
                    {
                        diagnostics.push(json!({
                            "file": span["file_name"],
                            "line": span["line_start"],
                            "column": span["column_start"],
                            "endLine": span["line_end"],
                            "endColumn": span["column_end"],
                            "severity": m["level"],
                            "message": m["message"],
                        }));
                    }
                }
                Some("compiler-artifact") => {
                    if let Some(file) = message["filenames"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .find(|f| f.ends_with(".wasm"))
                    {
                        wasm = Some(PathBuf::from(file));
                    }
                }
                _ => {}
            }
        }
        Ok((diagnostics, wasm))
    });
    let (diagnostics, wasm) = progress?;
    let ok = child.wait()?.success() && wasm.is_some();
    if let (true, Some(wasm)) = (ok, &wasm) {
        std::fs::copy(wasm, studio.dir.join(mtx_grader::wasm::BOT_WASM))?;
        studio.output("build", "built bot.wasm");
    }
    if ok {
        notice::print(NoticeLevel::Success, "Built", "bot.wasm");
    } else {
        let errors: Vec<String> = diagnostics
            .iter()
            .filter(|d| d["severity"] == json!("error"))
            .take(3)
            .map(|d| {
                format!(
                    "{}:{}  {}",
                    d["file"].as_str().unwrap_or("?"),
                    d["line"],
                    d["message"].as_str().unwrap_or_default()
                )
            })
            .collect();
        notice::print(NoticeLevel::Error, "Build failed", &errors.join("\n"));
    }
    let built = json!({ "ok": ok, "diagnostics": diagnostics });
    studio.event("built", built.clone());
    Ok(built)
}

/// Whether the strategy changed since `bot.wasm` was built.
fn stale(dir: &Path) -> bool {
    let built = std::fs::metadata(dir.join(mtx_grader::wasm::BOT_WASM)).and_then(|m| m.modified());
    let Ok(built) = built else {
        return true;
    };
    let mut files = Vec::new();
    walk(dir, &dir.join("src"), &mut files);
    files.push("Cargo.toml".into());
    files.iter().any(|f| {
        std::fs::metadata(dir.join(f))
            .and_then(|m| m.modified())
            .is_ok_and(|m| m > built)
    })
}

/// Start a session on a scenario: build if the strategy changed, then play
/// it in the grader, stopping where the page asked.
async fn launch(studio: &Arc<Studio>, args: &Value) -> Result<Value> {
    if let Some(control) = studio
        .debug
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        && !control.finished()
    {
        bail!("a session is running: terminate it first");
    }
    let name = args["scenario"]
        .as_str()
        .context("launch needs `scenario`")?
        .to_string();
    let path = studio.dir.join(SCENARIOS).join(format!("{name}.json"));
    let scenario = Scenario::load(&path).with_context(|| path.display().to_string())?;
    if stale(&studio.dir) {
        let builder = studio.clone();
        let built = tokio::task::spawn_blocking(move || build(&builder)).await??;
        if built["ok"] != json!(true) {
            bail!("the strategy doesn't build: see the build output");
        }
    }
    notice::print(
        NoticeLevel::Info,
        &format!("Playing {name}"),
        &scenario.description,
    );
    let control = Arc::new(Control::new(args, studio.clone()));
    *studio.debug.lock().unwrap_or_else(|e| e.into_inner()) = Some(control.clone());
    let studio = studio.clone();
    // Its own thread and runtime: the grader's session isn't `Send`, and the
    // observer blocks a worker while the page decides.
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                control.finish();
                studio.event("terminated", json!({ "error": format!("{error:#}") }));
                return;
            }
        };
        let logs = studio.dir.join("target/mtx-studio-logs");
        let _ = std::fs::create_dir_all(&logs);
        let report = runtime.block_on(async {
            tokio::select! {
                biased;
                () = control.cancel.notified() => None,
                report = grade::grade(GradeOptions {
                    snapshot_dir: &studio.snapshot,
                    bot_dir: &studio.dir,
                    scenarios: std::slice::from_ref(&scenario),
                    log_dir: &logs,
                    jobs: 1,
                    first_bot_user: 0,
                    reference_dir: None,
                    anchor_jobs: 1,
                    regimes: None,
                    stop_on_disqualify: false,
                    detail_dir: None,
                    annotations: None,
                    components_only: true,
                    observer: Some(control.clone() as Arc<dyn StepObserver>),
                }) => Some(report),
            }
        });
        let body = match report {
            // Ended from the page: at the step it was on.
            None => json!({
                "scenario": scenario.name,
                "terminated": true,
                "k": control.lock().k,
            }),
            Some(Ok(report)) => {
                let result = &report.scenarios[0];
                json!({
                    "scenario": scenario.name,
                    "pnl_usd": result.session.pnl_usd,
                    "bound_pnl_usd": result.bound_pnl_usd,
                    "reference_pnl_usd": scenario.reference_pnl_usd,
                    "disqualified": result.disqualified,
                    "stopped": result.session.turns.stopped,
                    "not_graded": report.not_graded,
                })
            }
            Some(Err(error)) => json!({ "error": format!("{error:#}") }),
        };
        let usd = |v: &Value| {
            v.as_f64().map(|v| {
                if v < 0.0 {
                    format!("−${:.2}", -v)
                } else {
                    format!("+${v:.2}")
                }
            })
        };
        match (&body["error"], &body["terminated"]) {
            (Value::String(error), _) => notice::print(
                NoticeLevel::Error,
                &format!("{} failed", scenario.name),
                error,
            ),
            (_, Value::Bool(true)) => notice::print(
                NoticeLevel::Warning,
                &format!("{} stopped", scenario.name),
                &format!("at step {}", body["k"].as_u64().map_or(0, |k| k + 1)),
            ),
            _ => notice::print(
                if body["disqualified"].is_null() {
                    NoticeLevel::Success
                } else {
                    NoticeLevel::Error
                },
                &format!(
                    "{} {}",
                    scenario.name,
                    usd(&body["pnl_usd"]).unwrap_or_default()
                ),
                &[
                    Some(format!(
                        "bound ${:.2}",
                        body["bound_pnl_usd"].as_f64().unwrap_or(0.0)
                    )),
                    usd(&body["reference_pnl_usd"]).map(|r| format!("reference {r}")),
                    body["disqualified"]
                        .as_str()
                        .map(|d| format!("disqualified: {d}")),
                    body["stopped"]
                        .as_str()
                        .map(|s| format!("the strategy stopped: {s}")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n"),
            ),
        }
        control.event("terminated", body);
        control.finish();
    });
    Ok(json!({ "scenario": name }))
}

/// A debug session's controls, shared by the page's requests and the
/// grader's observer, which blocks on them.
struct Control {
    state: Mutex<ControlState>,
    wake: Condvar,
    studio: Arc<Studio>,
    /// What `launch` was asked: `restart` asks it again.
    launch: Value,
    /// Fired by `terminate`: the session's runtime drops the grader's future,
    /// which stops its surfnet and its bot.
    cancel: tokio::sync::Notify,
}

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Running,
    /// Stopped at a step, waiting for the page.
    Stopped,
    /// Stop after the next step.
    Step,
    /// Stop at the next step there is.
    Pause,
    /// Run out the session without stopping or reporting.
    Terminating,
    Finished,
}

struct ControlState {
    mode: Mode,
    /// The strategy has started: before, the market is still being set up.
    started: bool,
    /// The session's events so far, for a page that reloads: `started`,
    /// each `step`, each `stopped`, `terminated`.
    history: Vec<Value>,
    breakpoints: Breakpoints,
    ms_per_step: u64,
    stop_on_entry: bool,
    /// The last step seen.
    k: Option<u64>,
}

#[derive(Default)]
struct Breakpoints {
    steps: Vec<u64>,
    on_orders: bool,
    on_fill: bool,
    on_trap: bool,
}

impl Breakpoints {
    fn from(args: &Value) -> Self {
        Self {
            steps: args["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .collect(),
            on_orders: args["onOrders"] == json!(true),
            on_fill: args["onFill"] == json!(true),
            on_trap: args["onTrap"] == json!(true),
        }
    }
}

impl Control {
    fn new(args: &Value, studio: Arc<Studio>) -> Self {
        Self {
            state: Mutex::new(ControlState {
                mode: Mode::Running,
                started: false,
                history: Vec::new(),
                breakpoints: Breakpoints::from(&args["breakpoints"]),
                ms_per_step: args["msPerStep"].as_u64().unwrap_or(0),
                stop_on_entry: args["stopOnEntry"] == json!(true),
                k: None,
            }),
            wake: Condvar::new(),
            studio,
            launch: args.clone(),
            cancel: tokio::sync::Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ControlState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn command(&self, command: &str, args: &Value) {
        let mut state = self.lock();
        if state.mode == Mode::Finished {
            return;
        }
        match command {
            "continue" => state.mode = Mode::Running,
            "next" => state.mode = Mode::Step,
            "pause" if state.mode == Mode::Running => state.mode = Mode::Pause,
            "terminate" => {
                state.mode = Mode::Terminating;
                self.cancel.notify_one();
            }
            "setBreakpoints" => state.breakpoints = Breakpoints::from(args),
            "setSpeed" => state.ms_per_step = args["msPerStep"].as_u64().unwrap_or(0),
            _ => {}
        }
        self.wake.notify_all();
    }

    fn status(&self) -> Value {
        let state = self.lock();
        json!({
            "state": match state.mode {
                Mode::Stopped => "stopped",
                Mode::Terminating | Mode::Finished => "terminated",
                _ if !state.started => "starting",
                _ => "running",
            },
            "k": state.k,
            "msPerStep": state.ms_per_step,
        })
    }

    /// Send an event to the page, and keep it for `session/history`.
    fn event(&self, event: &str, body: Value) {
        self.lock()
            .history
            .push(json!({ "event": event, "body": body }));
        self.studio.event(event, body);
    }

    fn history(&self) -> Vec<Value> {
        self.lock().history.clone()
    }

    fn finished(&self) -> bool {
        self.lock().mode == Mode::Finished
    }

    fn finish(&self) {
        self.lock().mode = Mode::Finished;
        self.wake.notify_all();
    }

    /// Stop, tell the page, and wait for it to say go.
    fn stop(
        &self,
        mut state: std::sync::MutexGuard<'_, ControlState>,
        reason: &str,
        k: Option<u64>,
    ) {
        state.mode = Mode::Stopped;
        let body = json!({ "reason": reason, "k": k });
        state
            .history
            .push(json!({ "event": "stopped", "body": body }));
        self.studio.event("stopped", body);
        while state.mode == Mode::Stopped {
            state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl StepObserver for Control {
    fn started(&self, session: &Session, printed: &[String], failed: Option<&str>) {
        *self
            .studio
            .start_mark
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(session.start_mark);
        self.lock().started = true;
        self.event(
            "started",
            json!({ "session": session_json(session), "printed": printed, "failed": failed }),
        );
        let state = self.lock();
        if state.stop_on_entry && state.mode == Mode::Running {
            self.stop(state, "entry", None);
        }
    }

    fn step(&self, trace: &StepTrace) {
        let mut state = self.lock();
        if state.mode == Mode::Terminating {
            return;
        }
        let k = trace.market.k;
        state.k = Some(k);
        drop(state);
        self.event("step", trace_json(trace));
        let state = self.lock();
        let b = &state.breakpoints;
        let trapped = trace
            .failed
            .as_deref()
            .is_some_and(|f| f.starts_with("trapped"));
        let hit = b.steps.contains(&(k + 1))
            || (b.on_orders && !trace.intents.is_empty())
            || (b.on_fill
                && trace
                    .sent
                    .iter()
                    .any(|(a, r)| r.is_ok() && !matches!(a, Action::CancelAll)))
            || (b.on_trap && trapped);
        match state.mode {
            Mode::Step => self.stop(state, "step", Some(k)),
            Mode::Pause => self.stop(state, "pause", Some(k)),
            Mode::Running if hit => {
                self.stop(state, if trapped { "trap" } else { "breakpoint" }, Some(k))
            }
            Mode::Running if state.ms_per_step > 0 => {
                let pace = Duration::from_millis(state.ms_per_step);
                // Paced: a pause or a step arriving meanwhile still stops here.
                let (state, _) = self
                    .wake
                    .wait_timeout(state, pace)
                    .unwrap_or_else(|e| e.into_inner());
                match state.mode {
                    Mode::Pause => self.stop(state, "pause", Some(k)),
                    Mode::Step => self.stop(state, "step", Some(k)),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn session_json(s: &Session) -> Value {
    json!({
        "symbol": s.symbol,
        "steps": s.steps,
        "slots_per_step": s.slots_per_step,
        "start_slot": s.start_slot,
        "end_slot": s.end_slot,
        "start_mark": s.start_mark,
        "start_oracle": s.start_oracle,
        "cap": s.cap,
        "max_drawdown": s.max_drawdown,
        "vars": s.vars,
    })
}

fn side(side: Side) -> &'static str {
    match side {
        Side::Bid => "bid",
        Side::Ask => "ask",
    }
}

fn intent_json(intent: &Intent) -> Value {
    match intent {
        Intent::Target {
            position,
            execution,
        } => json!({
            "kind": "target", "position": position,
            "slice": execution.slice, "maxPerStep": execution.max_per_step,
            "slippageBps": execution.slippage_bps,
        }),
        Intent::Quote { bid, ask, size } => {
            json!({ "kind": "quote", "bid": bid, "ask": ask, "size": size })
        }
        Intent::Market {
            side: s,
            size,
            limit,
        } => json!({ "kind": "market", "side": side(*s), "size": size, "limit": limit }),
        Intent::Limit {
            side: s,
            price,
            size,
        } => json!({ "kind": "limit", "side": side(*s), "price": price, "size": size }),
        Intent::CancelAll => json!({ "kind": "cancelAll" }),
    }
}

fn action_json(action: &Action) -> Value {
    match action {
        Action::Market {
            side: s,
            size,
            limit,
        } => json!({ "kind": "market", "side": side(*s), "size": size, "limit": limit }),
        Action::Limit {
            side: s,
            price,
            size,
        } => json!({ "kind": "limit", "side": side(*s), "price": price, "size": size }),
        Action::CancelAll => json!({ "kind": "cancelAll" }),
    }
}

/// A step's trace, as the `step` event carries it.
fn trace_json(trace: &StepTrace) -> Value {
    let m = &trace.market;
    json!({
        "k": m.k,
        "steps": trace.steps,
        "market": {
            "k": m.k, "slot": m.slot, "last": m.last,
            "steps_left": m.steps_left, "slots_left": m.slots_left,
            "mark": m.mark, "oracle": m.oracle, "index": m.index,
            "best_bid": m.best_bid, "best_ask": m.best_ask,
            "position": { "size": m.position.size, "entry_usd": m.position.entry_usd },
            "cap": m.cap, "max_drawdown": m.max_drawdown,
            "resting_bids": m.resting_bids, "resting_asks": m.resting_asks,
            "pnl": m.pnl, "peak_pnl": m.peak_pnl, "drawdown": m.drawdown,
        },
        "printed": trace.printed,
        "failed": trace.failed,
        "intents": trace.intents.iter().map(intent_json).collect::<Vec<_>>(),
        "notes": trace.notes,
        "sent": trace.sent.iter().map(|(a, r)| json!({
            "action": action_json(a),
            "ok": r.is_ok(),
            "error": r.as_ref().err(),
        })).collect::<Vec<_>>(),
        "fuel": trace.fuel,
        "rpc_calls": trace.rpc_calls,
        // As the session measured it at this step, before the strategy's
        // turn: `position_change` is what filled during the step before.
        "account": trace.account.map(|a| json!({
            "value_usd": a.value_usd,
            "pnl_usd": a.pnl_usd,
            "drawdown_usd": a.drawdown_usd,
            "position": a.position,
            "position_change": a.position_change,
            "level_usd": a.level_usd,
        })),
    })
}

/// Where a session is, for a page that loads or reloads.
fn session_state(studio: &Studio) -> Value {
    let debug = studio.debug.lock().unwrap_or_else(|e| e.into_inner());
    let Some(control) = debug.as_ref() else {
        return json!({ "state": "idle" });
    };
    let mut status = control.status();
    status["scenario"] = control.launch["scenario"].clone();
    if status["state"] != json!("terminated") {
        status["breakpoints"] = control.launch["breakpoints"].clone();
    }
    status
}

/// A scenario as the market panel draws it before a session: its plan in
/// steps, on the axis a session's steps share.
fn plan_json(studio: &Studio, scenario: &Scenario) -> Result<Value> {
    const SLOTS_PER_STEP: u64 = 2;
    let measured = *studio.start_mark.lock().unwrap_or_else(|e| e.into_inner());
    let snapshot = crate::review::read_json(&studio.snapshot.join("manifest.json"))
        .and_then(|m| m["mark_price_usd"].as_f64());
    let start = measured.or(snapshot).unwrap_or(100.0);
    let steps = scenario.duration_slots.div_ceil(SLOTS_PER_STEP);
    let plan: Vec<Value> = (0..steps)
        .map(|k| {
            let at = (k + 1) * SLOTS_PER_STEP;
            let level = mtx_grader::making::level_at(&scenario.path, at);
            json!({ "k": k, "mark_usd": start * (1.0 + level / 100.0), "level_pct": level })
        })
        .collect();
    let takers: Vec<Value> = scenario
        .making
        .iter()
        .flat_map(|m| m.takers.iter())
        .chain(scenario.tape.iter())
        .map(|t| {
            json!({
                "k": (t.slot / SLOTS_PER_STEP).saturating_sub(1),
                "side": serde_json::to_value(t.side).unwrap_or_default(),
                "size": t.size,
            })
        })
        .collect();
    Ok(json!({
        "scenario": serde_json::to_value(scenario)?,
        "steps": steps,
        "slots_per_step": SLOTS_PER_STEP,
        "start_mark_usd": start,
        // Before a session has played, the snapshot's mark: a session starts
        // a few cents from it.
        "start_mark_estimated": measured.is_none(),
        "plan": plan,
        "takers": takers,
    }))
}

#[derive(Default)]
struct AgentState {
    pick: Option<Agent>,
    model: Option<String>,
    client: Option<(Arc<AgentClient>, String)>,
    /// A turn is running: files changing now are the agent's.
    busy: bool,
    /// The conversation, for a page that reloads: each message and tool
    /// call as the page last saw it.
    history: Vec<Value>,
    /// The next message's number.
    messages: u64,
    /// Whether the agent takes images in a prompt (ACP's
    /// `promptCapabilities.image`), once it has started.
    images: bool,
}

fn agent_id(agent: Agent) -> &'static str {
    match agent {
        Agent::ClaudeCode => "claude-code",
        Agent::Codex => "codex",
    }
}

fn agent_named(id: &str) -> Option<Agent> {
    match id {
        "claude-code" => Some(Agent::ClaudeCode),
        "codex" => Some(Agent::Codex),
        _ => None,
    }
}

fn agent_pick(studio: &Studio) -> Value {
    let state = studio.agent.lock().unwrap_or_else(|e| e.into_inner());
    json!({ "agent": state.pick.map(agent_id), "model": state.model, "busy": state.busy })
}

/// The coding agents this machine can run, and whether each is ready.
fn discover_agents() -> Vec<Value> {
    let npx = crate::author::on_path("npx");
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    [Agent::ClaudeCode, Agent::Codex]
        .into_iter()
        .map(|agent| {
            let (program, package) = agent.adapter();
            let installed = crate::author::on_path(program);
            let (key, login, cli, models): (&str, PathBuf, &str, &[&str]) = match agent {
                Agent::ClaudeCode => (
                    "ANTHROPIC_API_KEY",
                    home.join(".claude"),
                    "claude",
                    &[
                        "claude-opus-5-5",
                        "claude-sonnet-5-5",
                        "claude-fable-5-1",
                        "claude-haiku-5-5",
                    ],
                ),
                Agent::Codex => (
                    "OPENAI_API_KEY",
                    home.join(".codex/auth.json"),
                    "codex",
                    &[],
                ),
            };
            let authenticated = std::env::var_os(key).is_some() || login.exists();
            let available = (installed || npx) && authenticated;
            let reason = if !(installed || npx) {
                Some(format!(
                    "install {program}: `npm install -g {package}` (or install Node for npx)"
                ))
            } else if !authenticated {
                Some(format!("set {key}, or run `{cli}` once to log in"))
            } else {
                None
            };
            json!({
                "id": agent_id(agent),
                "name": agent.name(),
                "available": available,
                "reason": reason,
                "adapter": program,
                "via": if installed { "path" } else if npx { "npx" } else { "missing" },
                "models": std::iter::once(json!({ "id": "", "name": "default" }))
                    .chain(models.iter().map(|m| json!({ "id": m, "name": m })))
                    .collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// What the agent is told about the workspace, before the first prompt.
fn primer(studio: &Studio) -> String {
    let mtx = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "mtx".into());
    format!(
        r#"You are working in an mtx studio workspace, {dir}: a trading strategy for the machine-trading bench, and the market scenarios it plays. The person sees the studio's page: an editor on these files, a chart of the scenario, and the session step by step when they press Play.

The strategy is a Rust crate (src/lib.rs) on mtx-kit, built into a WebAssembly component: `impl Strategy` with `start(&Session)`, `step(&Market, &mut Orders)` and `end`, and `mtx_kit::export!`. `step` reads the market (mark, oracle, best bid and ask, position, pnl) and fills an order sheet: `orders.target_position(units)`, `orders.buy(size, limit)`, `orders.sell(..)`, `orders.bid(price, size)`, `orders.ask(..)`, `orders.quote(bid, ask, size)`, `orders.cancel_all()`. `println!` shows in the session's log. The guide: https://lgalabru.github.io/machine-trading/guides/bots/writing-a-bot.html

Scenarios are JSON files in scenarios/, one market session each (the {task} task's format):
- `name` (the file's name, without .json), `description` (one plain sentence of what happens), `symbol` "SPCX".
- `duration_slots`: the session's length. A step is 2 slots: 10 slots is 5 steps.
- `path`: waypoints `{{"slot": s, "level_pct": p}}`. At slot s the market starts to move to p% from its starting mark (-3.0 is 3% below it, 0.0 back to the start), and holds there until the next waypoint.
- Phoenix moves its price at most 4% at once: a move larger than that lands in parts, one 4% step every 2 slots. The next waypoint must start after the move ends: at or after slot s + 2 * ceil(|move| / 4%) + 1. From 0% to +5% at slot 5 takes 2 steps, so the next waypoint starts at slot 10 or later.
- `mandate`: keep it as it is (`max_position` is the position cap, `max_drawdown_usd` the loss limit).
- After writing or changing a scenario, check it: `{mtx} scenario check scenarios/<name>.json`. It says what's wrong and how to fix it. The studio also checks every scenario you touched when your turn ends, and sends you its problems.

To try the strategy yourself: `{mtx} bot run --bot {dir} --scenario scenarios/<name>.json`. It builds, plays one session, and prints what the strategy printed and its PnL (and `--debug --auto 0` for every step). The person can also press Play on the page.

When you're asked to design a strategy for a situation (a market described, or a screenshot of a chart): first write a scenario that reproduces it, as a new file with a plain name, and check it. Then reason about what would trade it well, change src/lib.rs, run it on that scenario, and say what it made and why. Keep changes small and explained; each message may carry the page's context (the open file, the scenario, the step on screen, the last build's errors)."#,
        dir = studio.dir.display(),
        task = studio.task,
    )
}

/// The workspace's scenario files, and when each last changed.
fn scenario_times(dir: &Path) -> HashMap<String, Option<SystemTime>> {
    scenario_names(dir)
        .into_iter()
        .map(|name| {
            let modified = std::fs::metadata(dir.join(SCENARIOS).join(format!("{name}.json")))
                .and_then(|m| m.modified())
                .ok();
            (name, modified)
        })
        .collect()
}

/// The scenarios a turn wrote that don't load, and why.
fn broken_scenarios(
    dir: &Path,
    before: &HashMap<String, Option<SystemTime>>,
) -> Vec<(String, String)> {
    scenario_times(dir)
        .into_iter()
        .filter(|(name, at)| before.get(name) != Some(at))
        .filter_map(|(name, _)| {
            let path = dir.join(SCENARIOS).join(format!("{name}.json"));
            Scenario::load(&path)
                .err()
                .map(|error| (format!("{SCENARIOS}/{name}.json"), format!("{error:#}")))
        })
        .collect()
}

/// How many times the studio sends a turn's broken scenarios back to the
/// agent before leaving them to the person.
const SCENARIO_FIXES: usize = 2;

/// Record a composer event, and send it: a message's chunks land on the
/// message with its `id`, a tool call's updates on the call with its.
fn composer_event(studio: &Studio, event: &str, body: Value) {
    {
        let mut state = studio.agent.lock().unwrap_or_else(|e| e.into_inner());
        let id = body["id"].clone();
        match state
            .history
            .iter_mut()
            .rev()
            .find(|m| m["id"] == id && m["kind"] == json!(event))
        {
            Some(entry) if event == "composer/message" => {
                let text = format!(
                    "{}{}",
                    entry["text"].as_str().unwrap_or_default(),
                    body["text"].as_str().unwrap_or_default()
                );
                entry["text"] = json!(text);
                entry["done"] = body["done"].clone();
            }
            Some(entry) => {
                let kind = entry["kind"].clone();
                *entry = body.clone();
                entry["kind"] = kind;
            }
            None => {
                let mut entry = body.clone();
                entry["kind"] = json!(event);
                state.history.push(entry);
            }
        }
    }
    studio.event(event, body);
}

fn next_message_id(studio: &Studio) -> String {
    let mut state = studio.agent.lock().unwrap_or_else(|e| e.into_inner());
    state.messages += 1;
    format!("m{}", state.messages)
}

/// One prompt to the agent, its reply streamed to the page. `images` are
/// pasted screenshots, `{mimeType, data}` (base64), sent as ACP image
/// blocks. Scenarios the turn breaks go back to the agent to fix.
fn compose(studio: &Studio, text: &str, context: &Value, images: &[Value]) -> Result<()> {
    let first_line: String = text
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(100)
        .collect();
    let name = studio
        .agent
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pick
        .unwrap_or(Agent::ClaudeCode)
        .name();
    let shots = if images.is_empty() {
        String::new()
    } else {
        format!(
            " (and {} image{})",
            images.len(),
            if images.len() == 1 { "" } else { "s" }
        )
    };
    notice::print(
        NoticeLevel::Info,
        &format!("Asked {name}{shots}"),
        &first_line,
    );
    let user = next_message_id(studio);
    composer_event(
        studio,
        "composer/message",
        json!({ "id": user, "role": "user", "text": text, "done": true, "images": images.len() }),
    );
    let (client, session, first) = {
        let mut state = studio.agent.lock().unwrap_or_else(|e| e.into_inner());
        if state.busy {
            bail!("the agent is still answering: cancel it first");
        }
        let pick = state.pick.unwrap_or(Agent::ClaudeCode);
        let first = state.client.is_none();
        if first {
            studio.output("agent", &format!("starting {}…", pick.name()));
            let log = std::fs::File::create(studio.dir.join("target/mtx-studio-agent.log"))
                .or_else(|_| {
                    std::fs::File::create(std::env::temp_dir().join("mtx-studio-agent.log"))
                })?;
            let client = AgentClient::spawn_with_stderr(
                pick.command(state.model.as_deref()),
                PermissionPolicy::AllowAll,
                log.into(),
            )
            .with_context(|| {
                let (program, package) = pick.adapter();
                format!("starting {program}; install it with `npm install -g {package}`")
            })?;
            let init = client.initialize().context("ACP initialize")?;
            state.images = init.agent_capabilities["promptCapabilities"]["image"] == json!(true);
            let session = client
                .new_session(&studio.dir, &[])
                .context("ACP session/new")?;
            state.client = Some((Arc::new(client), session));
        }
        if !images.is_empty() && !state.images {
            bail!(
                "{} doesn't take images here: describe the chart instead",
                pick.name()
            );
        }
        state.busy = true;
        let (client, session) = state
            .client
            .as_ref()
            .map(|(c, s)| (c.clone(), s.clone()))
            .unwrap_or_else(|| unreachable!());
        (client, session, first)
    };
    let mut text = prompt_text(text, context);
    if first {
        text = format!("{}\n\n{text}", primer(studio));
    }
    let mut blocks = vec![json!({ "type": "text", "text": text })];
    blocks.extend(images.iter().map(
        |image| json!({ "type": "image", "mimeType": image["mimeType"], "data": image["data"] }),
    ));
    let mut before = scenario_times(&studio.dir);
    run(studio, &client, &session, blocks)?;
    for _ in 0..SCENARIO_FIXES {
        let broken = broken_scenarios(&studio.dir, &before);
        if broken.is_empty() {
            return Ok(());
        }
        let list: Vec<String> = broken
            .iter()
            .map(|(file, error)| format!("- {file}: {error}"))
            .collect();
        let note = format!(
            "mtx refused {}:\n{}",
            if broken.len() == 1 {
                "a scenario"
            } else {
                "scenarios"
            },
            list.join("\n")
        );
        notice::print(
            NoticeLevel::Warning,
            "A scenario doesn't load",
            &list.join("\n"),
        );
        let id = next_message_id(studio);
        composer_event(
            studio,
            "composer/message",
            json!({ "id": id, "role": "studio", "text": note, "done": true }),
        );
        before = scenario_times(&studio.dir);
        studio.agent.lock().unwrap_or_else(|e| e.into_inner()).busy = true;
        run(
            studio,
            &client,
            &session,
            vec![
                json!({ "type": "text", "text": format!("{note}\n\nFix the file (`mtx scenario check` says why), then reply in one line.") }),
            ],
        )?;
    }
    Ok(())
}

/// The person's message, with the page's context after it.
fn prompt_text(text: &str, context: &Value) -> String {
    if context.is_null() || context.as_object().is_some_and(|o| o.is_empty()) {
        return text.to_string();
    }
    format!(
        "{text}\n\n<studio-context>\n{}\n</studio-context>",
        serde_json::to_string_pretty(context).unwrap_or_default()
    )
}

/// Stream one turn to the page.
fn run(
    studio: &Studio,
    client: &Arc<AgentClient>,
    session: &str,
    prompt: Vec<Value>,
) -> Result<()> {
    let result = (|| -> Result<()> {
        let mut turn = client.prompt_blocks(session, prompt)?;
        // ACP updates a tool call by its id, often without its title: the
        // page gets the whole call each time.
        let mut calls: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
        let reply = next_message_id(studio);
        let thought = next_message_id(studio);
        loop {
            match turn.recv_timeout(Duration::from_secs(600))? {
                TurnEvent::Text(chunk) => composer_event(
                    studio,
                    "composer/message",
                    json!({ "id": reply, "role": "agent", "text": chunk, "done": false }),
                ),
                TurnEvent::Thought(chunk) => composer_event(
                    studio,
                    "composer/message",
                    json!({ "id": thought, "role": "thought", "text": chunk, "done": false }),
                ),
                TurnEvent::ToolCall { id, title, status } => {
                    let call = calls.entry(id.clone()).or_default();
                    if title.is_some() {
                        call.0 = title;
                    }
                    if status.is_some() {
                        call.1 = status;
                    }
                    composer_event(
                        studio,
                        "composer/tool",
                        json!({ "id": id, "title": call.0, "status": call.1.as_deref().unwrap_or("pending") }),
                    );
                }
                TurnEvent::Done(reason) => {
                    composer_event(
                        studio,
                        "composer/message",
                        json!({ "id": reply, "role": "agent", "text": "", "done": true, "stop": format!("{reason:?}") }),
                    );
                    return Ok(());
                }
                TurnEvent::Failed(error) => return Err(error.into()),
            }
        }
    })();
    let mut state = studio.agent.lock().unwrap_or_else(|e| e.into_inner());
    state.busy = false;
    if result.is_err() {
        state.client = None;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoints_read_the_page_s_arguments() {
        let b = Breakpoints::from(&json!({ "steps": [3, 40], "onOrders": true }));
        assert_eq!(b.steps, [3, 40]);
        assert!(b.on_orders && !b.on_fill && !b.on_trap);
    }

    #[test]
    fn a_prompt_carries_the_page_s_context_after_it() {
        assert_eq!(prompt_text("hi", &json!({})), "hi");
        let text = prompt_text("why no buy?", &json!({ "step": 33 }));
        assert!(
            text.starts_with("why no buy?\n\n<studio-context>"),
            "{text}"
        );
        assert!(text.contains("\"step\": 33"), "{text}");
    }

    #[test]
    fn agents_are_named_both_ways() {
        for agent in [Agent::ClaudeCode, Agent::Codex] {
            assert_eq!(agent_named(agent_id(agent)), Some(agent));
        }
        assert_eq!(discover_agents().len(), 2);
    }
}
