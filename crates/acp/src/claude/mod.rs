//! A Claude Code ACP agent, in Rust: `mtx claude-acp` (Axel's `axel claude-acp`).
//!
//! It replaces Zed's `claude-code-acp` (TypeScript, on Anthropic's Claude
//! Agent SDK) for Axel.  Both halves are ported, not reinvented:
//!
//! - **Down, to Claude Code:** what the Agent SDK does.  The `claude` CLI is
//!   started with `--output-format stream-json --input-format stream-json
//!   --permission-prompt-tool stdio …`, prompts go in as `user` messages, and
//!   the SDK's control protocol runs over the same pipes: we send
//!   `initialize` (system prompt, hooks), `interrupt`, `set_permission_mode`,
//!   `set_model`; Claude sends `can_use_tool` and `hook_callback`, and
//!   `control_cancel_request` to withdraw one.
//! - **Up, to the ACP client (Axel's session host):** what `claude-code-acp`
//!   0.16.2's `acp-agent.js` does: the same session/update notifications
//!   (message and thought chunks, tool calls with titles, kinds, diffs and
//!   locations from `tools.js`, plans from TodoWrite, slash commands, mode
//!   changes), the same permission options and outcomes, the same stop
//!   reasons, the same session replay on `session/load`.
//!
//! Deliberate differences, all of which Axel doesn't rely on:
//! - Claude uses its **native** Read / Write / Edit / Bash.  The adapter
//!   swaps them for `mcp__acp__*` tools that call back into the client's
//!   `fs/*` methods (for editors with unsaved buffers); Axel's client reads
//!   and writes the disk just like the native tools, and Claude Code's own
//!   permission rules then apply to its own tool names, so the adapter's
//!   settings-rules PreToolUse hook isn't needed either.
//! - Only what Axel calls is implemented: initialize, session/new,
//!   session/load, session/prompt, session/cancel, session/set_mode,
//!   session/set_model.  No session list / fork / resume, which aren't
//!   advertised.
//!
//! The process is synchronous: one thread reads the client, one per session
//! reads Claude, and each request or control request is answered on its own
//! thread so a permission question never blocks the pipes.

pub mod tools;

use std::{
    collections::{HashMap, HashSet},
    io::BufReader,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::Duration,
};

use serde_json::{Map, Value, json};

use crate::{
    agent_rpc::{Client, Lines, RpcError, RpcResult, bounded_lines},
    usage::{ClaudeMeter, claude_limits, limits_params},
};

/// One Claude Code process and what we know about its conversation.
struct Session {
    id: String,
    stdin: Lines<ChildStdin>,
    child: Mutex<Child>,
    /// Everything Claude prints that isn't the control protocol.
    messages: Mutex<Receiver<Value>>,
    /// Our control requests waiting for Claude's answer.
    controls: Mutex<HashMap<String, Sender<Value>>>,
    next_control: AtomicU64,
    /// Control requests Claude withdrew (`control_cancel_request`).
    withdrawn: Mutex<HashSet<String>>,
    permission_mode: Mutex<PermissionMode>,
    cancelled: AtomicBool,
    /// `tool_use` blocks by id, for naming their results.
    tool_uses: Mutex<ToolUses>,
    /// Tool uses whose PostToolUse hook should send the tool response.
    hooked: Mutex<HashSet<String>>,
    /// The turn's steering: whether a prompt is running, and the messages
    /// sent into it that Claude hasn't taken in yet (our uuid → the host's
    /// message id). One lock, so a steer can't slip in as the turn ends.
    steering: Mutex<Steering>,
    /// What the turn spent, from Claude's messages and results.
    meter: Mutex<ClaudeMeter>,
}

/// Messages sent into a running turn (`_axel/steer`). Claude takes each in
/// at the turn's next step and echoes it back (`--replay-user-messages`);
/// one that arrives after the turn's last step starts a turn of its own,
/// which the same prompt waits for.
#[derive(Default)]
struct Steering {
    prompting: bool,
    pending: HashMap<String, String>,
}

impl Session {
    /// A control request to Claude; waits for its `control_response`.
    fn control(&self, request: Value, timeout: Duration) -> Result<Value, String> {
        let n = self.next_control.fetch_add(1, Ordering::SeqCst);
        let request_id = format!("axel-{n}");
        let (tx, rx) = mpsc::channel();
        let previous = self
            .controls
            .lock()
            .expect("controls lock")
            .insert(request_id.clone(), tx);
        debug_assert!(
            previous.is_none(),
            "control request ids come from a counter: never reused"
        );
        self.stdin.send(
            &json!({ "request_id": request_id, "type": "control_request", "request": request }),
        );
        let resp = rx.recv_timeout(timeout).map_err(|_| {
            self.controls
                .lock()
                .expect("controls lock")
                .remove(&request_id);
            "Claude Code did not answer".to_string()
        })?;
        if resp.get("subtype").and_then(Value::as_str) == Some("success") {
            Ok(resp.get("response").cloned().unwrap_or(Value::Null))
        } else {
            Err(resp
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("control request failed")
                .to_string())
        }
    }

    fn mode(&self) -> PermissionMode {
        *self.permission_mode.lock().expect("mode lock")
    }

    fn set_mode(&self, mode: PermissionMode) {
        *self.permission_mode.lock().expect("mode lock") = mode;
    }
}

/// Claude Code's permission modes (`--permission-mode`, `set_permission_mode`),
/// which are also this adapter's ACP session modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    DontAsk,
    BypassPermissions,
}

impl PermissionMode {
    /// Claude Code's name, and the ACP mode id.
    fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::BypassPermissions => "bypassPermissions",
        }
    }

    fn parse(id: &str) -> Option<PermissionMode> {
        Some(match id {
            "default" => PermissionMode::Default,
            "acceptEdits" => PermissionMode::AcceptEdits,
            "plan" => PermissionMode::Plan,
            "dontAsk" => PermissionMode::DontAsk,
            "bypassPermissions" => PermissionMode::BypassPermissions,
            _ => return None,
        })
    }
}

impl Session {
    /// End Claude.  Called when the host goes away: the reader and idle
    /// relay threads hold the session until Claude's stdout closes, so a
    /// `Drop` alone would wait on the very process it is meant to end.
    fn kill_child(&self) {
        // Claude may have exited already (kill then fails, and wait returns
        // its status): either way it is gone, which is all that is wanted.
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill_child();
    }
}

struct Agent {
    client: Arc<Client>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    client_caps: Mutex<Value>,
}

/// The `PostToolUse` hook callback id we register with Claude.
const POST_TOOL_USE_HOOK: &str = "hook_0";

/// How long Claude has to answer a control request (a mode or model
/// switch, an interrupt): it answers at once unless it is stuck.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long Claude has to start up and answer `initialize`: it loads its
/// settings, plugins and MCP servers first.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long withdrawing a steered message may hold up the owner's edit.
const WITHDRAW_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the idle relay looks for messages Claude sent on its own.
const IDLE_RELAY_POLL: Duration = Duration::from_millis(50);
/// The slash commands go out this long after the session reply, so the
/// client knows the session before it hears about it.
const COMMANDS_DELAY: Duration = Duration::from_millis(20);

/// Run the agent on stdin/stdout until the client closes stdin.
pub fn serve_stdio() -> anyhow::Result<()> {
    let client = Arc::new(Client::stdio());
    let agent = Arc::new(Agent {
        client: client.clone(),
        sessions: Mutex::new(HashMap::new()),
        client_caps: Mutex::new(Value::Null),
    });
    let stdin = std::io::stdin();
    for line in bounded_lines(stdin.lock(), "axel claude-acp") {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
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
                        eprintln!("[axel claude-acp] {method}: {}", e.message);
                    }
                }
            }
        });
    }
    // The host is gone: no Claude outlives it.  Threads still holding a
    // session keep it alive, so end its process here, not in `Drop`.
    let sessions: Vec<Arc<Session>> = agent
        .sessions
        .lock()
        .expect("sessions lock")
        .drain()
        .map(|(_, s)| s)
        .collect();
    for session in sessions {
        session.kill_child();
    }
    Ok(())
}

/// Whether the `bypassPermissions` mode works: not as root, unless sandboxed.
fn allow_bypass() -> bool {
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    !root || std::env::var_os("IS_SANDBOX").is_some()
}

impl Agent {
    fn handle(self: &Arc<Self>, method: &str, params: Value) -> RpcResult {
        match method {
            "initialize" => {
                *self.client_caps.lock().expect("caps lock") = params
                    .get("clientCapabilities")
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "promptCapabilities": { "image": true, "embeddedContext": true },
                        "mcpCapabilities": { "http": true, "sse": true },
                        "loadSession": true,
                        // Messages can be sent into a running turn (`_axel/steer`).
                        "_meta": { "axel": { "steer": true } }
                    },
                    "agentInfo": { "name": "axel-claude-acp", "title": "Claude Code", "version": env!("CARGO_PKG_VERSION") },
                    "authMethods": [{ "id": "claude-login", "name": "Log in with Claude Code", "description": "Run `claude /login` in the terminal" }]
                }))
            }
            "authenticate" => Err(RpcError::invalid("Method not implemented.")),
            "session/new" => {
                let state_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(dirs_home);
                if state_dir.join(".claude.json.backup").exists()
                    && !state_dir.join(".claude.json").exists()
                {
                    return Err(RpcError::auth_required());
                }
                let resume = params
                    .pointer("/_meta/claudeCode/options/resume")
                    .and_then(Value::as_str)
                    .map(String::from);
                let (session, reply, commands) = self.create_session(&params, resume)?;
                self.send_commands_later(session, commands);
                Ok(reply)
            }
            "session/load" => {
                let sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let cwd = PathBuf::from(params.get("cwd").and_then(Value::as_str).unwrap_or("."));
                let file = find_session_file(&sid, &cwd)
                    .ok_or_else(|| RpcError::invalid("Session not found"))?;
                let (session, reply, commands) = self.create_session(&params, Some(sid.clone()))?;
                replay(&self.client, &sid, &file);
                self.send_commands_later(session, commands);
                Ok(json!({ "modes": reply.get("modes"), "models": reply.get("models") }))
            }
            "session/prompt" => self.prompt(&params),
            "session/cancel" => self.cancel(&params),
            "_axel/steer" => self.steer(&params),
            "_axel/unsteer" => {
                let s = self.session(&params)?;
                let id = params
                    .get("messageId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                Ok(json!({ "withdrawn": self.withdraw(&s, id) }))
            }
            "session/set_model" => {
                let s = self.session(&params)?;
                let model = params.get("modelId").and_then(Value::as_str).unwrap_or("");
                s.control(
                    json!({ "subtype": "set_model", "model": model }),
                    CONTROL_TIMEOUT,
                )
                .map_err(RpcError::invalid)?;
                Ok(json!({}))
            }
            "session/set_mode" => {
                let s = self.session(&params)?;
                let mode = params
                    .get("modeId")
                    .and_then(Value::as_str)
                    .and_then(PermissionMode::parse)
                    .ok_or_else(|| RpcError::invalid("Invalid Mode"))?;
                s.set_mode(mode);
                s.control(
                    json!({ "subtype": "set_permission_mode", "mode": mode.as_str() }),
                    CONTROL_TIMEOUT,
                )
                .map_err(|e| {
                    RpcError::invalid(if e.is_empty() {
                        "Invalid Mode".into()
                    } else {
                        e
                    })
                })?;
                Ok(json!({}))
            }
            other => Err(RpcError::not_found(other)),
        }
    }

    /// `session/cancel`: interrupt Claude's turn.
    fn cancel(&self, params: &Value) -> RpcResult {
        let s = self.session(params)?;
        s.cancelled.store(true, Ordering::SeqCst);
        // Messages sent into the turn and not taken in yet would start a
        // turn of their own after the interrupt: withdraw them, and tell
        // the host, which keeps them queued.
        let withdrawn = self.withdraw_all(&s);
        if !withdrawn.is_empty() {
            self.client.notify(
                "_axel/unsteered",
                json!({ "sessionId": s.id, "messageIds": withdrawn }),
            );
        }
        s.control(json!({ "subtype": "interrupt" }), CONTROL_TIMEOUT)
            .map_err(RpcError::invalid)?;
        Ok(Value::Null)
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

    /// The slash commands, sent once the session reply is out.
    fn send_commands_later(&self, session: String, commands: Value) {
        let client = self.client.clone();
        std::thread::spawn(move || {
            std::thread::sleep(COMMANDS_DELAY);
            client.update(&session, json!({ "sessionUpdate": "available_commands_update", "availableCommands": available_commands(&commands) }));
        });
    }

    /// Start Claude Code for a session: `createSession` in the adapter.
    fn create_session(
        self: &Arc<Self>,
        params: &Value,
        resume: Option<String>,
    ) -> Result<(String, Value, Value), RpcError> {
        let cwd = PathBuf::from(params.get("cwd").and_then(Value::as_str).unwrap_or("."));
        let session_id = resume.clone().unwrap_or_else(new_uuid);
        let meta = params.get("_meta").cloned().unwrap_or(Value::Null);
        let user_options = meta
            .pointer("/claudeCode/options")
            .cloned()
            .unwrap_or(Value::Null);
        let mcp = mcp_servers(params, &user_options);
        let args = claude_args(params, &user_options, &mcp, resume.as_deref(), &session_id);
        let session = self.start_claude(&cwd, &args, &session_id, resume.is_some())?;
        let init = session
            .control(initialize_request(&meta), INITIALIZE_TIMEOUT)
            .map_err(RpcError::internal)?;
        let models = select_model(&session, &init, &cwd);
        let reply = json!({
            "sessionId": session_id,
            "models": models,
            "modes": { "currentModeId": PermissionMode::Default.as_str(), "availableModes": available_modes() },
        });
        let commands = init
            .get("commands")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok((session_id, reply, commands))
    }

    /// Spawn Claude Code with `args` in `cwd`, and the threads that read it.
    fn start_claude(
        self: &Arc<Self>,
        cwd: &Path,
        args: &[String],
        session_id: &str,
        resumed: bool,
    ) -> Result<Arc<Session>, RpcError> {
        let exe = std::env::var("CLAUDE_CODE_EXECUTABLE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "claude".into());
        let mut cmd = claude_command(&exe);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd
            .spawn()
            .map_err(|e| RpcError::internal(format!("starting {exe}: {e}")))?;
        let stdin = child.stdin.take().expect("stdin was set to piped above");
        let stdout = child.stdout.take().expect("stdout was set to piped above");
        let (tx, rx) = mpsc::channel();
        let session = Arc::new(Session {
            id: session_id.to_string(),
            stdin: Lines::new(stdin),
            child: Mutex::new(child),
            messages: Mutex::new(rx),
            controls: Mutex::new(HashMap::new()),
            next_control: AtomicU64::new(0),
            withdrawn: Mutex::new(HashSet::new()),
            permission_mode: Mutex::new(PermissionMode::Default),
            cancelled: AtomicBool::new(false),
            tool_uses: Mutex::new(ToolUses::default()),
            hooked: Mutex::new(HashSet::new()),
            steering: Mutex::new(Steering::default()),
            meter: Mutex::new(if resumed {
                ClaudeMeter::resumed()
            } else {
                ClaudeMeter::default()
            }),
        });
        self.spawn_reader(session.clone(), stdout, tx);
        self.spawn_idle_relay(session.clone());
        self.sessions
            .lock()
            .expect("sessions lock")
            .insert(session_id.to_string(), session.clone());
        Ok(session)
    }

    /// Read Claude's stdout: control traffic is answered here, the rest goes
    /// to whoever is running a prompt.
    fn spawn_reader(
        self: &Arc<Self>,
        session: Arc<Session>,
        stdout: std::process::ChildStdout,
        tx: Sender<Value>,
    ) {
        let agent = self.clone();
        std::thread::spawn(move || {
            for line in bounded_lines(BufReader::new(stdout), "axel claude-acp") {
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match msg.get("type").and_then(Value::as_str) {
                    Some("control_response") => {
                        let resp = msg.get("response").cloned().unwrap_or(Value::Null);
                        let id = resp
                            .get("request_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if let Some(waiter) =
                            session.controls.lock().expect("controls lock").remove(&id)
                        {
                            // The requester timed out meanwhile: nobody to tell.
                            let _ = waiter.send(resp);
                        }
                    }
                    Some("control_request") => {
                        let (agent, session) = (agent.clone(), session.clone());
                        std::thread::spawn(move || agent.answer_control(&session, &msg));
                    }
                    Some("control_cancel_request") => {
                        if let Some(id) = msg.get("request_id").and_then(Value::as_str) {
                            session
                                .withdrawn
                                .lock()
                                .expect("withdrawn lock")
                                .insert(id.to_string());
                        }
                    }
                    Some("keep_alive")
                    | Some("streamlined_text")
                    | Some("streamlined_tool_use_summary") => {}
                    _ => {
                        if tx.send(msg).is_err() {
                            break;
                        }
                    }
                }
            }
            // Claude exited: anyone waiting on a control request won't get an answer.
            session.controls.lock().expect("controls lock").clear();
        });
    }

    /// Answer one control request from Claude.
    fn answer_control(&self, session: &Session, msg: &Value) {
        let request_id = msg.get("request_id").cloned().unwrap_or(Value::Null);
        let request = msg.get("request").cloned().unwrap_or(Value::Null);
        let result: Result<Value, String> = match request.get("subtype").and_then(Value::as_str) {
            Some("can_use_tool") => self.can_use_tool(session, &request).map(|mut r| {
                r["toolUseID"] = request.get("tool_use_id").cloned().unwrap_or(Value::Null);
                r
            }),
            Some("hook_callback") => Ok(self.hook_callback(session, &request)),
            Some("elicitation") => Ok(self.elicitation(session, &request)),
            Some(other) => Err(format!("Unsupported control request subtype: {other}")),
            None => Err("Unsupported control request subtype: undefined".into()),
        };
        let id = request_id.as_str().unwrap_or("").to_string();
        let withdrawn = session
            .withdrawn
            .lock()
            .expect("withdrawn lock")
            .remove(&id);
        let response = match result {
            Ok(_) if withdrawn => {
                json!({ "subtype": "error", "request_id": request_id, "error": "Tool use aborted" })
            }
            Ok(r) => json!({ "subtype": "success", "request_id": request_id, "response": r }),
            Err(e) => json!({ "subtype": "error", "request_id": request_id, "error": e }),
        };
        session
            .stdin
            .send(&json!({ "type": "control_response", "response": response }));
    }

    /// `canUseTool` from the adapter: ExitPlanMode asks how to proceed;
    /// bypass and accept-edits allow on their own; anything else asks.
    fn can_use_tool(&self, session: &Session, request: &Value) -> Result<Value, String> {
        let tool = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        let tool_use_id = request.get("tool_use_id").cloned().unwrap_or(Value::Null);
        let suggestions = request
            .get("permission_suggestions")
            .filter(|v| !v.is_null())
            .cloned();
        let title = tools::tool_info(tool, &input)
            .get("title")
            .cloned()
            .unwrap_or(Value::Null);
        let tool_call = json!({ "toolCallId": tool_use_id, "rawInput": input, "title": title });
        // A request Claude withdrew while we asked is answered as aborted by `answer_control`.

        if tool == "AskUserQuestion" {
            return self.ask_questions(session, &tool_use_id, input);
        }

        if tool == "ExitPlanMode" {
            return self.exit_plan_mode(session, &tool_call, input, suggestions);
        }

        let allowed_by_mode = match session.mode() {
            PermissionMode::BypassPermissions => true,
            PermissionMode::AcceptEdits => is_edit_tool(tool),
            PermissionMode::Default | PermissionMode::Plan | PermissionMode::DontAsk => false,
        };
        if allowed_by_mode {
            return Ok(json!({
                "behavior": "allow",
                "updatedInput": input,
                "updatedPermissions": suggestions.unwrap_or_else(|| always_rule(tool)),
            }));
        }

        let outcome = self.ask(
            session,
            &tool_call,
            json!([
                { "kind": "allow_always", "name": t!("Always Allow"), "optionId": "allow_always" },
                { "kind": "allow_once", "name": t!("Allow"), "optionId": "allow" },
                { "kind": "reject_once", "name": t!("Reject"), "optionId": "reject" }
            ]),
        )?;
        if outcome.is_none() {
            return Err("Tool use aborted".into());
        }
        Ok(match outcome.as_deref() {
            Some("allow_always") => {
                json!({ "behavior": "allow", "updatedInput": input, "updatedPermissions": suggestions.unwrap_or_else(|| always_rule(tool)) })
            }
            Some("allow") => json!({ "behavior": "allow", "updatedInput": input }),
            _ => {
                json!({ "behavior": "deny", "message": "User refused permission to run tool", "interrupt": true })
            }
        })
    }

    /// ExitPlanMode: ask how to go on, and switch to the mode picked
    /// (accepting edits or not), or keep planning.
    fn exit_plan_mode(
        &self,
        session: &Session,
        tool_call: &Value,
        input: Value,
        suggestions: Option<Value>,
    ) -> Result<Value, String> {
        let outcome = self.ask(
            session,
            tool_call,
            json!([
                { "kind": "allow_always", "name": t!("Yes, and auto-accept edits"), "optionId": PermissionMode::AcceptEdits.as_str() },
                { "kind": "allow_once", "name": t!("Yes, and manually approve edits"), "optionId": PermissionMode::Default.as_str() },
                { "kind": "reject_once", "name": t!("No, keep planning"), "optionId": PermissionMode::Plan.as_str() }
            ]),
        )?;
        let Some(picked) = outcome else {
            return Err("Tool use aborted".into());
        };
        Ok(match PermissionMode::parse(&picked) {
            Some(mode @ (PermissionMode::Default | PermissionMode::AcceptEdits)) => {
                session.set_mode(mode);
                self.client.update(&session.id, json!({ "sessionUpdate": "current_mode_update", "currentModeId": mode.as_str() }));
                json!({
                    "behavior": "allow",
                    "updatedInput": input,
                    "updatedPermissions": suggestions.unwrap_or_else(|| json!([{ "type": "setMode", "mode": mode.as_str(), "destination": "session" }])),
                })
            }
            _ => {
                json!({ "behavior": "deny", "message": "User rejected request to exit plan mode.", "interrupt": true })
            }
        })
    }

    /// `AskUserQuestion`: each question in turn as a permission request whose
    /// options are its choices (ids `answer:<n>`, which tells the host it is a
    /// question for the owner, never a permission its policy may grant), plus
    /// "none of these".  The labels picked go back as the tool's `answers`.
    fn ask_questions(
        &self,
        session: &Session,
        tool_use_id: &Value,
        input: Value,
    ) -> Result<Value, String> {
        let questions = input
            .get("questions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut answers = serde_json::Map::new();
        for (qi, q) in questions.iter().enumerate() {
            let text = q
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let (tool_call, options, labels) = question_request(tool_use_id, qi, q);
            let Some(picked) = self.ask(session, &tool_call, options)? else {
                return Err("Tool use aborted".into());
            };
            match answer_indices(
                &picked,
                labels.len(),
                q.get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ) {
                Some(indices) if !indices.is_empty() => {
                    answers.insert(
                        text,
                        Value::String(
                            indices
                                .iter()
                                .map(|i| labels[*i].clone())
                                .collect::<Vec<_>>()
                                .join(", "),
                        ),
                    );
                }
                _ => {
                    return Ok(json!({
                        "behavior": "deny",
                        "message": "The owner didn't pick any of these options. Ask them in plain text what they want instead.",
                    }));
                }
            }
        }
        let mut updated = input;
        if let Some(obj) = updated.as_object_mut() {
            obj.insert("answers".into(), Value::Object(answers));
        }
        Ok(json!({ "behavior": "allow", "updatedInput": updated }))
    }

    /// An MCP server asks the owner something (`elicitation`), e.g. Axel's
    /// `send` asking whether an agent may reach another.  A form of one
    /// field is asked like a question (see [`elicitation_choice`]); enum arrays
    /// permit multiple selections. Other forms are declined.
    fn elicitation(&self, session: &Session, request: &Value) -> Value {
        if request.get("mode").and_then(Value::as_str) == Some("url") {
            return json!({ "action": "decline" });
        }
        let message = request
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let schema = request
            .get("requested_schema")
            .cloned()
            .unwrap_or(Value::Null);
        let Some(ElicitationChoice {
            field,
            choices,
            multiple,
        }) = elicitation_choice(&schema)
        else {
            return json!({ "action": "decline" });
        };
        let mut options: Vec<Value> = choices
            .iter()
            .enumerate()
            .map(|(i, (_, label))| json!({ "kind": "allow_once", "name": label, "optionId": format!("{ANSWER_PREFIX}{i}") }))
            .collect();
        options.push(json!({ "kind": "reject_once", "name": t!("Decline"), "optionId": format!("{ANSWER_PREFIX}none") }));
        let id = request
            .get("elicitation_id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("elicitation-{}", chrono::Utc::now().timestamp_millis()));
        let server = request
            .get("mcp_server_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let title = if message.is_empty() {
            t!("{server} asks", server = server)
        } else {
            message
        };
        let tool_call = json!({ "toolCallId": id, "title": title, "kind": "think", "rawInput":{"multiSelect":multiple.is_some(),"minSelections":multiple.map(|b|b.0),"maxSelections":multiple.map(|b|b.1)} });
        match self.ask(session, &tool_call, Value::Array(options)) {
            Ok(Some(picked)) => match answer_indices(&picked, choices.len(), multiple.is_some()) {
                Some(indices)
                    if multiple
                        .is_some_and(|(min, max)| indices.len() >= min && indices.len() <= max)
                        || (multiple.is_none() && indices.len() == 1) =>
                {
                    let values: Vec<_> = indices.iter().map(|i| choices[*i].0.clone()).collect();
                    let value = if multiple.is_some() {
                        Value::Array(values)
                    } else {
                        values[0].clone()
                    };
                    json!({"action":"accept","content":{field:value}})
                }
                _ => json!({ "action": "decline" }),
            },
            _ => json!({ "action": "cancel" }),
        }
    }

    /// `session/request_permission`: the option picked, or `None` if cancelled.
    fn ask(
        &self,
        session: &Session,
        tool_call: &Value,
        options: Value,
    ) -> Result<Option<String>, String> {
        let reply = self
            .client
            .request(
                "session/request_permission",
                json!({ "sessionId": session.id, "toolCall": tool_call, "options": options }),
            )
            .map_err(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("permission request failed")
                    .to_string()
            })?;
        let outcome = reply.get("outcome").cloned().unwrap_or(Value::Null);
        Ok(match outcome.get("outcome").and_then(Value::as_str) {
            Some("selected") => outcome
                .get("optionId")
                .and_then(Value::as_str)
                .map(String::from),
            _ => None,
        })
    }

    /// The PostToolUse hook: entering plan mode, and the tool's response for
    /// tool calls registered while streaming.
    fn hook_callback(&self, session: &Session, request: &Value) -> Value {
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        if request.get("callback_id").and_then(Value::as_str) == Some(POST_TOOL_USE_HOOK)
            && input.get("hook_event_name").and_then(Value::as_str) == Some("PostToolUse")
        {
            if input.get("tool_name").and_then(Value::as_str) == Some("EnterPlanMode") {
                session.set_mode(PermissionMode::Plan);
                self.client.update(&session.id, json!({ "sessionUpdate": "current_mode_update", "currentModeId": PermissionMode::Plan.as_str() }));
            }
            if let Some(id) = request.get("tool_use_id").and_then(Value::as_str)
                && session.hooked.lock().expect("hooked lock").remove(id)
            {
                let name = session
                    .tool_uses
                    .lock()
                    .expect("tool uses lock")
                    .get(id)
                    .and_then(|t| t.get("name").cloned());
                if let Some(name) = name {
                    self.client.update(
                        &session.id,
                        json!({
                            "_meta": { "claudeCode": { "toolResponse": input.get("tool_response"), "toolName": name } },
                            "toolCallId": id,
                            "sessionUpdate": "tool_call_update",
                        }),
                    );
                }
            }
        }
        json!({ "continue": true })
    }

    /// `_axel/steer`: send a message into the running turn, for Claude to take
    /// in at its next step. `accepted: false` when no turn is running (the
    /// host sends it as a prompt instead).
    fn steer(&self, params: &Value) -> RpcResult {
        let s = self.session(params)?;
        let message_id = params
            .get("messageId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut steering = s.steering.lock().expect("steering lock");
        if !steering.prompting || s.cancelled.load(Ordering::SeqCst) {
            return Ok(json!({ "accepted": false }));
        }
        let uuid = new_uuid();
        let mut message = prompt_to_claude(params);
        message["uuid"] = json!(uuid);
        steering.pending.insert(uuid, message_id);
        s.stdin.send(&message);
        Ok(json!({ "accepted": true }))
    }

    /// Withdraw a message sent into the turn that Claude hasn't taken in yet
    /// (`cancel_async_message`). False when it already has, or isn't ours.
    fn withdraw(&self, s: &Session, message_id: &str) -> bool {
        let uuid = s
            .steering
            .lock()
            .expect("steering lock")
            .pending
            .iter()
            .find(|(_, m)| *m == message_id)
            .map(|(u, _)| u.clone());
        let Some(uuid) = uuid else { return false };
        let cancelled = s
            .control(
                json!({ "subtype": "cancel_async_message", "message_uuid": uuid }),
                WITHDRAW_TIMEOUT,
            )
            .ok()
            .and_then(|r| r.get("cancelled").and_then(Value::as_bool))
            .unwrap_or(false);
        if cancelled {
            s.steering
                .lock()
                .expect("steering lock")
                .pending
                .remove(&uuid);
        }
        cancelled
    }

    /// Withdraw every message still waiting to be taken in; their message ids.
    fn withdraw_all(&self, s: &Session) -> Vec<String> {
        let ids: Vec<String> = s
            .steering
            .lock()
            .expect("steering lock")
            .pending
            .values()
            .cloned()
            .collect();
        ids.into_iter().filter(|id| self.withdraw(s, id)).collect()
    }

    /// `prompt` from the adapter: send the user message, relay Claude's
    /// stream until its `result` (and, when messages were sent into the turn
    /// too late for it, until the turn they start ends too).
    fn prompt(&self, params: &Value) -> RpcResult {
        let session = self.session(params)?;
        session.cancelled.store(false, Ordering::SeqCst);
        let result = self.relay_prompt(&session, params);
        {
            let mut steering = session.steering.lock().expect("steering lock");
            steering.prompting = false;
            steering.pending.clear();
        }
        // The prompt's spend goes in its reply; a failed one's counts toward the next.
        let Ok(reply) = result else { return result };
        let mut meter = session.meter.lock().expect("meter lock");
        if let Some((used, size)) = meter.context() {
            self.client.update(
                &session.id,
                json!({ "sessionUpdate": "usage_update", "used": used, "size": size }),
            );
        }
        Ok(meter.take().into_reply(reply))
    }

    fn relay_prompt(&self, session: &Arc<Session>, params: &Value) -> RpcResult {
        let rx = session.messages.lock().expect("messages lock");
        // What Claude said on its own since the last turn and the idle relay
        // hasn't shown yet: relayed now, so a stale `result` can't end this
        // prompt before Claude has read it.
        while let Ok(message) = rx.try_recv() {
            // Not this prompt's: a `result` among them ended Claude's own turn.
            let _ = self.relay_message(session, &message);
        }
        // Only now may messages be sent into the turn: one accepted before the
        // prompt itself went out would reach Claude first, as a prompt of its own.
        session.steering.lock().expect("steering lock").prompting = true;
        session.stdin.send(&prompt_to_claude(params));
        loop {
            let Ok(message) = rx.recv() else {
                if session.cancelled.load(Ordering::SeqCst) {
                    return Ok(json!({ "stopReason": "cancelled" }));
                }
                return Err(RpcError::invalid("Session did not end in result"));
            };
            if let Some(end) = self.relay_message(session, &message) {
                return end;
            }
        }
    }

    /// Relay one message from Claude to the client.  `Some` when it ends the
    /// turn (a `result`, or an error that does); the idle relay ignores that.
    fn relay_message(&self, session: &Arc<Session>, message: &Value) -> Option<RpcResult> {
        match message.get("type").and_then(Value::as_str).unwrap_or("") {
            "system" | "tool_progress" | "tool_use_summary" | "auth_status" => None,
            // How much of the plan's windows the account has used.
            "rate_limit_event" => {
                let limits = claude_limits(message);
                if !limits.is_empty() {
                    self.client.notify("_axel/limits", limits_params(&limits));
                }
                None
            }
            "result" => self.on_result(session, message),
            "stream_event" => {
                self.relay_stream_event(session, message);
                None
            }
            // Claude's echo of a user message it took in: a steer that
            // landed, or the prompt itself. Neither is relayed.
            "user" if message.get("isReplay").and_then(Value::as_bool) == Some(true) => {
                let uuid = message.get("uuid").and_then(Value::as_str).unwrap_or("");
                let landed = session
                    .steering
                    .lock()
                    .expect("steering lock")
                    .pending
                    .remove(uuid);
                if let Some(message_id) = landed {
                    self.client.notify(
                        "_axel/steered",
                        json!({ "sessionId": session.id, "messageId": message_id }),
                    );
                }
                None
            }
            kind @ ("user" | "assistant") => self.relay_conversation(session, message, kind),
            other => {
                eprintln!("[axel claude-acp] unexpected message type {other}");
                None
            }
        }
    }

    /// A `result`: the turn's spend, and how the prompt ends, unless a
    /// message steered in too late runs on as a turn of its own.
    fn on_result(&self, session: &Session, message: &Value) -> Option<RpcResult> {
        session.meter.lock().expect("meter lock").on_result(message);
        if session.cancelled.load(Ordering::SeqCst) {
            return Some(Ok(json!({ "stopReason": "cancelled" })));
        }
        // A message sent in after the turn's last step: Claude runs it as a
        // turn of its own. Its result ends this prompt.
        if !session
            .steering
            .lock()
            .expect("steering lock")
            .pending
            .is_empty()
        {
            return None;
        }
        let ends = result_stop(message);
        if ends.is_none() {
            let subtype = message.get("subtype").and_then(Value::as_str).unwrap_or("");
            eprintln!("[axel claude-acp] unexpected result subtype {subtype}");
        }
        ends
    }

    /// A streamed content block, or a delta of one.
    fn relay_stream_event(&self, session: &Session, message: &Value) {
        let event = message.get("event").cloned().unwrap_or(Value::Null);
        let block = match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => event.get("content_block").cloned(),
            Some("content_block_delta") => event.get("delta").cloned(),
            _ => None,
        };
        if let Some(block) = block {
            self.relay_content(session, &Value::Array(vec![block]), "assistant");
        }
    }

    /// A whole user or assistant message: its tool calls and results (its
    /// text and thinking already streamed), and the output of local
    /// commands.  `Some` when it says Claude isn't logged in.
    fn relay_conversation(
        &self,
        session: &Session,
        message: &Value,
        kind: &str,
    ) -> Option<RpcResult> {
        let is_assistant = kind == "assistant";
        if is_assistant {
            let mut meter = session.meter.lock().expect("meter lock");
            meter.on_assistant(message);
            // A long turn's spend shows as it happens, not at its end.
            if let Some(p) = meter.progress(std::time::Instant::now()) {
                self.client.notify(
                    "_axel/usage",
                    p.into_reply(json!({ "sessionId": session.id })),
                );
            }
        }
        if session.cancelled.load(Ordering::SeqCst) {
            return None;
        }
        let msg = message.get("message").cloned().unwrap_or(Value::Null);
        let content = msg.get("content").cloned().unwrap_or(Value::Null);
        if let Value::String(text) = &content
            && self.relay_local_command(session, text)
        {
            return None;
        }
        let single_text = content.as_array().is_some_and(|a| {
            a.len() == 1 && a[0].get("type").and_then(Value::as_str) == Some("text")
        });
        if !is_assistant && (content.is_string() || single_text) {
            return None;
        }
        if is_assistant
            && msg.get("model").and_then(Value::as_str) == Some("<synthetic>")
            && single_text
            && content[0]
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|t| t.contains("Please run /login"))
        {
            return Some(Err(RpcError::auth_required()));
        }
        // Assistant text and thinking already streamed as deltas.
        let content = if is_assistant {
            Value::Array(
                content
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter(|c| {
                                !matches!(
                                    c.get("type").and_then(Value::as_str),
                                    Some("text" | "thinking")
                                )
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        } else {
            content
        };
        let role = msg
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or(kind)
            .to_string();
        self.relay_content(session, &content, &role);
        None
    }

    /// The output of a local command (`/context`): its context usage is
    /// shown, the rest isn't.  Whether `text` was one.
    fn relay_local_command(&self, session: &Session, text: &str) -> bool {
        if text.contains("<local-command-stdout>") {
            if text.contains("Context Usage") {
                let clean = text
                    .replace("<local-command-stdout>", "")
                    .replace("</local-command-stdout>", "");
                self.relay_content(session, &Value::String(clean), "assistant");
            }
            return true;
        }
        if text.contains("<local-command-stderr>") {
            eprintln!("{text}");
            return true;
        }
        false
    }

    /// Claude content as session updates to the client.
    fn relay_content(&self, session: &Session, content: &Value, role: &str) {
        let mut cache = session.tool_uses.lock().expect("tool uses lock");
        for u in to_acp(content, role, &mut cache, Some(&session.hooked)) {
            self.client.update(&session.id, u);
        }
    }

    /// Between prompts, Claude can go on by itself: a background task it
    /// started finishes and wakes it, and it reads the output and says what
    /// it found.  Nothing asked, so nothing is reading its stream; left in
    /// the channel, those messages would come out at the next prompt as if
    /// they answered it, and their `result` would end that prompt before
    /// Claude had read it (the real answer then waiting for the prompt after).
    /// This thread relays them as they come, whenever no prompt is running.
    fn spawn_idle_relay(self: &Arc<Self>, session: Arc<Session>) {
        let agent = self.clone();
        std::thread::spawn(move || {
            loop {
                // Polled, never waited on: the receiver is held for an instant,
                // so a prompt starting right now isn't kept waiting for it.
                if !session.steering.lock().expect("steering lock").prompting
                    && let Ok(rx) = session.messages.try_lock()
                {
                    loop {
                        match rx.try_recv() {
                            Ok(message) => {
                                // No prompt is running for a `result` to end.
                                let _ = agent.relay_message(&session, &message);
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => return,
                        }
                    }
                }
                std::thread::sleep(IDLE_RELAY_POLL);
            }
        });
    }
}

/// MCP servers for Claude's `--mcp-config`: any from
/// `_meta.claudeCode.options.mcpServers`, then the client's.
fn mcp_servers(params: &Value, user_options: &Value) -> Map<String, Value> {
    let mut mcp = user_options
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for server in params
        .get("mcpServers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = server.get("name").and_then(Value::as_str) else {
            continue;
        };
        let pairs = |k: &str| -> Option<Value> {
            let a = server.get(k)?.as_array()?;
            Some(Value::Object(
                a.iter()
                    .filter_map(|e| {
                        Some((
                            e.get("name")?.as_str()?.to_string(),
                            e.get("value").cloned().unwrap_or(Value::Null),
                        ))
                    })
                    .collect(),
            ))
        };
        let entry = match server.get("type").and_then(Value::as_str) {
            Some(kind) => {
                json!({ "type": kind, "url": server.get("url"), "headers": pairs("headers") })
            }
            None => {
                json!({ "type": "stdio", "command": server.get("command"), "args": server.get("args"), "env": pairs("env") })
            }
        };
        mcp.insert(name.to_string(), strip_nulls(entry));
    }
    mcp
}

/// Claude Code's command line for a session, as the Agent SDK builds it.
fn claude_args(
    params: &Value,
    user_options: &Value,
    mcp: &Map<String, Value>,
    resume: Option<&str>,
    session_id: &str,
) -> Vec<String> {
    // `--replay-user-messages`: Claude echoes each user message as it takes
    // it in, which is how a steer is known to have landed.
    let mut args: Vec<String> = [
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
        "--replay-user-messages",
    ]
    .map(String::from)
    .to_vec();
    if let Some(n) = std::env::var("MAX_THINKING_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
    {
        args.extend(["--max-thinking-tokens".into(), n.to_string()]);
    }
    args.extend(["--permission-prompt-tool".into(), "stdio".into()]);
    // The agent's reasoning effort, from the host (`_meta.axel.effort`).
    if let Some(effort) = crate::meta_effort(params) {
        args.extend(["--effort".into(), effort.into()]);
    }
    if let Some(r) = resume {
        args.extend(["--resume".into(), r.to_string()]);
    }
    // AskUserQuestion stays on: each question is asked as a permission
    // request whose options are the choices (see `ask_questions`).
    args.extend(["--tools".into(), "default".into()]);
    if !mcp.is_empty() {
        args.extend([
            "--mcp-config".into(),
            json!({ "mcpServers": mcp }).to_string(),
        ]);
    }
    args.extend(["--setting-sources".into(), "user,project,local".into()]);
    args.extend([
        "--permission-mode".into(),
        PermissionMode::Default.as_str().into(),
    ]);
    if allow_bypass() {
        args.push("--allow-dangerously-skip-permissions".into());
    }
    args.push("--include-partial-messages".into());
    if resume.is_none() {
        args.extend(["--session-id".into(), session_id.to_string()]);
    }
    for (k, v) in user_options
        .get("extraArgs")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        args.push(format!("--{k}"));
        match v {
            Value::Null => {}
            Value::String(s) => args.push(s.clone()),
            other => args.push(other.to_string()),
        }
    }
    args
}

/// The command that runs Claude Code `exe` (a script runs under node), with
/// the environment the Agent SDK gives it.
fn claude_command(exe: &str) -> Command {
    let mut cmd = if [".js", ".mjs", ".cjs"].iter().any(|ext| exe.ends_with(ext)) {
        let mut c = Command::new("node");
        c.arg(exe);
        c
    } else {
        Command::new(exe)
    };
    if std::env::var_os("CLAUDE_CODE_ENTRYPOINT").is_none() {
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
    }
    cmd.env_remove("NODE_OPTIONS");
    if std::env::var_os("DEBUG_CLAUDE_AGENT_SDK").is_some() {
        cmd.env("DEBUG", "1");
    } else {
        cmd.env_remove("DEBUG");
    }
    cmd
}

/// The `initialize` control request: our hook, and the system prompt
/// (Claude Code's own, plus an appended part, or a replacement).
fn initialize_request(meta: &Value) -> Value {
    let mut init = json!({
        "subtype": "initialize",
        "hooks": { "PostToolUse": [{ "hookCallbackIds": [POST_TOOL_USE_HOOK] }] }
    });
    match meta.get("systemPrompt") {
        Some(Value::String(replacement)) => {
            init["systemPrompt"] = Value::String(replacement.clone())
        }
        Some(v) => {
            if let Some(append) = v.get("append").and_then(Value::as_str) {
                init["appendSystemPrompt"] = Value::String(append.to_string());
            }
        }
        None => {}
    }
    init
}

/// Put Claude on the settings' `model` when it names one Claude offers,
/// else the first it offers; the ACP `models` for the session reply.
fn select_model(session: &Session, init: &Value, cwd: &Path) -> Value {
    let models = init
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let wanted = settings_model(cwd);
    let current = wanted
        .as_deref()
        .and_then(|want| models.iter().find(|m| model_matches(m, want)))
        .or_else(|| models.first())
        .cloned();
    if let Some(value) = current
        .as_ref()
        .and_then(|m| m.get("value"))
        .and_then(Value::as_str)
        && let Err(e) = session.control(
            json!({ "subtype": "set_model", "model": value }),
            CONTROL_TIMEOUT,
        )
    {
        // Claude keeps its own default; the session still works.
        eprintln!("[axel claude-acp] set_model {value}: {e}");
    }
    json!({
        "availableModels": models.iter().map(|m| json!({ "modelId": m.get("value"), "name": m.get("displayName"), "description": m.get("description") })).collect::<Vec<_>>(),
        "currentModelId": current.as_ref().and_then(|m| m.get("value")),
    })
}

/// The ACP session modes: Claude Code's permission modes, bypass only where
/// it works (see [`allow_bypass`]).
fn available_modes() -> Vec<Value> {
    let mut modes = vec![
        json!({ "id": PermissionMode::Default.as_str(), "name": "Default", "description": "Standard behavior, prompts for dangerous operations" }),
        json!({ "id": PermissionMode::AcceptEdits.as_str(), "name": "Accept Edits", "description": "Auto-accept file edit operations" }),
        json!({ "id": PermissionMode::Plan.as_str(), "name": "Plan Mode", "description": "Planning mode, no actual tool execution" }),
        json!({ "id": PermissionMode::DontAsk.as_str(), "name": "Don't Ask", "description": "Don't prompt for permissions, deny if not pre-approved" }),
    ];
    if allow_bypass() {
        modes.push(json!({ "id": PermissionMode::BypassPermissions.as_str(), "name": "Bypass Permissions", "description": "Bypass all permission checks" }));
    }
    modes
}

/// How a `result` ends the prompt: its stop reason, or the error it
/// reports.  `None` for a subtype this adapter doesn't know.
fn result_stop(message: &Value) -> Option<RpcResult> {
    let is_error = message
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let subtype = message.get("subtype").and_then(Value::as_str).unwrap_or("");
    let errors = || {
        let e: Vec<String> = message
            .get("errors")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if e.is_empty() {
            subtype.to_string()
        } else {
            e.join(", ")
        }
    };
    Some(match subtype {
        "success" => {
            let result = message.get("result").and_then(Value::as_str).unwrap_or("");
            if result.contains("Please run /login") {
                Err(RpcError::auth_required())
            } else if is_error {
                Err(RpcError::internal(result))
            } else {
                Ok(json!({ "stopReason": "end_turn" }))
            }
        }
        "error_during_execution" if is_error => Err(RpcError::internal(errors())),
        "error_during_execution" => Ok(json!({ "stopReason": "end_turn" })),
        "error_max_budget_usd" | "error_max_turns" | "error_max_structured_output_retries"
            if is_error =>
        {
            Err(RpcError::internal(errors()))
        }
        "error_max_budget_usd" | "error_max_turns" | "error_max_structured_output_retries" => {
            Ok(json!({ "stopReason": "max_turn_requests" }))
        }
        _ => return None,
    })
}

fn is_edit_tool(tool: &str) -> bool {
    // The adapter's EDIT_TOOL_NAMES are its `mcp__acp__Edit` / `Write`; with
    // native tools those are Claude Code's Edit and Write (and NotebookEdit,
    // which Claude Code's own accept-edits mode covers too).
    matches!(tool, "Edit" | "Write" | "NotebookEdit" | "MultiEdit")
}

/// A single-field form: a scalar choice or a bounded array of enum choices.
/// Labels come from enumNames, titled oneOf/anyOf alternatives, or Yes/No.
struct ElicitationChoice {
    field: String,
    choices: Vec<(Value, String)>,
    multiple: Option<(usize, usize)>,
}

fn elicitation_choice(schema: &Value) -> Option<ElicitationChoice> {
    let props = schema.get("properties")?.as_object()?;
    if props.len() != 1 {
        return None;
    }
    let (field, property) = props.iter().next()?;
    let multi = property.get("type").and_then(Value::as_str) == Some("array");
    let p = if multi {
        property.get("items")?
    } else {
        property
    };
    let choices: Vec<(Value, String)> =
        if let Some(values) = p.get("enum").and_then(Value::as_array) {
            let names = p.get("enumNames").and_then(Value::as_array);
            values
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let label = names
                        .and_then(|n| n.get(i))
                        .and_then(Value::as_str)
                        .map(String::from)
                        .or_else(|| v.as_str().map(String::from))
                        .unwrap_or_else(|| v.to_string());
                    (v.clone(), label)
                })
                .collect()
        } else if let Some(alts) = p
            .get("oneOf")
            .or_else(|| p.get("anyOf"))
            .and_then(Value::as_array)
        {
            alts.iter()
                .filter_map(|a| {
                    let v = a.get("const")?.clone();
                    let label = a
                        .get("title")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .or_else(|| v.as_str().map(String::from))
                        .unwrap_or_else(|| v.to_string());
                    Some((v, label))
                })
                .collect()
        } else if p.get("type").and_then(Value::as_str) == Some("boolean") {
            vec![
                (Value::Bool(true), t!("Yes").into()),
                (Value::Bool(false), t!("No").into()),
            ]
        } else {
            return None;
        };
    if choices.is_empty() {
        return None;
    }
    let multiple = if multi {
        // Saturating: an absurd bound is refused below rather than wrapped.
        let count = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
        let min = property
            .get("minItems")
            .and_then(Value::as_u64)
            .map_or(0, count);
        let max = property
            .get("maxItems")
            .and_then(Value::as_u64)
            .map_or(choices.len(), count)
            .min(choices.len());
        if min > max {
            return None;
        }
        Some((min, max))
    } else {
        None
    };
    Some(ElicitationChoice {
        field: field.clone(),
        choices,
        multiple,
    })
}

/// Option ids of a question's choices start with this: the host asks the
/// owner for these, whatever its policy would do with a permission.
pub const ANSWER_PREFIX: &str = "answer:";
/// Native multi-choice answers travel over the existing ACP option-id channel.
pub(crate) const MULTI_ANSWER_PREFIX: &str = "answer:multi:";

fn answer_indices(answer: &str, size: usize, multiple: bool) -> Option<Vec<usize>> {
    let ids = if let Some(encoded) = answer.strip_prefix(MULTI_ANSWER_PREFIX) {
        if !multiple {
            return None;
        }
        serde_json::from_str::<Vec<String>>(encoded).ok()?
    } else {
        vec![answer.to_string()]
    };
    let mut out = Vec::new();
    for id in ids {
        let i = id.strip_prefix(ANSWER_PREFIX)?.parse::<usize>().ok()?;
        if i >= size || out.contains(&i) {
            return None;
        }
        out.push(i);
    }
    Some(out)
}

/// The permission request for question `qi` of an `AskUserQuestion` call:
/// the tool call (its own id per question, the question as its title), the
/// options (each choice, then "none of these"), and the choices' labels.
fn question_request(tool_use_id: &Value, qi: usize, q: &Value) -> (Value, Value, Vec<String>) {
    let text = q
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let header = q
        .get("header")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|h| !h.is_empty());
    let multi = q
        .get("multiSelect")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut title = match header {
        Some(h) => format!("{h}: {text}"),
        None => text.to_string(),
    };
    if multi {
        title = t!("{question} (select all that apply)", question = title);
    }
    let mut labels = Vec::new();
    let mut options: Vec<Value> = Vec::new();
    for (i, o) in q
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let label = o
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let name = match o
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            Some(d) => format!("{label} — {d}"),
            None => label.clone(),
        };
        options.push(json!({ "kind": "allow_once", "name": name, "optionId": format!("{ANSWER_PREFIX}{i}") }));
        labels.push(label);
    }
    options.push(json!({ "kind": "reject_once", "name": t!("None of these (I'll answer in the chat)"), "optionId": format!("{ANSWER_PREFIX}none") }));
    let id = format!("{}#{qi}", tool_use_id.as_str().unwrap_or("question"));
    let tool_call = json!({ "toolCallId": id, "title": title, "kind": "think", "rawInput": q });
    (tool_call, Value::Array(options), labels)
}

fn always_rule(tool: &str) -> Value {
    json!([{ "type": "addRules", "rules": [{ "toolName": tool }], "behavior": "allow", "destination": "session" }])
}

/// ACP session/update payloads for Claude content blocks: `toAcpNotifications`.
/// The tool uses most recently seen, to name their results.  A result
/// follows its call within the turn, and a turn rarely makes more than a
/// few hundred calls: past this many, the oldest are forgotten (a result
/// that late shows as untracked) rather than kept for the whole session.
const TOOL_USES_MAX: usize = 1024;

/// `tool_use` blocks by id, the newest [`TOOL_USES_MAX`].
#[derive(Default)]
struct ToolUses {
    by_id: HashMap<String, Value>,
    /// Ids, oldest first.
    order: std::collections::VecDeque<String>,
}

impl ToolUses {
    /// Remember a tool use; returns the id forgotten to make room, if any.
    fn insert(&mut self, id: String, tool_use: Value) -> Option<String> {
        if self.by_id.insert(id.clone(), tool_use).is_some() {
            return None;
        }
        self.order.push_back(id);
        let forgotten = if self.order.len() > TOOL_USES_MAX {
            self.order.pop_front()
        } else {
            None
        };
        if let Some(old) = &forgotten {
            self.by_id.remove(old);
        }
        debug_assert_eq!(self.by_id.len(), self.order.len());
        assert!(self.by_id.len() <= TOOL_USES_MAX);
        forgotten
    }

    fn get(&self, id: &str) -> Option<&Value> {
        self.by_id.get(id)
    }
}

fn to_acp(
    content: &Value,
    role: &str,
    cache: &mut ToolUses,
    hooks: Option<&Mutex<HashSet<String>>>,
) -> Vec<Value> {
    let chunk_kind = if role == "assistant" {
        "agent_message_chunk"
    } else {
        "user_message_chunk"
    };
    if let Value::String(text) = content {
        return vec![
            json!({ "sessionUpdate": chunk_kind, "content": { "type": "text", "text": text } }),
        ];
    }
    let mut out = Vec::new();
    for chunk in content.as_array().into_iter().flatten() {
        let kind = chunk.get("type").and_then(Value::as_str).unwrap_or("");
        let update = match kind {
            "text" | "text_delta" => Some(
                json!({ "sessionUpdate": chunk_kind, "content": { "type": "text", "text": chunk.get("text") } }),
            ),
            "image" => {
                Some(json!({ "sessionUpdate": chunk_kind, "content": image_content(chunk) }))
            }
            "thinking" | "thinking_delta" => Some(
                json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": chunk.get("thinking") } }),
            ),
            "tool_use" | "server_tool_use" | "mcp_tool_use" => tool_use_update(chunk, cache, hooks),
            "tool_result"
            | "tool_search_tool_result"
            | "web_fetch_tool_result"
            | "web_search_tool_result"
            | "code_execution_tool_result"
            | "bash_code_execution_tool_result"
            | "text_editor_code_execution_tool_result"
            | "mcp_tool_result" => tool_result_update(chunk, cache),
            _ => None,
        };
        out.extend(update);
    }
    out
}

/// An image block as ACP image content.
fn image_content(chunk: &Value) -> Value {
    let src = chunk.get("source").cloned().unwrap_or(Value::Null);
    let b64 = src.get("type").and_then(Value::as_str) == Some("base64");
    let mut c = json!({
        "type": "image",
        "data": if b64 { src.get("data").cloned().unwrap_or(Value::Null) } else { json!("") },
        "mimeType": if b64 { src.get("media_type").cloned().unwrap_or(Value::Null) } else { json!("") },
    });
    if src.get("type").and_then(Value::as_str) == Some("url") {
        c["uri"] = src.get("url").cloned().unwrap_or(Value::Null);
    }
    c
}

/// A `tool_use` block: a plan for TodoWrite, else a tool call (remembered,
/// to name its result, and registered for its PostToolUse hook).
fn tool_use_update(
    chunk: &Value,
    cache: &mut ToolUses,
    hooks: Option<&Mutex<HashSet<String>>>,
) -> Option<Value> {
    let id = chunk
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // A forgotten call can't be named any more: its hook can go too.
    if let Some(forgotten) = cache.insert(id.clone(), chunk.clone())
        && let Some(h) = hooks
    {
        h.lock().expect("hooked lock").remove(&forgotten);
    }
    let name = chunk.get("name").and_then(Value::as_str).unwrap_or("");
    let input = chunk.get("input").cloned().unwrap_or(json!({}));
    if name == "TodoWrite" {
        return input
            .get("todos")
            .filter(|t| t.is_array())
            .map(|_| json!({ "sessionUpdate": "plan", "entries": tools::plan_entries(&input) }));
    }
    if let Some(h) = hooks {
        h.lock().expect("hooked lock").insert(id.clone());
    }
    let mut u = json!({
        "_meta": { "claudeCode": { "toolName": name } },
        "toolCallId": id,
        "sessionUpdate": "tool_call",
        "rawInput": input,
        "status": "pending",
    });
    for (k, v) in tools::tool_info(name, &input) {
        u[k] = v;
    }
    Some(u)
}

/// A tool result block: an update to the call it answers (none for
/// TodoWrite, whose plan is its own update, or a call never seen).
fn tool_result_update(chunk: &Value, cache: &ToolUses) -> Option<Value> {
    let id = chunk
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(tool_use) = cache.get(id) else {
        eprintln!("[axel claude-acp] tool result for an untracked tool use: {id}");
        return None;
    };
    let name = tool_use
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if name == "TodoWrite" {
        return None;
    }
    let failed = chunk
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut u = json!({
        "_meta": { "claudeCode": { "toolName": name } },
        "toolCallId": id,
        "sessionUpdate": "tool_call_update",
        "status": if failed { "failed" } else { "completed" },
        "rawOutput": chunk.get("content"),
    });
    for (k, v) in tools::tool_update(chunk, &name) {
        u[k] = v;
    }
    Some(u)
}

/// A random (version 4) UUID: the id of a new Claude session, or of a user
/// message sent into a turn.
fn new_uuid() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        // No randomness to be had: a clock and a counter are unique enough here.
        static N: AtomicU64 = AtomicU64::new(0);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // The clock's low 64 bits: they are the ones that change.
        b[..8].copy_from_slice(&t.to_le_bytes()[..8]);
        b[8..].copy_from_slice(&N.fetch_add(1, Ordering::SeqCst).to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let uuid = format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    );
    debug_assert_eq!(uuid.len(), 36, "the canonical 8-4-4-4-12 form");
    uuid
}

/// A Claude user message for an ACP prompt: `promptToClaude`.
fn prompt_to_claude(params: &Value) -> Value {
    let mut content = Vec::new();
    let mut context = Vec::new();
    for chunk in params
        .get("prompt")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match chunk.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let mut text = chunk
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // `/mcp:server:command args` → `/server:command (MCP) args`
                if let Some(rest) = text.strip_prefix("/mcp:")
                    && let Some((server, tail)) = rest.split_once(':')
                    && !server.is_empty()
                    && !server.contains(char::is_whitespace)
                {
                    let (command, args) = match tail.find(char::is_whitespace) {
                        Some(i) => (&tail[..i], &tail[i..]),
                        None => (tail, ""),
                    };
                    if !command.is_empty() {
                        text = format!("/{server}:{command} (MCP){args}");
                    }
                }
                content.push(json!({ "type": "text", "text": text }));
            }
            "resource_link" => {
                content.push(json!({ "type": "text", "text": uri_link(chunk.get("uri").and_then(Value::as_str).unwrap_or("")) }));
            }
            "resource" => {
                let res = chunk.get("resource").cloned().unwrap_or(Value::Null);
                if let Some(text) = res.get("text").and_then(Value::as_str) {
                    let uri = res.get("uri").and_then(Value::as_str).unwrap_or("");
                    content.push(json!({ "type": "text", "text": uri_link(uri) }));
                    context.push(json!({ "type": "text", "text": format!("\n<context ref=\"{uri}\">\n{text}\n</context>") }));
                }
            }
            "image" => {
                if let Some(data) = chunk
                    .get("data")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                {
                    content.push(json!({ "type": "image", "source": { "type": "base64", "data": data, "media_type": chunk.get("mimeType") } }));
                } else if let Some(uri) = chunk
                    .get("uri")
                    .and_then(Value::as_str)
                    .filter(|u| u.starts_with("http"))
                {
                    content
                        .push(json!({ "type": "image", "source": { "type": "url", "url": uri } }));
                }
            }
            _ => {}
        }
    }
    content.extend(context);
    json!({
        "type": "user",
        "message": { "role": "user", "content": content },
        "session_id": params.get("sessionId"),
        "parent_tool_use_id": null,
    })
}

/// `formatUriAsLink`: `[@name](uri)` for file and zed URIs.
fn uri_link(uri: &str) -> String {
    if let Some(path) = uri.strip_prefix("file://") {
        let name = path
            .rsplit('/')
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or(path);
        return format!("[@{name}]({uri})");
    }
    if uri.starts_with("zed://") {
        let name = uri
            .rsplit('/')
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or(uri);
        return format!("[@{name}]({uri})");
    }
    uri.to_string()
}

/// `getAvailableSlashCommands`.
fn available_commands(commands: &Value) -> Vec<Value> {
    const UNSUPPORTED: [&str; 7] = [
        "cost",
        "keybindings-help",
        "login",
        "logout",
        "output-style:new",
        "release-notes",
        "todos",
    ];
    commands
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let raw = c.get("name").and_then(Value::as_str)?;
            let name = match raw.strip_suffix(" (MCP)") {
                Some(n) => format!("mcp:{n}"),
                None => raw.to_string(),
            };
            if UNSUPPORTED.contains(&name.as_str()) {
                return None;
            }
            let hint = match c.get("argumentHint") {
                Some(Value::Array(a)) => Some(
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            Some(json!({
                "name": name,
                "description": c.get("description").and_then(Value::as_str).unwrap_or(""),
                "input": hint.map(|h| json!({ "hint": h })),
            }))
        })
        .collect()
}

/// Whether a model `{value, displayName}` is the one settings name.
fn model_matches(m: &Value, want: &str) -> bool {
    let value = m.get("value").and_then(Value::as_str).unwrap_or("");
    let display = m
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let w = want.to_lowercase();
    value == want
        || value.contains(want)
        || want.contains(value) && !value.is_empty()
        || display == w
        || display.contains(&w)
}

fn claude_config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".claude"))
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The `model` the settings files pick: user, project, local, then managed,
/// later ones winning, as the adapter's SettingsManager merges them.
fn settings_model(cwd: &Path) -> Option<String> {
    let managed = if cfg!(target_os = "macos") {
        "/Library/Application Support/ClaudeCode/managed-settings.json"
    } else {
        "/etc/claude-code/managed-settings.json"
    };
    [
        claude_config_dir().join("settings.json"),
        cwd.join(".claude").join("settings.json"),
        cwd.join(".claude").join("settings.local.json"),
        PathBuf::from(managed),
    ]
    .iter()
    .rev()
    .filter_map(|p| std::fs::read_to_string(p).ok())
    .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
    .find_map(|v| v.get("model").and_then(Value::as_str).map(String::from))
}

/// `~/.claude/projects/<cwd with non-alphanumerics as ->/<id>.jsonl`, else any project's.
fn find_session_file(session_id: &str, cwd: &Path) -> Option<PathBuf> {
    let projects = claude_config_dir().join("projects");
    let encoded: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let expected = projects.join(encoded).join(format!("{session_id}.jsonl"));
    if expected.exists() {
        return Some(expected);
    }
    std::fs::read_dir(&projects)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join(format!("{session_id}.jsonl")))
        .find(|p| p.exists())
}

/// Send a saved conversation to the client: `replaySessionHistory`.
fn replay(client: &Client, session_id: &str, file: &Path) {
    let Ok(f) = std::fs::File::open(file) else {
        return;
    };
    let mut cache = ToolUses::default();
    for line in bounded_lines(BufReader::new(f), "axel claude-acp").map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "user" && kind != "assistant" {
            continue;
        }
        if entry
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        if entry
            .get("sessionId")
            .and_then(Value::as_str)
            .is_some_and(|s| s != session_id)
        {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        let role = match message.get("role").and_then(Value::as_str) {
            Some(r @ ("assistant" | "user")) => r,
            _ => continue,
        };
        let content = message.get("content").cloned().unwrap_or(Value::Null);
        if !content.is_string() && !content.is_array() {
            continue;
        }
        for u in to_acp(&content, role, &mut cache, None) {
            client.update(session_id, u);
        }
    }
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .collect::<Map<_, _>>(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elicitation_array_choices_keep_values_titles_and_selection_bounds() {
        let form = json!({"properties":{"databases":{"type":"array","minItems":1,"maxItems":2,"items":{"type":"string","anyOf":[{"const":"pg","title":"Postgres"},{"const":"lite","title":"SQLite"}]}}}});
        let parsed = elicitation_choice(&form).unwrap();
        assert_eq!(parsed.field, "databases");
        assert_eq!(parsed.multiple, Some((1, 2)));
        assert_eq!(
            parsed.choices,
            vec![
                (json!("pg"), "Postgres".into()),
                (json!("lite"), "SQLite".into())
            ]
        );
        assert_eq!(
            answer_indices(r#"answer:multi:["answer:0","answer:1"]"#, 2, true),
            Some(vec![0, 1])
        );
        assert!(answer_indices(r#"answer:multi:["answer:0","answer:0"]"#, 2, true).is_none());
        assert!(answer_indices(r#"answer:multi:["answer:0"]"#, 2, false).is_none());
        assert_eq!(answer_indices("answer:1", 2, false), Some(vec![1]));
        assert_eq!(answer_indices("answer:multi:[]", 2, true), Some(vec![]));
        let impossible = json!({"properties":{"x":{"type":"array","minItems":3,"items":{"enum":["one","two"]}}}});
        assert!(elicitation_choice(&impossible).is_none());
    }

    #[test]
    fn an_elicitation_of_one_choice_is_asked_like_a_question() {
        // What Axel's `send` asks when an agent may not reach another.
        let form = json!({ "type": "object", "properties": { "answer": { "type": "string", "enum": ["Allow this message", "Always allow", "Don't allow"] } }, "required": ["answer"] });
        let ElicitationChoice {
            field,
            choices,
            multiple,
        } = elicitation_choice(&form).unwrap();
        assert!(multiple.is_none());
        assert_eq!(field, "answer");
        assert_eq!(
            choices.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(),
            vec!["Allow this message", "Always allow", "Don't allow"]
        );
        let titled = json!({ "properties": { "db": { "oneOf": [{ "const": "pg", "title": "Postgres" }, { "const": "lite", "title": "SQLite" }] } } });
        assert_eq!(
            elicitation_choice(&titled).unwrap().choices,
            vec![
                (json!("pg"), "Postgres".to_string()),
                (json!("lite"), "SQLite".to_string())
            ]
        );
        let yes_no = json!({ "properties": { "ok": { "type": "boolean" } } });
        assert_eq!(
            elicitation_choice(&yes_no).unwrap().choices[0],
            (json!(true), "Yes".to_string())
        );
        // Free text, or several fields: no form here, so it's declined.
        assert!(
            elicitation_choice(&json!({ "properties": { "name": { "type": "string" } } }))
                .is_none()
        );
        assert!(
            elicitation_choice(
                &json!({ "properties": { "a": { "type": "boolean" }, "b": { "type": "boolean" } } })
            )
            .is_none()
        );
    }

    #[test]
    fn a_question_becomes_a_request_with_its_choices_and_a_way_out() {
        let q = json!({
            "question": "Which database should staging use?",
            "header": "Database",
            "multiSelect": false,
            "options": [
                { "label": "Postgres", "description": "what production runs" },
                { "label": "SQLite" }
            ]
        });
        let (tool_call, options, labels) = question_request(&json!("toolu_1"), 0, &q);
        assert_eq!(
            tool_call["toolCallId"], "toolu_1#0",
            "one item per question"
        );
        assert_eq!(
            tool_call["title"],
            "Database: Which database should staging use?"
        );
        assert_eq!(labels, vec!["Postgres".to_string(), "SQLite".to_string()]);
        let names: Vec<&str> = options
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "Postgres — what production runs",
                "SQLite",
                "None of these (I'll answer in the chat)"
            ]
        );
        assert!(
            options
                .as_array()
                .unwrap()
                .iter()
                .all(|o| o["optionId"].as_str().unwrap().starts_with(ANSWER_PREFIX)),
            "the host knows it's a question"
        );
        let (multi, _, _) = question_request(
            &json!("toolu_1"),
            1,
            &json!({ "question": "Which?", "multiSelect": true, "options": [] }),
        );
        assert_eq!(multi["title"], "Which? (select all that apply)");
    }

    #[test]
    fn prompts_become_claude_user_messages() {
        let p = json!({
            "sessionId": "s1",
            "prompt": [
                { "type": "text", "text": "/mcp:axel:send hi there" },
                { "type": "resource_link", "uri": "file:///w/src/main.rs" },
                { "type": "resource", "resource": { "uri": "file:///w/a.md", "text": "notes" } },
                { "type": "image", "data": "AAAA", "mimeType": "image/png" },
                { "type": "audio", "data": "x" }
            ]
        });
        let m = prompt_to_claude(&p);
        let c = &m["message"]["content"];
        assert_eq!(c[0]["text"], "/axel:send (MCP) hi there");
        assert_eq!(c[1]["text"], "[@main.rs](file:///w/src/main.rs)");
        assert_eq!(c[2]["text"], "[@a.md](file:///w/a.md)");
        assert_eq!(
            c[3]["source"],
            json!({ "type": "base64", "data": "AAAA", "media_type": "image/png" })
        );
        assert_eq!(
            c[4]["text"], "\n<context ref=\"file:///w/a.md\">\nnotes\n</context>",
            "context goes last"
        );
        assert_eq!(c.as_array().unwrap().len(), 5);
        assert_eq!(
            (m["type"].as_str(), m["session_id"].as_str()),
            (Some("user"), Some("s1"))
        );
    }

    #[test]
    fn blocks_become_acp_updates() {
        let mut cache = ToolUses::default();
        let hooked = Mutex::new(HashSet::new());
        let blocks = json!([
            { "type": "text", "text": "hello" },
            { "type": "thinking", "thinking": "hmm" },
            { "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ls" } },
            { "type": "tool_use", "id": "t2", "name": "TodoWrite", "input": { "todos": [{ "content": "a", "status": "pending" }] } },
            { "type": "redacted_thinking" }
        ]);
        let u = to_acp(&blocks, "assistant", &mut cache, Some(&hooked));
        assert_eq!(u.len(), 4);
        assert_eq!(
            u[0],
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hello" } })
        );
        assert_eq!(u[1]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(
            (
                u[2]["sessionUpdate"].as_str(),
                u[2]["title"].as_str(),
                u[2]["status"].as_str()
            ),
            (Some("tool_call"), Some("`ls`"), Some("pending"))
        );
        assert_eq!(u[2]["_meta"]["claudeCode"]["toolName"], "Bash");
        assert_eq!(u[3]["sessionUpdate"], "plan");
        assert!(hooked.lock().unwrap().contains("t1") && !hooked.lock().unwrap().contains("t2"));

        let results = json!([
            { "type": "tool_result", "tool_use_id": "t1", "content": "a\nb", "is_error": false },
            { "type": "tool_result", "tool_use_id": "t2", "content": "ok" },
            { "type": "tool_result", "tool_use_id": "nope", "content": "?" }
        ]);
        let r = to_acp(&results, "user", &mut cache, None);
        assert_eq!(r.len(), 1, "TodoWrite and untracked results send nothing");
        assert_eq!(
            (
                r[0]["status"].as_str(),
                r[0]["content"][0]["content"]["text"].as_str()
            ),
            (Some("completed"), Some("a\nb"))
        );
        assert_eq!(
            to_acp(&json!("hi"), "user", &mut cache, None)[0]["sessionUpdate"],
            "user_message_chunk"
        );
    }

    #[test]
    fn slash_commands_drop_unsupported_and_rename_mcp() {
        let c = available_commands(&json!([
            { "name": "compact", "description": "Compact", "argumentHint": "<instructions>" },
            { "name": "login", "description": "x" },
            { "name": "axel:send (MCP)", "description": "Send", "argumentHint": ["to", "text"] }
        ]));
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["input"]["hint"], "<instructions>");
        assert_eq!(
            (c[1]["name"].as_str(), c[1]["input"]["hint"].as_str()),
            (Some("mcp:axel:send"), Some("to text"))
        );
    }

    #[test]
    fn models_match_like_the_adapter() {
        let m = json!({ "value": "claude-opus-5-5", "displayName": "Opus 5.5" });
        assert!(
            model_matches(&m, "opus")
                && model_matches(&m, "claude-opus-5-5")
                && model_matches(&m, "Opus 5.5")
        );
        assert!(!model_matches(&m, "haiku"));
    }

    #[test]
    fn session_ids_are_uuids() {
        let id = new_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
    }
}
