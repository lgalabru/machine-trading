//! `axel claude-acp` end to end, against `fake_claude` (the `claude` CLI's
//! stream-json and control protocol, faked): the flags and `initialize` it
//! sends Claude, and the ACP it sends the client, compared with what Zed's
//! `claude-code-acp` 0.16.2 sends for the same conversation.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};

struct Agent {
    child: Child,
    /// `None` once closed, as a host that went away leaves it.
    stdin: Option<ChildStdin>,
    rx: Receiver<Value>,
    next: u64,
}

impl Agent {
    fn start(home: &std::path::Path) -> Agent {
        let mut child = Command::new(env!("CARGO_BIN_EXE_claude_acp"))
            .env("CLAUDE_CODE_EXECUTABLE", env!("CARGO_BIN_EXE_fake_claude"))
            .env("FAKE_CLAUDE_LOG", home.join("claude.log"))
            .env("FAKE_CLAUDE_PIDFILE", home.join("claude.pid"))
            .env("HOME", home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env_remove("MAX_THINKING_TOKENS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start claude_acp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        Agent {
            child,
            stdin: Some(stdin),
            rx,
            next: 1,
        }
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    /// What the adapter sees when its host dies: end of input.
    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Send a request; return everything until its response, and the response.
    fn call(
        &mut self,
        method: &str,
        params: Value,
        mut on_request: impl FnMut(&Value) -> Value,
    ) -> (Vec<Value>, Value) {
        let id = self.next;
        self.next += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut seen = Vec::new();
        loop {
            let msg = self
                .rx
                .recv_timeout(Duration::from_secs(20))
                .expect("agent went quiet");
            if msg.get("id") == Some(&json!(id)) && msg.get("method").is_none() {
                return (seen, msg);
            }
            if msg.get("method").is_some() && msg.get("id").is_some() {
                let result = on_request(&msg);
                self.send(json!({ "jsonrpc": "2.0", "id": msg["id"], "result": result }));
            }
            seen.push(msg);
        }
    }

    fn notifications(&self, wait: Duration) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx.recv_timeout(wait) {
            out.push(m);
        }
        out
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn updates(msgs: &[Value]) -> Vec<Value> {
    msgs.iter()
        .filter(|m| m["method"] == "session/update")
        .map(|m| m["params"]["update"].clone())
        .collect()
}

fn claude_log(home: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("claude.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn new_session(agent: &mut Agent, cwd: &std::path::Path) -> String {
    let (_, init) = agent.call(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true }, "terminal": false } }),
        |_| json!({}),
    );
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        init["result"]["agentCapabilities"]["promptCapabilities"],
        json!({ "image": true, "embeddedContext": true })
    );
    let (_, reply) = agent.call(
        "session/new",
        json!({
            "cwd": cwd,
            "mcpServers": [{ "name": "axel", "command": "/usr/bin/true", "args": ["mcp"], "env": [{ "name": "AXEL_AGENT", "value": "pk" }] }],
            "_meta": {
                "systemPrompt": { "append": "You are atlas." },
                "claudeCode": { "options": { "extraArgs": { "settings": "/tmp/guard.json", "plugin-dir": "/tmp/plugin" } } }
            }
        }),
        |_| json!({}),
    );
    let result = &reply["result"];
    let sid = result["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    assert_eq!(sid.len(), 36, "a uuid: {sid}");
    assert_eq!(result["modes"]["currentModeId"], "default");
    let modes: Vec<&str> = result["modes"]["availableModes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(
        modes.starts_with(&["default", "acceptEdits", "plan", "dontAsk"]),
        "{modes:?}"
    );
    sid
}

#[test]
fn a_turn_streams_asks_permission_and_runs_the_tool() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    // The settings' model picks the session's model, as in the adapter.
    std::fs::write(
        home.path().join(".claude/settings.json"),
        r#"{"model":"opus"}"#,
    )
    .unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(home.path());
    let sid = new_session(&mut agent, cwd.path());

    // Claude was started like the Agent SDK starts it.
    let log = claude_log(home.path());
    let argv: Vec<String> = serde_json::from_value(log[0]["argv"].clone()).unwrap();
    let joined = argv.join(" ");
    for flag in [
        "--output-format stream-json --verbose --input-format stream-json --replay-user-messages",
        "--permission-prompt-tool stdio",
        "--tools default",
        "--setting-sources user,project,local",
        "--permission-mode default",
        "--include-partial-messages",
        "--settings /tmp/guard.json",
        "--plugin-dir /tmp/plugin",
    ] {
        assert!(joined.contains(flag), "missing `{flag}` in {joined}");
    }
    // AskUserQuestion stays on: the adapter asks its questions as permission requests.
    assert!(!joined.contains("AskUserQuestion"), "{joined}");
    assert!(joined.contains(&format!("--session-id {sid}")), "{joined}");
    let mcp: Value =
        serde_json::from_str(&argv[argv.iter().position(|a| a == "--mcp-config").unwrap() + 1])
            .unwrap();
    assert_eq!(
        mcp["mcpServers"]["axel"],
        json!({ "type": "stdio", "command": "/usr/bin/true", "args": ["mcp"], "env": { "AXEL_AGENT": "pk" } })
    );
    // initialize carries the appended system prompt and the PostToolUse hook; then the model is set.
    assert_eq!(log[1]["subtype"], "initialize");
    assert_eq!(log[1]["appendSystemPrompt"], "You are atlas.");
    assert!(
        log[1].get("systemPrompt").is_none(),
        "Claude Code's own prompt stays"
    );
    assert_eq!(
        log[1]["hooks"]["PostToolUse"][0]["hookCallbackIds"],
        json!(["hook_0"])
    );
    assert_eq!(
        log[2],
        json!({ "subtype": "set_model", "model": "claude-opus-test" })
    );

    // Slash commands follow the session reply, without the unsupported ones.
    let after = updates(&agent.notifications(Duration::from_millis(300)));
    let cmds = &after
        .iter()
        .find(|u| u["sessionUpdate"] == "available_commands_update")
        .expect("commands")["availableCommands"];
    assert_eq!(
        cmds,
        &json!([{ "name": "compact", "description": "Compact the conversation", "input": { "hint": "<instructions>" } }])
    );

    // One turn: text, a Bash tool call, the permission question, the result.
    let mut asked = Vec::new();
    let (seen, reply) = agent.call(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "list the files" }] }),
        |req| {
            asked.push(req.clone());
            json!({ "outcome": { "outcome": "selected", "optionId": "allow" } })
        },
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn");
    // What the turn spent rides in the reply: Claude's own counts and cost.
    let usage = &reply["result"]["usage"];
    assert_eq!(
        (
            usage["inputTokens"].as_u64(),
            usage["outputTokens"].as_u64(),
            usage["cachedReadTokens"].as_u64()
        ),
        (Some(3), Some(40), Some(1000))
    );
    assert_eq!(
        reply["result"]["_meta"]["axel"],
        json!({ "costUsd": 0.25, "model": "claude-sonnet-test" })
    );
    // And where the plan's windows stand.
    let limits = seen
        .iter()
        .find(|m| m["method"] == "_axel/limits")
        .expect("limits sent");
    let mut windows: Vec<(String, f64)> = limits["params"]["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["window"].as_str().unwrap().to_string(),
                w["usedPercent"].as_f64().unwrap(),
            )
        })
        .collect();
    windows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        windows,
        vec![("five_hour".to_string(), 25.0), ("week".to_string(), 50.0)]
    );
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["method"], "session/request_permission");
    assert_eq!(
        asked[0]["params"]["toolCall"],
        json!({ "toolCallId": "toolu_1", "rawInput": { "command": "ls", "description": "List files" }, "title": "`ls`" })
    );
    let option_ids: Vec<&str> = asked[0]["params"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["optionId"].as_str())
        .collect();
    assert_eq!(option_ids, ["allow_always", "allow", "reject"]);

    let u = updates(&seen);
    let text: String = u
        .iter()
        .filter(|x| x["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|x| x["content"]["text"].as_str())
        .collect();
    assert_eq!(
        text, "Hello",
        "streamed once, not again from the full assistant message"
    );
    let calls: Vec<&Value> = u
        .iter()
        .filter(|x| x["sessionUpdate"] == "tool_call")
        .collect();
    assert_eq!(
        calls.len(),
        2,
        "at block start (empty input) and complete, as the adapter does"
    );
    assert_eq!(
        (
            calls[1]["title"].as_str(),
            calls[1]["kind"].as_str(),
            calls[1]["status"].as_str()
        ),
        (Some("`ls`"), Some("execute"), Some("pending"))
    );
    assert_eq!(
        calls[1]["content"],
        json!([{ "type": "content", "content": { "type": "text", "text": "List files" } }])
    );
    let hooked = u
        .iter()
        .find(|x| {
            x["sessionUpdate"] == "tool_call_update"
                && x["_meta"]["claudeCode"].get("toolResponse").is_some()
        })
        .expect("PostToolUse update");
    assert_eq!(
        hooked["_meta"]["claudeCode"]["toolResponse"],
        json!({ "stdout": "a\nb" })
    );
    let done = u
        .iter()
        .find(|x| x["sessionUpdate"] == "tool_call_update" && x["status"] == "completed")
        .expect("completed");
    assert_eq!(
        done["content"],
        json!([{ "type": "content", "content": { "type": "text", "text": "a\nb" } }])
    );

    // What Claude got back for the permission and the hook.
    let log = claude_log(home.path());
    let perm = log.iter().find_map(|l| l.get("permission_reply")).unwrap();
    assert_eq!(perm["subtype"], "success");
    assert_eq!(
        perm["response"],
        json!({ "behavior": "allow", "updatedInput": { "command": "ls", "description": "List files" }, "toolUseID": "toolu_1" })
    );
    assert_eq!(
        log.iter().find_map(|l| l.get("hook_reply")).unwrap()["response"],
        json!({ "continue": true })
    );
}

#[test]
fn a_refusal_denies_and_interrupts_and_bypass_never_asks() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(home.path());
    let sid = new_session(&mut agent, cwd.path());
    let _ = agent.notifications(Duration::from_millis(200));

    let (seen, reply) = agent.call(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "x" }] }),
        |_| json!({ "outcome": { "outcome": "selected", "optionId": "reject" } }),
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn");
    let perm = claude_log(home.path())
        .into_iter()
        .find_map(|l| l.get("permission_reply").cloned())
        .unwrap();
    assert_eq!(
        perm["response"],
        json!({ "behavior": "deny", "message": "User refused permission to run tool", "interrupt": true, "toolUseID": "toolu_1" })
    );
    let failed = updates(&seen)
        .into_iter()
        .find(|u| u["status"] == "failed")
        .expect("failed tool call");
    assert_eq!(
        failed["content"][0]["content"]["text"],
        "```\nUser refused permission to run tool\n```"
    );

    // In bypassPermissions (Axel's autopilot) the tool is allowed without asking.
    let (_, r) = agent.call(
        "session/set_mode",
        json!({ "sessionId": sid, "modeId": "bypassPermissions" }),
        |_| json!({}),
    );
    assert_eq!(r["result"], json!({}));
    let (_, reply) = agent.call(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "y" }] }),
        |req| {
            panic!("asked in bypass mode: {req}");
        },
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn");
    let log = claude_log(home.path());
    assert!(log.iter().any(|l| l == &json!({ "subtype": "set_permission_mode", "mode": "bypassPermissions" })));
    let last = log
        .iter()
        .rev()
        .find_map(|l| l.get("permission_reply"))
        .unwrap();
    assert_eq!(last["response"]["behavior"], "allow");
    assert_eq!(
        last["response"]["updatedPermissions"],
        json!([{ "type": "addRules", "rules": [{ "toolName": "Bash" }], "behavior": "allow", "destination": "session" }])
    );

    let (_, bad) = agent.call(
        "session/set_mode",
        json!({ "sessionId": sid, "modeId": "yolo" }),
        |_| json!({}),
    );
    assert_eq!(bad["error"]["message"], "Invalid Mode");
}

#[test]
fn cancel_interrupts_claude_and_the_turn_stops_cancelled() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(home.path());
    let sid = new_session(&mut agent, cwd.path());
    let _ = agent.notifications(Duration::from_millis(200));
    let id = agent.next;
    agent.next += 1;
    agent.send(json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": { "sessionId": sid, "prompt": [{ "type": "text", "text": "sleep" }] } }));
    std::thread::sleep(Duration::from_millis(300));
    agent.send(
        json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": sid } }),
    );
    let reply = loop {
        let m = agent
            .rx
            .recv_timeout(Duration::from_secs(10))
            .expect("prompt reply");
        if m.get("id") == Some(&json!(id)) {
            break m;
        }
    };
    assert_eq!(reply["result"], json!({ "stopReason": "cancelled" }));
    assert!(
        claude_log(home.path())
            .iter()
            .any(|l| l == &json!({ "subtype": "interrupt" }))
    );
}

#[test]
fn load_replays_the_saved_conversation() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let cwd_path = cwd.path().canonicalize().unwrap();
    let encoded: String = cwd_path
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = home.path().join(".claude/projects").join(encoded);
    std::fs::create_dir_all(&dir).unwrap();
    let sid = "11111111-2222-4333-8444-555555555555";
    let lines = [
        json!({ "type": "user", "sessionId": sid, "message": { "role": "user", "content": "remember PAPAYA" } }),
        json!({ "type": "assistant", "sessionId": sid, "message": { "role": "assistant", "content": [{ "type": "text", "text": "Noted." }] } }),
        json!({ "type": "assistant", "isSidechain": true, "sessionId": sid, "message": { "role": "assistant", "content": "sub-agent chatter" } }),
        json!({ "type": "summary", "summary": "x" }),
    ];
    std::fs::write(
        dir.join(format!("{sid}.jsonl")),
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();

    let mut agent = Agent::start(home.path());
    let _ = agent.call(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
        |_| json!({}),
    );
    let (seen, reply) = agent.call(
        "session/load",
        json!({ "sessionId": sid, "cwd": cwd_path, "mcpServers": [] }),
        |_| json!({}),
    );
    assert!(
        reply["result"]["modes"].is_object() && reply["result"]["models"].is_object(),
        "{reply}"
    );
    let u = updates(&seen);
    assert_eq!(
        u.len(),
        2,
        "user and assistant, not the sidechain or summary: {u:?}"
    );
    assert_eq!(
        (
            u[0]["sessionUpdate"].as_str(),
            u[0]["content"]["text"].as_str()
        ),
        (Some("user_message_chunk"), Some("remember PAPAYA"))
    );
    assert_eq!(
        (
            u[1]["sessionUpdate"].as_str(),
            u[1]["content"]["text"].as_str()
        ),
        (Some("agent_message_chunk"), Some("Noted."))
    );
    let argv: Vec<String> =
        serde_json::from_value(claude_log(home.path())[0]["argv"].clone()).unwrap();
    assert!(
        argv.join(" ").contains(&format!("--resume {sid}"))
            && !argv.contains(&"--session-id".to_string())
    );

    let (_, missing) = agent.call(
        "session/load",
        json!({ "sessionId": "nope", "cwd": cwd_path, "mcpServers": [] }),
        |_| json!({}),
    );
    assert_eq!(missing["error"]["message"], "Session not found");
}

/// Send a prompt without waiting for its reply; its request id.
fn prompt_async(agent: &mut Agent, sid: &str, text: &str) -> u64 {
    let id = agent.next;
    agent.next += 1;
    agent.send(json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": { "sessionId": sid, "prompt": [{ "type": "text", "text": text }] } }));
    id
}

/// A request sent while a prompt runs: its reply, and what came before it.
fn call_during(agent: &mut Agent, method: &str, params: Value, seen: &mut Vec<Value>) -> Value {
    let id = agent.next;
    agent.next += 1;
    agent.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    loop {
        let m = agent
            .rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a reply");
        if m.get("id") == Some(&json!(id)) && m.get("method").is_none() {
            return m["result"].clone();
        }
        seen.push(m);
    }
}

/// Everything up to the reply to request `id`, and the reply.
fn until_reply(agent: &mut Agent, id: u64, seen: &mut Vec<Value>) -> Value {
    loop {
        let m = agent
            .rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the prompt's reply");
        if m.get("id") == Some(&json!(id)) && m.get("method").is_none() {
            return m;
        }
        seen.push(m);
    }
}

fn steer_params(sid: &str, message_id: &str, text: &str) -> Value {
    json!({ "sessionId": sid, "messageId": message_id, "prompt": [{ "type": "text", "text": text }] })
}

fn steered(seen: &[Value]) -> Vec<String> {
    seen.iter()
        .filter(|m| m["method"] == "_axel/steered")
        .map(|m| m["params"]["messageId"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_message_sent_into_the_turn_lands_at_its_next_step() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(home.path());
    let sid = new_session(&mut agent, cwd.path());
    let _ = agent.notifications(Duration::from_millis(200));
    // Not while no turn runs: the host sends it as a prompt.
    let mut seen = Vec::new();
    assert_eq!(
        call_during(
            &mut agent,
            "_axel/steer",
            steer_params(&sid, "m0", "hi"),
            &mut seen
        ),
        json!({ "accepted": false })
    );

    let id = prompt_async(&mut agent, &sid, "work");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        call_during(
            &mut agent,
            "_axel/steer",
            steer_params(&sid, "m1", "also this"),
            &mut seen
        ),
        json!({ "accepted": true })
    );
    assert!(steered(&seen).is_empty(), "waiting for the next step");
    assert_eq!(
        call_during(
            &mut agent,
            "_axel/steer",
            steer_params(&sid, "m2", "fold"),
            &mut seen
        ),
        json!({ "accepted": true })
    );
    let reply = until_reply(&mut agent, id, &mut seen);
    assert_eq!(
        reply["result"],
        json!({ "stopReason": "end_turn" }),
        "one turn for all of it"
    );
    assert_eq!(
        steered(&seen),
        vec!["m1", "m2"],
        "each reported as it landed"
    );
    let text: String = updates(&seen)
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert_eq!(text, "read: also this, fold");
    assert!(
        !updates(&seen)
            .iter()
            .any(|u| u["sessionUpdate"] == "user_message_chunk"),
        "Claude's echoes aren't relayed"
    );
    // Each went to Claude with a uuid of its own.
    let uuids: Vec<String> = claude_log(home.path())
        .iter()
        .filter_map(|l| l["steer"]["uuid"].as_str().map(String::from))
        .collect();
    assert_eq!(uuids.len(), 2);
    assert!(
        uuids
            .iter()
            .all(|u| u.len() == 36 && u.as_bytes()[14] == b'4')
            && uuids[0] != uuids[1],
        "{uuids:?}"
    );
}

#[test]
fn a_message_too_late_for_the_turn_runs_in_the_same_prompt() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(home.path());
    let sid = new_session(&mut agent, cwd.path());
    let _ = agent.notifications(Duration::from_millis(200));
    let id = prompt_async(&mut agent, &sid, "work");
    std::thread::sleep(Duration::from_millis(200));
    let mut seen = Vec::new();
    assert_eq!(
        call_during(
            &mut agent,
            "_axel/steer",
            steer_params(&sid, "m1", "late"),
            &mut seen
        ),
        json!({ "accepted": true })
    );
    let reply = until_reply(&mut agent, id, &mut seen);
    assert_eq!(reply["result"], json!({ "stopReason": "end_turn" }));
    assert_eq!(steered(&seen), vec!["m1"]);
    let text: String = updates(&seen)
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert_eq!(
        text, "second turn",
        "the prompt waited for the turn the message started"
    );
}

#[test]
fn a_message_not_taken_in_yet_can_be_withdrawn() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(home.path());
    let sid = new_session(&mut agent, cwd.path());
    let _ = agent.notifications(Duration::from_millis(200));
    let id = prompt_async(&mut agent, &sid, "work");
    std::thread::sleep(Duration::from_millis(200));
    let mut seen = Vec::new();
    call_during(
        &mut agent,
        "_axel/steer",
        steer_params(&sid, "m1", "a typo"),
        &mut seen,
    );
    let unsteer = |agent: &mut Agent, seen: &mut Vec<Value>, m: &str| {
        call_during(
            agent,
            "_axel/unsteer",
            json!({ "sessionId": sid, "messageId": m }),
            seen,
        )
    };
    assert_eq!(
        unsteer(&mut agent, &mut seen, "m1"),
        json!({ "withdrawn": true })
    );
    assert_eq!(
        unsteer(&mut agent, &mut seen, "m1"),
        json!({ "withdrawn": false }),
        "gone already"
    );
    // The edit, sent again: it's the one that lands.
    call_during(
        &mut agent,
        "_axel/steer",
        steer_params(&sid, "m1", "fold"),
        &mut seen,
    );
    let reply = until_reply(&mut agent, id, &mut seen);
    assert_eq!(reply["result"], json!({ "stopReason": "end_turn" }));
    let text: String = updates(&seen)
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert_eq!(
        text, "read: fold",
        "the withdrawn text never reached Claude's turn"
    );
    assert!(
        claude_log(home.path())
            .iter()
            .any(|l| l["subtype"] == "cancel_async_message")
    );
}

#[test]
fn cancelling_withdraws_what_was_sent_in_and_says_so() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(home.path());
    let sid = new_session(&mut agent, cwd.path());
    let _ = agent.notifications(Duration::from_millis(200));
    let id = prompt_async(&mut agent, &sid, "work");
    std::thread::sleep(Duration::from_millis(200));
    let mut seen = Vec::new();
    call_during(
        &mut agent,
        "_axel/steer",
        steer_params(&sid, "m1", "next please"),
        &mut seen,
    );
    agent.send(
        json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": sid } }),
    );
    let reply = until_reply(&mut agent, id, &mut seen);
    assert_eq!(reply["result"], json!({ "stopReason": "cancelled" }));
    let unsteered: Vec<&Value> = seen
        .iter()
        .filter(|m| m["method"] == "_axel/unsteered")
        .collect();
    assert_eq!(unsteered.len(), 1, "{seen:#?}");
    assert_eq!(
        unsteered[0]["params"]["messageIds"],
        json!(["m1"]),
        "the host keeps it queued"
    );
}

/// The text of every agent message chunk, joined.
fn said(ups: &[Value]) -> String {
    ups.iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect()
}

#[test]
fn what_claude_says_between_prompts_shows_up_at_once_and_doesnt_end_the_next_turn() {
    // A background task finished and woke Claude between turns: what it said
    // used to wait in the channel, come out at the next prompt as its reply,
    // and end that prompt in milliseconds, the real answer waiting for the
    // prompt after that.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(home.path());
    let sid = new_session(&mut agent, cwd.path());
    let _ = agent.notifications(Duration::from_millis(300));

    let (_, reply) = agent.call(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "later" }] }),
        |_| json!({}),
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn");
    // Nothing asked, and it shows up anyway, as it is said.
    let idle = updates(&agent.notifications(Duration::from_millis(1500)));
    assert!(said(&idle).contains("background done"), "{idle:?}");

    // The next prompt gets its own answer, nothing left over, and runs its full course.
    let started = std::time::Instant::now();
    let (seen, reply) = agent.call(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "list the files" }] }),
        |_| json!({ "outcome": { "outcome": "selected", "optionId": "allow" } }),
    );
    let ups = updates(&seen);
    assert!(
        said(&ups).contains("Hello") && !said(&ups).contains("background done"),
        "{ups:?}"
    );
    assert!(
        ups.iter().any(|u| u["sessionUpdate"] == "tool_call"),
        "the turn ran its tool call: {ups:?}"
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn");
    let _ = started;
}

#[test]
fn when_the_host_goes_away_claude_goes_too() {
    // The adapter used to clear its session table and exit, leaving the
    // `claude` it started behind (its reader thread held the session, so the
    // kill in Drop never ran).
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(home.path());
    let _ = new_session(&mut agent, cwd.path());
    let pid: i32 = std::fs::read_to_string(home.path().join("claude.pid"))
        .expect("fake claude wrote its pid")
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks that the process exists.
    let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
    assert!(alive(pid), "claude runs while the session does");
    agent.close_stdin();
    let start = std::time::Instant::now();
    while alive(pid) && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(pid), "claude outlived its host");
    let status = agent.child.wait().expect("adapter exits");
    assert!(status.success(), "the adapter exits cleanly: {status}");
}

fn claude_argv(home: &std::path::Path, launch: usize) -> Vec<String> {
    let launches: Vec<Value> = claude_log(home)
        .into_iter()
        .filter(|l| l.get("argv").is_some())
        .collect();
    serde_json::from_value(launches[launch]["argv"].clone()).unwrap()
}

#[test]
fn the_agents_effort_starts_claude_with_the_effort_flag_and_none_adds_no_flag() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(home.path());
    let _ = agent.call(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
        |_| json!({}),
    );
    let (_, plain) = agent.call(
        "session/new",
        json!({ "cwd": cwd.path(), "mcpServers": [] }),
        |_| json!({}),
    );
    assert!(plain["result"]["sessionId"].is_string(), "{plain}");
    assert!(!claude_argv(home.path(), 0).contains(&"--effort".to_string()));

    let (_, set) = agent.call(
        "session/new",
        json!({ "cwd": cwd.path(), "mcpServers": [], "_meta": { "axel": { "effort": "xhigh" } } }),
        |_| json!({}),
    );
    assert!(set["result"]["sessionId"].is_string(), "{set}");
    let argv = claude_argv(home.path(), 1);
    let at = argv.iter().position(|a| a == "--effort").expect("--effort");
    assert_eq!(argv[at + 1], "xhigh");

    // A resumed conversation starts at the effort too.
    let cwd_path = cwd.path().canonicalize().unwrap();
    let encoded: String = cwd_path
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = home.path().join(".claude/projects").join(encoded);
    std::fs::create_dir_all(&dir).unwrap();
    let saved = "11111111-2222-4333-8444-555555555555";
    std::fs::write(dir.join(format!("{saved}.jsonl")), json!({ "type": "user", "sessionId": saved, "message": { "role": "user", "content": "hi" } }).to_string()).unwrap();
    let (_, loaded) = agent.call("session/load", json!({ "sessionId": saved, "cwd": cwd_path, "mcpServers": [], "_meta": { "axel": { "effort": "low" } } }), |_| json!({}));
    assert!(loaded["result"]["modes"].is_object(), "{loaded}");
    let argv = claude_argv(home.path(), 2).join(" ");
    assert!(
        argv.contains("--effort low") && argv.contains(&format!("--resume {saved}")),
        "{argv}"
    );
}
