// Ported from axel (https://github.com/scarce/axel), crates/axel-tui/src/tools.rs,
// under the Apache License 2.0: https://www.apache.org/licenses/LICENSE-2.0.
// Kept whole so the two stay easy to diff; mtx uses `describe_line`,
// `describe` and `summarize`.
#![allow(dead_code)]

//! What a tool call does, in words: `sed -n 1,140p src/send.rs` is "Read
//! lines 1–140 of send.rs", `grep -rn 'a\|b' src` is "Search for “a”, “b”
//! in src", Claude's `Read` with an offset is "Read lines 40–90 of lib.rs".
//!
//! ACP normalizes only a tool's `kind` and `locations`; its title and input
//! are each harness's own. So, best first:
//! 1. the harness's own summary: Claude's `Bash` `description`, Codex's
//!    `commandActions` (its parse of the command);
//! 2. structured input: Claude's `Read`/`Edit`/`Grep`/`Glob`/`WebFetch`…,
//!    MCP tool names;
//! 3. the command, parsed: split into its commands (`&&`, `;`, newlines,
//!    pipes, heredocs), wrappers unwrapped (`env`, `sudo`, `timeout`,
//!    `bash -c`), setup skipped (`cd`, `export`), then a verb per program;
//! 4. the title.
//!
//! [`summarize`] turns a run of calls into one line, merging by verb:
//! "Read send.rs, main.rs, search for “max_tx”, build".

use serde_json::Value;

/// What kind of thing a call did: calls with the same verb merge in a
/// run's summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verb {
    Read,
    Write,
    Edit,
    Delete,
    Move,
    List,
    Find,
    Search,
    Fetch,
    WebSearch,
    Build,
    Test,
    Lint,
    Format,
    Git,
    GitHub,
    Install,
    Run,
    Stop,
    Plan,
    Agent,
    Ask,
    Tool,
    /// The harness said what it did, in its own words.
    Described,
    Other,
}

impl Verb {
    /// What a run's summary says before this verb's objects: "read a.rs,
    /// b.rs". In the imperative, like every action's words, so a summary
    /// reads the same running or done.
    fn past(self) -> &'static str {
        match self {
            Verb::Read => "read",
            Verb::Write => "write",
            Verb::Edit => "edit",
            Verb::Delete => "delete",
            Verb::Move => "move",
            Verb::List => "list",
            Verb::Find => "find",
            Verb::Search => "search for",
            Verb::Fetch => "fetch",
            Verb::WebSearch => "search the web for",
            _ => "",
        }
    }

    /// Whether a run lists this verb's objects ("read a.rs, b.rs") rather
    /// than each call's own words.
    fn merges(self) -> bool {
        !self.past().is_empty()
    }
}

/// One call, in words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub verb: Verb,
    /// What it acted on, short: a file name, a quoted pattern, a host.
    pub object: Option<String>,
    /// The whole of it, capitalized: "Read lines 1–140 of send.rs".
    pub text: String,
}

impl Action {
    fn new(verb: Verb, object: impl Into<String>, text: impl Into<String>) -> Action {
        Action {
            verb,
            object: Some(object.into()),
            text: text.into(),
        }
    }

    fn plain(verb: Verb, text: impl Into<String>) -> Action {
        Action {
            verb,
            object: None,
            text: text.into(),
        }
    }
}

/// What one tool call does, from its title, ACP kind and raw input.
pub fn describe(title: &str, kind: Option<&str>, raw: &Value) -> Action {
    describe_all(title, kind, raw)
        .into_iter()
        .next()
        .unwrap_or_else(|| Action::plain(Verb::Other, first_line(title)))
}

/// One line for one call: its actions, the first few, joined.
pub fn describe_line(title: &str, kind: Option<&str>, raw: &Value) -> String {
    let actions = merge_ranges(describe_all(title, kind, raw));
    match actions.len() {
        0 => first_line(title),
        1 => actions[0].text.clone(),
        n => {
            let shown: Vec<String> = actions
                .iter()
                .take(3)
                .enumerate()
                .map(|(i, a)| {
                    if i == 0 {
                        a.text.clone()
                    } else {
                        lower_first(&a.text)
                    }
                })
                .collect();
            let more = if n > 3 {
                format!(" +{}", n - 3)
            } else {
                String::new()
            };
            format!("{}{more}", shown.join(", "))
        }
    }
}

/// Every action in one call: a command can do several things.
pub fn describe_all(title: &str, kind: Option<&str>, raw: &Value) -> Vec<Action> {
    let s = |k: &str| {
        raw.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
    };
    // 1. The harness's own words.
    if let Some(d) = s("description")
        && (s("command").is_some() || kind == Some("execute") || kind == Some("think"))
    {
        return vec![Action::plain(
            Verb::Described,
            capitalize(first_line(d).trim_end_matches('.')),
        )];
    }
    if let Some(acts) = raw.get("commandActions").and_then(Value::as_array) {
        let parsed: Vec<Action> = acts.iter().filter_map(codex_action).collect();
        if !parsed.is_empty() {
            return dedupe(parsed);
        }
    }
    // 2. Structured input.
    let tool = title.split_whitespace().next().unwrap_or("");
    if let Some(a) = structured(tool, title, kind, raw) {
        return vec![a];
    }
    // 3. The command.
    let command = s("command")
        .or_else(|| s("cmd"))
        .map(String::from)
        .or_else(|| {
            raw.get("command").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        });
    let command = command
        .or_else(|| (kind == Some("execute")).then(|| title.trim().trim_matches('`').to_string()));
    if let Some(cmd) = command.filter(|c| !c.trim().is_empty()) {
        let acts = describe_command(&cmd);
        if !acts.is_empty() {
            return acts;
        }
    }
    // 4. The title: Claude's say what they touch even when the input is
    // cut off ("Write /w/a.rs", "Edit `/w/a.rs`").
    let path = title
        .split_once(' ')
        .map(|(_, p)| p.trim().trim_matches('`'))
        .filter(|p| p.contains('/'));
    match (tool, path) {
        ("Read", Some(p)) => {
            return vec![Action::new(
                Verb::Read,
                short(p),
                format!("Read {}", short(p)),
            )];
        }
        ("Write", Some(p)) => {
            return vec![Action::new(
                Verb::Write,
                short(p),
                format!("Write {}", short(p)),
            )];
        }
        ("Edit", Some(p)) => {
            return vec![Action::new(
                Verb::Edit,
                short(p),
                format!("Edit {}", short(p)),
            )];
        }
        _ => {}
    }
    if matches!(tool, "EnterPlanMode" | "Exited") || kind == Some("switch_mode") {
        return vec![Action::plain(
            Verb::Plan,
            if tool == "EnterPlanMode" {
                "Plan first"
            } else {
                "Leave plan mode"
            },
        )];
    }
    vec![Action::plain(Verb::Other, first_line(title))]
}

/// Codex's own parse of a command.
fn codex_action(a: &Value) -> Option<Action> {
    let s = |k: &str| a.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    Some(match s("type")? {
        "read" => {
            let p = short(s("path").or(s("name"))?);
            Action::new(Verb::Read, p.clone(), format!("Read {p}"))
        }
        "listFiles" => {
            let p = s("path").map(short_dir).unwrap_or_else(|| ".".into());
            Action::new(Verb::List, p.clone(), format!("List {p}"))
        }
        "search" => {
            let q = quote(s("query").unwrap_or(""));
            let place = s("path")
                .map(|p| format!(" in {}", short_dir(p)))
                .unwrap_or_default();
            Action::new(Verb::Search, q.clone(), format!("Search for {q}{place}"))
        }
        _ => {
            return s("command")
                .map(describe_command)
                .and_then(|v| v.into_iter().next());
        }
    })
}

/// Claude Code's tools (and MCP tools), from their input.
fn structured(tool: &str, title: &str, kind: Option<&str>, raw: &Value) -> Option<Action> {
    let s = |k: &str| {
        raw.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
    };
    let n = |k: &str| raw.get(k).and_then(Value::as_u64);
    let path = s("file_path").or_else(|| s("notebook_path")).or_else(|| {
        matches!(kind, Some("read" | "edit"))
            .then(|| s("path"))
            .flatten()
    });
    if let Some(path) = path {
        let f = short(path);
        return Some(match (kind, tool) {
            (_, "Write") => Action::new(Verb::Write, f.clone(), format!("Write {f}")),
            (Some("edit"), _) | (_, "Edit" | "MultiEdit" | "NotebookEdit") => {
                Action::new(Verb::Edit, f.clone(), format!("Edit {f}"))
            }
            _ if raw.get("content").is_some() => {
                Action::new(Verb::Write, f.clone(), format!("Write {f}"))
            }
            _ => match (n("offset"), n("limit")) {
                (Some(o), Some(l)) => Action::new(
                    Verb::Read,
                    f.clone(),
                    format!("Read lines {}–{} of {f}", o.max(1), o.max(1) + l - 1),
                ),
                (Some(o), None) => {
                    Action::new(Verb::Read, f.clone(), format!("Read {f} from line {o}"))
                }
                (None, Some(l)) => Action::new(
                    Verb::Read,
                    f.clone(),
                    format!("Read the first {l} lines of {f}"),
                ),
                _ => Action::new(Verb::Read, f.clone(), format!("Read {f}")),
            },
        });
    }
    if let Some(p) = s("pattern") {
        let place = s("path")
            .map(|p| format!(" in {}", short_dir(p)))
            .unwrap_or_default();
        let only = s("glob")
            .or_else(|| s("type"))
            .map(|g| format!(" ({g})"))
            .unwrap_or_default();
        // Claude titles a Glob "Find …"; a Grep says what it outputs.
        if matches!(tool, "Glob" | "Find") || (raw.get("output_mode").is_none() && p.contains("**"))
        {
            return Some(Action::new(
                Verb::Find,
                p.to_string(),
                format!("Find {p}{place}"),
            ));
        }
        let q = patterns(p, true);
        return Some(Action::new(
            Verb::Search,
            q.clone(),
            format!("Search for {q}{place}{only}"),
        ));
    }
    if let Some(url) = s("url") {
        let h = host(url);
        return Some(Action::new(
            Verb::Fetch,
            h.clone(),
            format!("Fetch {}", trim_url(url)),
        ));
    }
    if let Some(q) = s("query")
        && (tool == "WebSearch" || kind == Some("fetch"))
    {
        let q = quote(q);
        return Some(Action::new(
            Verb::WebSearch,
            q.clone(),
            format!("Search the web for {q}"),
        ));
    }
    // mcp__server__tool
    if let Some(rest) = title.strip_prefix("mcp__") {
        let (server, name) = rest.split_once("__").unwrap_or(("", rest));
        let words = name.replace('_', " ");
        let on = if server.is_empty() {
            String::new()
        } else {
            format!(" ({server})")
        };
        return Some(Action::plain(
            Verb::Tool,
            format!("{}{on}", capitalize(&words)),
        ));
    }
    let desc = s("description").map(first_line);
    Some(match tool {
        "Task" | "Agent" => Action::plain(
            Verb::Agent,
            format!(
                "Agent: {}",
                desc.or_else(|| s("prompt").map(first_line))
                    .unwrap_or_else(|| "a task".into())
            ),
        ),
        "TodoWrite" => Action::plain(Verb::Plan, "Update the plan"),
        "ExitPlanMode" => Action::plain(Verb::Plan, "Propose a plan"),
        "AskUserQuestion" => Action::plain(Verb::Ask, "Ask you"),
        "ToolSearch" => {
            let q = s("query").unwrap_or("");
            let names = q
                .strip_prefix("select:")
                .unwrap_or(q)
                .split(',')
                .map(|n| {
                    n.trim()
                        .strip_prefix("mcp__")
                        .and_then(|m| m.split_once("__"))
                        .map(|(_, t)| t.to_string())
                        .unwrap_or_else(|| n.trim().to_string())
                })
                .collect::<Vec<_>>()
                .join(", ");
            Action::plain(Verb::Tool, format!("Load {names}"))
        }
        "Skill" => Action::plain(
            Verb::Tool,
            format!(
                "Use the {} skill",
                s("skill").or_else(|| s("name")).unwrap_or("a")
            ),
        ),
        "TaskOutput" | "BashOutput" => Action::plain(Verb::Run, "Check on a background task"),
        "TaskStop" | "KillShell" | "KillBash" => {
            Action::plain(Verb::Stop, "Stop a background task")
        }
        _ => return None,
    })
}

// ------------------------------------------------------------ commands

/// One command of a command line: its words, where its output goes, the
/// heredoc it reads, and whether a pipe feeds it.
#[derive(Debug, Default)]
struct Cmd {
    words: Vec<String>,
    out: Option<String>,
    heredoc: Option<String>,
    piped: bool,
}

/// Split a command line into commands. Quotes and escapes keep their words
/// whole; `$(…)` and backticks stay inside theirs; a heredoc's body goes to
/// the command that reads it; `#` starts a comment.
fn split(cmd: &str) -> Vec<Cmd> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut out: Vec<Cmd> = Vec::new();
    let mut cur = Cmd::default();
    let mut word = String::new();
    let mut in_word = false;
    let mut redirect = false;
    let mut pending_heredoc: Vec<String> = Vec::new();
    let mut heredoc_for: Vec<usize> = Vec::new();
    let mut i = 0;
    let push_word = |cur: &mut Cmd, word: &mut String, in_word: &mut bool, redirect: &mut bool| {
        if *in_word {
            if *redirect {
                cur.out = Some(std::mem::take(word));
                *redirect = false;
            } else {
                cur.words.push(std::mem::take(word));
            }
            *in_word = false;
        }
    };
    let end_cmd = |out: &mut Vec<Cmd>, cur: &mut Cmd, piped_next: bool| {
        let done = std::mem::take(cur);
        if !done.words.is_empty() || done.out.is_some() {
            out.push(done);
        }
        cur.piped = piped_next;
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\'
                        && i + 1 < chars.len()
                        && matches!(chars[i + 1], '"' | '\\' | '$' | '`')
                    {
                        i += 1;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '\\' if i + 1 < chars.len() => {
                in_word = true;
                if chars[i + 1] != '\n' {
                    word.push(chars[i + 1]);
                }
                i += 1;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                // $(…): kept whole inside the word.
                in_word = true;
                let mut depth = 0;
                while i < chars.len() {
                    word.push(chars[i]);
                    if chars[i] == '(' {
                        depth += 1;
                    } else if chars[i] == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    i += 1;
                }
            }
            '`' => {
                in_word = true;
                word.push(c);
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    word.push(chars[i]);
                    i += 1;
                }
                word.push('`');
            }
            '#' if !in_word => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            ' ' | '\t' => push_word(&mut cur, &mut word, &mut in_word, &mut redirect),
            '\n' => {
                push_word(&mut cur, &mut word, &mut in_word, &mut redirect);
                end_cmd(&mut out, &mut cur, false);
                // Heredoc bodies follow the line that opened them.
                for (delim, at) in pending_heredoc.drain(..).zip(heredoc_for.drain(..)) {
                    let mut body = String::new();
                    i += 1;
                    loop {
                        let start = i;
                        while i < chars.len() && chars[i] != '\n' {
                            i += 1;
                        }
                        let line: String = chars[start..i].iter().collect();
                        if line.trim() == delim || i >= chars.len() {
                            if line.trim() != delim {
                                body.push_str(&line);
                            }
                            break;
                        }
                        body.push_str(&line);
                        body.push('\n');
                        i += 1;
                    }
                    if let Some(cmd) = out.get_mut(at) {
                        cmd.heredoc = Some(body);
                    }
                }
            }
            ';' | '&' | '|' => {
                push_word(&mut cur, &mut word, &mut in_word, &mut redirect);
                let pipe = c == '|' && chars.get(i + 1) != Some(&'|');
                if c == '&' && chars.get(i + 1) == Some(&'>') {
                    // &> file
                    redirect = true;
                    i += 2;
                    continue;
                }
                end_cmd(&mut out, &mut cur, pipe);
                if matches!(chars.get(i + 1), Some('&' | '|')) {
                    i += 1;
                }
            }
            '>' => {
                push_word(&mut cur, &mut word, &mut in_word, &mut redirect);
                if word_is_fd(&cur) {
                    cur.words.pop();
                }
                // 2>&1, >&2: not a file.
                if chars.get(i + 1) == Some(&'&') {
                    i += 2;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                    continue;
                }
                if chars.get(i + 1) == Some(&'>') {
                    i += 1;
                }
                redirect = true;
            }
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                push_word(&mut cur, &mut word, &mut in_word, &mut redirect);
                i += 2;
                if chars.get(i) == Some(&'-') {
                    i += 1;
                }
                while chars.get(i) == Some(&' ') {
                    i += 1;
                }
                let mut delim = String::new();
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], ';' | '|' | '&' | ')')
                {
                    if !matches!(chars[i], '\'' | '"') {
                        delim.push(chars[i]);
                    }
                    i += 1;
                }
                pending_heredoc.push(delim);
                heredoc_for.push(out.len());
                continue;
            }
            '(' | ')' | '{' | '}' if !in_word => {}
            _ => {
                in_word = true;
                word.push(c);
            }
        }
        i += 1;
    }
    push_word(&mut cur, &mut word, &mut in_word, &mut redirect);
    end_cmd(&mut out, &mut cur, false);
    out
}

/// `2>` and the like: the digit before `>` is a file descriptor, not a word.
fn word_is_fd(c: &Cmd) -> bool {
    c.words
        .last()
        .is_some_and(|w| w.len() == 1 && w.chars().all(|c| c.is_ascii_digit()))
}

/// Commands that set things up rather than do the work.
const SETUP: &[&str] = &[
    "cd", "pushd", "popd", "export", "set", "unset", "source", ".", "sleep", "true", "false", ":",
    "local", "alias", "trap", "wait", "shopt", "ulimit", "umask", "echo", "printf", "fi", "done",
    "esac", "then", "else", "do", "for", "while", "until", "case", "read", "break", "continue",
    "exit", "return", "clear", "date",
];
/// Words in front of a command that run it some way.
const WRAPPERS: &[&str] = &[
    "sudo",
    "nohup",
    "time",
    "command",
    "exec",
    "builtin",
    "nice",
    "caffeinate",
    "noglob",
    "!",
    "if",
    "elif",
    "then",
    "do",
    "else",
    "while",
    "until",
];
/// Programs that, behind a pipe, only filter what came before.
const FILTERS: &[&str] = &[
    "head", "tail", "grep", "rg", "sort", "uniq", "wc", "cut", "tr", "awk", "sed", "jq", "less",
    "more", "cat", "tee", "column", "xargs", "fold", "nl", "fmt", "paste", "python3", "python",
];

/// What a command line does: an action per command that does something.
pub fn describe_command(cmd: &str) -> Vec<Action> {
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    for c in split(cmd) {
        let mut words: Vec<String> = c.words.clone();
        // VAR=value: remembered, so `curl $R` can say where it goes.
        while let Some(w) = words.first() {
            match w.split_once('=') {
                Some((k, v))
                    if !k.is_empty()
                        && k.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') =>
                {
                    vars.push((k.to_string(), v.to_string()));
                    words.remove(0);
                }
                _ => break,
            }
        }
        let words = unwrap(words);
        let Some(program) = words
            .first()
            .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
        else {
            if let Some(f) = &c.out {
                out.push(Action::new(
                    Verb::Write,
                    short(f),
                    format!("Write {}", short(f)),
                ));
            }
            continue;
        };
        // `bash -c '…'`: what's inside.
        if matches!(program.as_str(), "bash" | "sh" | "zsh")
            && let Some(at) = words.iter().position(|w| w == "-c" || w == "-lc")
            && let Some(inner) = words.get(at + 1)
        {
            out.extend(describe_command(inner));
            continue;
        }
        if SETUP.contains(&program.as_str()) {
            // `echo … > f` writes f.
            if matches!(program.as_str(), "echo" | "printf")
                && let Some(f) = &c.out
            {
                out.push(Action::new(
                    Verb::Write,
                    short(f),
                    format!("Write {}", short(f)),
                ));
            }
            continue;
        }
        if c.piped && FILTERS.contains(&program.as_str()) && !(program == "tee" && words.len() > 1)
        {
            continue;
        }
        if let Some(a) = program_action(&program, &words[1..], &c, &vars) {
            out.push(a);
        }
    }
    dedupe(out)
}

/// Drop what only runs the command some way: `sudo`, `timeout 60`, `env A=b`.
fn unwrap(mut words: Vec<String>) -> Vec<String> {
    loop {
        let Some(first) = words.first().cloned() else {
            return words;
        };
        let p = first.rsplit('/').next().unwrap_or(&first).to_string();
        if WRAPPERS.contains(&p.as_str()) {
            words.remove(0);
            // sudo -E, nice -n 10: the wrapper's own flags.
            while words.first().is_some_and(|w| w.starts_with('-')) {
                let w = words.remove(0);
                if matches!(w.as_str(), "-n" | "-u" | "-g") && !words.is_empty() {
                    words.remove(0);
                }
            }
        } else if p == "timeout" || p == "gtimeout" {
            words.remove(0);
            while words.first().is_some_and(|w| w.starts_with('-')) {
                words.remove(0);
            }
            if !words.is_empty() {
                words.remove(0); // the duration
            }
        } else if p == "env" {
            words.remove(0);
            while words
                .first()
                .is_some_and(|w| w.contains('=') || w.starts_with('-'))
            {
                words.remove(0);
            }
        } else if p == "xargs" && words.len() > 1 {
            words.remove(0);
            while words.first().is_some_and(|w| w.starts_with('-')) {
                let w = words.remove(0);
                if matches!(w.as_str(), "-n" | "-I" | "-P" | "-L") && !words.is_empty() {
                    words.remove(0);
                }
            }
        } else {
            return words;
        }
    }
}

/// The words after a program that aren't flags, given the flags that take
/// a value (and so swallow the next word).
fn operands<'a>(args: &'a [String], with_value: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut skip = false;
    let mut ended = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if ended || !a.starts_with('-') || a == "-" {
            out.push(a.as_str());
            continue;
        }
        if a == "--" {
            ended = true;
            continue;
        }
        if with_value.contains(&a.as_str()) {
            skip = true;
        }
    }
    out
}

/// The value of a flag (`-n 20`, `-n20`, `--lines=20`).
fn flag<'a>(args: &'a [String], names: &[&str]) -> Option<&'a str> {
    for (i, a) in args.iter().enumerate() {
        for n in names {
            if a == n {
                return args.get(i + 1).map(String::as_str);
            }
            if let Some(v) = a.strip_prefix(&format!("{n}=")) {
                return Some(v);
            }
            if n.len() == 2
                && !n.starts_with("--")
                && a.starts_with(n)
                && a.len() > 2
                && !a.starts_with("--")
            {
                return Some(&a[2..]);
            }
        }
    }
    None
}

fn has(args: &[String], names: &[&str]) -> bool {
    args.iter().any(|a| {
        names.contains(&a.as_str())
            || (a.starts_with('-')
                && !a.starts_with("--")
                && names.iter().any(|n| {
                    n.len() == 2
                        && n.starts_with('-')
                        && !n.starts_with("--")
                        && a[1..].contains(&n[1..])
                }))
    })
}

fn files(list: &[&str]) -> (String, String) {
    let shown: Vec<String> = list.iter().take(2).map(|f| short(f)).collect();
    let more = if list.len() > 2 {
        format!(" +{}", list.len() - 2)
    } else {
        String::new()
    };
    let joined = format!("{}{more}", shown.join(", "));
    (shown.first().cloned().unwrap_or_default(), joined)
}

fn program_action(p: &str, args: &[String], c: &Cmd, vars: &[(String, String)]) -> Option<Action> {
    let ops = |with: &[&str]| operands(args, with);
    let sub = ops(&[]).first().copied().unwrap_or("").to_string();
    Some(match p {
        "cat" | "bat" | "less" | "more" => {
            let fs = ops(&[]);
            if let Some(f) = &c.out {
                Action::new(Verb::Write, short(f), format!("Write {}", short(f)))
            } else if fs.is_empty() {
                return None;
            } else {
                let (first, all) = files(&fs);
                Action::new(Verb::Read, first, format!("Read {all}"))
            }
        }
        "head" | "tail" => {
            let fs = ops(&["-n", "-c"]);
            let (first, all) = files(&fs);
            if fs.is_empty() {
                return None;
            }
            if p == "tail" && has(args, &["-f", "-F"]) {
                return Some(Action::new(Verb::Read, first, format!("Follow {all}")));
            }
            let n = flag(args, &["-n", "--lines"]).or_else(|| {
                args.iter()
                    .find(|a| {
                        a.len() > 1
                            && a.starts_with('-')
                            && a[1..].chars().all(|c| c.is_ascii_digit())
                    })
                    .map(|a| &a[1..])
            });
            let which = if p == "head" { "first" } else { "last" };
            match n {
                Some(n) => Action::new(
                    Verb::Read,
                    first,
                    format!(
                        "Read the {which} {} lines of {all}",
                        n.trim_start_matches('+')
                    ),
                ),
                None => Action::new(
                    Verb::Read,
                    first,
                    format!("Read the {which} lines of {all}"),
                ),
            }
        }
        "sed" | "gsed" => {
            // BSD `sed -i ''`: the empty suffix isn't a word.
            let fs: Vec<&str> = ops(&["-e", "-f"])
                .into_iter()
                .filter(|f| !f.is_empty())
                .collect();
            let script_given = flag(args, &["-e"]).is_some();
            let (script, targets): (&str, &[&str]) = if script_given {
                (flag(args, &["-e"]).unwrap_or(""), &fs[..])
            } else {
                (
                    fs.first().copied().unwrap_or(""),
                    fs.get(1..).unwrap_or(&[]),
                )
            };
            let (first, all) = files(targets);
            if has(args, &["-i"]) || args.iter().any(|a| a.starts_with("-i")) {
                return Some(Action::new(
                    Verb::Edit,
                    first,
                    format!(
                        "Edit {}",
                        if all.is_empty() { "a file".into() } else { all }
                    ),
                ));
            }
            if targets.is_empty() {
                return None;
            }
            let ranges: Vec<String> = script
                .split(';')
                .filter_map(|part| {
                    let part = part.trim().strip_suffix('p')?;
                    let (a, b) = part.split_once(',').unwrap_or((part, part));
                    let b = if b == "$" {
                        "end".to_string()
                    } else {
                        b.to_string()
                    };
                    (a.chars().all(|c| c.is_ascii_digit()) && !a.is_empty()).then(|| {
                        if a == b {
                            format!("line {a}")
                        } else {
                            format!("{a}–{b}")
                        }
                    })
                })
                .collect();
            match ranges.len() {
                0 => Action::new(Verb::Read, first, format!("Read {all}")),
                1 if ranges[0].starts_with("line") => {
                    Action::new(Verb::Read, first, format!("Read {} of {all}", ranges[0]))
                }
                _ => Action::new(
                    Verb::Read,
                    first,
                    format!("Read lines {} of {all}", ranges.join(", ")),
                ),
            }
        }
        "awk" | "gawk" => {
            let fs = ops(&["-F", "-v", "-f"]);
            let targets = fs.get(1..).unwrap_or(&[]);
            if targets.is_empty() {
                return None;
            }
            let (first, all) = files(targets);
            Action::new(Verb::Read, first, format!("Read {all}"))
        }
        "grep" | "egrep" | "rg" | "ag" | "ack" | "ggrep" => {
            let with = [
                "-e",
                "-f",
                "-A",
                "-B",
                "-C",
                "-m",
                "-g",
                "-t",
                "-T",
                "--type",
                "--glob",
                "--max-count",
                "--context",
                "-d",
                "-D",
                "--exclude",
                "--include",
                "--max-depth",
            ];
            let fs = ops(&with);
            let explicit = flag(args, &["-e", "--regexp"]);
            let (pat, places): (&str, &[&str]) = match explicit {
                Some(e) => (e, &fs[..]),
                None => (
                    fs.first().copied().unwrap_or(""),
                    fs.get(1..).unwrap_or(&[]),
                ),
            };
            let extended = p != "grep" || has(args, &["-E", "-P"]) || p == "egrep";
            let q = patterns(pat, extended);
            // `$f` from a loop says nothing about where.
            let places: Vec<&str> = places
                .iter()
                .copied()
                .filter(|f| !f.starts_with('$'))
                .collect();
            let place = if places.is_empty() {
                if c.piped || p == "grep" && !has(args, &["-r", "-R"]) {
                    String::new()
                } else {
                    " in .".to_string()
                }
            } else {
                let mut names: Vec<String> = Vec::new();
                for f in &places {
                    let n = short_dir(f);
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
                let more = if names.len() > 2 {
                    format!(" +{}", names.len() - 2)
                } else {
                    String::new()
                };
                format!(" in {}{more}", names[..names.len().min(2)].join(", "))
            };
            Action::new(Verb::Search, q.clone(), format!("Search for {q}{place}"))
        }
        "ls" | "eza" | "exa" | "tree" | "lsd" => {
            let fs = ops(&["-L", "-I"]);
            let d = fs
                .first()
                .map(|f| short_dir(f))
                .unwrap_or_else(|| ".".into());
            Action::new(Verb::List, d.clone(), format!("List {d}"))
        }
        "find" | "fd" | "fdfind" => {
            let fs = ops(&[
                "-name",
                "-iname",
                "-path",
                "-type",
                "-maxdepth",
                "-mindepth",
                "-newer",
                "-not",
                "-o",
                "-e",
                "-t",
                "-d",
                "-E",
            ]);
            let name = flag(args, &["-name", "-iname", "-path"])
                .map(String::from)
                .or_else(|| {
                    if p == "find" {
                        None
                    } else {
                        fs.first().map(|s| s.to_string())
                    }
                });
            let dir = if p == "find" { fs.first() } else { fs.get(1) }
                .map(|f| short_dir(f))
                .unwrap_or_else(|| ".".into());
            match name {
                Some(n) => Action::new(Verb::Find, n.clone(), format!("Find {n} in {dir}")),
                None => Action::new(Verb::Find, dir.clone(), format!("Find files in {dir}")),
            }
        }
        "wc" => {
            let fs = ops(&[]);
            if fs.is_empty() {
                return None;
            }
            let (first, all) = files(&fs);
            Action::new(Verb::Read, first, format!("Count lines in {all}"))
        }
        "diff" | "cmp" | "colordiff" | "difft" => {
            let fs = ops(&[]);
            Action::plain(
                Verb::Read,
                format!(
                    "Compare {}",
                    fs.iter()
                        .take(2)
                        .map(|f| short(f))
                        .collect::<Vec<_>>()
                        .join(" and ")
                ),
            )
        }
        "git" => git(args),
        "gh" => {
            let o = ops(&[
                "-R",
                "--repo",
                "-q",
                "--jq",
                "--json",
                "-L",
                "--limit",
                "-X",
                "--method",
                "--input",
                "-f",
                "-F",
                "--field",
                "--raw-field",
                "-H",
                "--header",
                "-t",
                "--template",
                "-b",
                "--body",
                "--title",
                "-w",
            ]);
            let what = o.iter().take(2).copied().collect::<Vec<_>>().join(" ");
            let target = o
                .get(2)
                .filter(|t| t.chars().all(|c| c.is_ascii_digit()))
                .map(|n| format!(" #{n}"))
                .unwrap_or_default();
            if o.first() == Some(&"api") {
                Action::plain(
                    Verb::GitHub,
                    format!("Call the GitHub API: {}", o.get(1).unwrap_or(&"")),
                )
            } else {
                Action::plain(Verb::GitHub, format!("GitHub: {what}{target}"))
            }
        }
        "cargo" => {
            let krate = flag(args, &["-p", "--package"])
                .map(|k| format!(" {k}"))
                .unwrap_or_default();
            match sub.as_str() {
                "build" | "b" => Action::plain(Verb::Build, format!("Build{krate}")),
                "check" | "c" => Action::plain(Verb::Build, format!("Check{krate} compiles")),
                "test" | "t" | "nextest" => Action::plain(Verb::Test, format!("Run{krate} tests")),
                "clippy" => Action::plain(Verb::Lint, format!("Lint{krate}")),
                "fmt" => Action::plain(Verb::Format, "Format the code"),
                "run" | "r" => Action::plain(
                    Verb::Run,
                    format!(
                        "Run{}",
                        if krate.is_empty() {
                            " the program".into()
                        } else {
                            krate
                        }
                    ),
                ),
                "add" | "install" => Action::plain(
                    Verb::Install,
                    format!(
                        "{} {}",
                        if sub == "add" { "Add" } else { "Install" },
                        ops(&[]).get(1).unwrap_or(&"a crate")
                    ),
                ),
                "" => return None,
                other => Action::plain(Verb::Run, format!("cargo {other}")),
            }
        }
        "swift" | "xcodebuild" | "go" | "make" | "just" | "gradle" | "./gradlew" | "mvn"
        | "bazel" | "zig" | "deno" | "dotnet" => {
            let s = sub.as_str();
            match s {
                "build" | "" if p != "go" || s == "build" => Action::plain(
                    Verb::Build,
                    if p == "make" || p == "just" {
                        "Build".to_string()
                    } else {
                        format!("Build with {p}")
                    },
                ),
                "test" => Action::plain(Verb::Test, "Run tests"),
                "run" => Action::plain(
                    Verb::Run,
                    format!(
                        "Run {}",
                        ops(&[])
                            .get(1)
                            .map(|x| short(x))
                            .unwrap_or_else(|| "the program".into())
                    ),
                ),
                "fmt" | "format" => Action::plain(Verb::Format, "Format the code"),
                "lint" | "vet" => Action::plain(Verb::Lint, "Lint"),
                other if p == "make" || p == "just" => {
                    Action::plain(Verb::Build, format!("{p} {other}"))
                }
                other => Action::plain(Verb::Run, format!("{p} {other}")),
            }
        }
        "npm" | "pnpm" | "yarn" | "bun" => {
            let o = ops(&[]);
            match o.first().copied().unwrap_or("") {
                "install" | "i" | "add" | "ci" => Action::plain(
                    Verb::Install,
                    if o.len() > 1 {
                        format!("Install {}", o[1..].join(", "))
                    } else {
                        "Install dependencies".into()
                    },
                ),
                "test" | "t" => Action::plain(Verb::Test, "Run tests"),
                "run" => {
                    let script = o.get(1).copied().unwrap_or("");
                    match script {
                        "build" => Action::plain(Verb::Build, "Build"),
                        "test" => Action::plain(Verb::Test, "Run tests"),
                        "lint" => Action::plain(Verb::Lint, "Lint"),
                        s => Action::plain(Verb::Run, format!("Run {s}")),
                    }
                }
                "build" => Action::plain(Verb::Build, "Build"),
                "" => return None,
                s => Action::plain(Verb::Run, format!("{p} {s}")),
            }
        }
        "npx" | "bunx" | "pnpx" | "uvx" => {
            let o = ops(&[]);
            let tool = o.first().copied().unwrap_or("a tool");
            let rest = o.get(1).map(|s| format!(" {s}")).unwrap_or_default();
            Action::plain(Verb::Run, format!("Run {tool}{rest}"))
        }
        "pip" | "pip3" | "uv" | "brew" | "apt" | "apt-get" | "gem" | "rustup" => {
            let o = ops(&[]);
            match o.first().copied().unwrap_or("") {
                "install" | "add" => Action::plain(
                    Verb::Install,
                    format!(
                        "Install {}",
                        o.get(1..)
                            .map(|r| r.join(", "))
                            .filter(|r| !r.is_empty())
                            .unwrap_or_else(|| "packages".into())
                    ),
                ),
                "target" if o.get(1) == Some(&"add") => Action::plain(
                    Verb::Install,
                    format!("Install the {} target", o.get(2).unwrap_or(&"")),
                ),
                s => Action::plain(Verb::Run, format!("{p} {s}").trim_end().to_string()),
            }
        }
        "python" | "python3" | "node" | "ruby" | "perl" | "bash" | "sh" | "zsh" | "deno-run"
        | "tsx" | "ts-node" => script(p, args, c),
        "pytest" => Action::plain(Verb::Test, "Run tests"),
        "curl" | "wget" | "http" | "xh" => {
            let url = ops(&[
                "-X",
                "-H",
                "-d",
                "--data",
                "--data-raw",
                "-o",
                "-u",
                "-m",
                "--max-time",
                "-A",
                "-e",
                "-w",
                "--connect-timeout",
                "-F",
                "--retry",
            ])
            .into_iter()
            .map(|u| expand(u, vars))
            .find(|u| u.contains("://") || u.contains('.'));
            let host = url.as_deref().map(host).unwrap_or_else(|| "a URL".into());
            // A JSON-RPC call says which method.
            let body = flag(args, &["-d", "--data", "--data-raw"]).unwrap_or("");
            let method = body
                .split("\"method\"")
                .nth(1)
                .and_then(|r| r.split('"').nth(1))
                .filter(|m| !m.is_empty());
            match method {
                Some(m) => Action::new(Verb::Fetch, host.clone(), format!("Call {m} on {host}")),
                None => Action::new(
                    Verb::Fetch,
                    host.clone(),
                    format!("Fetch {}", url.as_deref().map(trim_url).unwrap_or(host)),
                ),
            }
        }
        "rm" | "rmdir" | "trash" => {
            let fs = ops(&[]);
            let (first, all) = files(&fs);
            Action::new(Verb::Delete, first, format!("Delete {all}"))
        }
        "mv" | "cp" | "ln" | "rsync" | "scp" => {
            let fs = ops(&["-t"]);
            let verb = match p {
                "mv" => "Move",
                "ln" => "Link",
                _ => "Copy",
            };
            let (from, to) = (
                fs.first().map(|f| short(f)).unwrap_or_default(),
                fs.last().map(|f| short_dir(f)).unwrap_or_default(),
            );
            Action::new(
                if p == "mv" { Verb::Move } else { Verb::Write },
                from.clone(),
                format!("{verb} {from} to {to}"),
            )
        }
        "mkdir" => {
            let fs = ops(&["-m"]);
            let (first, all) = files(&fs);
            Action::new(Verb::Write, first, format!("Create {all}"))
        }
        "touch" => {
            let (first, all) = files(&ops(&[]));
            Action::new(Verb::Write, first, format!("Create {all}"))
        }
        "chmod" => {
            let o = ops(&[]);
            let (_, all) = files(o.get(1..).unwrap_or(&[]));
            if o.first().is_some_and(|m| m.contains("+x")) {
                Action::plain(Verb::Edit, format!("Make {all} executable"))
            } else {
                Action::plain(Verb::Edit, format!("Change permissions of {all}"))
            }
        }
        "kill" | "pkill" | "killall" => {
            let o = ops(&["-s", "-signal"]);
            let what = o
                .iter()
                .find(|w| !w.starts_with('%') && !w.chars().all(|c| c.is_ascii_digit()))
                .map(|w| short(w));
            Action::plain(
                Verb::Stop,
                format!("Stop {}", what.unwrap_or_else(|| "a process".into())),
            )
        }
        "du" | "df" => Action::plain(
            Verb::Run,
            format!(
                "Check disk usage{}",
                ops(&["-d"])
                    .first()
                    .map(|d| format!(" of {}", short_dir(d)))
                    .unwrap_or_default()
            ),
        ),
        "ps" | "pgrep" | "lsof" => Action::plain(
            Verb::Run,
            format!(
                "Check {}",
                if p == "lsof" {
                    "open files and ports"
                } else {
                    "running processes"
                }
            ),
        ),
        "open" => Action::plain(
            Verb::Run,
            format!(
                "Open {}",
                ops(&["-a"])
                    .first()
                    .map(|f| short(f))
                    .or_else(|| flag(args, &["-a"]).map(String::from))
                    .unwrap_or_default()
            ),
        ),
        "sqlite3" | "psql" | "mysql" => {
            let o = ops(&["-c", "-cmd"]);
            Action::plain(
                Verb::Read,
                format!(
                    "Query {}",
                    o.first()
                        .map(|d| short(d))
                        .unwrap_or_else(|| "the database".into())
                ),
            )
        }
        "jq" | "yq" => {
            let o = ops(&["--arg", "--argjson"]);
            let fs = o.get(1..).unwrap_or(&[]);
            if fs.is_empty() {
                return None;
            }
            let (first, all) = files(fs);
            Action::new(Verb::Read, first, format!("Read {all}"))
        }
        "which" | "type" | "command -v" => {
            Action::plain(Verb::Find, format!("Look for {}", ops(&[]).join(", ")))
        }
        "ssh" => Action::plain(
            Verb::Run,
            format!(
                "Run on {}",
                ops(&["-p", "-i", "-o", "-l", "-J"])
                    .first()
                    .unwrap_or(&"a host")
            ),
        ),
        "docker" | "podman" | "kubectl" | "rex" | "axel" | "tmux" | "systemctl" | "launchctl"
        | "defaults" | "security" | "codesign" | "xcrun" => {
            let o = ops(&["-s", "-b", "-n", "--namespace", "-c", "-w"]);
            Action::plain(
                Verb::Run,
                format!(
                    "{p} {}",
                    o.iter().take(2).copied().collect::<Vec<_>>().join(" ")
                )
                .trim_end()
                .to_string(),
            )
        }
        _ => {
            // A program of the project's (`./target/debug/pay send`,
            // `scripts/bundle.sh`): its name and what it was asked to do.
            let o = ops(&[]);
            let what = o
                .first()
                .filter(|w| {
                    w.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                        && w.len() < 24
                })
                .map(|w| format!(" {w}"))
                .unwrap_or_default();
            Action::plain(Verb::Run, format!("Run {p}{what}"))
        }
    })
}

fn git(args: &[String]) -> Action {
    // `git -C dir …`, `git -c k=v …`
    let mut a: &[String] = args;
    while let Some(first) = a.first() {
        if first == "-C" || first == "-c" {
            a = a.get(2..).unwrap_or(&[]);
        } else if first.starts_with('-') {
            a = &a[1..];
        } else {
            break;
        }
    }
    let sub = a.first().map(String::as_str).unwrap_or("");
    let rest = operands(
        a.get(1..).unwrap_or(&[]),
        &[
            "-m", "-n", "--format", "--pretty", "-U", "--author", "--since",
        ],
    );
    let text = match sub {
        "status" => "Check git status".to_string(),
        "diff" => match rest.first() {
            Some(r) => format!("Diff {}", short(r)),
            None => "Diff the changes".to_string(),
        },
        "log" => "Read git history".to_string(),
        "show" => format!(
            "Show {}",
            rest.first()
                .map(|r| short(r))
                .unwrap_or_else(|| "the last commit".into())
        ),
        "blame" => format!(
            "Blame {}",
            rest.first().map(|r| short(r)).unwrap_or_default()
        ),
        "commit" => "Commit".to_string(),
        "push" => "Push".to_string(),
        "pull" => "Pull".to_string(),
        "fetch" => "Fetch from the remote".to_string(),
        "add" => format!(
            "Stage {}",
            rest.first()
                .map(|r| short(r))
                .unwrap_or_else(|| "changes".into())
        ),
        "checkout" | "switch" => format!(
            "Switch to {}",
            rest.last()
                .map(|r| r.to_string())
                .unwrap_or_else(|| "a branch".into())
        ),
        "branch" => "List branches".to_string(),
        "stash" => "Stash changes".to_string(),
        "rebase" => "Rebase".to_string(),
        "merge" => format!("Merge {}", rest.first().unwrap_or(&"")),
        "reset" => "Reset".to_string(),
        "restore" => format!(
            "Restore {}",
            rest.first().map(|r| short(r)).unwrap_or_default()
        ),
        "rm" => format!(
            "Remove {} from git",
            rest.first().map(|r| short(r)).unwrap_or_default()
        ),
        "mv" => "Move files in git".to_string(),
        "clone" => format!(
            "Clone {}",
            rest.first().map(|r| host_or_name(r)).unwrap_or_default()
        ),
        "worktree" => "Manage worktrees".to_string(),
        "grep" => {
            return Action::new(
                Verb::Search,
                quote(rest.first().unwrap_or(&"")),
                format!("Search the repo for {}", quote(rest.first().unwrap_or(&""))),
            );
        }
        "ls-files" => return Action::new(Verb::List, "the repo", "List the repo's files"),
        "rev-parse" => "Look up a revision".to_string(),
        "tag" => "Tag".to_string(),
        "remote" => "Check remotes".to_string(),
        "init" => "Start a repository".to_string(),
        "" => "git".to_string(),
        other => format!("git {other}"),
    };
    Action::plain(Verb::Git, text)
}

/// A script run inline (`python3 - <<EOF`, `node -e`, `perl -pi -e`) or from a file.
fn script(p: &str, args: &[String], c: &Cmd) -> Action {
    let lang = match p {
        "python" | "python3" => "Python",
        "node" | "tsx" | "ts-node" => "JavaScript",
        "ruby" => "Ruby",
        "perl" => "Perl",
        _ => "shell",
    };
    // perl -pi -e '…' files: an edit; perl -ne '…' file: a read.
    if p == "perl" {
        let fs = operands(args, &["-e", "-E", "-M", "-I"]);
        let (first, all) = files(&fs);
        if args
            .iter()
            .any(|a| a.starts_with("-") && a.contains('i') && !a.starts_with("--"))
            && !fs.is_empty()
        {
            return Action::new(Verb::Edit, first, format!("Edit {all}"));
        }
        if !fs.is_empty() {
            return Action::new(Verb::Read, first, format!("Read {all}"));
        }
    }
    let inline = c
        .heredoc
        .clone()
        .or_else(|| flag(args, &["-c", "-e"]).map(String::from));
    match inline {
        Some(body) => {
            // The files it names, and whether it writes.
            let mut named: Vec<String> = Vec::new();
            for q in body.split(['\'', '"']).skip(1).step_by(2) {
                let looks = q.contains('.')
                    && !q.contains(' ')
                    && q.len() < 120
                    && q.rsplit('.').next().is_some_and(|e| {
                        (1..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric())
                    })
                    && !q.starts_with('.')
                    && !q.contains("://");
                if looks && !named.contains(&short(q)) {
                    named.push(short(q));
                }
            }
            let writes = [
                "write(",
                "write_text",
                ".replace(",
                "'w'",
                "\"w\"",
                "writeFileSync",
                "fs.writeFile",
                "dump(",
            ]
            .iter()
            .any(|w| body.contains(w));
            match (named.is_empty(), writes) {
                (false, true) => {
                    let list: Vec<&str> = named.iter().map(String::as_str).collect();
                    let (first, all) = files(&list);
                    Action::new(Verb::Edit, first, format!("Edit {all}"))
                }
                (false, false) => {
                    let list: Vec<&str> = named.iter().map(String::as_str).collect();
                    let (first, all) = files(&list);
                    Action::new(Verb::Read, first, format!("Read {all}"))
                }
                _ => Action::plain(Verb::Run, format!("Run a {lang} script")),
            }
        }
        None => match operands(args, &["-m", "-W", "-X"]).first() {
            Some(file) if *file != "-" => Action::plain(Verb::Run, format!("Run {}", short(file))),
            _ => match flag(args, &["-m"]) {
                Some("pytest") => Action::plain(Verb::Test, "Run tests"),
                Some(m) => Action::plain(Verb::Run, format!("Run {m}")),
                None => Action::plain(Verb::Run, format!("Run a {lang} script")),
            },
        },
    }
}

// ------------------------------------------------------------ a run

/// A run of calls as one line, at most `room` characters: actions merged by
/// verb in the order they first came ("Read a.rs, b.rs +2, search for “x”,
/// build"), the last ones dropped for "+N more" when it doesn't fit.
pub fn summarize(actions: &[Action], room: usize) -> String {
    let mut parts: Vec<(Verb, Vec<String>)> = Vec::new();
    for a in actions {
        let item = if a.verb.merges() {
            a.object.clone().unwrap_or_else(|| lower_first(&a.text))
        } else {
            lower_first(&a.text)
        };
        let key = if a.verb.merges() { a.verb } else { Verb::Other };
        match parts.iter_mut().find(|(v, items)| {
            *v == key && (key != Verb::Other || items.is_empty() || !items.contains(&item))
        }) {
            Some((_, items)) if key != Verb::Other => {
                if !items.contains(&item) {
                    items.push(item);
                }
            }
            _ => {
                if key == Verb::Other
                    && parts
                        .iter()
                        .any(|(v, items)| *v == Verb::Other && items.contains(&item))
                {
                    continue;
                }
                parts.push((key, vec![item]));
            }
        }
    }
    let render = |(v, items): &(Verb, Vec<String>), objects: usize| -> String {
        if !v.merges() {
            return items[0].clone();
        }
        let shown = items
            .iter()
            .take(objects)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = if items.len() > objects {
            format!(" +{}", items.len() - objects)
        } else {
            String::new()
        };
        format!("{} {shown}{more}", v.past())
    };
    // As many objects per verb as fit, fewer until it does, then fewer parts.
    for objects in [3, 2, 1] {
        let line = parts
            .iter()
            .map(|p| render(p, objects))
            .collect::<Vec<_>>()
            .join(", ");
        if line.chars().count() <= room {
            return capitalize(&line);
        }
    }
    // Still too long: a verb's objects as a count ("edit 3 files"), and the
    // parts that don't fit as "and N more".
    let counted = |(v, items): &(Verb, Vec<String>)| -> String {
        if v.merges() && items.len() > 1 {
            let what = match v {
                Verb::Search | Verb::WebSearch => "patterns",
                Verb::Fetch => "URLs",
                Verb::List | Verb::Find => "places",
                _ => "files",
            };
            format!("{} {} {what}", v.past(), items.len())
        } else {
            render(&(*v, items.clone()), 1)
        }
    };
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        let piece = counted(p);
        let left = parts.len() - i - 1;
        let tail = if left > 0 {
            format!(" and {left} more")
        } else {
            String::new()
        };
        let next = if out.is_empty() {
            piece.clone()
        } else {
            format!("{out}, {piece}")
        };
        if next.chars().count() + tail.chars().count() > room && !out.is_empty() {
            return capitalize(&format!("{out} and {} more", parts.len() - i));
        }
        out = next;
    }
    capitalize(&out)
}

// ------------------------------------------------------------ words

/// A search pattern as people read it: `a\|b` (or `a|b` in an extended
/// pattern) is two alternatives, each quoted; regex escapes dropped.
fn patterns(p: &str, extended: bool) -> String {
    let parts: Vec<String> = if extended {
        p.split('|').map(String::from).collect()
    } else {
        p.split("\\|").map(String::from).collect()
    };
    let clean: Vec<String> = parts
        .iter()
        .map(|s| {
            s.replace("\\b", "")
                .replace("\\(", "(")
                .replace("\\)", ")")
                .replace("\\.", ".")
                .replace("\\s", " ")
                .replace("\\[", "[")
                .replace("\\{", "{")
                .replace("\\", "")
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if clean.is_empty() {
        return quote(p);
    }
    let shown: Vec<String> = clean.iter().take(2).map(|s| quote(s)).collect();
    let more = if clean.len() > 2 {
        format!(" +{}", clean.len() - 2)
    } else {
        String::new()
    };
    format!("{}{more}", shown.join(", "))
}

fn quote(s: &str) -> String {
    let s: String = s.chars().take(32).collect();
    format!("“{s}”")
}

/// A file's name.
fn short(p: &str) -> String {
    let p = p.trim_end_matches('/');
    p.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(p)
        .to_string()
}

/// A directory, as its last part (`src`, `~` for home, `.` for here).
fn short_dir(p: &str) -> String {
    let p = p.trim_end_matches('/');
    if p.is_empty() || p == "." {
        return ".".into();
    }
    if p == "~" || p.ends_with("/~") {
        return "~".into();
    }
    short(p)
}

fn host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split(['/', '?', '#', ':'])
        .next()
        .unwrap_or(rest)
        .to_string()
}

fn host_or_name(url: &str) -> String {
    let t = url.trim_end_matches(".git").trim_end_matches('/');
    t.rsplit(['/', ':'])
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("/")
}

fn trim_url(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let rest = rest
        .split(['?', '#'])
        .next()
        .unwrap_or(rest)
        .trim_end_matches('/');
    if rest.chars().count() > 48 {
        format!("{}…", rest.chars().take(47).collect::<String>())
    } else {
        rest.to_string()
    }
}

/// `$R` or `${R}` from an earlier `R=…` in the same command line.
fn expand(word: &str, vars: &[(String, String)]) -> String {
    let mut out = word.to_string();
    for (k, v) in vars.iter().rev() {
        out = out
            .replace(&format!("${{{k}}}"), v)
            .replace(&format!("${k}"), v);
    }
    out
}

fn first_line(s: &str) -> String {
    let t = s.trim();
    let first = t.lines().next().unwrap_or("").trim();
    if first.chars().count() > 80 {
        format!("{}…", first.chars().take(79).collect::<String>())
    } else {
        first.to_string()
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// "Read x" → "read x", but not "README" or "GitHub": only a first word
/// that's capitalized and nothing more.
fn lower_first(s: &str) -> String {
    let word = s.split_whitespace().next().unwrap_or("");
    let mut w = word.chars();
    let plain = w.next().is_some_and(char::is_uppercase)
        && w.all(|c| !c.is_uppercase())
        && word.chars().count() > 1;
    if !plain {
        return s.to_string();
    }
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_lowercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// Reads of one file in one call, as one: "Read lines 1–80, 100–125 of a.rs".
fn merge_ranges(v: Vec<Action>) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    for a in v {
        let ranges = |t: &str, obj: &str| {
            t.strip_prefix("Read lines ")
                .and_then(|r| r.strip_suffix(&format!(" of {obj}")))
                .map(String::from)
        };
        if a.verb == Verb::Read
            && let Some(obj) = a.object.clone()
            && let Some(prev) = out
                .iter_mut()
                .find(|p| p.verb == Verb::Read && p.object.as_deref() == Some(obj.as_str()))
        {
            if let (Some(r1), Some(r2)) = (ranges(&prev.text, &obj), ranges(&a.text, &obj)) {
                prev.text = format!("Read lines {r1}, {r2} of {obj}");
            }
            continue;
        }
        out.push(a);
    }
    out
}

fn dedupe(v: Vec<Action>) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    for a in v {
        if !out.iter().any(|b| b.text == a.text) {
            out.push(a);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cmd(c: &str) -> String {
        describe_line(&format!("`{c}`"), Some("execute"), &json!({ "command": c }))
    }

    /// Commands from real threads, and what each should say.
    #[test]
    fn real_commands_say_what_they_do() {
        let cases = [
            (
                "sed -n 1,140p crates/cli/src/commands/send.rs",
                "Read lines 1–140 of send.rs",
            ),
            (
                "sed -n '60,130p' crates/cli/src/main.rs; grep -n -i \"ledger\" crates/cli/",
                "Read lines 60–130 of main.rs, search for “ledger” in cli",
            ),
            (
                "grep -rn 'max_tx_version\\|negotiat\\|TxVersion' crates/core/src --include=*.rs | head",
                "Search for “max_tx_version”, “negotiat” +1 in src",
            ),
            (
                "cat /Users/ludo/Coding/axel-next/docs/DESIGN.md",
                "Read DESIGN.md",
            ),
            (
                "cat > /Users/ludo/Coding/axel-next/crates/axel-mcp/src/server.rs << 'EOF'\nfn main() {}\nEOF",
                "Write server.rs",
            ),
            (
                "cd ~/Coding/userscripts && ls -la && git log --oneline | head -20",
                "List ., read git history",
            ),
            (
                "cd ~/Coding/p0 && cargo build --release 2>&1 | grep -E \"^(error|warning)\" -A8 | head -80",
                "Build",
            ),
            (
                "cargo test -p axel-tui 2>&1 | tail -3",
                "Run axel-tui tests",
            ),
            ("git status --short", "Check git status"),
            (
                "git -C ~/Coding/axel diff HEAD~1 -- src/main.rs",
                "Diff HEAD~1",
            ),
            ("gh pr view 42 --json title", "GitHub: pr view #42"),
            ("pkill -f Sonata.app", "Stop Sonata.app"),
            ("rm -rf build/ dist/", "Delete build, dist"),
            ("mkdir -p ~/Coding/p0/{wit,crates}", "Create {wit,crates}"),
            (
                "timeout 120 ./target/debug/pay --no-dna -v send 0.01 CHPEgF7X",
                "Run pay send",
            ),
            (
                "scripts/bundle.sh 2>&1 | grep -E \"error|Built\"",
                "Run bundle.sh",
            ),
            (
                "R=https://api.mainnet-beta.solana.com; curl -s $R -H 'content-type: application/json' -d '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getSlot\"}'",
                "Call getSlot on api.mainnet-beta.solana.com",
            ),
            (
                "cd ~/Coding/p0 && python3 - <<'EOF'\np='crates/p0-core/src/host.rs'\ns=open(p).read()\ns=s.replace('a','b')\nopen(p,'w').write(s)\nEOF",
                "Edit host.rs",
            ),
            ("python3 -c 'import json; print(1)'", "Run a Python script"),
            ("perl -pi -e 's/a/b/' src/a.rs src/b.rs", "Edit a.rs, b.rs"),
            (
                "find . -name '*.rs' -not -path './target/*' | head",
                "Find *.rs in .",
            ),
            (
                "head -n 40 README.md",
                "Read the first 40 lines of README.md",
            ),
            ("tail -f ~/.axel/logs/host.log", "Follow host.log"),
            (
                "wc -l $(find src -name '*.rs')",
                "Count lines in $(find src -name '*.rs')",
            ),
            ("bash -lc 'git diff --stat'", "Diff the changes"),
            ("sudo -E env FOO=1 npm install", "Install dependencies"),
            (
                "for f in a.rs b.rs; do grep -n TODO $f; done",
                "Search for “TODO”",
            ),
            (
                "npx -y agent-browser open https://www.apple.com/shop",
                "Run agent-browser open",
            ),
            ("echo done > /tmp/flag", "Write flag"),
            (
                "rustup target add wasm32-wasip2 2>&1 | tail -1",
                "Install the wasm32-wasip2 target",
            ),
            ("sqlite3 ~/.axel/axel.db \"SELECT 1\"", "Query axel.db"),
            (
                "sed -n 170,300p crates/core/src/signer.rs; sed -n 540,620p crates/core/src/signer.rs",
                "Read lines 170–300, 540–620 of signer.rs",
            ),
            (
                "sed -i '' 's/context: a/context: b/' Sources/Model.swift",
                "Edit Model.swift",
            ),
            (
                "gh pr view 543 --json title && gh pr diff 543",
                "GitHub: pr view #543, GitHub: pr diff #543",
            ),
            (
                "gh api -X PUT repos/lgalabru/sonata/branches/main/protection --input -",
                "Call the GitHub API: repos/lgalabru/sonata/branches/main/protection",
            ),
            (
                "pkill -f Sonata.app/Contents/MacOS/Sonata; sleep 1",
                "Stop Sonata",
            ),
            (
                "grep -n glass Modules/a.swift Modules/b.swift",
                "Search for “glass” in a.swift, b.swift",
            ),
            ("du -sh ~/Coding/ofm/jobs", "Check disk usage of jobs"),
        ];
        let mut wrong = Vec::new();
        for (c, want) in cases {
            let got = cmd(c);
            if got != want {
                wrong.push(format!("{c:?}\n   got  {got:?}\n   want {want:?}"));
            }
        }
        assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
    }

    #[test]
    fn the_harness_words_come_first() {
        let raw = json!({ "command": "ls -la ~/Coding/p0; which cargo", "description": "Check p0 directory and toolchain" });
        assert_eq!(
            describe_line("`ls`", Some("execute"), &raw),
            "Check p0 directory and toolchain"
        );
        // Codex's parse of its command.
        let raw = json!({ "command": "rg -n foo src && sed -n 1,20p a.rs", "commandActions": [
            { "type": "search", "query": "foo", "path": "src", "command": "rg -n foo src" },
            { "type": "read", "path": "/w/a.rs", "name": "a.rs", "command": "sed -n 1,20p a.rs" } ] });
        assert_eq!(
            describe_line("rg", Some("execute"), &raw),
            "Search for “foo” in src, read a.rs"
        );
    }

    #[test]
    fn claude_tools_say_what_they_touch() {
        let d = |title: &str, kind: &str, raw: Value| describe_line(title, Some(kind), &raw);
        assert_eq!(
            d(
                "Read /w/src/lib.rs",
                "read",
                json!({ "file_path": "/w/src/lib.rs", "offset": 40, "limit": 51 })
            ),
            "Read lines 40–90 of lib.rs"
        );
        assert_eq!(
            d("Read /w/a.rs", "read", json!({ "file_path": "/w/a.rs" })),
            "Read a.rs"
        );
        assert_eq!(
            d(
                "Write /w/p0.wit",
                "edit",
                json!({ "file_path": "/w/p0.wit", "content": "x" })
            ),
            "Write p0.wit"
        );
        assert_eq!(
            d(
                "Edit `/w/lib.rs`",
                "edit",
                json!({ "file_path": "/w/lib.rs", "old_string": "a", "new_string": "b" })
            ),
            "Edit lib.rs"
        );
        assert_eq!(
            d(
                "grep",
                "search",
                json!({ "pattern": "fn main|struct App", "path": "/w/crates/axel", "glob": "*.rs" })
            ),
            "Search for “fn main”, “struct App” in axel (*.rs)"
        );
        assert_eq!(
            d(
                "Find `/w` `**/Cargo.toml`",
                "search",
                json!({ "pattern": "**/Cargo.toml", "path": "/w" })
            ),
            "Find **/Cargo.toml in w"
        );
        assert_eq!(
            d(
                "Fetch https://github.com/vercel-labs/agent-browser",
                "fetch",
                json!({ "url": "https://github.com/vercel-labs/agent-browser", "prompt": "x" })
            ),
            "Fetch github.com/vercel-labs/agent-browser"
        );
        assert_eq!(
            d(
                "\"iPhone release\"",
                "fetch",
                json!({ "query": "iPhone release date" })
            ),
            "Search the web for “iPhone release date”"
        );
        assert_eq!(
            d("mcp__axel__send", "other", json!({ "to": ["@opus"] })),
            "Send (axel)"
        );
        assert_eq!(
            d(
                "ToolSearch",
                "other",
                json!({ "query": "select:WebFetch,WebSearch" })
            ),
            "Load WebFetch, WebSearch"
        );
        assert_eq!(
            d(
                "ToolSearch",
                "other",
                json!({ "query": "select:mcp__axel__read_inbox" })
            ),
            "Load read_inbox"
        );
        // Input cut off in the store: the title still names the file.
        assert_eq!(
            describe_line("Write /w/src/publish.rs", Some("edit"), &Value::Null),
            "Write publish.rs"
        );
        assert_eq!(
            d(
                "Explore axel codebase",
                "think",
                json!({ "description": "Explore axel codebase", "prompt": "…" })
            ),
            "Explore axel codebase"
        );
    }

    #[test]
    fn a_run_merges_by_verb() {
        let acts = [
            Action::new(Verb::Read, "send.rs", "Read lines 1–140 of send.rs"),
            Action::new(Verb::Search, "“max_tx”", "Search for “max_tx” in src"),
            Action::new(Verb::Read, "main.rs", "Read main.rs"),
            Action::plain(Verb::Build, "Build"),
            Action::new(Verb::Read, "send.rs", "Read send.rs"),
        ];
        assert_eq!(
            summarize(&acts, 200),
            "Read send.rs, main.rs, search for “max_tx”, build"
        );
        assert_eq!(
            summarize(&acts, 30),
            "Read 2 files and 2 more",
            "counted, then the rest"
        );
        let described = [
            Action::plain(Verb::Described, "Check p0 directory and toolchain"),
            Action::plain(Verb::Described, "Check p0 directory and toolchain"),
        ];
        assert_eq!(
            summarize(&described, 200),
            "Check p0 directory and toolchain",
            "the same words once"
        );
    }
}
