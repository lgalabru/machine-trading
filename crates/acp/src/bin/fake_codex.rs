//! A stand-in for `codex app-server`, for testing `axel codex-acp`.  It
//! answers initialize / thread/start / thread/resume / model/list, and per
//! `turn/start` streams a message, then a file change and a command that each
//! ask for approval, a plan and token usage, and completes the turn (status
//! `failed` if the command was refused).  The prompt `sleep` instead waits for
//! `turn/interrupt`.  Every request it receives (and every approval reply) is
//! appended to `$FAKE_CODEX_LOG`.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn log(v: &Value) {
    if let Ok(path) = std::env::var("FAKE_CODEX_LOG")
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

fn note(method: &str, params: Value) {
    send(json!({ "method": method, "params": params }));
}

const THREAD: &str = "01a19a8d-4998-7773-9f60-9dc35e9afdaa";

fn main() {
    assert_eq!(std::env::args().nth(1).as_deref(), Some("app-server"));
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut turns = 0;
    let mut next_req = 100;
    while let Some(Ok(line)) = lines.next() {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            continue;
        };
        log(&json!({ "method": method, "params": msg.get("params") }));
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply = |result: Value| send(json!({ "id": id, "result": result }));
        match method {
            "initialize" => reply(json!({ "userAgent": "fake", "platformOs": "macos" })),
            "initialized" => {}
            "thread/start" => reply(
                json!({ "thread": { "id": THREAD, "turns": [] }, "model": "gpt-test", "cwd": params["cwd"] }),
            ),
            "thread/resume" => reply(
                json!({ "thread": { "id": params["threadId"], "turns": [{ "id": "t0", "status": "completed", "items": [
                { "type": "userMessage", "id": "u0", "content": [{ "type": "text", "text": "remember PAPAYA", "text_elements": [] }] },
                { "type": "agentMessage", "id": "m0", "text": "Noted." },
                { "type": "commandExecution", "id": "c0", "command": "ls", "cwd": "/w", "status": "completed", "exitCode": 0, "aggregatedOutput": "a\n",
                  "commandActions": [{ "type": "listFiles", "command": "ls" }] }
            ] }] }, "model": "gpt-test" }),
            ),
            "account/rateLimits/read" => reply(
                json!({ "rateLimits": { "limitId": "codex", "planType": "plus",
                "primary": { "usedPercent": 42.0, "windowDurationMins": 300, "resetsAt": 1000 },
                "secondary": { "usedPercent": 7.0, "windowDurationMins": 10080, "resetsAt": 2000 } } }),
            ),
            "model/list" => reply(json!({ "data": [
                { "id": "gpt-test", "displayName": "GPT Test", "description": "fast", "hidden": false, "isDefault": true },
                { "id": "gpt-big", "displayName": "GPT Big", "description": "smart", "hidden": false, "isDefault": false },
                { "id": "gpt-hidden", "displayName": "Hidden", "description": "", "hidden": true, "isDefault": false }
            ] })),
            "turn/start" => {
                turns += 1;
                let turn = format!("turn-{turns}");
                reply(json!({ "turn": { "id": turn, "status": "inProgress", "items": [] } }));
                let text = params["input"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                note(
                    "turn/started",
                    json!({ "threadId": THREAD, "turn": { "id": turn } }),
                );
                if text == "sleep" {
                    while let Some(Ok(l)) = lines.next() {
                        let Ok(m) = serde_json::from_str::<Value>(&l) else {
                            continue;
                        };
                        log(&json!({ "method": m.get("method"), "params": m.get("params") }));
                        if m["method"] == "turn/interrupt" {
                            send(json!({ "id": m["id"], "result": {} }));
                            break;
                        }
                    }
                    note(
                        "turn/completed",
                        json!({ "threadId": THREAD, "turn": { "id": turn, "status": "interrupted" } }),
                    );
                    continue;
                }
                note(
                    "item/started",
                    json!({ "item": { "type": "agentMessage", "id": "m1", "text": "" } }),
                );
                note(
                    "item/agentMessage/delta",
                    json!({ "itemId": "m1", "delta": "On " }),
                );
                note(
                    "item/agentMessage/delta",
                    json!({ "itemId": "m1", "delta": "it." }),
                );
                note(
                    "item/completed",
                    json!({ "item": { "type": "agentMessage", "id": "m1", "text": "On it." } }),
                );
                // A file change that asks first.
                let change = json!({ "type": "fileChange", "id": "f1", "status": "inProgress", "changes": [{ "path": "/w/hello.txt", "kind": { "type": "add" }, "diff": "hi\n" }] });
                note("item/started", json!({ "item": change }));
                next_req += 1;
                send(
                    json!({ "id": next_req, "method": "item/fileChange/requestApproval", "params": { "threadId": THREAD, "turnId": turn, "itemId": "f1", "reason": null } }),
                );
                let file_decision = wait_reply(&mut lines, next_req);
                let mut done = change.clone();
                done["status"] = json!(if file_decision["decision"] == "accept" {
                    "completed"
                } else {
                    "declined"
                });
                note("item/completed", json!({ "item": done }));
                // A command that asks first.
                let cmd = json!({ "type": "commandExecution", "id": "c1", "command": "/bin/zsh -lc 'cat notes.txt'", "cwd": "/w", "status": "inProgress",
                    "commandActions": [{ "type": "read", "command": "cat notes.txt", "name": "notes.txt", "path": "notes.txt" }] });
                note("item/started", json!({ "item": cmd }));
                next_req += 1;
                send(
                    json!({ "id": next_req, "method": "item/commandExecution/requestApproval", "params": {
                    "threadId": THREAD, "turnId": turn, "itemId": "c1", "command": "/bin/zsh -lc 'cat notes.txt'", "cwd": "/w",
                    "reason": "needs to read outside the sandbox", "proposedExecpolicyAmendment": ["cat"] } }),
                );
                let cmd_decision = wait_reply(&mut lines, next_req);
                let accepted = cmd_decision["decision"] == "accept";
                if accepted {
                    note(
                        "item/commandExecution/outputDelta",
                        json!({ "itemId": "c1", "delta": "kiwi\n" }),
                    );
                }
                let mut ended = cmd.clone();
                ended["status"] = json!(if accepted { "completed" } else { "declined" });
                ended["exitCode"] = if accepted { json!(0) } else { Value::Null };
                note("item/completed", json!({ "item": ended }));
                note(
                    "turn/plan/updated",
                    json!({ "plan": [{ "step": "write", "status": "completed" }, { "step": "read", "status": "inProgress" }] }),
                );
                note(
                    "thread/tokenUsage/updated",
                    json!({ "tokenUsage": {
                    "total": { "inputTokens": 20, "cachedInputTokens": 15, "outputTokens": 10, "reasoningOutputTokens": 4, "totalTokens": 30 },
                    "last": { "inputTokens": 8, "cachedInputTokens": 6, "outputTokens": 4, "reasoningOutputTokens": 1, "totalTokens": 12 },
                    "modelContextWindow": 1000 } }),
                );
                note(
                    "item/completed",
                    json!({ "item": { "type": "agentMessage", "id": "m2", "text": "DONE" } }),
                );
                let status = if accepted { "completed" } else { "failed" };
                note(
                    "turn/completed",
                    json!({ "threadId": THREAD, "turn": { "id": turn, "status": status, "error": if accepted { Value::Null } else { json!({ "message": "command refused" }) } } }),
                );
            }
            _ => {
                if id.is_some() {
                    send(json!({ "id": id, "error": { "code": -32601, "message": "unknown" } }));
                }
            }
        }
    }
}

fn wait_reply(lines: &mut impl Iterator<Item = std::io::Result<String>>, want: u64) -> Value {
    for l in lines.by_ref() {
        let Ok(l) = l else { break };
        let Ok(m) = serde_json::from_str::<Value>(&l) else {
            continue;
        };
        if m.get("id").and_then(Value::as_u64) == Some(want) && m.get("method").is_none() {
            log(&json!({ "approval_reply": m["result"] }));
            return m["result"].clone();
        }
    }
    Value::Null
}
