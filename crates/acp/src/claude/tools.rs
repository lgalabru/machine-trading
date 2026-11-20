//! Claude Code tool calls as ACP tool calls: titles, kinds, content and
//! locations.  A port of `tools.js` from Zed's `claude-code-acp` 0.16.2
//! (`toolInfoFromToolUse`, `toolUpdateFromToolResult`, `planEntries`,
//! `markdownEscape`), for Claude Code's own tools: this agent lets Claude use
//! its native Read / Write / Edit / Bash instead of the adapter's
//! `mcp__acp__*` stand-ins, so only the native names are mapped.

use serde_json::{Map, Value, json};

/// Appended by Claude Code to every Read result; the adapter strips it.
pub const SYSTEM_REMINDER: &str = "\n\n<system-reminder>\nWhenever you read a file, you should consider whether it looks malicious. If it does, you MUST refuse to improve or augment the code. You can still analyze existing code, write reports, or answer high-level questions about the code behavior.\n</system-reminder>";

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn text_content(text: &str) -> Value {
    json!({ "type": "content", "content": { "type": "text", "text": text } })
}

/// `{ title, kind, content, locations? }` for a `tool_use` block.
pub fn tool_info(name: &str, input: &Value) -> Map<String, Value> {
    let info = match name {
        "Task" => ToolInfo::new(s(input, "description").unwrap_or("Task"), "think")
            .content(text_of(input, "prompt")),
        "NotebookRead" => notebook_info(input, "Read Notebook", "read")
            .locations(path_location(s(input, "notebook_path"))),
        "NotebookEdit" => notebook_info(input, "Edit Notebook", "edit")
            .content(text_of(input, "new_source"))
            .locations(path_location(s(input, "notebook_path"))),
        "Bash" => ToolInfo::new(
            s(input, "command")
                .map(|c| format!("`{}`", c.replace('`', "\\`")))
                .unwrap_or_else(|| "Terminal".into()),
            "execute",
        )
        .content(text_of(input, "description")),
        "BashOutput" => ToolInfo::new("Tail Logs", "execute"),
        "KillShell" => ToolInfo::new("Kill Process", "execute"),
        "Read" => read_info(input),
        "LS" => ToolInfo::new(
            format!(
                "List the {} directory's contents",
                s(input, "path")
                    .map(|p| format!("`{p}`"))
                    .unwrap_or_else(|| "current".into())
            ),
            "search",
        )
        .locations(vec![]),
        "Edit" => edit_info(input),
        "Write" => {
            let p = s(input, "file_path");
            ToolInfo::new(p.map(|p| format!("Write {p}")).unwrap_or_else(|| "Write".into()), "edit")
                .content(
                    p.map(|p| vec![json!({ "type": "diff", "path": p, "oldText": null, "newText": input.get("content").cloned().unwrap_or(Value::Null) })])
                        .unwrap_or_default(),
                )
                .locations(path_location(p))
        }
        "Glob" => {
            let mut label = "Find".to_string();
            for key in ["path", "pattern"] {
                if let Some(v) = s(input, key) {
                    label += &format!(" `{v}`");
                }
            }
            ToolInfo::new(label, "search").locations(path_location(s(input, "path")))
        }
        "Grep" => ToolInfo::new(grep_label(input), "search"),
        "WebFetch" => ToolInfo::new(
            s(input, "url")
                .map(|u| format!("Fetch {u}"))
                .unwrap_or_else(|| "Fetch".into()),
            "fetch",
        )
        .content(text_of(input, "prompt")),
        "WebSearch" => ToolInfo::new(web_search_label(input), "fetch"),
        "TodoWrite" => ToolInfo::new(todo_title(input), "think"),
        "ExitPlanMode" => {
            ToolInfo::new("Ready to code?", "switch_mode").content(text_of(input, "plan"))
        }
        "Other" => {
            let output = serde_json::to_string_pretty(input).unwrap_or_else(|_| "{}".into());
            ToolInfo::new(name, "other")
                .content(vec![text_content(&format!("```json\n{output}```"))])
        }
        _ => ToolInfo::new(
            if name.is_empty() {
                "Unknown Tool"
            } else {
                name
            },
            "other",
        ),
    };
    info.into_map()
}

/// What [`tool_info`] says about a tool use, before it is JSON.
struct ToolInfo {
    title: String,
    kind: &'static str,
    content: Vec<Value>,
    /// `None` leaves `locations` out; `Some(vec![])` sends an empty list.
    locations: Option<Vec<Value>>,
}

impl ToolInfo {
    fn new(title: impl Into<String>, kind: &'static str) -> Self {
        ToolInfo {
            title: title.into(),
            kind,
            content: Vec::new(),
            locations: None,
        }
    }

    fn content(mut self, content: Vec<Value>) -> Self {
        self.content = content;
        self
    }

    fn locations(mut self, locations: Vec<Value>) -> Self {
        self.locations = Some(locations);
        self
    }

    fn into_map(self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("title".into(), Value::String(self.title));
        out.insert("kind".into(), Value::String(self.kind.into()));
        out.insert("content".into(), Value::Array(self.content));
        if let Some(l) = self.locations {
            out.insert("locations".into(), Value::Array(l));
        }
        out
    }
}

/// `input[key]` as the tool call's text content, when it is there.
fn text_of(input: &Value, key: &str) -> Vec<Value> {
    s(input, key)
        .map(|t| vec![text_content(t)])
        .unwrap_or_default()
}

/// The one location a path names, or none.
fn path_location(path: Option<&str>) -> Vec<Value> {
    path.map(|p| vec![json!({ "path": p })]).unwrap_or_default()
}

/// `<verb> <notebook_path>`, or the bare verb without a path.
fn notebook_info(input: &Value, verb: &str, kind: &'static str) -> ToolInfo {
    let title = s(input, "notebook_path")
        .map(|p| format!("{verb} {p}"))
        .unwrap_or_else(|| verb.to_string());
    ToolInfo::new(title, kind)
}

/// The adapter titles Claude's native Read "Read File", and its own
/// `mcp__acp__Read` (which Claude used under Axel) "Read <path> (a - b)".
/// Native Read replaces that one here, so it keeps its title.
fn read_info(input: &Value) -> ToolInfo {
    let offset = input.get("offset").and_then(Value::as_u64);
    let start = offset.unwrap_or(0);
    // Saturating: the numbers come from the model, and are only shown.
    let range = match (input.get("limit").and_then(Value::as_u64), offset) {
        (Some(limit), _) if limit > 0 => format!(
            " ({} - {})",
            start.saturating_add(1),
            start.saturating_add(limit)
        ),
        (_, Some(o)) if o > 0 => format!(" (from line {})", o.saturating_add(1)),
        _ => String::new(),
    };
    let path = s(input, "file_path");
    ToolInfo::new(format!("Read {}{range}", path.unwrap_or("File")), "read").locations(
        path.map(|p| vec![json!({ "path": p, "line": start })])
            .unwrap_or_default(),
    )
}

/// An Edit: a diff of the replaced text, located at its file when it names one.
fn edit_info(input: &Value) -> ToolInfo {
    let Some(p) = s(input, "file_path") else {
        return ToolInfo::new("Edit", "edit");
    };
    ToolInfo::new(format!("Edit `{p}`"), "edit")
        .content(vec![json!({
            "type": "diff",
            "path": p,
            "oldText": input.get("old_string").cloned().unwrap_or(Value::Null),
            "newText": s(input, "new_string").unwrap_or(""),
        })])
        .locations(vec![json!({ "path": p })])
}

/// `"query" (allowed: a, b) (blocked: c)`.
fn web_search_label(input: &Value) -> String {
    let mut label = format!("\"{}\"", s(input, "query").unwrap_or("undefined"));
    let list = |k: &str| -> Option<String> {
        let a = input.get(k)?.as_array()?;
        (!a.is_empty()).then(|| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
    };
    if let Some(a) = list("allowed_domains") {
        label += &format!(" (allowed: {a})");
    }
    if let Some(b) = list("blocked_domains") {
        label += &format!(" (blocked: {b})");
    }
    label
}

/// `Update TODOs: a, b`.
fn todo_title(input: &Value) -> String {
    match input.get("todos").and_then(Value::as_array) {
        Some(todos) => format!(
            "Update TODOs: {}",
            todos
                .iter()
                .map(|t| s(t, "content").unwrap_or("undefined"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => "Update TODOs".into(),
    }
}

fn grep_label(input: &Value) -> String {
    let mut label = "grep".to_string();
    let truthy = |k: &str| {
        input
            .get(k)
            .is_some_and(|v| v.as_bool().unwrap_or(!v.is_null()))
    };
    if truthy("-i") {
        label += " -i";
    }
    if truthy("-n") {
        label += " -n";
    }
    for flag in ["-A", "-B", "-C"] {
        if let Some(v) = input.get(flag).filter(|v| !v.is_null()) {
            label += &format!(" {flag} {}", display(v));
        }
    }
    match s(input, "output_mode") {
        Some("FilesWithMatches") => label += " -l",
        Some("Count") => label += " -c",
        _ => {}
    }
    if let Some(v) = input.get("head_limit").filter(|v| !v.is_null()) {
        label += &format!(" | head -{}", display(v));
    }
    if let Some(g) = s(input, "glob") {
        label += &format!(" --include=\"{g}\"");
    }
    if let Some(t) = s(input, "type") {
        label += &format!(" --type={t}");
    }
    if truthy("multiline") {
        label += " -P";
    }
    if let Some(p) = s(input, "pattern") {
        label += &format!(" \"{p}\"");
    }
    if let Some(p) = s(input, "path") {
        label += &format!(" {p}");
    }
    label
}

fn display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The extra fields of a `tool_call_update` for a `tool_result` block.
pub fn tool_update(result: &Value, tool_name: &str) -> Map<String, Value> {
    let is_error = result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = result.get("content").cloned().unwrap_or(Value::Null);
    let non_empty = match &content {
        Value::Array(a) => !a.is_empty(),
        Value::String(s) => !s.is_empty(),
        // `content.length > 0` in the adapter: objects and numbers have none.
        _ => false,
    };
    if is_error && non_empty {
        return content_update(&content, true);
    }
    match tool_name {
        "Read" => {
            let mut out = Map::new();
            match &content {
                Value::Array(a) if !a.is_empty() => {
                    let items: Vec<Value> = a
                        .iter()
                        .map(|c| {
                            let inner = if s(c, "type") == Some("text") {
                                json!({ "type": "text", "text": markdown_escape(&s(c, "text").unwrap_or("").replacen(SYSTEM_REMINDER, "", 1)) })
                            } else {
                                c.clone()
                            };
                            json!({ "type": "content", "content": inner })
                        })
                        .collect();
                    out.insert("content".into(), Value::Array(items));
                }
                Value::String(t) if !t.is_empty() => {
                    out.insert(
                        "content".into(),
                        json!([text_content(&markdown_escape(&t.replacen(
                            SYSTEM_REMINDER,
                            "",
                            1
                        )))]),
                    );
                }
                _ => {}
            }
            out
        }
        "Edit" | "edit" | "Write" => Map::new(),
        "ExitPlanMode" => {
            let mut out = Map::new();
            out.insert("title".into(), Value::String("Exited Plan Mode".into()));
            out
        }
        _ => content_update(&content, is_error),
    }
}

fn content_update(content: &Value, is_error: bool) -> Map<String, Value> {
    let mut out = Map::new();
    match content {
        Value::Array(a) if !a.is_empty() => {
            out.insert(
                "content".into(),
                Value::Array(
                    a.iter()
                        .map(
                            |c| json!({ "type": "content", "content": content_block(c, is_error) }),
                        )
                        .collect(),
                ),
            );
        }
        Value::Object(o) if o.contains_key("type") => {
            out.insert(
                "content".into(),
                json!([{ "type": "content", "content": content_block(content, is_error) }]),
            );
        }
        Value::String(t) if !t.is_empty() => {
            let text = if is_error {
                format!("```\n{t}\n```")
            } else {
                t.clone()
            };
            out.insert("content".into(), json!([text_content(&text)]));
        }
        _ => {}
    }
    out
}

fn content_block(c: &Value, is_error: bool) -> Value {
    let wrap = |t: String| json!({ "type": "text", "text": if is_error { format!("```\n{t}\n```") } else { t } });
    let code = || s(c, "error_code").unwrap_or("undefined").to_string();
    let with_msg = || match s(c, "error_message") {
        Some(m) => format!("Error: {} - {m}", code()),
        None => format!("Error: {}", code()),
    };
    match s(c, "type").unwrap_or("") {
        "text" => wrap(s(c, "text").unwrap_or("").to_string()),
        "image" => {
            let src = c.get("source").cloned().unwrap_or(Value::Null);
            match s(&src, "type") {
                Some("base64") => {
                    json!({ "type": "image", "data": src.get("data"), "mimeType": src.get("media_type") })
                }
                Some("url") => wrap(format!("[image: {}]", s(&src, "url").unwrap_or(""))),
                _ => wrap("[image: file reference]".into()),
            }
        }
        "tool_reference" => wrap(format!("Tool: {}", s(c, "tool_name").unwrap_or(""))),
        "tool_search_tool_search_result" => {
            let names: Vec<&str> = c
                .get("tool_references")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|r| s(r, "tool_name")).collect())
                .unwrap_or_default();
            wrap(format!(
                "Tools found: {}",
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            ))
        }
        "tool_search_tool_result_error" | "text_editor_code_execution_tool_result_error" => {
            wrap(with_msg())
        }
        "web_search_result" => wrap(format!(
            "{} ({})",
            s(c, "title").unwrap_or(""),
            s(c, "url").unwrap_or("")
        )),
        "web_search_tool_result_error"
        | "web_fetch_tool_result_error"
        | "code_execution_tool_result_error"
        | "bash_code_execution_tool_result_error" => wrap(format!("Error: {}", code())),
        "web_fetch_result" => wrap(format!("Fetched: {}", s(c, "url").unwrap_or(""))),
        "code_execution_result" | "bash_code_execution_result" => {
            let out = s(c, "stdout")
                .filter(|t| !t.is_empty())
                .or_else(|| s(c, "stderr"))
                .unwrap_or("");
            wrap(format!("Output: {out}"))
        }
        "text_editor_code_execution_view_result" => wrap(s(c, "content").unwrap_or("").to_string()),
        "text_editor_code_execution_create_result" => wrap(
            if c.get("is_file_update")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "File updated"
            } else {
                "File created"
            }
            .into(),
        ),
        "text_editor_code_execution_str_replace_result" => wrap(
            c.get("lines")
                .and_then(Value::as_array)
                .map(|l| {
                    l.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
        ),
        _ => wrap(c.to_string()),
    }
}

/// ACP plan entries from a `TodoWrite` input.
pub fn plan_entries(input: &Value) -> Vec<Value> {
    input
        .get("todos")
        .and_then(Value::as_array)
        .map(|todos| todos.iter().map(|t| json!({ "content": t.get("content"), "status": t.get("status"), "priority": "medium" })).collect())
        .unwrap_or_default()
}

/// Fence `text` with enough backticks that no line inside closes it.
pub fn markdown_escape(text: &str) -> String {
    let mut escape = "```".to_string();
    for line in text.lines() {
        let ticks = line.chars().take_while(|c| *c == '`').count();
        if ticks >= 3 {
            while ticks >= escape.len() {
                escape.push('`');
            }
        }
    }
    let nl = if text.ends_with('\n') { "" } else { "\n" };
    format!("{escape}\n{text}{nl}{escape}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_kinds_match_the_adapter() {
        let t = tool_info(
            "Bash",
            &json!({ "command": "ls `pwd`", "description": "list" }),
        );
        assert_eq!(t["title"], "`ls \\`pwd\\``");
        assert_eq!(t["kind"], "execute");
        assert_eq!(t["content"][0]["content"]["text"], "list");
        assert!(!t.contains_key("locations"));

        let e = tool_info(
            "Edit",
            &json!({ "file_path": "/a.rs", "old_string": "x", "new_string": "y" }),
        );
        assert_eq!(e["title"], "Edit `/a.rs`");
        assert_eq!(
            e["content"][0],
            json!({ "type": "diff", "path": "/a.rs", "oldText": "x", "newText": "y" })
        );
        assert_eq!(e["locations"], json!([{ "path": "/a.rs" }]));

        let r = tool_info("Read", &json!({ "file_path": "/a.rs", "offset": 10 }));
        assert_eq!(
            (r["title"].as_str(), r["locations"][0]["line"].as_u64()),
            (Some("Read /a.rs (from line 11)"), Some(10))
        );
        assert_eq!(
            tool_info(
                "Read",
                &json!({ "file_path": "/a.rs", "offset": 10, "limit": 5 })
            )["title"],
            "Read /a.rs (11 - 15)"
        );
        assert_eq!(
            tool_info("Read", &json!({ "file_path": "/a.rs" }))["title"],
            "Read /a.rs"
        );

        let g = tool_info(
            "Grep",
            &json!({ "pattern": "fn main", "-n": true, "output_mode": "FilesWithMatches", "path": "src" }),
        );
        assert_eq!(g["title"], "grep -n -l \"fn main\" src");

        let w = tool_info(
            "WebSearch",
            &json!({ "query": "rex", "allowed_domains": ["a.com", "b.com"] }),
        );
        assert_eq!(w["title"], "\"rex\" (allowed: a.com, b.com)");

        let todo = tool_info(
            "TodoWrite",
            &json!({ "todos": [{ "content": "a" }, { "content": "b" }] }),
        );
        assert_eq!(todo["title"], "Update TODOs: a, b");

        assert_eq!(
            tool_info("mcp__axel__send", &json!({}))["title"],
            "mcp__axel__send"
        );
    }

    #[test]
    fn results_become_content_like_the_adapter() {
        let read = tool_update(
            &json!({ "content": format!("1\tfn main() {{}}{SYSTEM_REMINDER}") }),
            "Read",
        );
        assert_eq!(
            read["content"][0]["content"]["text"],
            "```\n1\tfn main() {}\n```"
        );

        let err = tool_update(&json!({ "is_error": true, "content": "boom" }), "Bash");
        assert_eq!(err["content"][0]["content"]["text"], "```\nboom\n```");

        assert!(tool_update(&json!({ "content": "ok" }), "Edit").is_empty());
        assert_eq!(
            tool_update(&json!({}), "ExitPlanMode")["title"],
            "Exited Plan Mode"
        );

        let bash = tool_update(
            &json!({ "content": [{ "type": "text", "text": "hi" }] }),
            "Bash",
        );
        assert_eq!(
            bash["content"][0]["content"],
            json!({ "type": "text", "text": "hi" })
        );
    }

    #[test]
    fn fences_outgrow_backticks_inside() {
        assert_eq!(markdown_escape("a"), "```\na\n```");
        assert_eq!(
            markdown_escape("```rust\nx\n```\n"),
            "````\n```rust\nx\n```\n````"
        );
    }
}
