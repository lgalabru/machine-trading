//! Codex thread items as ACP tool calls: titles, kinds, locations and diffs.
//! A port of Zed's `codex-acp` 0.16.0 (`parse_command_tool_call`,
//! `extract_tool_call_content_from_changes`, the web search and MCP titles,
//! `build_exec_permission_options`), reading the app-server's items instead
//! of codex-core's events: the same parsed commands (`commandActions`) and
//! file changes arrive there.

use std::path::Path;

use serde_json::{Value, json};

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn join(cwd: &str, path: &str) -> String {
    if Path::new(path).is_absolute() {
        path.to_string()
    } else {
        Path::new(cwd).join(path).display().to_string()
    }
}

/// A command's title, kind, locations and the extension its output is fenced with.
pub struct Command {
    pub title: String,
    pub kind: &'static str,
    pub locations: Vec<Value>,
    pub extension: Option<String>,
}

/// `parse_command_tool_call`: from the item's `commandActions`.
pub fn command(item: &Value) -> Command {
    let cwd = s(item, "cwd").unwrap_or("");
    let mut titles = Vec::new();
    let mut locations = Vec::new();
    let mut extension = None;
    let mut kind = "execute";
    for action in item
        .get("commandActions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut path = None;
        match s(action, "type").unwrap_or("") {
            "read" => {
                titles.push(format!("Read {}", s(action, "name").unwrap_or("")));
                let p = s(action, "path").unwrap_or("");
                extension = Path::new(p)
                    .extension()
                    .map(|e| e.to_string_lossy().into_owned());
                path = Some(p.to_string());
                kind = "read";
            }
            "listFiles" => {
                let dir = match s(action, "path") {
                    Some(p) => join(cwd, p),
                    None => cwd.to_string(),
                };
                titles.push(format!("List {dir}"));
                path = s(action, "path").map(String::from);
                kind = "search";
            }
            "search" => {
                titles.push(match (s(action, "query"), s(action, "path")) {
                    (Some(q), Some(p)) => format!("Search {q} in {p}"),
                    (Some(q), None) => format!("Search {q}"),
                    _ => format!("Search {}", s(action, "command").unwrap_or("")),
                });
                kind = "search";
            }
            _ => titles.push(s(action, "command").unwrap_or("").to_string()),
        }
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            locations.push(json!({ "path": join(cwd, &p) }));
        }
    }
    let title = if titles.is_empty() {
        s(item, "command").unwrap_or("Terminal").to_string()
    } else {
        titles.join(", ")
    };
    Command {
        title,
        kind,
        locations,
        extension,
    }
}

/// The output block shown when a command ends: fenced by the file it read.
pub fn command_output(output: &str, extension: Option<&str>) -> Option<Value> {
    if output.is_empty() {
        return None;
    }
    let text = match extension {
        Some("md") => output.to_string(),
        Some(ext) => format!("```{ext}\n{}\n```\n", output.trim_end_matches('\n')),
        None => format!("```sh\n{}\n```\n", output.trim_end_matches('\n')),
    };
    Some(json!({ "type": "content", "content": { "type": "text", "text": text } }))
}

/// `extract_tool_call_content_from_changes`: title, locations and diffs.
pub fn file_changes(item: &Value) -> (String, Vec<Value>, Vec<Value>) {
    let changes = item
        .get("changes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = |c: &Value| -> String {
        c.pointer("/kind/move_path")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| s(c, "path").unwrap_or("").to_string())
    };
    let title = if changes.is_empty() {
        "Edit".to_string()
    } else {
        format!(
            "Edit {}",
            changes.iter().map(target).collect::<Vec<_>>().join(", ")
        )
    };
    let locations = changes
        .iter()
        .map(|c| json!({ "path": target(c) }))
        .collect();
    let mut content = Vec::new();
    for c in &changes {
        let path = target(c);
        let diff = s(c, "diff").unwrap_or("");
        match c.pointer("/kind/type").and_then(Value::as_str) {
            Some("add") => content
                .push(json!({ "type": "diff", "path": path, "oldText": null, "newText": diff })),
            Some("delete") => content
                .push(json!({ "type": "diff", "path": path, "oldText": diff, "newText": "" })),
            _ => content.extend(unified_diff(&path, diff)),
        }
    }
    (title, locations, content)
}

/// One ACP diff per hunk of a unified diff; the raw text when it won't parse.
pub fn unified_diff(path: &str, diff: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut hunk: Option<(String, String)> = None;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("@@") {
            if let Some((old, new)) = hunk.take() {
                out.push(json!({ "type": "diff", "path": path, "oldText": old, "newText": new }));
            }
            hunk = Some((String::new(), String::new()));
            continue;
        }
        let Some((old, new)) = hunk.as_mut() else {
            continue;
        };
        if let Some(t) = line.strip_prefix('+') {
            new.push_str(t);
        } else if let Some(t) = line.strip_prefix('-') {
            old.push_str(t);
        } else if let Some(t) = line.strip_prefix(' ') {
            old.push_str(t);
            new.push_str(t);
        } else if line.starts_with('\\') {
            // "\ No newline at end of file"
        } else if line == "\n" {
            old.push('\n');
            new.push('\n');
        }
    }
    if let Some((old, new)) = hunk {
        out.push(json!({ "type": "diff", "path": path, "oldText": old, "newText": new }));
    }
    if out.is_empty() {
        out.push(json!({ "type": "content", "content": { "type": "text", "text": diff } }));
    }
    out
}

/// `update_web_search_query`: the title once the action is known.
pub fn web_search_title(item: &Value) -> String {
    let action = item.get("action").cloned().unwrap_or(Value::Null);
    match s(&action, "type") {
        Some("search") => action
            .get("queries")
            .and_then(Value::as_array)
            .filter(|q| !q.is_empty())
            .map(|q| {
                format!(
                    "Searching for: {}",
                    q.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .or_else(|| s(&action, "query").map(|q| format!("Searching for: {q}")))
            .or_else(|| {
                s(item, "query")
                    .filter(|q| !q.is_empty())
                    .map(|q| format!("Searching for: {q}"))
            })
            .unwrap_or_else(|| "Web search".into()),
        Some("openPage") => s(&action, "url")
            .map(|u| format!("Opening: {u}"))
            .unwrap_or_else(|| "Open page".into()),
        Some("findInPage") => match (s(&action, "pattern"), s(&action, "url")) {
            (Some(p), Some(u)) => format!("Finding: {p} in {u}"),
            (Some(p), None) => format!("Finding: {p}"),
            (None, Some(u)) => format!("Find in page: {u}"),
            (None, None) => "Find in page".into(),
        },
        _ => "Web search".into(),
    }
}

/// MCP result content as ACP tool content.
pub fn mcp_content(result: &Value) -> Vec<Value> {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").is_some())
                .map(|b| json!({ "type": "content", "content": b }))
                .collect()
        })
        .unwrap_or_default()
}

/// One permission option: its ACP form and the app-server decision it maps to.
pub struct ExecOption {
    pub id: &'static str,
    pub name: String,
    pub kind: &'static str,
    pub decision: Value,
}

/// `build_exec_permission_options` for a command approval request.
pub fn exec_options(params: &Value) -> Vec<ExecOption> {
    let network = params
        .get("networkApprovalContext")
        .is_some_and(|v| !v.is_null());
    let mut options = vec![ExecOption {
        id: "approved",
        name: if network {
            t!("Yes, just this once")
        } else {
            t!("Yes, proceed")
        }
        .into(),
        kind: "allow_once",
        decision: json!("accept"),
    }];
    if let Some(amendment) = params
        .get("proposedExecpolicyAmendment")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        let prefix = amendment
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        let name = if prefix.is_empty() || prefix.contains('\n') || prefix.contains('\r') {
            t!("Yes, and remember this command pattern").to_string()
        } else {
            t!(
                "Yes, and don't ask again for commands that start with `{prefix}`",
                prefix = prefix
            )
        };
        options.push(ExecOption {
            id: "approved-execpolicy-amendment",
            name,
            kind: "allow_always",
            decision: json!({ "acceptWithExecpolicyAmendment": { "execpolicy_amendment": amendment } }),
        });
    }
    options.push(ExecOption {
        id: "approved-for-session",
        name: if network {
            t!("Yes, and allow this host for this session")
        } else {
            t!("Yes, and don't ask again for this command in this session")
        }
        .into(),
        kind: "allow_always",
        decision: json!("acceptForSession"),
    });
    options.push(ExecOption {
        id: "abort",
        name: t!("No, and tell Codex what to do differently").into(),
        kind: "reject_once",
        decision: json!("cancel"),
    });
    options
}

/// The details shown with a command approval: reason, amendment, network host.
pub fn exec_approval_text(params: &Value) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(r) = s(params, "reason") {
        lines.push(r.to_string());
    }
    if let Some(a) = params
        .get("proposedExecpolicyAmendment")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        lines.push(t!(
            "Proposed Amendment: {command}",
            command = a
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if let Some(n) = params
        .get("networkApprovalContext")
        .filter(|v| !v.is_null())
    {
        lines.push(t!(
            "Network Approval Context: {protocol} {host}",
            protocol = s(n, "protocol").unwrap_or(""),
            host = s(n, "host").unwrap_or("")
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_titled_by_what_they_do() {
        let read = command(
            &json!({ "cwd": "/w", "command": "zsh -lc 'cat notes.md'", "commandActions": [{ "type": "read", "command": "cat notes.md", "name": "notes.md", "path": "notes.md" }] }),
        );
        assert_eq!(
            (read.title.as_str(), read.kind, read.extension.as_deref()),
            ("Read notes.md", "read", Some("md"))
        );
        assert_eq!(read.locations, vec![json!({ "path": "/w/notes.md" })]);

        let ls = command(
            &json!({ "cwd": "/w", "commandActions": [{ "type": "listFiles", "command": "ls src", "path": "src" }] }),
        );
        assert_eq!((ls.title.as_str(), ls.kind), ("List /w/src", "search"));
        let rg = command(
            &json!({ "cwd": "/w", "commandActions": [{ "type": "search", "command": "rg foo", "query": "foo", "path": "src" }] }),
        );
        assert_eq!(rg.title, "Search foo in src");
        let other = command(
            &json!({ "cwd": "/w", "commandActions": [{ "type": "unknown", "command": "cargo test" }] }),
        );
        assert_eq!(
            (other.title.as_str(), other.kind),
            ("cargo test", "execute")
        );

        assert_eq!(
            command_output("a\n", Some("rs")).unwrap()["content"]["text"],
            "```rs\na\n```\n"
        );
        assert_eq!(
            command_output("a\n", None).unwrap()["content"]["text"],
            "```sh\na\n```\n"
        );
        assert!(command_output("", None).is_none());
    }

    #[test]
    fn file_changes_become_diffs() {
        let item = json!({ "changes": [
            { "path": "/w/new.txt", "kind": { "type": "add" }, "diff": "hi\n" },
            { "path": "/w/a.rs", "kind": { "type": "update", "move_path": null }, "diff": "@@ -1,2 +1,2 @@\n fn a() {}\n-old\n+new\n" },
            { "path": "/w/b.rs", "kind": { "type": "update", "move_path": "/w/c.rs" }, "diff": "@@ -1 +1 @@\n-x\n\\ No newline at end of file\n+y\n" }
        ] });
        let (title, locations, content) = file_changes(&item);
        assert_eq!(title, "Edit /w/new.txt, /w/a.rs, /w/c.rs");
        assert_eq!(locations[2], json!({ "path": "/w/c.rs" }));
        assert_eq!(
            content[0],
            json!({ "type": "diff", "path": "/w/new.txt", "oldText": null, "newText": "hi\n" })
        );
        assert_eq!(
            content[1],
            json!({ "type": "diff", "path": "/w/a.rs", "oldText": "fn a() {}\nold\n", "newText": "fn a() {}\nnew\n" })
        );
        assert_eq!(
            content[2],
            json!({ "type": "diff", "path": "/w/c.rs", "oldText": "x\n", "newText": "y\n" })
        );
    }

    #[test]
    fn approvals_offer_codex_acps_options() {
        let ids: Vec<&str> = exec_options(&json!({})).iter().map(|o| o.id).collect();
        assert_eq!(ids, ["approved", "approved-for-session", "abort"]);
        let with = exec_options(&json!({ "proposedExecpolicyAmendment": ["cargo", "test"] }));
        assert_eq!(
            with[1].name,
            "Yes, and don't ask again for commands that start with `cargo test`"
        );
        assert_eq!(
            with[1].decision,
            json!({ "acceptWithExecpolicyAmendment": { "execpolicy_amendment": ["cargo", "test"] } })
        );
        assert_eq!(
            web_search_title(
                &json!({ "query": "x", "action": { "type": "search", "query": "rex terminal" } })
            ),
            "Searching for: rex terminal"
        );
        assert_eq!(
            web_search_title(&json!({ "action": { "type": "openPage", "url": "https://a.b" } })),
            "Opening: https://a.b"
        );
    }
}
