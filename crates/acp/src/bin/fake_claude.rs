//! A stand-in for the `claude` CLI in stream-json mode, for testing
//! `axel claude-acp`.  It speaks the Agent SDK's control protocol and, per
//! prompt, streams "Hello", runs one Bash tool call through `can_use_tool`
//! and the PostToolUse hook, and ends the turn.  The prompt `sleep` instead
//! waits for an `interrupt`.  The prompt `work` starts a tool call and takes
//! messages sent into the turn, as Claude Code does: each waits in its queue
//! (`cancel_async_message` withdraws it) until one reading `fold` arrives,
//! which takes them all in (echoed, with `--replay-user-messages`) and ends
//! the turn, or one reading `late`, which ends the turn first and runs them
//! as a turn of their own.  The prompt `later` answers at once and, a moment
//! after, says more on its own, as Claude Code does when a background task it
//! started finishes.  Its argv and every control request it receives
//! are appended to `$FAKE_CLAUDE_LOG`, one JSON value per line.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn log(v: &Value) {
    if let Ok(path) = std::env::var("FAKE_CLAUDE_LOG")
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(f, "{v}");
    }
}

fn send(v: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn ok(request_id: &Value, response: Value) {
    send(
        json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": response } }),
    );
}

/// Claude's echo of a user message it took in (`--replay-user-messages`).
fn echo(uuid: &Value, text: &str) {
    send(
        json!({ "type": "user", "isReplay": true, "uuid": uuid, "message": { "role": "user", "content": [{ "type": "text", "text": text }] } }),
    );
}

fn text_reply(text: &str) {
    send(
        json!({ "type": "stream_event", "event": { "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } } }),
    );
    send(
        json!({ "type": "stream_event", "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } } }),
    );
}

/// The `work` prompt: a tool call that runs until a steer says how to end.
fn work(lines: &mut impl Iterator<Item = std::io::Result<String>>) {
    send(
        json!({ "type": "assistant", "message": { "role": "assistant", "model": "claude-sonnet-test", "content": [
        { "type": "tool_use", "id": "toolu_w", "name": "Bash", "input": { "command": "sleep 60", "description": "Work" } }
    ] } }),
    );
    let mut queued: Vec<(Value, String)> = Vec::new();
    while let Some(Ok(l)) = lines.next() {
        let Ok(m) = serde_json::from_str::<Value>(&l) else {
            continue;
        };
        if m["type"] == "control_request" {
            log(&m["request"]);
            match m["request"]["subtype"].as_str() {
                Some("cancel_async_message") => {
                    let before = queued.len();
                    queued.retain(|(u, _)| *u != m["request"]["message_uuid"]);
                    ok(
                        &m["request_id"],
                        json!({ "cancelled": queued.len() < before }),
                    );
                }
                Some("interrupt") => {
                    ok(&m["request_id"], json!({}));
                    send(
                        json!({ "type": "result", "subtype": "error_during_execution", "is_error": false, "errors": [] }),
                    );
                    return;
                }
                _ => ok(&m["request_id"], json!({})),
            }
            continue;
        }
        if m["type"] != "user" {
            continue;
        }
        log(&json!({ "steer": m }));
        let text = m["message"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        queued.push((m["uuid"].clone(), text.clone()));
        match text.as_str() {
            // Taken in at the next step: echoed, then the turn goes on and ends.
            "fold" => {
                send(
                    json!({ "type": "user", "message": { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_w", "content": "worked", "is_error": false }
                ] } }),
                );
                let all: Vec<String> = queued.iter().map(|(_, t)| t.clone()).collect();
                for (u, t) in queued.drain(..) {
                    echo(&u, &t);
                }
                text_reply(&format!("read: {}", all.join(", ")));
                send(
                    json!({ "type": "result", "subtype": "success", "is_error": false, "result": "done" }),
                );
                return;
            }
            // After the last step: the turn ends, then they run as a turn of their own.
            "late" => {
                send(
                    json!({ "type": "user", "message": { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_w", "content": "worked", "is_error": false }
                ] } }),
                );
                send(
                    json!({ "type": "result", "subtype": "success", "is_error": false, "result": "first" }),
                );
                for (u, t) in queued.drain(..) {
                    echo(&u, &t);
                }
                text_reply("second turn");
                send(
                    json!({ "type": "result", "subtype": "success", "is_error": false, "result": "second" }),
                );
                return;
            }
            _ => {}
        }
    }
}

fn main() {
    log(&json!({ "argv": std::env::args().skip(1).collect::<Vec<_>>() }));
    if let Ok(path) = std::env::var("FAKE_CLAUDE_PIDFILE") {
        let _ = std::fs::write(path, std::process::id().to_string());
    }
    let replays = std::env::args().any(|a| a == "--replay-user-messages");
    let session = std::env::args()
        .skip_while(|a| a != "--session-id" && a != "--resume")
        .nth(1)
        .unwrap_or_default();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match msg.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                log(&msg["request"]);
                let id = msg["request_id"].clone();
                match msg["request"]["subtype"].as_str() {
                    Some("initialize") => ok(
                        &id,
                        json!({
                            "commands": [
                                { "name": "compact", "description": "Compact the conversation", "argumentHint": "<instructions>" },
                                { "name": "login", "description": "Log in" }
                            ],
                            "models": [
                                { "value": "claude-sonnet-test", "displayName": "Sonnet Test", "description": "fast" },
                                { "value": "claude-opus-test", "displayName": "Opus Test", "description": "smart" }
                            ]
                        }),
                    ),
                    _ => ok(&id, json!({})),
                }
            }
            Some("user") => {
                let text = msg["message"]["content"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                send(json!({ "type": "system", "subtype": "init", "session_id": session }));
                if replays {
                    echo(&json!("prompt-uuid"), &text);
                }
                if text == "work" {
                    work(&mut lines);
                    continue;
                }
                if text == "later" {
                    // Answer, then go on alone a moment later, as Claude Code
                    // does when a background task it started finishes.
                    text_reply("ok");
                    send(
                        json!({ "type": "result", "subtype": "success", "is_error": false, "result": "ok" }),
                    );
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        text_reply("background done");
                        send(
                            json!({ "type": "assistant", "message": { "role": "assistant", "model": "claude-sonnet-test", "content": [{ "type": "text", "text": "background done" }] } }),
                        );
                        send(
                            json!({ "type": "result", "subtype": "success", "is_error": false, "result": "background done" }),
                        );
                    });
                    continue;
                }
                if text == "sleep" {
                    // Wait for the interrupt, then end the turn as Claude Code does.
                    while let Some(Ok(l)) = lines.next() {
                        let Ok(m) = serde_json::from_str::<Value>(&l) else {
                            continue;
                        };
                        if m["type"] == "control_request" {
                            log(&m["request"]);
                            ok(&m["request_id"], json!({}));
                            if m["request"]["subtype"] == "interrupt" {
                                break;
                            }
                        }
                    }
                    send(
                        json!({ "type": "result", "subtype": "error_during_execution", "is_error": false, "errors": [] }),
                    );
                    continue;
                }
                send(json!({ "type": "stream_event", "event": { "type": "message_start" } }));
                send(
                    json!({ "type": "stream_event", "event": { "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } } }),
                );
                send(
                    json!({ "type": "stream_event", "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hello" } } }),
                );
                send(
                    json!({ "type": "stream_event", "event": { "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {} } } }),
                );
                send(
                    json!({ "type": "assistant", "message": { "role": "assistant", "model": "claude-sonnet-test", "content": [
                    { "type": "text", "text": "Hello" },
                    { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls", "description": "List files" } }
                ] } }),
                );
                // Ask, like Claude Code does with --permission-prompt-tool stdio.
                send(
                    json!({ "type": "control_request", "request_id": "perm-1", "request": {
                    "subtype": "can_use_tool", "tool_name": "Bash", "input": { "command": "ls", "description": "List files" }, "tool_use_id": "toolu_1"
                } }),
                );
                let mut allowed = false;
                while let Some(Ok(l)) = lines.next() {
                    let Ok(m) = serde_json::from_str::<Value>(&l) else {
                        continue;
                    };
                    if m["type"] == "control_response" && m["response"]["request_id"] == "perm-1" {
                        log(&json!({ "permission_reply": m["response"] }));
                        allowed = m["response"]["response"]["behavior"] == "allow";
                        break;
                    }
                }
                if allowed {
                    send(
                        json!({ "type": "control_request", "request_id": "hook-1", "request": {
                        "subtype": "hook_callback", "callback_id": "hook_0", "tool_use_id": "toolu_1",
                        "input": { "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": { "command": "ls" }, "tool_response": { "stdout": "a\nb" } }
                    } }),
                    );
                    while let Some(Ok(l)) = lines.next() {
                        let Ok(m) = serde_json::from_str::<Value>(&l) else {
                            continue;
                        };
                        if m["type"] == "control_response"
                            && m["response"]["request_id"] == "hook-1"
                        {
                            log(&json!({ "hook_reply": m["response"] }));
                            break;
                        }
                    }
                    send(
                        json!({ "type": "user", "message": { "role": "user", "content": [
                        { "type": "tool_result", "tool_use_id": "toolu_1", "content": "a\nb", "is_error": false }
                    ] } }),
                    );
                } else {
                    send(
                        json!({ "type": "user", "message": { "role": "user", "content": [
                        { "type": "tool_result", "tool_use_id": "toolu_1", "content": "User refused permission to run tool", "is_error": true }
                    ] } }),
                    );
                }
                send(
                    json!({ "type": "rate_limit_event", "rate_limit_info": { "status": "allowed", "unifiedWindows": { "five_hour": { "utilization": 0.25, "resetsAt": 1000 }, "seven_day": { "utilization": 0.5, "resetsAt": 2000 } } } }),
                );
                send(
                    json!({ "type": "result", "subtype": "success", "is_error": false, "result": "done", "total_cost_usd": 0.25,
                    "usage": { "input_tokens": 3, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 200, "output_tokens": 40 },
                    "modelUsage": { "claude-sonnet-test": { "costUSD": 0.25, "contextWindow": 200000 } } }),
                );
            }
            _ => {}
        }
    }
}
