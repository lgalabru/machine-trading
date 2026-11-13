//! A release as tables, for the results dataset's viewer: `trials.jsonl`,
//! a row per trial, and `sessions.jsonl`, a row per hidden session a bot
//! played. And the dataset's card (`README.md`): what the benchmark is,
//! the latest release's leaderboard, every release and how to read one.
//!
//! Every row has every column, in the same order and type in every release,
//! so the Hub reads all versions as one table (`*/trials.jsonl`).

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::review::{self, Root};

/// Where the review page of a release is served.
pub const SITE: &str = "https://lgalabru.github.io/machine-trading/bench/";
const GITHUB: &str = "https://github.com/lgalabru/machine-trading";

/// A task's short name: `flash-crash-spcx`, not `solana-foundation/…`.
fn short(task: &str) -> &str {
    task.rsplit('/').next().unwrap_or(task)
}

fn f64_of(v: &Value) -> Value {
    v.as_f64().map_or(Value::Null, |x| json!(x))
}

fn u64_of(v: &Value) -> Value {
    v.as_u64().map_or(Value::Null, |x| json!(x))
}

/// Seconds between a Harbor `{started_at, finished_at}`'s two times.
fn seconds(span: &Value) -> Value {
    let parse = |v: &Value| v.as_str().and_then(chrono_like);
    match (parse(&span["started_at"]), parse(&span["finished_at"])) {
        (Some(a), Some(b)) if b >= a => json!(((b - a) * 10.0).round() / 10.0),
        _ => Value::Null,
    }
}

/// `2026-11-13T00:32:04.568130` (Harbor writes UTC, without a zone) as
/// seconds since an epoch good enough for differences.
fn chrono_like(s: &str) -> Option<f64> {
    let (date, time) = s.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':');
    let (h, min) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    let sec: f64 = t.next()?.split('+').next()?.parse().ok()?;
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3_600 + min * 60) as f64 + sec)
}

/// The release in `dir` as its two tables, rows in trial order.
pub fn tables(dir: &Path, version: &str) -> (Vec<Value>, Vec<Value>) {
    let root = Root {
        label: version.to_string(),
        path: dir.to_path_buf(),
        published: true,
    };
    let mut trials = Vec::new();
    let mut sessions = Vec::new();
    for (n, trial) in review::discover(dir).iter().enumerate() {
        let s = review::summary_of(&root, n, trial);
        let task = short(s["task"].as_str().unwrap_or_default()).to_string();
        let name = s["trial"].as_str().unwrap_or_default().to_string();
        let model = s["model"].as_str().unwrap_or_default().to_string();
        trials.push(json!({
            "version": version,
            "task": task,
            "model": model,
            "agent": s["agent"].as_str().unwrap_or_default(),
            "agent_version": s["agent_version"].as_str().unwrap_or_default(),
            "trial": name,
            "status": s["status"],
            "graded": s["graded"],
            "skill": f64_of(&s["skill"]),
            "reward": f64_of(&s["reward"]),
            "pnl_usd": f64_of(&s["pnl_usd"]),
            "reference_pnl_usd": f64_of(&s["reference_pnl_usd"]),
            "bound_pnl_usd": f64_of(&s["bound_pnl_usd"]),
            "sessions": u64_of(&s["sessions"]),
            "disqualified": u64_of(&s["disqualified"]),
            "cost_usd": f64_of(&s["cost_usd"]),
            "input_tokens": u64_of(&s["tokens"]["input"]),
            "cache_tokens": u64_of(&s["tokens"]["cache"]),
            "output_tokens": u64_of(&s["tokens"]["output"]),
            "agent_seconds": seconds(&s["agent_execution"]),
            "verifier_seconds": seconds(&s["verifier_execution"]),
            "started_at": s["started_at"].as_str(),
            "exception": s["exception"]["type"].as_str(),
            "task_ref": s["task_ref"].as_str(),
            "path": s["path"],
            "review": format!(
                "{SITE}#/@{version}/task/{task}?run={n}",
                task = s["task"].as_str().unwrap_or_default().replace('/', "%2F")
            ),
        }));
        let report = review::read_json(&trial.join("verifier/report.json"));
        for scenario in report
            .as_ref()
            .and_then(|r| r["scenarios"].as_array())
            .into_iter()
            .flatten()
        {
            sessions.push(json!({
                "version": version,
                "task": task,
                "model": model,
                "trial": name,
                "scenario": scenario["scenario"].as_str().unwrap_or_default(),
                "pnl_usd": f64_of(&scenario["pnl_usd"]),
                "reference_pnl_usd": f64_of(&scenario["reference_pnl_usd"]),
                "bound_pnl_usd": f64_of(&scenario["bound_pnl_usd"]),
                "max_drawdown_usd": f64_of(&scenario["max_drawdown_usd"]),
                "peak_position": f64_of(&scenario["peak_position"]),
                "end_position": f64_of(&scenario["end_position"]),
                "start_mark_usd": f64_of(&scenario["start_mark_usd"]),
                "end_mark_usd": f64_of(&scenario["end_mark_usd"]),
                "liquidatable": scenario["liquidatable"].as_bool(),
                "refused_calls": scenario["refused_calls"].as_array().map_or(0, Vec::len),
                "late_slots": u64_of(&scenario["late_slots"]),
                "disqualified": scenario["disqualified"].as_str().map(str::to_string).or_else(|| {
                    (!scenario["disqualified"].is_null()).then(|| scenario["disqualified"].to_string())
                }),
            }));
        }
    }
    (trials, sessions)
}

/// The trials table's columns, in the order the viewer shows them.
pub const TRIALS: &[&str] = &[
    "version",
    "task",
    "model",
    "skill",
    "reward",
    "pnl_usd",
    "reference_pnl_usd",
    "bound_pnl_usd",
    "status",
    "graded",
    "sessions",
    "disqualified",
    "cost_usd",
    "input_tokens",
    "cache_tokens",
    "output_tokens",
    "agent_seconds",
    "verifier_seconds",
    "agent",
    "agent_version",
    "trial",
    "started_at",
    "exception",
    "task_ref",
    "path",
    "review",
];

/// The sessions table's columns, in order.
pub const SESSIONS: &[&str] = &[
    "version",
    "task",
    "model",
    "trial",
    "scenario",
    "pnl_usd",
    "reference_pnl_usd",
    "bound_pnl_usd",
    "max_drawdown_usd",
    "peak_position",
    "end_position",
    "start_mark_usd",
    "end_mark_usd",
    "liquidatable",
    "refused_calls",
    "late_slots",
    "disqualified",
];

/// Rows as JSON Lines, each with `columns` in that order.
pub fn jsonl(rows: &[Value], columns: &[&str]) -> String {
    let mut out = String::new();
    for row in rows {
        out.push('{');
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&Value::from(*column).to_string());
            out.push(':');
            out.push_str(&row[*column].to_string());
        }
        out.push_str("}\n");
    }
    out
}

/// The dataset's card, from every release's `release.json` in `results`,
/// newest first.
pub fn card(results: &Path) -> Result<String> {
    let mut releases: Vec<(String, Value)> = std::fs::read_dir(results)?
        .flatten()
        .filter_map(|e| {
            let version = e.file_name().to_string_lossy().to_string();
            let release = review::read_json(&e.path().join("release.json"))?;
            (!version.starts_with('.') && !release["store"].is_null()).then_some((version, release))
        })
        .collect();
    releases.sort_by_key(|(v, _)| std::cmp::Reverse(crate::publish::version_key(v)));
    let mut out = String::from(
        "---
license: mit
pretty_name: Machine Trading benchmark results
language: [en]
tags: [agents, benchmark, trading, code-generation, solana, webassembly]
size_categories: [n<1K]
configs:
  - config_name: trials
    data_files: \"*/trials.jsonl\"
    default: true
  - config_name: sessions
    data_files: \"*/sessions.jsonl\"
---

# Machine Trading benchmark results

Every published run of the [Machine Trading benchmark](",
    );
    out.push_str(GITHUB);
    out.push_str(
        "): coding agents write a trading
bot, a WebAssembly component, that is then graded on hidden replays of
Solana markets it never saw. A bot's **skill** is its total PnL over the
hidden sessions divided by a reference bot's on the same sessions: 1.0
matches the reference. Its **reward**, what the agent is scored on, is its
PnL against the most a bot could have made (`bound_pnl_usd`).

Two tables, across every release (the `version` column):

- **`trials`**: one row per agent run. Model, harness, skill, reward, PnL,
  tokens, cost and time, and a link to the run on the review site.
- **`sessions`**: one row per hidden session a bot played. Its PnL next to
  the reference's and the bound, drawdown, positions, and the calls the
  grader refused.

```python
from datasets import load_dataset
trials = load_dataset(\"solana-foundation/machine-trading-results\", \"trials\", split=\"train\")
```

",
    );
    if let Some((version, release)) = releases.first() {
        out.push_str(&format!(
            "## Leaderboard: {version}\n\nMean skill over each model's runs (runs in parentheses).\n\n"
        ));
        out.push_str(&leaderboard(&release["scores"]));
        out.push('\n');
    }
    out.push_str(
        "## Releases\n\n| Version | Ran | Runs | Review | Files |\n| --- | --- | --- | --- | --- |\n",
    );
    for (version, release) in &releases {
        let store = &release["store"];
        out.push_str(&format!(
            "| {version} | {} | {} | [review]({SITE}#/@{version}/) | [`release.tar`](https://huggingface.co/datasets/{}/blob/{}/{}) ({:.0} MB) |\n",
            release["ran_at"].as_str().map_or("", |s| s.get(..10).unwrap_or(s)),
            release["trials"],
            store["repo"].as_str().unwrap_or_default(),
            store["revision"].as_str().unwrap_or_default(),
            store["path"].as_str().unwrap_or_default(),
            store["bytes"].as_f64().unwrap_or(0.0) / 1e6,
        ));
    }
    out.push_str(&format!(
        "
## What a release holds

`<version>/release.tar` is everything the review site shows for that
release: each run's result and verifier report, every session step by step,
the bot's logs and source, and the agent's trajectory, gzipped. The
repository's `bench/results/<version>/release.json` pins it by revision and
sha256, and `mtx bench review <version>` fetches, checks and unpacks it.
`<version>/trials.jsonl` and `<version>/sessions.jsonl` are the tables
above, for that release. Nothing in a release holds the batch's secret or
an API key: `mtx bench publish` checks every file before it uploads.

How a run is set up, graded and published: [docs/development.md]({GITHUB}/blob/main/docs/development.md).
"
    ));
    Ok(out)
}

/// Each task's models by mean skill, as a Markdown table: a row per model,
/// a column per task.
fn leaderboard(scores: &Value) -> String {
    let Some(tasks) = scores["tasks"].as_object() else {
        return String::new();
    };
    let mut names: Vec<&String> = tasks.keys().collect();
    names.sort();
    let mut models: Vec<String> = tasks
        .values()
        .filter_map(|t| t["models"].as_object())
        .flat_map(|m| m.keys().cloned())
        .collect();
    models.sort();
    models.dedup();
    let mut out = String::from("| Model |");
    for name in &names {
        out.push_str(&format!(
            " {} |",
            tasks[*name]["short"].as_str().unwrap_or(name)
        ));
    }
    out.push_str("\n| --- |");
    out.push_str(&" ---: |".repeat(names.len()));
    out.push('\n');
    for model in &models {
        out.push_str(&format!("| {model} |"));
        for name in &names {
            let m = &tasks[*name]["models"][model];
            match m["skill"].as_f64() {
                Some(skill) => out.push_str(&format!(" {skill:.2}× ({}) |", m["trials"])),
                None => out.push_str(" – |"),
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_has_every_column_in_order() {
        let rows = vec![json!({ "task": "a", "version": "0.12.0", "extra": 1 })];
        assert_eq!(
            jsonl(&rows, &["version", "task", "model"]),
            "{\"version\":\"0.12.0\",\"task\":\"a\",\"model\":null}\n"
        );
        for table in [TRIALS, SESSIONS] {
            let mut sorted = table.to_vec();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), table.len());
        }
    }

    #[test]
    fn harbor_times_give_their_difference_in_seconds() {
        let span = json!({
            "started_at": "2026-11-13T23:59:58.5",
            "finished_at": "2026-11-14T00:01:00",
        });
        assert_eq!(seconds(&span), json!(61.5));
        assert_eq!(
            seconds(&json!({ "started_at": "2026-11-13T00:00:00" })),
            Value::Null
        );
    }

    #[test]
    fn the_leaderboard_has_a_row_per_model_and_a_column_per_task() {
        let scores = json!({ "tasks": {
            "flash-crash": { "short": "flash-crash-spcx", "models": {
                "gpt-6-astra": { "skill": 1.234, "trials": 5 },
                "claude-opus-5-5": { "skill": 2.0, "trials": 5 },
            }},
            "weekend-gap": { "short": "weekend-gap-spcx", "models": {
                "gpt-6-astra": { "skill": 0.5, "trials": 4 },
            }},
        }});
        assert_eq!(
            leaderboard(&scores),
            "| Model | flash-crash-spcx | weekend-gap-spcx |\n| --- | ---: | ---: |\n\
             | claude-opus-5-5 | 2.00× (5) | – |\n| gpt-6-astra | 1.23× (5) | 0.50× (4) |\n"
        );
    }
}
