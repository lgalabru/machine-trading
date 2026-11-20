//! `axel codex-acp` end to end, against `fake_codex` (`codex app-server`'s
//! JSON-RPC, faked): what it asks Codex for, and the ACP it sends the client.

use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::Duration,
};

use serde_json::{Value, json};

struct Agent {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next: u64,
}

impl Agent {
    fn start(dir: &Path) -> Agent {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codex_acp"))
            .env("CODEX_EXECUTABLE", env!("CARGO_BIN_EXE_fake_codex"))
            .env("FAKE_CODEX_LOG", dir.join("codex.log"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start codex_acp");
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
            stdin,
            rx,
            next: 1,
        }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
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

fn codex_log(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("codex.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn logged<'a>(log: &'a [Value], method: &str) -> Vec<&'a Value> {
    log.iter()
        .filter(|m| m["method"] == method)
        .map(|m| &m["params"])
        .collect()
}

fn init(agent: &mut Agent) -> Value {
    let (_, r) = agent.call("initialize", json!({ "protocolVersion": 1, "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true }, "terminal": false } }), |_| json!({}));
    r["result"].clone()
}

fn allow(option: &'static str) -> impl FnMut(&Value) -> Value {
    move |_| json!({ "outcome": { "outcome": "selected", "optionId": option } })
}

#[test]
fn a_turn_streams_asks_for_approvals_and_runs() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(dir.path());
    let caps = init(&mut agent);
    assert_eq!(caps["agentCapabilities"]["loadSession"], true);

    let (new_seen, new) = agent.call(
        "session/new",
        json!({ "cwd": dir.path(), "mcpServers": [{ "name": "axel", "command": "/bin/axel", "args": ["mcp"], "env": [] }],
                "_meta": { "systemPrompt": { "append": "You are atlas." } } }),
        |_| json!({}),
    );
    let r = &new["result"];
    assert_eq!(r["sessionId"], "01a19a8d-4998-7773-9f60-9dc35e9afdaa");
    let modes: Vec<&str> = r["modes"]["availableModes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        modes,
        ["default", "acceptEdits", "plan", "bypassPermissions"]
    );
    let models: Vec<&str> = r["models"]["availableModels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["modelId"].as_str().unwrap())
        .collect();
    assert_eq!(
        models,
        ["gpt-test", "gpt-big"],
        "hidden models are left out"
    );
    assert_eq!(r["models"]["currentModelId"], "gpt-test");
    let sid = r["sessionId"].as_str().unwrap().to_string();

    // The thread carries the identity and the client's MCP servers.
    let log = codex_log(dir.path());
    let start = logged(&log, "thread/start")[0];
    assert_eq!(start["developerInstructions"], "You are atlas.");
    assert_eq!(start["approvalPolicy"], "untrusted");
    assert_eq!(
        start["config"]["mcp_servers"]["axel"]["command"],
        "/bin/axel"
    );
    assert_eq!(logged(&log, "initialized").len(), 1);

    let (set_seen, set) = agent.call(
        "session/set_model",
        json!({ "sessionId": sid, "modelId": "gpt-big" }),
        |_| json!({}),
    );
    assert!(set.get("result").is_some());

    let mut asked = Vec::new();
    let (seen, reply) = agent.call(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "go" }] }),
        |m| {
            asked.push(m["params"].clone());
            json!({ "outcome": { "outcome": "selected", "optionId": "approved" } })
        },
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn");
    // The spend arrives as the turn runs. The thread's first report counts
    // its newest response only (a resumed thread's running total holds its
    // history); input excludes the cached part.
    let progress = seen
        .iter()
        .find(|m| m["method"] == "_axel/usage")
        .expect("spend reported as it happened");
    let usage = &progress["params"]["usage"];
    assert_eq!(
        (
            usage["inputTokens"].as_u64(),
            usage["cachedReadTokens"].as_u64(),
            usage["outputTokens"].as_u64()
        ),
        (Some(2), Some(6), Some(4))
    );
    assert_eq!(progress["params"]["_meta"]["axel"]["model"], "gpt-big");
    assert!(
        reply["result"].get("usage").is_none(),
        "nothing left for the reply to carry"
    );
    // The plan's limits, read as the session starts.
    let limits = new_seen
        .iter()
        .chain(&set_seen)
        .chain(&seen)
        .find(|m| m["method"] == "_axel/limits")
        .expect("limits sent");
    assert_eq!(
        limits["params"]["windows"][0],
        json!({ "window": "five_hour", "usedPercent": 42.0, "resetsAt": 1000 })
    );
    assert_eq!(limits["params"]["windows"][1]["window"], "week");

    // Two approvals: the file change, then the command, with codex-acp's options.
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0]["toolCall"]["title"], "Edit /w/hello.txt");
    let file_opts: Vec<&str> = asked[0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(file_opts, ["approved", "abort"]);
    assert_eq!(asked[1]["toolCall"]["title"], "Read notes.txt");
    let cmd_opts: Vec<&str> = asked[1]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(
        cmd_opts,
        [
            "approved",
            "approved-execpolicy-amendment",
            "approved-for-session",
            "abort"
        ]
    );
    let text = asked[1]["toolCall"]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(
        text.contains("needs to read outside the sandbox")
            && text.contains("Proposed Amendment: cat"),
        "{text}"
    );

    let ups = updates(&seen);
    let kinds: Vec<&str> = ups
        .iter()
        .map(|u| u["sessionUpdate"].as_str().unwrap())
        .collect();
    // Streamed text isn't sent twice; the unstreamed final message is.
    let said: String = ups
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .map(|u| u["content"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(said, "On it.DONE");
    assert!(
        kinds.contains(&"plan") && kinds.contains(&"usage_update"),
        "{kinds:?}"
    );
    let file_done = ups
        .iter()
        .find(|u| u["toolCallId"] == "f1" && u["sessionUpdate"] == "tool_call_update")
        .unwrap();
    assert_eq!(file_done["status"], "completed");
    assert_eq!(file_done["content"][0]["type"], "diff");
    assert_eq!(file_done["content"][0]["newText"], "hi\n");
    let cmd_start = ups
        .iter()
        .find(|u| u["toolCallId"] == "c1" && u["sessionUpdate"] == "tool_call")
        .unwrap();
    assert_eq!(cmd_start["kind"], "read");
    let cmd_done = ups
        .iter()
        .find(|u| u["toolCallId"] == "c1" && u["sessionUpdate"] == "tool_call_update")
        .unwrap();
    assert_eq!(cmd_done["status"], "completed");
    assert!(
        cmd_done["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("kiwi")
    );
    let usage = ups
        .iter()
        .find(|u| u["sessionUpdate"] == "usage_update")
        .unwrap();
    assert_eq!(
        (usage["used"].as_u64(), usage["size"].as_u64()),
        (Some(12), Some(1000))
    );

    // The turn ran with the session's policy and the chosen model; the
    // approvals went back as Codex decisions.
    let log = codex_log(dir.path());
    let turn = logged(&log, "turn/start")[0];
    assert_eq!(turn["model"], "gpt-big");
    assert_eq!(turn["approvalPolicy"], "untrusted");
    assert_eq!(turn["sandboxPolicy"]["type"], "workspaceWrite");
    assert_eq!(
        turn["input"][0],
        json!({ "type": "text", "text": "go", "text_elements": [] })
    );
    let decisions: Vec<&Value> = log.iter().filter_map(|m| m.get("approval_reply")).collect();
    assert_eq!(
        decisions,
        [
            &json!({ "decision": "accept" }),
            &json!({ "decision": "accept" })
        ]
    );
}

#[test]
fn refusing_cancels_and_full_access_runs_unsandboxed() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(dir.path());
    init(&mut agent);
    let (_, new) = agent.call(
        "session/new",
        json!({ "cwd": dir.path(), "mcpServers": [] }),
        |_| json!({}),
    );
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();

    let (_, mode) = agent.call(
        "session/set_mode",
        json!({ "sessionId": sid, "modeId": "bypassPermissions" }),
        |_| json!({}),
    );
    assert!(mode.get("result").is_some());
    let (_, bad) = agent.call(
        "session/set_mode",
        json!({ "sessionId": sid, "modeId": "yolo" }),
        |_| json!({}),
    );
    assert!(bad.get("error").is_some());

    // Refuse both: the fake fails the turn, which surfaces as an error.
    let (_, reply) = agent.call(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "go" }] }),
        allow("abort"),
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("command refused"),
        "{reply}"
    );

    let log = codex_log(dir.path());
    let turn = logged(&log, "turn/start")[0];
    assert_eq!(turn["approvalPolicy"], "never");
    assert_eq!(turn["sandboxPolicy"]["type"], "dangerFullAccess");
    assert!(turn.get("model").is_none_or(Value::is_null));
    let decisions: Vec<&Value> = log.iter().filter_map(|m| m.get("approval_reply")).collect();
    assert_eq!(
        decisions,
        [
            &json!({ "decision": "cancel" }),
            &json!({ "decision": "cancel" })
        ]
    );
}

#[test]
fn cancel_interrupts_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(dir.path());
    init(&mut agent);
    let (_, new) = agent.call(
        "session/new",
        json!({ "cwd": dir.path(), "mcpServers": [] }),
        |_| json!({}),
    );
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();

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
            .recv_timeout(Duration::from_secs(20))
            .expect("no reply to the prompt");
        if m.get("id") == Some(&json!(id)) {
            break m;
        }
    };
    assert_eq!(reply["result"]["stopReason"], "cancelled");
    let log = codex_log(dir.path());
    assert_eq!(
        logged(&log, "turn/interrupt")[0],
        &json!({ "threadId": sid, "turnId": "turn-1" })
    );
}

#[test]
fn load_resumes_the_thread_and_replays_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(dir.path());
    init(&mut agent);
    let (seen, reply) = agent.call(
        "session/load",
        json!({ "sessionId": "01a19a8d-0000-7000-8000-000000000001", "cwd": dir.path(), "mcpServers": [], "_meta": { "systemPrompt": { "append": "You are atlas." } } }),
        |_| json!({}),
    );
    assert!(
        reply["result"]["modes"].is_object() && reply["result"]["models"].is_object(),
        "{reply}"
    );
    let log = codex_log(dir.path());
    let resume = logged(&log, "thread/resume")[0];
    assert_eq!(resume["threadId"], "01a19a8d-0000-7000-8000-000000000001");
    assert_eq!(resume["developerInstructions"], "You are atlas.");

    let replayed: Vec<String> = updates(&seen)
        .iter()
        .map(|u| {
            let k = u["sessionUpdate"].as_str().unwrap();
            match u.get("title").and_then(Value::as_str) {
                Some(t) => format!("{k}:{t}"),
                None => k.to_string(),
            }
        })
        .collect();
    assert_eq!(
        replayed,
        [
            "user_message_chunk",
            "agent_message_chunk",
            "tool_call:List /w",
            "tool_call_update"
        ],
        "{replayed:?}"
    );
}

#[test]
fn the_agents_effort_is_a_config_override_on_new_and_resumed_threads() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = Agent::start(dir.path());
    init(&mut agent);
    let server = json!({ "name": "axel", "command": "/bin/axel", "args": ["mcp"], "env": [] });
    agent.call(
        "session/new",
        json!({ "cwd": dir.path(), "mcpServers": [] }),
        |_| json!({}),
    );
    agent.call("session/new", json!({ "cwd": dir.path(), "mcpServers": [server], "_meta": { "axel": { "effort": "xhigh" } } }), |_| json!({}));
    agent.call(
        "session/load",
        json!({ "sessionId": "01a19a8d-0000-7000-8000-000000000001", "cwd": dir.path(), "mcpServers": [], "_meta": { "axel": { "effort": "low" } } }),
        |_| json!({}),
    );
    let log = codex_log(dir.path());
    let starts = logged(&log, "thread/start");
    assert!(
        starts[0].get("config").is_none_or(Value::is_null),
        "no effort, no override: {}",
        starts[0]
    );
    assert_eq!(starts[1]["config"]["model_reasoning_effort"], "xhigh");
    assert_eq!(
        starts[1]["config"]["mcp_servers"]["axel"]["command"], "/bin/axel",
        "the MCP override stays"
    );
    assert_eq!(
        logged(&log, "thread/resume")[0]["config"],
        json!({ "model_reasoning_effort": "low" })
    );
}
