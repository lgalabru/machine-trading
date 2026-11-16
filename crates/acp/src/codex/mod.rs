//! A Codex ACP agent, in Rust: `mtx codex-acp` (Axel's `axel codex-acp`).
//!
//! It replaces Zed's `codex-acp` for Axel.  That adapter compiles OpenAI's
//! Codex engine in (codex-core, pinned to one Codex release); this one drives
//! the `codex` you have installed through its own JSON-RPC protocol,
//! `codex app-server`, so it always runs your Codex, with your config,
//! login and MCP servers.
//!
//! - **Down, to Codex:** `initialize`, `thread/start` or `thread/resume`,
//!   `turn/start` per prompt (with the session's approval policy, sandbox and
//!   model), `turn/interrupt`, `model/list`, `review/start`,
//!   `thread/compact/start`; Codex's approval requests (commands, file
//!   changes, permissions) are answered from the ACP client.
//! - **Up, to the ACP client:** what `codex-acp` 0.16.0 sends for the same
//!   events (see `tools`): message and reasoning chunks (the full text only
//!   when nothing streamed), command, file-change, MCP, web-search and
//!   image tool calls with its titles, kinds, locations and diffs, plans,
//!   usage, its slash commands and permission options, `end_turn` /
//!   `cancelled` stop reasons, and replay on `session/load`.
//!
//! Deliberate differences, for Axel's session host:
//! - Modes use the ids the host asks for (`default`, `acceptEdits`, `plan`,
//!   `bypassPermissions`), each an approval policy and sandbox; codex-acp's
//!   preset ids (`read-only`, `auto`, `full-access`) are accepted too.
//! - The model is listed as ACP `models` and set with `session/set_model`,
//!   which the host uses, rather than as a config option.
//! - `_meta.systemPrompt.append` becomes Codex developer instructions, so a
//!   Codex agent knows who it is, as a Claude agent does.

pub mod tools;

use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::agent_rpc::{Client, Lines, RpcError, RpcResult, bounded_lines};
use crate::usage::{CodexMeter, codex_limits, limits_params};

/// codex-acp's `/init` prompt.
const INIT_PROMPT: &str = include_str!("init_prompt.md");

/// How long Codex has to answer an ordinary request (a steer, an
/// interrupt): it answers at once unless it is stuck.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long Codex has to start something: `initialize`, a turn, a review,
/// a compaction, the model list.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long Codex has to open a thread: resuming one reads its history.
const THREAD_TIMEOUT: Duration = Duration::from_secs(120);
/// The slash commands go out this long after the session reply, so the
/// client knows the session before it hears about it.
const COMMANDS_DELAY: Duration = Duration::from_millis(20);

/// `codex app-server` and the conversation it holds.
struct Codex {
    stdin: Lines<ChildStdin>,
    child: Mutex<Child>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Result<Value, Value>>>>,
    /// Notifications and server requests, in order.
    events: Mutex<Receiver<Value>>,
}

impl Codex {
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        let previous = self.pending.lock().expect("pending lock").insert(id, tx);
        debug_assert!(
            previous.is_none(),
            "request ids come from a counter: never reused"
        );
        self.stdin
            .send(&json!({ "id": id, "method": method, "params": params }));
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Codex refused the request")
                .to_string()),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(format!("Codex did not answer {method}"))
            }
        }
    }

    fn reply(&self, id: &Value, result: Value) {
        self.stdin.send(&json!({ "id": id, "result": result }));
    }
}

impl Drop for Codex {
    fn drop(&mut self) {
        // Codex may have exited already (kill then fails, and wait returns
        // its status): either way it is gone, which is all that is wanted.
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// The most of one command's output kept while it runs.  Axel's host stores
/// 16 KiB of a tool's output; 1 MiB leaves room for a client that shows
/// more, without holding a runaway build log whole.
const COMMAND_OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;

/// A running command's output, up to [`COMMAND_OUTPUT_LIMIT_BYTES`].
#[derive(Default)]
struct CommandOutput {
    text: String,
    is_cut: bool,
}

impl CommandOutput {
    fn push(&mut self, delta: &str) {
        if self.is_cut {
            return;
        }
        let room_bytes = COMMAND_OUTPUT_LIMIT_BYTES.saturating_sub(self.text.len());
        if delta.len() <= room_bytes {
            self.text.push_str(delta);
            return;
        }
        let mut end = room_bytes;
        while !delta.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&delta[..end]);
        self.text.push_str("\n[output cut]\n");
        self.is_cut = true;
    }
}

/// Per-item state while a turn runs (one turn's items, dropped when it ends).
#[derive(Default)]
struct Items {
    /// Output collected per command, shown when it ends.
    output: HashMap<String, CommandOutput>,
    extension: HashMap<String, Option<String>>,
    /// Items whose text arrived as deltas (so the full text isn't sent again).
    streamed: HashSet<String>,
    /// The latest copy of each item, for approvals that name it by id.
    latest: HashMap<String, Value>,
}

struct Session {
    id: String,
    codex: Arc<Codex>,
    mode: Mutex<Mode>,
    model: Mutex<Option<String>>,
    turn: Mutex<Option<String>>,
    /// What each turn spent, from Codex's token reports.
    meter: Mutex<CodexMeter>,
}

struct Agent {
    client: Arc<Client>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

/// Run the agent on stdin/stdout until the client closes stdin.
pub fn serve_stdio() -> anyhow::Result<()> {
    let client = Arc::new(Client::stdio());
    let agent = Arc::new(Agent {
        client: client.clone(),
        sessions: Mutex::new(HashMap::new()),
    });
    for line in bounded_lines(std::io::stdin().lock(), "axel codex-acp") {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = msg.get("method").and_then(Value::as_str).map(String::from) else {
            client.on_response(&msg);
            continue;
        };
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let agent = agent.clone();
        std::thread::spawn(move || {
            let result = agent.handle(&method, params);
            match id {
                Some(id) => agent.client.respond(&id, result),
                None => {
                    if let Err(e) = result {
                        eprintln!("[axel codex-acp] {method}: {}", e.message);
                    }
                }
            }
        });
    }
    agent.sessions.lock().expect("sessions lock").clear();
    Ok(())
}

/// The session modes, by the ids the host asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
}

impl Mode {
    /// The ACP mode id.
    fn as_str(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::AcceptEdits => "acceptEdits",
            Mode::Plan => "plan",
            Mode::BypassPermissions => "bypassPermissions",
        }
    }

    /// A mode id: ours, or one of codex-acp's preset ids.
    fn parse(id: &str) -> Option<Mode> {
        Some(match id {
            "default" => Mode::Default,
            "acceptEdits" | "auto" => Mode::AcceptEdits,
            "plan" | "read-only" => Mode::Plan,
            "bypassPermissions" | "full-access" => Mode::BypassPermissions,
            _ => return None,
        })
    }
}

/// Approval policy and sandbox for a mode: the policy, the sandbox as a
/// turn takes it, and as a thread takes it.
fn policy(mode: Mode) -> (&'static str, Value, &'static str) {
    match mode {
        // Never asks, no sandbox: Axel's autopilot (its guard still applies).
        Mode::BypassPermissions => (
            "never",
            json!({ "type": "dangerFullAccess" }),
            "danger-full-access",
        ),
        // Edits inside the workspace go through; anything else asks.
        Mode::AcceptEdits => (
            "on-request",
            json!({ "type": "workspaceWrite" }),
            "workspace-write",
        ),
        // Read-only: look and propose.
        Mode::Plan => ("on-request", json!({ "type": "readOnly" }), "read-only"),
        // Ask before anything that isn't known to be safe.
        Mode::Default => (
            "untrusted",
            json!({ "type": "workspaceWrite" }),
            "workspace-write",
        ),
    }
}

fn modes(current: Mode) -> Value {
    json!({
        "currentModeId": current.as_str(),
        "availableModes": [
            { "id": "default", "name": "Default", "description": "Asks before anything that isn't known to be safe" },
            { "id": "acceptEdits", "name": "Accept Edits", "description": "Edits inside the workspace go through; anything else asks" },
            { "id": "plan", "name": "Read Only", "description": "Read-only sandbox: looks and proposes" },
            { "id": "bypassPermissions", "name": "Full Access", "description": "No sandbox and never asks" }
        ]
    })
}

impl Agent {
    fn handle(self: &Arc<Self>, method: &str, params: Value) -> RpcResult {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "promptCapabilities": { "image": true, "embeddedContext": true },
                    "mcpCapabilities": { "http": true },
                    "loadSession": true,
                    // Messages can be sent into a running turn (`_axel/steer`).
                    "_meta": { "axel": { "steer": true } }
                },
                "agentInfo": { "name": "axel-codex-acp", "title": "Codex", "version": env!("CARGO_PKG_VERSION") },
                "authMethods": [{ "id": "codex-login", "name": "Log in with Codex", "description": "Run `codex login` in the terminal" }]
            })),
            "authenticate" => Err(RpcError::invalid("Method not implemented.")),
            "session/new" => self.open(&params, None),
            "session/load" => {
                let sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let reply = self.open(&params, Some(sid))?;
                Ok(json!({ "modes": reply.get("modes"), "models": reply.get("models") }))
            }
            "session/prompt" => self.prompt(&params),
            // A message into the running turn: Codex adds it to the turn's
            // input (`turn/steer`), so it has landed once accepted.
            "_axel/steer" => {
                let s = self.session(&params)?;
                let Some(turn) = s.turn.lock().expect("turn lock").clone() else {
                    return Ok(json!({ "accepted": false }));
                };
                let message_id = params
                    .get("messageId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let steer = json!({ "threadId": s.id, "expectedTurnId": turn, "input": prompt_input(&params), "clientUserMessageId": message_id });
                match s.codex.request("turn/steer", steer, REQUEST_TIMEOUT) {
                    Ok(_) => {
                        self.client.notify(
                            "_axel/steered",
                            json!({ "sessionId": s.id, "messageId": message_id }),
                        );
                        Ok(json!({ "accepted": true }))
                    }
                    // The turn ended (or another started) in the meantime.
                    Err(_) => Ok(json!({ "accepted": false })),
                }
            }
            // Codex takes a steer in at once: there's nothing left to withdraw.
            "_axel/unsteer" => Ok(json!({ "withdrawn": false })),
            "session/cancel" => {
                let s = self.session(&params)?;
                let turn = s.turn.lock().expect("turn lock").clone();
                if let Some(turn) = turn {
                    s.codex
                        .request(
                            "turn/interrupt",
                            json!({ "threadId": s.id, "turnId": turn }),
                            REQUEST_TIMEOUT,
                        )
                        .map_err(RpcError::invalid)?;
                }
                Ok(Value::Null)
            }
            "session/set_mode" => {
                let s = self.session(&params)?;
                let mode = params
                    .get("modeId")
                    .and_then(Value::as_str)
                    .and_then(Mode::parse)
                    .ok_or_else(|| RpcError::invalid("Invalid Mode"))?;
                *s.mode.lock().expect("mode lock") = mode;
                Ok(json!({}))
            }
            "session/set_model" => {
                let s = self.session(&params)?;
                let model = params
                    .get("modelId")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                    .map(String::from);
                *s.model.lock().expect("model lock") = model;
                Ok(json!({}))
            }
            other => Err(RpcError::not_found(other)),
        }
    }

    fn session(&self, params: &Value) -> Result<Arc<Session>, RpcError> {
        let sid = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("");
        self.sessions
            .lock()
            .expect("sessions lock")
            .get(sid)
            .cloned()
            .ok_or_else(|| RpcError::invalid("Session not found"))
    }

    /// Start `codex app-server` and a thread on it (or resume one).
    fn open(self: &Arc<Self>, params: &Value, resume: Option<String>) -> RpcResult {
        let cwd = PathBuf::from(params.get("cwd").and_then(Value::as_str).unwrap_or("."));
        let codex = spawn_codex(&cwd)?;
        codex
            .request("initialize", json!({ "clientInfo": { "name": "axel", "title": "Axel", "version": env!("CARGO_PKG_VERSION") } }), START_TIMEOUT)
            .map_err(RpcError::internal)?;
        codex.stdin.send(&json!({ "method": "initialized" }));

        let mut thread = thread_params(params, &cwd);
        let started = match &resume {
            Some(id) => {
                thread["threadId"] = json!(id);
                codex.request("thread/resume", thread, THREAD_TIMEOUT)
            }
            None => codex.request("thread/start", thread, THREAD_TIMEOUT),
        }
        .map_err(|e| {
            if resume.is_some() {
                RpcError::invalid(format!("Session not found: {e}"))
            } else {
                RpcError::internal(e)
            }
        })?;
        let thread_id = started
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let current_model = started
            .get("model")
            .and_then(Value::as_str)
            .map(String::from);

        let session = Arc::new(Session {
            id: thread_id.clone(),
            codex: codex.clone(),
            mode: Mutex::new(Mode::Default),
            model: Mutex::new(None),
            turn: Mutex::new(None),
            meter: Mutex::new(CodexMeter::new(current_model.clone())),
        });
        self.sessions
            .lock()
            .expect("sessions lock")
            .insert(thread_id.clone(), session.clone());
        self.read_limits_later(&codex);
        if resume.is_some() {
            let mut items = Items::default();
            for turn in started
                .pointer("/thread/turns")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                for item in turn
                    .get("items")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    self.replay(&session, &mut items, item);
                }
            }
        }
        let models = list_models(&codex, current_model);
        self.send_commands_later(&thread_id);
        Ok(json!({
            "sessionId": thread_id,
            "modes": modes(Mode::Default),
            "models": models,
        }))
    }

    /// Where the plan's limits stand, without holding up the session.
    fn read_limits_later(&self, codex: &Arc<Codex>) {
        let (codex, client) = (codex.clone(), self.client.clone());
        std::thread::spawn(move || {
            match codex.request("account/rateLimits/read", Value::Null, REQUEST_TIMEOUT) {
                Ok(reply) => {
                    let limits = codex_limits(&reply);
                    if !limits.is_empty() {
                        client.notify("_axel/limits", limits_params(&limits));
                    }
                }
                // The limits show once Codex reports them on its own.
                Err(e) => eprintln!("[axel codex-acp] account/rateLimits/read: {e}"),
            }
        });
    }

    /// codex-acp's slash commands, once the session reply is out.
    fn send_commands_later(&self, session_id: &str) {
        let client = self.client.clone();
        let sid = session_id.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(COMMANDS_DELAY);
            client.update(&sid, json!({ "sessionUpdate": "available_commands_update", "availableCommands": commands() }));
        });
    }

    /// One prompt: a turn (or a review or compaction), relayed until it ends.
    fn prompt(&self, params: &Value) -> RpcResult {
        let s = self.session(params)?;
        let input = prompt_input(params);
        let text = input
            .first()
            .and_then(|i| i.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let (approval, sandbox, _) = policy(*s.mode.lock().expect("mode lock"));

        let started = if slash(&text, "compact").is_some() {
            s.codex
                .request(
                    "thread/compact/start",
                    json!({ "threadId": s.id }),
                    START_TIMEOUT,
                )
                .map_err(RpcError::internal)?;
            None
        } else if let Some(target) = review_target(&text) {
            Some(
                s.codex
                    .request(
                        "review/start",
                        json!({ "threadId": s.id, "target": target }),
                        START_TIMEOUT,
                    )
                    .map_err(RpcError::internal)?,
            )
        } else {
            let input = if slash(&text, "init").is_some() {
                vec![json!({ "type": "text", "text": INIT_PROMPT, "text_elements": [] })]
            } else {
                input
            };
            let mut turn = json!({ "threadId": s.id, "input": input, "approvalPolicy": approval, "sandboxPolicy": sandbox });
            if let Some(m) = s.model.lock().expect("model lock").clone() {
                turn["model"] = json!(m);
            }
            Some(
                s.codex
                    .request("turn/start", turn, START_TIMEOUT)
                    .map_err(RpcError::internal)?,
            )
        };
        let turn_id = started
            .as_ref()
            .and_then(|r| r.pointer("/turn/id"))
            .and_then(Value::as_str)
            .map(String::from);
        *s.turn.lock().expect("turn lock") = turn_id.clone();
        // The turn's model, for the progress reports sent while it runs.
        if let Some(m) = s.model.lock().expect("model lock").clone() {
            s.meter.lock().expect("meter lock").model = Some(m);
        }
        let result = self.relay(&s, turn_id.as_deref());
        *s.turn.lock().expect("turn lock") = None;
        // The turn's spend goes in its reply; a failed one's counts toward the next.
        let reply = result?;
        let mut meter = s.meter.lock().expect("meter lock");
        if let Some(m) = s.model.lock().expect("model lock").clone() {
            meter.model = Some(m);
        }
        Ok(meter.take().into_reply(reply))
    }

    /// Relay Codex's events for a turn to the client until `turn/completed`.
    fn relay(&self, s: &Session, turn_id: Option<&str>) -> RpcResult {
        let events = s.codex.events.lock().expect("events lock");
        let mut items = Items::default();
        loop {
            let Ok(ev) = events.recv() else {
                return Err(RpcError::internal("Codex exited"));
            };
            let method = ev
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let p = ev.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = ev.get("id").cloned() {
                self.answer(s, &mut items, &method, &id, &p);
                continue;
            }
            if let Some(end) = self.relay_event(s, &mut items, &method, &p, turn_id) {
                return end;
            }
        }
    }

    /// One of Codex's notifications.  `Some` when it ends the turn.
    fn relay_event(
        &self,
        s: &Session,
        items: &mut Items,
        method: &str,
        p: &Value,
        turn_id: Option<&str>,
    ) -> Option<RpcResult> {
        let delta = || p.get("delta").and_then(Value::as_str).unwrap_or("");
        let item_id = || {
            p.get("itemId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match method {
            "item/started" => self.item_started(s, items, p.get("item").unwrap_or(&Value::Null)),
            "item/completed" => {
                self.item_completed(s, items, p.get("item").unwrap_or(&Value::Null))
            }
            "item/agentMessage/delta" => {
                items.streamed.insert(item_id());
                self.text(s, "agent_message_chunk", delta());
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                items.streamed.insert(item_id());
                self.text(s, "agent_thought_chunk", delta());
            }
            "item/reasoning/summaryPartAdded" => self.text(s, "agent_thought_chunk", "\n\n"),
            "item/commandExecution/outputDelta" => {
                items.output.entry(item_id()).or_default().push(delta())
            }
            "turn/plan/updated" => self.client.update(
                &s.id,
                json!({ "sessionUpdate": "plan", "entries": plan_entries(p) }),
            ),
            "account/rateLimits/updated" => self.notify_limits(p),
            "thread/tokenUsage/updated" => self.on_token_usage(s, p),
            "warning" | "guardianWarning" => {
                if let Some(m) = p.get("message").and_then(Value::as_str) {
                    self.text(s, "agent_message_chunk", m);
                }
            }
            "thread/compacted" if turn_id.is_none() => {
                return Some(Ok(json!({ "stopReason": "end_turn" })));
            }
            "turn/completed" => return turn_end(p, turn_id),
            _ => {}
        }
        None
    }

    /// A token report: the turn's spend so far, and how full the context is.
    fn on_token_usage(&self, s: &Session, p: &Value) {
        let usage = p.get("tokenUsage").cloned().unwrap_or(Value::Null);
        let progress = {
            let mut meter = s.meter.lock().expect("meter lock");
            meter.observe(&usage);
            meter.progress(std::time::Instant::now())
        };
        // A long turn's spend shows as it happens, not at its end.
        if let Some(p) = progress {
            self.client
                .notify("_axel/usage", p.into_reply(json!({ "sessionId": s.id })));
        }
        if let Some(size) = usage.get("modelContextWindow").and_then(Value::as_u64) {
            let used = usage
                .pointer("/last/totalTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.client.update(
                &s.id,
                json!({ "sessionUpdate": "usage_update", "used": used, "size": size }),
            );
        }
    }

    /// Tell the host where the plan's limits stand.
    fn notify_limits(&self, params: &Value) {
        let limits = codex_limits(params);
        if !limits.is_empty() {
            self.client.notify("_axel/limits", limits_params(&limits));
        }
    }

    fn text(&self, s: &Session, kind: &str, text: &str) {
        if !text.is_empty() {
            self.client.update(
                &s.id,
                json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } }),
            );
        }
    }

    fn item_started(&self, s: &Session, items: &mut Items, item: &Value) {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        items.latest.insert(id.clone(), item.clone());
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "commandExecution" => {
                let c = tools::command(item);
                items.extension.insert(id.clone(), c.extension.clone());
                self.client.update(
                    &s.id,
                    json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": c.title, "kind": c.kind, "status": "in_progress", "locations": c.locations, "rawInput": item }),
                );
            }
            "fileChange" => {
                let (title, locations, content) = tools::file_changes(item);
                self.client.update(
                    &s.id,
                    json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "edit", "status": "in_progress", "locations": locations, "content": content, "rawInput": item }),
                );
            }
            "mcpToolCall" => {
                let title = format!(
                    "Tool: {}/{}",
                    item.get("server").and_then(Value::as_str).unwrap_or(""),
                    item.get("tool").and_then(Value::as_str).unwrap_or("")
                );
                let raw = json!({ "server": item.get("server"), "tool": item.get("tool"), "arguments": item.get("arguments") });
                self.client.update(&s.id, json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "status": "in_progress", "rawInput": raw }));
            }
            "webSearch" => {
                self.client.update(&s.id, json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": "Searching the Web", "kind": "fetch" }));
            }
            _ => {}
        }
    }

    fn item_completed(&self, s: &Session, items: &mut Items, item: &Value) {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        items.latest.insert(id.clone(), item.clone());
        let failed = |status: Option<&str>| matches!(status, Some("failed" | "declined"));
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "agentMessage" if !items.streamed.remove(&id) => {
                self.text(
                    s,
                    "agent_message_chunk",
                    item.get("text").and_then(Value::as_str).unwrap_or(""),
                );
            }
            "reasoning" if !items.streamed.remove(&id) => {
                let summary: Vec<&str> = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                self.text(s, "agent_thought_chunk", &summary.join("\n\n"));
            }
            "commandExecution" => self.command_completed(s, items, &id, item),
            "fileChange" => {
                let (title, locations, content) = tools::file_changes(item);
                let status = if failed(item.get("status").and_then(Value::as_str)) {
                    "failed"
                } else {
                    "completed"
                };
                self.client.update(
                    &s.id,
                    json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": status, "rawOutput": item, "title": title, "locations": locations, "content": content }),
                );
            }
            "mcpToolCall" => {
                let result = item.get("result").cloned().unwrap_or(Value::Null);
                let is_error = item.get("error").is_some_and(|e| !e.is_null())
                    || result
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                let mut u = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": if is_error { "failed" } else { "completed" },
                    "rawOutput": if result.is_null() { item.get("error").cloned().unwrap_or(Value::Null) } else { result.clone() },
                });
                let content = tools::mcp_content(&result);
                if !content.is_empty() {
                    u["content"] = json!(content);
                }
                self.client.update(&s.id, u);
            }
            "webSearch" => {
                self.client.update(
                    &s.id,
                    json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed", "title": tools::web_search_title(item), "rawInput": { "query": item.get("query"), "action": item.get("action") } }),
                );
            }
            "imageView" => {
                let path = item.get("path").and_then(Value::as_str).unwrap_or("");
                self.client.update(
                    &s.id,
                    json!({
                        "sessionUpdate": "tool_call", "toolCallId": id, "title": format!("View Image {path}"), "kind": "read", "status": "completed",
                        "content": [{ "type": "content", "content": { "type": "resource_link", "name": path, "uri": path } }],
                        "locations": [{ "path": path }],
                    }),
                );
            }
            "contextCompaction" => self.text(s, "agent_message_chunk", "Context compacted\n"),
            _ => {}
        }
    }

    /// A command that ended: its status, and the output collected while it ran.
    fn command_completed(&self, s: &Session, items: &mut Items, id: &str, item: &Value) {
        let status = item.get("status").and_then(Value::as_str);
        let failed = matches!(status, Some("failed" | "declined"));
        let mut output = items.output.remove(id).map(|o| o.text).unwrap_or_default();
        if output.is_empty() {
            output = item
                .get("aggregatedOutput")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        let ext = items.extension.remove(id).flatten();
        let mut u = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            // codex-acp: completed, or exited 0; failed and declined fail.
            "status": if status == Some("completed") || item.get("exitCode").and_then(Value::as_i64) == Some(0) || !failed { "completed" } else { "failed" },
            "rawOutput": item,
        });
        if let Some(c) = tools::command_output(&output, ext.as_deref()) {
            u["content"] = json!([c]);
        }
        self.client.update(&s.id, u);
    }

    /// A saved item, for `session/load`: messages as chunks, tools as finished calls.
    fn replay(&self, s: &Session, items: &mut Items, item: &Value) {
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "userMessage" => {
                for c in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        self.text(s, "user_message_chunk", t);
                    }
                }
            }
            "agentMessage" | "reasoning" | "contextCompaction" => {
                self.item_completed(s, items, item)
            }
            "commandExecution" | "fileChange" | "mcpToolCall" | "webSearch" => {
                self.item_started(s, items, item);
                self.item_completed(s, items, item);
            }
            "imageView" => self.item_completed(s, items, item),
            _ => {}
        }
    }

    /// Answer one of Codex's requests: approvals go to the client.
    fn answer(&self, s: &Session, items: &mut Items, method: &str, id: &Value, p: &Value) {
        let item_id = p
            .get("itemId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match method {
            "item/commandExecution/requestApproval" => {
                let item = items.latest.get(&item_id).cloned().unwrap_or_else(|| json!({ "command": p.get("command"), "cwd": p.get("cwd"), "commandActions": p.get("commandActions") }));
                let c = tools::command(&item);
                let options = tools::exec_options(p);
                let mut tool_call = json!({ "toolCallId": item_id, "title": c.title, "kind": c.kind, "status": "pending", "rawInput": p });
                if !c.locations.is_empty() {
                    tool_call["locations"] = json!(c.locations);
                }
                if let Some(text) = tools::exec_approval_text(p) {
                    tool_call["content"] = json!([{ "type": "content", "content": { "type": "text", "text": text } }]);
                }
                let acp_options: Vec<Value> = options.iter().map(|o| json!({ "optionId": o.id, "name": o.name, "kind": o.kind })).collect();
                let picked = self.ask(s, &tool_call, acp_options);
                let decision = picked.and_then(|p| options.iter().find(|o| o.id == p).map(|o| o.decision.clone())).unwrap_or(json!("cancel"));
                s.codex.reply(id, json!({ "decision": decision }));
            }
            "item/fileChange/requestApproval" => {
                let item = items.latest.get(&item_id).cloned().unwrap_or(Value::Null);
                let (title, locations, mut content) = tools::file_changes(&item);
                if let Some(r) = p.get("reason").and_then(Value::as_str) {
                    content.push(json!({ "type": "content", "content": { "type": "text", "text": r } }));
                }
                let tool_call = json!({ "toolCallId": item_id, "title": title, "kind": "edit", "status": "pending", "locations": locations, "content": content, "rawInput": p });
                let picked = self.ask(
                    s,
                    &tool_call,
                    vec![
                        json!({ "optionId": "approved", "name": t!("Yes"), "kind": "allow_once" }),
                        json!({ "optionId": "abort", "name": t!("No, provide feedback"), "kind": "reject_once" }),
                    ],
                );
                let decision = if picked.as_deref() == Some("approved") { "accept" } else { "cancel" };
                s.codex.reply(id, json!({ "decision": decision }));
            }
            "item/permissions/requestApproval" => {
                let mut text = Vec::new();
                if let Some(r) = p.get("reason").and_then(Value::as_str) {
                    text.push(r.to_string());
                }
                text.push(t!("Permissions: {permissions}", permissions = p.get("permissions").map(|v| v.to_string()).unwrap_or_default()));
                let tool_call = json!({ "toolCallId": item_id, "title": "Request permissions", "status": "pending", "rawInput": p,
                    "content": [{ "type": "content", "content": { "type": "text", "text": text.join("\n") } }] });
                let picked = self.ask(
                    s,
                    &tool_call,
                    vec![
                        json!({ "optionId": "approved", "name": t!("Yes, for this turn"), "kind": "allow_once" }),
                        json!({ "optionId": "approved-for-session", "name": t!("Yes, for this session"), "kind": "allow_always" }),
                        json!({ "optionId": "denied", "name": t!("No"), "kind": "reject_once" }),
                    ],
                );
                let grant = match picked.as_deref() {
                    Some("approved") => json!({ "permissions": p.get("permissions"), "scope": "turn" }),
                    Some("approved-for-session") => json!({ "permissions": p.get("permissions"), "scope": "session" }),
                    _ => json!({ "permissions": {} }),
                };
                s.codex.reply(id, grant);
            }
            // Questions to the user and MCP elicitations can't be answered over ACP.
            "item/tool/requestUserInput" => s.codex.reply(id, json!({ "answers": {} })),
            "mcpServer/elicitation/request" => s.codex.reply(id, json!({ "action": "decline" })),
            other => s.codex.stdin.send(&json!({ "id": id, "error": { "code": -32601, "message": format!("axel does not handle {other}") } })),
        }
    }

    /// `session/request_permission`: the option picked, or `None` if cancelled.
    fn ask(&self, s: &Session, tool_call: &Value, options: Vec<Value>) -> Option<String> {
        // A failed request (the client went away) is a cancelled one: Codex
        // is told no, which is the safe answer.
        let reply = self
            .client
            .request(
                "session/request_permission",
                json!({ "sessionId": s.id, "toolCall": tool_call, "options": options }),
            )
            .ok()?;
        let outcome = reply.get("outcome")?;
        (outcome.get("outcome").and_then(Value::as_str) == Some("selected"))
            .then(|| {
                outcome
                    .get("optionId")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .flatten()
    }
}

/// `codex app-server` in `cwd`, with a reader that routes its messages.
fn spawn_codex(cwd: &PathBuf) -> Result<Arc<Codex>, RpcError> {
    let exe = std::env::var("CODEX_EXECUTABLE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "codex".into());
    let provider = inspector_provider_args(std::env::var("AXEL_CODEX_BASE_URL").ok().as_deref());
    let mut child = Command::new(&exe)
        .arg("app-server")
        .args(provider)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| RpcError::internal(format!("starting {exe} app-server: {e}")))?;
    let stdin = child.stdin.take().expect("stdin was set to piped above");
    let stdout = child.stdout.take().expect("stdout was set to piped above");
    let (tx, rx) = mpsc::channel();
    let codex = Arc::new(Codex {
        stdin: Lines::new(stdin),
        child: Mutex::new(child),
        next_id: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        events: Mutex::new(rx),
    });
    let reader = codex.clone();
    std::thread::spawn(move || {
        for line in bounded_lines(BufReader::new(stdout), "axel codex-acp").map_while(Result::ok) {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("method").is_none() {
                let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                if let Some(waiter) = reader.pending.lock().expect("pending lock").remove(&id) {
                    // The requester timed out meanwhile: nobody to tell.
                    let _ = waiter.send(match msg.get("error") {
                        Some(e) => Err(e.clone()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    });
                }
            } else if tx.send(msg).is_err() {
                break;
            }
        }
        reader.pending.lock().expect("pending lock").clear();
    });
    Ok(codex)
}

/// `-c` flags that send Codex's model requests through the session's
/// inspector proxy (ADR 0011), as a custom model provider: Codex ignores
/// `OPENAI_BASE_URL` with a ChatGPT login, but a provider that
/// `requires_openai_auth` keeps the login and sends it to `base_url`.  Only
/// a loopback `http` URL of plain characters is used, so nothing in it can
/// break out of the TOML string; anything else runs Codex direct.
fn inspector_provider_args(base_url: Option<&str>) -> Vec<String> {
    let Some(url) = base_url.filter(|u| !u.is_empty()) else {
        return Vec::new();
    };
    let is_plain = url
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'/' | b'.' | b'-' | b'_'));
    if !is_plain || !url.starts_with("http://127.0.0.1:") {
        return Vec::new();
    }
    [
        "model_provider=axel".to_string(),
        "model_providers.axel.name=\"axel\"".to_string(),
        format!("model_providers.axel.base_url=\"{url}\""),
        "model_providers.axel.wire_api=\"responses\"".to_string(),
        "model_providers.axel.requires_openai_auth=true".to_string(),
    ]
    .into_iter()
    .flat_map(|setting| ["-c".to_string(), setting])
    .collect()
}

/// The thread's settings: approval and sandbox for the default mode, the
/// instructions, MCP servers and reasoning effort the host asks for.
fn thread_params(params: &Value, cwd: &Path) -> Value {
    let (approval, _, sandbox) = policy(Mode::Default);
    let mut thread = json!({ "cwd": cwd, "approvalPolicy": approval, "sandbox": sandbox });
    if let Some(append) = params
        .pointer("/_meta/systemPrompt/append")
        .and_then(Value::as_str)
    {
        thread["developerInstructions"] = json!(append);
    } else if let Some(sp) = params
        .pointer("/_meta/systemPrompt")
        .and_then(Value::as_str)
    {
        thread["baseInstructions"] = json!(sp);
    }
    let mcp = mcp_config(params);
    if !mcp.is_empty() {
        thread["config"] = json!({ "mcp_servers": mcp });
    }
    // The agent's reasoning effort, from the host (`_meta.axel.effort`):
    // the same override as `-c model_reasoning_effort=LEVEL`.
    if let Some(effort) = crate::meta_effort(params).filter(|effort| *effort != "max") {
        thread["config"]["model_reasoning_effort"] = json!(effort);
    }
    thread
}

/// The ACP `models` for a session reply: what Codex lists (but hides), and
/// the thread's model, else Codex's default.
fn list_models(codex: &Codex, current_model: Option<String>) -> Value {
    // Without the list the session still runs, on Codex's default model.
    let listed = codex
        .request("model/list", json!({}), START_TIMEOUT)
        .unwrap_or_else(|e| {
            eprintln!("[axel codex-acp] model/list: {e}");
            Value::Null
        });
    let models: Vec<Value> = listed
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| !m.get("hidden").and_then(Value::as_bool).unwrap_or(false))
        .map(|m| json!({ "modelId": m.get("id"), "name": m.get("displayName"), "description": m.get("description") }))
        .collect();
    let current = current_model.or_else(|| {
        listed
            .get("data")
            .and_then(Value::as_array)?
            .iter()
            .find(|m| m.get("isDefault").and_then(Value::as_bool) == Some(true))?
            .get("id")?
            .as_str()
            .map(String::from)
    });
    json!({ "availableModels": models, "currentModelId": current })
}

/// ACP plan entries from `turn/plan/updated`.
fn plan_entries(p: &Value) -> Vec<Value> {
    p.get("plan")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|step| {
            let status = match step.get("status").and_then(Value::as_str) {
                Some("inProgress") => "in_progress",
                Some("completed") => "completed",
                _ => "pending",
            };
            json!({ "content": step.get("step"), "priority": "medium", "status": status })
        })
        .collect()
}

/// How `turn/completed` ends the prompt: `None` when it is another turn's.
fn turn_end(p: &Value, turn_id: Option<&str>) -> Option<RpcResult> {
    let turn = p.get("turn").cloned().unwrap_or(Value::Null);
    let id = turn.get("id").and_then(Value::as_str);
    if turn_id.is_some() && id != turn_id {
        return None;
    }
    Some(match turn.get("status").and_then(Value::as_str) {
        Some("interrupted") => Ok(json!({ "stopReason": "cancelled" })),
        Some("failed") => Err(RpcError::internal(
            turn.pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Codex failed the turn"),
        )),
        _ => Ok(json!({ "stopReason": "end_turn" })),
    })
}

/// The client's MCP servers as Codex `mcp_servers` config.
fn mcp_config(params: &Value) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for server in params
        .get("mcpServers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = server.get("name").and_then(Value::as_str) else {
            continue;
        };
        let pairs = |k: &str| -> serde_json::Map<String, Value> {
            server
                .get(k)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|e| {
                    Some((
                        e.get("name")?.as_str()?.to_string(),
                        e.get("value").cloned().unwrap_or(Value::Null),
                    ))
                })
                .collect()
        };
        let entry = match server.get("type").and_then(Value::as_str) {
            Some("http") | Some("sse") => {
                json!({ "url": server.get("url"), "http_headers": pairs("headers") })
            }
            _ => {
                json!({ "command": server.get("command"), "args": server.get("args").cloned().unwrap_or(json!([])), "env": pairs("env") })
            }
        };
        out.insert(name.to_string(), entry);
    }
    out
}

/// `build_prompt_items`: ACP content blocks as Codex user input.
fn prompt_input(params: &Value) -> Vec<Value> {
    let text = |t: String| json!({ "type": "text", "text": t, "text_elements": [] });
    params
        .get("prompt")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => Some(text(block.get("text").and_then(Value::as_str).unwrap_or("").to_string())),
            "image" => Some(json!({
                "type": "image",
                "url": format!("data:{};base64,{}", block.get("mimeType").and_then(Value::as_str).unwrap_or(""), block.get("data").and_then(Value::as_str).unwrap_or("")),
            })),
            "resource_link" => Some(text(uri_link(block.get("name").and_then(Value::as_str), block.get("uri").and_then(Value::as_str).unwrap_or("")))),
            "resource" => {
                let res = block.get("resource")?;
                let body = res.get("text")?.as_str()?;
                let uri = res.get("uri").and_then(Value::as_str).unwrap_or("");
                Some(text(format!("{}\n<context ref=\"{uri}\">\n{body}\n</context>", uri_link(None, uri))))
            }
            _ => None,
        })
        .collect()
}

/// `format_uri_as_link`.
fn uri_link(name: Option<&str>, uri: &str) -> String {
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        return format!("[@{n}]({uri})");
    }
    if let Some(path) = uri.strip_prefix("file://") {
        return format!("[@{}]({uri})", path.rsplit('/').next().unwrap_or(path));
    }
    if uri.starts_with("zed://") {
        return format!("[@{}]({uri})", uri.rsplit('/').next().unwrap_or(uri));
    }
    uri.to_string()
}

/// `/name rest` → `Some(rest)`.
fn slash<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let rest = text.strip_prefix('/')?.strip_prefix(name)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

/// The review a `/review…` prompt asks for.
fn review_target(text: &str) -> Option<Value> {
    if let Some(branch) = slash(text, "review-branch") {
        return Some(json!({ "type": "baseBranch", "branch": branch }));
    }
    if let Some(sha) = slash(text, "review-commit") {
        return Some(json!({ "type": "commit", "sha": sha }));
    }
    let rest = slash(text, "review")?;
    Some(if rest.is_empty() {
        json!({ "type": "uncommittedChanges" })
    } else {
        json!({ "type": "custom", "instructions": rest })
    })
}

/// codex-acp's built-in slash commands (less `logout`, which Axel doesn't offer).
fn commands() -> Value {
    json!([
        { "name": "review", "description": "Review my current changes and find issues", "input": { "hint": "optional custom review instructions" } },
        { "name": "review-branch", "description": "Review the code changes against a specific branch", "input": { "hint": "branch name" } },
        { "name": "review-commit", "description": "Review the code changes introduced by a commit", "input": { "hint": "commit sha" } },
        { "name": "init", "description": "create an AGENTS.md file with instructions for Codex", "input": null },
        { "name": "compact", "description": "summarize conversation to prevent hitting the context limit", "input": null }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inspector_provider_is_set_only_for_a_plain_loopback_url() {
        let url = "http://127.0.0.1:50123/0123abcd/openai";
        let args = inspector_provider_args(Some(url));
        assert_eq!(args.len(), 10);
        assert!(args.chunks(2).all(|pair| pair[0] == "-c"));
        assert_eq!(args[1], "model_provider=axel");
        assert_eq!(args[5], format!("model_providers.axel.base_url=\"{url}\""));
        assert!(args.contains(&"model_providers.axel.requires_openai_auth=true".to_string()));
        for bad in [
            None,
            Some(""),
            Some("https://evil.example/openai"),
            Some("http://127.0.0.1:1/x\"\nmodel=\"y"),
            Some("http://127.0.0.1.evil:1/x"),
        ] {
            assert!(inspector_provider_args(bad).is_empty(), "{bad:?}");
        }
    }

    #[test]
    fn modes_map_to_policies() {
        assert_eq!(policy(Mode::BypassPermissions).0, "never");
        assert_eq!(
            policy(Mode::BypassPermissions).1,
            json!({ "type": "dangerFullAccess" })
        );
        assert_eq!(
            (policy(Mode::Plan).0, policy(Mode::Plan).2),
            ("on-request", "read-only")
        );
        assert_eq!(
            (policy(Mode::AcceptEdits).0, policy(Mode::AcceptEdits).2),
            ("on-request", "workspace-write")
        );
        assert_eq!(policy(Mode::Default).0, "untrusted");
        // codex-acp's preset ids name the same modes.
        assert_eq!(Mode::parse("full-access"), Some(Mode::BypassPermissions));
        assert_eq!(Mode::parse("auto"), Some(Mode::AcceptEdits));
        assert_eq!(Mode::parse("read-only"), Some(Mode::Plan));
        assert_eq!(Mode::parse("yolo"), None);
        for mode in [
            Mode::Default,
            Mode::AcceptEdits,
            Mode::Plan,
            Mode::BypassPermissions,
        ] {
            assert_eq!(Mode::parse(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn prompts_and_slash_commands() {
        let input = prompt_input(&json!({ "prompt": [
            { "type": "text", "text": "hi" },
            { "type": "image", "mimeType": "image/png", "data": "AA" },
            { "type": "resource_link", "name": "a.rs", "uri": "file:///w/a.rs" },
            { "type": "resource", "resource": { "uri": "file:///w/n.md", "text": "notes" } }
        ] }));
        assert_eq!(
            input[0],
            json!({ "type": "text", "text": "hi", "text_elements": [] })
        );
        assert_eq!(
            input[1],
            json!({ "type": "image", "url": "data:image/png;base64,AA" })
        );
        assert_eq!(input[2]["text"], "[@a.rs](file:///w/a.rs)");
        assert_eq!(
            input[3]["text"],
            "[@n.md](file:///w/n.md)\n<context ref=\"file:///w/n.md\">\nnotes\n</context>"
        );

        assert_eq!(
            review_target("/review"),
            Some(json!({ "type": "uncommittedChanges" }))
        );
        assert_eq!(
            review_target("/review focus on auth"),
            Some(json!({ "type": "custom", "instructions": "focus on auth" }))
        );
        assert_eq!(
            review_target("/review-branch main"),
            Some(json!({ "type": "baseBranch", "branch": "main" }))
        );
        assert_eq!(review_target("/reviewer"), None);
        assert!(slash("/compact", "compact").is_some() && slash("/compactly", "compact").is_none());
    }

    #[test]
    fn mcp_servers_become_codex_config() {
        let c = mcp_config(
            &json!({ "mcpServers": [{ "name": "axel", "command": "/bin/axel", "args": ["mcp"], "env": [{ "name": "A", "value": "1" }] }] }),
        );
        assert_eq!(
            c["axel"],
            json!({ "command": "/bin/axel", "args": ["mcp"], "env": { "A": "1" } })
        );
    }
}
