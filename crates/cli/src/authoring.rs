//! Guided task authoring: what `mtx task` and the `mtx mcp` tools share.
//!
//! A new task goes through the same steps every time (see the
//! `trading-task-design` skill): a thesis and a design record, a scenario
//! family, a reference bot, an image anchor, practice scenarios, the agent's
//! instruction, the README, CI and the dataset, lint, then the checks that
//! need the image. This module scaffolds a task from the closest existing one,
//! and reads a task's files to say which steps are done and what to do next.
//! Nothing here keeps state of its own: the files are the state.
//!
//! A new task is scored by v2 from the start (GRADING_DECISIONS D1): its
//! `[metadata.mtx]` says `headline = "v2"`, its verifier grades with
//! `--scoring v2-headline` (Harbor's `reward` is v2's clamped score, with no
//! v1 score beside it), its ladder names the two anchors (the reference, 0.5, and
//! an informed bot, 1, in `checks/bots/informed`) and opts into the probe
//! gate, and its instruction carries the scoring rules in plain words
//! ([`crate::scoring_text`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{scoring_text, tasks};

/// The task an author starts from: the closest existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Base {
    /// Directional: the market moves along a scripted path (crashes,
    /// squeezes, halts…) and the bot trades it. Starts from
    /// flash-crash-spcx.
    Shocks,
    /// Quoting: the grader runs counterparties (takers, competing makers)
    /// that trade with the bot. Starts from market-making-spcx.
    Making,
}

impl Base {
    fn task(self) -> &'static str {
        match self {
            Base::Shocks => "flash-crash-spcx",
            Base::Making => "market-making-spcx",
        }
    }
    /// The scenario generator the base's task draws, which the scaffold
    /// draws until its own exists.
    fn generator(self) -> &'static str {
        match self {
            Base::Shocks => "flash-crash",
            Base::Making => "market-making",
        }
    }
}

/// What the interview collects: enough to scaffold a task and start its
/// design record.
/// Every field may be left out: the interview asks for what's missing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct TaskSpec {
    #[schemars(
        description = "The task's id: lowercase words joined by hyphens: the skill, then the instrument, as in `funding-squeeze-spcx`. It names the directory bench/tasks/<name>, the Harbor task solana-foundation/<name>, and (without the instrument) the scenario family."
    )]
    pub name: String,
    #[schemars(description = "A short human title, as in `Funding squeeze`.")]
    pub title: String,
    #[schemars(
        description = "One sentence for task.toml and the README: what the agent's bot does, on which market. As in `Write a bot that trades Phoenix SPCX (SpaceX) perps through funding spikes against the crowded side, on an offline copy of Solana mainnet.`"
    )]
    pub summary: String,
    #[schemars(
        description = "The closest existing task to start from: `shocks` (the market moves along a scripted path and the bot trades it; from flash-crash-spcx) or `making` (the grader runs counterparties the bot trades with; from market-making-spcx)."
    )]
    pub base: Option<Base>,
    #[schemars(
        description = "The skill thesis, completing `A good bot profits here because it ___`. If it can't be filled in, the task measures noise."
    )]
    pub thesis: String,
    #[schemars(description = "What wins: the expert's method, in a line or two.")]
    pub wins: String,
    #[schemars(
        description = "The habits that lose: what a non-expert does, and in which regime it fails."
    )]
    pub loses: String,
    #[schemars(
        description = "How scenarios vary: the regimes and their odds, as in `half the sessions spike, a quarter decay, a quarter stay calm`."
    )]
    pub regimes: String,
    #[schemars(
        description = "Position cap, in units of the market's base asset (SPCX). About 1x collateral (10,000 USDC) is 70 SPCX at $143."
    )]
    pub max_position: Option<f64>,
    #[schemars(
        description = "Loss limit: the most a session may draw down from its peak, in USD. About 10% of collateral is $1,000."
    )]
    pub max_drawdown_usd: Option<f64>,
    #[schemars(
        description = "Hidden sessions per grade. 72 is what both tasks use: fewer couldn't separate the references from simple bots."
    )]
    pub hidden_sessions: Option<u32>,
}

/// Fields the interview asks, with the question it puts to the author.
pub const QUESTIONS: &[(&str, &str)] = &[
    ("name", "The task's id, as in `funding-squeeze-spcx`"),
    ("title", "A short title, as in `Funding squeeze`"),
    (
        "summary",
        "One sentence: what the agent's bot does, on which market",
    ),
    (
        "base",
        "Start from `shocks` (scripted market moves) or `making` (counterparties the bot trades with)",
    ),
    ("thesis", "Complete: a good bot profits here because it ___"),
    ("wins", "What wins: the expert's method"),
    ("loses", "The habits that lose, and where they fail"),
    ("regimes", "How scenarios vary: regimes and their odds"),
    (
        "max_position",
        "Position cap in SPCX (70 is about 1x collateral)",
    ),
    (
        "max_drawdown_usd",
        "Loss limit per session in USD ($1,000 is 10% of collateral)",
    ),
];

impl TaskSpec {
    /// The fields still empty, in interview order.
    pub fn missing(&self) -> Vec<&'static str> {
        let text = |s: &str| s.trim().is_empty();
        QUESTIONS
            .iter()
            .filter(|(field, _)| match *field {
                "name" => text(&self.name),
                "title" => text(&self.title),
                "summary" => text(&self.summary),
                "base" => self.base.is_none(),
                "thesis" => text(&self.thesis),
                "wins" => text(&self.wins),
                "loses" => text(&self.loses),
                "regimes" => text(&self.regimes),
                "max_position" => self.max_position.is_none(),
                "max_drawdown_usd" => self.max_drawdown_usd.is_none(),
                _ => false,
            })
            .map(|(field, _)| *field)
            .collect()
    }

    /// Every problem at once, so an author (or an agent) fixes them in one go.
    pub fn problems(&self, root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for field in self.missing() {
            let question = QUESTIONS
                .iter()
                .find(|(f, _)| *f == field)
                .map(|(_, q)| *q)
                .unwrap_or("");
            out.push(format!("`{field}` is missing: {question}."));
        }
        let name = self.name.trim();
        if !name.is_empty() {
            let ok = name.split('-').all(|w| {
                !w.is_empty()
                    && w.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            });
            if !ok || !name.contains('-') {
                out.push(format!(
                    "`name` is `{name}`: it must be lowercase words joined by hyphens, the skill then the instrument, as in `funding-squeeze-spcx`."
                ));
            } else if root.join("bench/tasks").join(name).exists() {
                out.push(format!("`name` is `{name}`, but bench/tasks/{name} already exists: pick another, or run task_status on it."));
            } else if tasks::load(root).is_ok_and(|ts| {
                ts.iter()
                    .any(|t| t.reference.package == format!("mtx-bot-{}", family_of(name)))
            }) {
                out.push(format!(
                    "`name` is `{name}`, whose reference package mtx-bot-{} a task already has: pick another name.",
                    family_of(name)
                ));
            }
        }
        if let Some(cap) = self.max_position.filter(|c| !c.is_finite() || *c <= 0.0) {
            out.push(format!(
                "`max_position` is {cap}: it must be positive (70 SPCX is about 1x collateral)."
            ));
        }
        if let Some(limit) = self
            .max_drawdown_usd
            .filter(|l| !l.is_finite() || *l <= 0.0)
        {
            out.push(format!(
                "`max_drawdown_usd` is {limit}: it must be positive ($1,000 is 10% of collateral)."
            ));
        }
        if !self.thesis.trim().is_empty() && self.thesis.trim().len() < 20 {
            out.push(format!(
                "`thesis` is `{}`: say what the bot does that earns, as in `fades shocks that have stopped, sized to survive the ones that keep going`.",
                self.thesis.trim()
            ));
        }
        out
    }
}

/// A task's scenario family: its name without the instrument it trades
/// (`flash-crash-spcx` draws `flash-crash`). Tasks are named for the skill
/// and the instrument; the venue is a means to an end.
pub fn family_of(task: &str) -> String {
    task.rsplit_once('-')
        .map_or(task, |(skill, _)| skill)
        .to_string()
}

/// The repository: found from the working directory up, else from where this
/// binary was built.
pub fn repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        if d.join("bench/tasks").is_dir() && d.join("Justfile").is_file() {
            return d;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

const TODO: &str = "TODO";

/// Create bench/tasks/<name>, with its reference crate, from the spec's base,
/// and write the lists that name the task. Returns what it wrote.
pub fn scaffold(root: &Path, spec: &TaskSpec) -> Result<Value> {
    let problems = spec.problems(root);
    if !problems.is_empty() {
        bail!(
            "the task can't be created yet:\n- {}",
            problems.join("\n- ")
        );
    }
    let base = spec.base.expect("checked");
    let name = spec.name.trim();
    let family = family_of(name);
    let from = root.join("bench/tasks").join(base.task());
    let to = root.join("bench/tasks").join(name);
    let mut written: Vec<String> = Vec::new();

    // The task: the base's files, renamed, and its own README, design record
    // and instruction. The practice set is the family's, so it starts empty.
    copy_dir(
        &from,
        &to,
        &[
            "practice",
            "README.md",
            "DESIGN.md",
            "instruction.md",
            "checks",
            "bin",
            "target",
        ],
    )?;
    let practice = to.join("environment/practice");
    std::fs::create_dir_all(&practice)?;
    std::fs::write(practice.join(".gitkeep"), "")?;

    let toml_path = to.join("task.toml");
    let toml = std::fs::read_to_string(&toml_path)?;
    let toml = replace_line(
        &toml,
        "name = ",
        &format!("name = \"solana-foundation/{name}\""),
    );
    let toml = replace_line(&toml, "version = ", "version = \"0.1.0\"");
    let toml = replace_line(
        &toml,
        "description = ",
        &format!("description = {}", toml_string(&spec.summary)),
    );
    let toml = replace_section(&toml, "[metadata.mtx]", &metadata(spec, &family, base));
    std::fs::write(&toml_path, toml)?;

    let test_path = to.join("tests/test.sh");
    // The verifier's 0.5 is this task's reference, which the image builds
    // under the family's anchor name.
    let test = v2_verifier(&std::fs::read_to_string(&test_path)?).replace(
        &format!("--reference /opt/mtx/anchors/{} ", base.generator()),
        &format!("--reference /opt/mtx/anchors/{family} "),
    );
    let test = test.replacen(
        "set -euo pipefail\n",
        &format!(
            "set -euo pipefail\n\n# {TODO}(family): until the `{family}` family exists, this grades on the\n# {base}'s family. See `mtx task status {name}`.\n",
            base = base.task()
        ),
        1,
    );
    std::fs::write(&test_path, &test)?;

    let canary = std::fs::read_to_string(from.join("instruction.md"))?
        .lines()
        .next()
        .filter(|l| l.contains("canary"))
        .unwrap_or_default()
        .to_string();
    let contract = std::fs::read_to_string(from.join("instruction.md"))?;
    let contract = contract
        .find("## The bot contract")
        .map(|i| &contract[i..])
        .unwrap_or("");
    let settings = scoring_text::Settings::of_verifier(&test)?;
    std::fs::write(
        to.join("instruction.md"),
        instruction(spec, &canary, contract, &settings),
    )?;
    std::fs::write(to.join("README.md"), readme(spec))?;
    std::fs::write(to.join("DESIGN.md"), design(spec))?;
    written.push(format!("bench/tasks/{name}/ (from {})", base.task()));

    // The reference bot is the base's, under the family's name; the oracle is
    // a copy of it (the sync writes that).
    let base_task =
        meta_of(root, base.task()).map_err(|e| anyhow::anyhow!("{}: {e}", base.task()))?;
    let bot_to = to.join("reference");
    let new_package = format!("mtx-bot-{family}");
    let new_bin = format!("mtx-{family}");
    let cargo = std::fs::read_to_string(bot_to.join("Cargo.toml"))?
        .replace(
            &format!("\"{}\"", base_task.reference.package),
            &format!("\"{new_package}\""),
        )
        .replace(
            &format!("\"{}\"", base_task.reference.bin),
            &format!("\"{new_bin}\""),
        );
    std::fs::write(bot_to.join("Cargo.toml"), cargo)?;
    let justfile = std::fs::read_to_string(bot_to.join("Justfile"))?.replace(
        &format!("--bin {}", base_task.reference.bin),
        &format!("--bin {new_bin}"),
    );
    std::fs::write(bot_to.join("Justfile"), justfile)?;
    std::fs::create_dir_all(to.join("solution/src"))?;
    for name in strategy_sources(&bot_to) {
        std::fs::copy(
            bot_to.join("src").join(&name),
            to.join("solution/src").join(&name),
        )?;
    }
    written.push(format!(
        "bench/tasks/{name}/reference (package {new_package}; run `cargo update -w` to lock it)"
    ));

    // The checks: the informed bot's slot, the ladder with both anchors,
    // and the probe gate.
    let informed = to.join("checks/bots/informed");
    copy_dir(&bot_to, &informed, &["bin", "target", "bot.wasm"])?;
    let informed_package = format!("{new_package}-informed");
    let informed_bin = format!("{new_bin}-informed");
    let cargo = std::fs::read_to_string(informed.join("Cargo.toml"))?
        .replace(
            &format!("\"{new_package}\""),
            &format!("\"{informed_package}\""),
        )
        .replace(&format!("\"{new_bin}\""), &format!("\"{informed_bin}\""))
        // Two levels deeper than reference/.
        .replace("path = \"../../../../", "path = \"../../../../../../");
    std::fs::write(informed.join("Cargo.toml"), cargo)?;
    let justfile = std::fs::read_to_string(informed.join("Justfile"))?.replace(
        &format!("--bin {new_bin}"),
        &format!("--bin {informed_bin}"),
    );
    std::fs::write(informed.join("Justfile"), justfile)?;
    let lib = informed.join("src/lib.rs");
    let source = std::fs::read_to_string(&lib)?;
    std::fs::write(
        &lib,
        format!(
            "//! {INFORMED_TODO}: this is a copy of the reference. Make it the task's informed\n//! bot, scoring v2's 1: it reads every fact the task's data could tell in\n//! principle (`MTX_REGIME`, which the ladder's `tell_regime` hands it)\n//! but not the session's future randomness, and trades under the same\n//! costs, mandate and latency. Then drop this paragraph.\n//!\n{source}"
        ),
    )?;
    let probes = probe_rungs(
        root,
        base.task(),
        name,
        &family,
        &base_task.reference.package,
        &new_package,
    )?;
    std::fs::write(
        to.join("checks/ladder.json"),
        serde_json::to_string_pretty(&ladder(name, base.generator(), &probes, sessions(spec)))?
            + "\n",
    )?;
    written.push(format!(
        "bench/tasks/{name}/checks: ladder.json (both anchors, the probe gate), the informed bot's slot (package {informed_package})"
    ));

    // Everything that lists the task is written from its task.toml.
    tasks::sync(root)?;
    written.push("the generated lists (mtx task sync)".to_string());

    Ok(json!({ "task": name, "family": family, "written": written, "status": status(root, name)? }))
}

/// A task's `[metadata.mtx]`, for the scaffold: the base's generator until
/// the author's exists.
fn metadata(spec: &TaskSpec, family: &str, base: Base) -> String {
    format!(
        "generator = {}\nanchor = {}\nemoji = \"🆕\"\nmarket = {}\nsummary = {}\nskill = {}\nleak_needles = []\nheadline = \"v2\"\n",
        toml_string(base.generator()),
        toml_string(family),
        toml_string(&format!(
            "{TODO}: venue and instrument ({} base)",
            base.task()
        )),
        toml_string(&spec.thesis),
        toml_string(&spec.thesis),
    )
}

/// The instruction's `mtx-practice` line for a v2 task.
const PRACTICE_LINE: &str = "- `mtx-practice`: grades `/app/bot` on the practice scenarios in `/opt/mtx/practice` by the same rules and settings as the hidden scenarios (scoring v2, each scenario against the reference's and the informed bot's PnL on it), and prints a JSON report: your `reward` (the v2 score), the `v2` block that explains it, and each session's PnL. Bot logs go to `/tmp/mtx-practice-logs/`. It runs three sessions at a time (`MTX_JOBS=1` for one).";
/// The instruction's `mtx` line for a v2 task.
const MTX_LINE: &str = "- `mtx`: the grader itself. `mtx scenario generate --out DIR --prefix NAME --count N --seed S` draws more scenarios from the same family. They carry neither anchor's PnL, so `mtx-practice` can't score them by v2 and says so: use them to see your bot run and read its logs.";

/// What marks the informed bot's slot until it is written.
pub const INFORMED_TODO: &str = "TODO(informed)";

/// A verifier script graded by scoring v2 alone: `--scoring v2-headline` and
/// `--v2-sessions`, and every session graded (no stop at the first breach:
/// v2 scores each one, D9).
fn v2_verifier(script: &str) -> String {
    let v2 = "  --scoring v2-headline \\\n  --v2-sessions \\\n";
    if script.contains("  --stop-on-disqualify \\\n") {
        script.replacen("  --stop-on-disqualify \\\n", v2, 1)
    } else if scoring_text::has_flag(script, "--scoring") {
        script.to_string()
    } else {
        script.replacen("mtx grade \\\n", &format!("mtx grade \\\n{v2}"), 1)
    }
}

/// The base task's probe rungs (the probe gate's, `bench/checks/probes`),
/// rewritten for the new task, and its `probes` section; the probe crate that
/// wraps the base's reference is copied too. A base without them gives
/// none, and an empty `probes`: the task still opts in.
fn probe_rungs(
    root: &Path,
    base: &str,
    name: &str,
    family: &str,
    base_package: &str,
    new_package: &str,
) -> Result<(Vec<Value>, Value)> {
    let ladder: Value = std::fs::read_to_string(
        root.join("bench/tasks")
            .join(base)
            .join("checks/ladder.json"),
    )
    .ok()
    .and_then(|t| serde_json::from_str(&t).ok())
    .unwrap_or_default();
    let own = format!("bench/tasks/{base}/checks/rungs/probe");
    let mut rungs = Vec::new();
    for rung in ladder["rungs"].as_array().into_iter().flatten() {
        let dir = rung["dir"].as_str().unwrap_or_default();
        if dir.starts_with("bench/checks/probes") {
            rungs.push(rung.clone());
        } else if dir == own {
            let mut rung = rung.clone();
            rung["dir"] = json!(format!("bench/tasks/{name}/checks/rungs/probe"));
            rungs.push(rung);
        }
    }
    let from = root.join(&own);
    if from.join("Cargo.toml").is_file() {
        let to = root
            .join("bench/tasks")
            .join(name)
            .join("checks/rungs/probe");
        copy_dir(&from, &to, &["bin", "target"])?;
        let cargo = replace_line(
            &std::fs::read_to_string(to.join("Cargo.toml"))?,
            "name = \"mtx-probe-",
            &format!("name = \"mtx-probe-{family}\""),
        )
        .replace(
            &format!("{base_package} = {{ path"),
            &format!("{base_package} = {{ package = \"{new_package}\", path"),
        );
        std::fs::write(to.join("Cargo.toml"), cargo)?;
    }
    let probes = match &ladder["probes"] {
        Value::Object(p) if !p.contains_key("exempt") => Value::Object(p.clone()),
        _ => json!({}),
    };
    Ok((rungs, probes))
}

/// A new task's `checks/ladder.json`: doing nothing, the blind bets, the
/// reference and the informed bot, the order between them, both anchors,
/// and the probe gate's rungs and section.
fn ladder(name: &str, family: &str, probes: &(Vec<Value>, Value), count: u32) -> Value {
    let mut rungs = vec![
        json!({ "name": "nop", "dir": "bots/starter" }),
        json!({ "name": "blind-long", "dir": "bench/checks/ladder/blind-long", "may_breach": true }),
        json!({ "name": "blind-short", "dir": "bench/checks/ladder/blind-short", "may_breach": true }),
        json!({ "name": "reference", "dir": format!("bench/tasks/{name}/reference") }),
        json!({ "name": "informed", "dir": format!("bench/tasks/{name}/checks/bots/informed"), "tell_regime": true }),
    ];
    rungs.extend(probes.0.iter().cloned());
    json!({
        "family": family,
        "count": count,
        "min_rule_odds": 0.85,
        "rungs": rungs,
        "anchors": { "reference": "reference", "informed": "informed" },
        "gates": {
            "informed": ["informed"],
            "non_reading": ["nop", "blind-long", "blind-short"]
        },
        "rules": [
            "max(blind-long, blind-short) < reference",
            "reference < informed < hindsight"
        ],
        "exploits": { "bots": [], "max_profitable": 0.75, "below": "informed" },
        "probes": probes.1,
    })
}

/// `text` with the body of the table `header` replaced.
fn replace_section(text: &str, header: &str, body: &str) -> String {
    let Some(start) = text.find(&format!("{header}\n")) else {
        return text.to_string();
    };
    let from = start + header.len() + 1;
    let to = text[from..]
        .find("\n[")
        .map_or(text.len(), |i| from + i + 1);
    format!("{}{body}\n{}", &text[..from], &text[to..])
}

fn replace_line(text: &str, prefix: &str, line: &str) -> String {
    let mut done = false;
    text.lines()
        .map(|l| {
            if !done && l.starts_with(prefix) {
                done = true;
                line.to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn toml_string(s: &str) -> String {
    serde_json::to_string(s.trim()).unwrap_or_default()
}

fn copy_dir(from: &Path, to: &Path, skip: &[&str]) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| to.display().to_string())?;
    for entry in std::fs::read_dir(from).with_context(|| from.display().to_string())? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if skip.contains(&name.as_str()) || name.starts_with('.') {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &to.join(&name), skip)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), to.join(&name))?;
        }
    }
    Ok(())
}

fn cap(spec: &TaskSpec) -> String {
    spec.max_position
        .map(|c| format!("{c} SPCX"))
        .unwrap_or_default()
}
fn limit(spec: &TaskSpec) -> String {
    spec.max_drawdown_usd
        .map(|l| format!("${l:.0}"))
        .unwrap_or_default()
}
fn sessions(spec: &TaskSpec) -> u32 {
    spec.hidden_sessions.unwrap_or(72)
}

fn instruction(
    spec: &TaskSpec,
    canary: &str,
    contract: &str,
    settings: &scoring_text::Settings,
) -> String {
    // The base's contract, with a Scoring section of v2's rules in place of
    // the base's.
    let scoring = format!(
        "## Scoring\n\nYour bot is graded on {sessions} hidden scenarios, drawn from the family for each batch of runs from a secret the grader never writes down: no file anywhere holds them. Every session is graded, a breached one too. <!-- {TODO}: how the hidden set splits by regime, how the account is valued, and anything else this task's grade adds. The rest of this contract came from the base task: make every line describe scoring v2 only (no reward over the hindsight bound, no skill). -->\n\n{block}\n\nThe report also gives your return on capital, your per-session Sharpe ratio, your drawdowns against the loss limit, the share of sessions that made money and your worst session. These explain the score; they aren't scored.\n\n",
        sessions = sessions(spec),
        block = scoring_text::block(settings),
    );
    // The base's tool lines describe v1 practice: under v2, mtx-practice
    // scores by the verifier's v2 settings, and only calibrated scenarios.
    let contract = contract
        .lines()
        .map(|l| {
            if l.starts_with("- `mtx-practice`") {
                PRACTICE_LINE.to_string()
            } else if l.starts_with("- `mtx`:") {
                MTX_LINE.to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if contract.ends_with('\n') { "\n" } else { "" };
    let contract = contract.as_str();
    let contract = match contract.find("\n## Scoring\n") {
        Some(i) => {
            let rest = &contract[i + 1..];
            let end = rest[1..].find("\n## ").map_or(rest.len(), |j| j + 2);
            format!("{}{scoring}{}", &contract[..=i], &rest[end..])
        }
        None => format!("{contract}\n{scoring}"),
    };
    format!(
        "{canary}\n{summary}\n\n**You have an hour** (at the standard time limit). When time is up you're stopped, and whatever is in `/app/bot` is graded as it stands, with the `bin/bot` it last built. Keep a working, built bot there all along, and replace it only with a version you've checked is better: an experiment or a parameter sweep still running when time runs out counts for nothing.\n\n## Where it trades\n\n<!-- {TODO}: the market, the snapshot, and the scenario family the hidden sessions are drawn from: every range and odd the grader uses, as distributions, not structure. Regimes: {regimes} -->\n\n{contract}",
        summary = spec.summary.trim(),
        regimes = spec.regimes.trim(),
    )
}

fn readme(spec: &TaskSpec) -> String {
    let name = spec.name.trim();
    format!(
        "# solana-foundation/{name}\n\n{summary}\n\n| | |\n| --- | --- |\n| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |\n| Sessions | {sessions} hidden per grade; <!-- {TODO}: how they split by regime --> practice |\n| Mandate | {cap} position cap; {limit} loss limit |\n| Bound | <!-- {TODO}: what the reward divides by --> |\n| Reference | <!-- {TODO}: its reward, and what it does in one line --> |\n| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- {TODO}: what it knows, in one line --> |\n\nHow every task is scored, and how risk is enforced, is in the\n[dataset README](../README.md#how-a-bot-is-judged).\n\n## The skill\n\nA good bot profits here because it {thesis}\n\n- **Wins:** {wins}\n- **Loses:** {loses}\n\n## The market\n\n<!-- {TODO}: the scenario family: what moves, how often, what the agent can observe. Regimes: {regimes} -->\n\n## Scoring\n\n<!-- {TODO}: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->\n\n## Baselines\n\n<!-- {TODO}: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->\n\n## Realism limits\n\n<!-- {TODO}: what the simulation doesn't model. -->\n\n## More\n\n- [DESIGN.md](DESIGN.md): the design record.\n- [instruction.md](instruction.md): what the agent is told.\n",
        summary = spec.summary.trim(),
        sessions = sessions(spec),
        cap = cap(spec),
        limit = limit(spec),
        thesis = spec.thesis.trim().trim_end_matches('.').to_string() + ".",
        wins = spec.wins.trim(),
        loses = spec.loses.trim(),
        regimes = spec.regimes.trim(),
    )
}

/// The design record's sections, each a prompt until it's written
/// (see skills/trading-task-design/references/design-record.md).
const DESIGN_SECTIONS: &[(&str, &str)] = &[
    (
        "Expert research",
        "why this is a real expert task, the expert's method, where non-experts go wrong, with sources",
    ),
    (
        "The central decision",
        "the one decision the task is about, and the options that look defensible",
    ),
    (
        "Variants",
        "each regime and the best response to it; a table of which habit fails where",
    ),
    (
        "Levers",
        "what the market does, what a good bot does, and how an honest operator detects each lever",
    ),
    (
        "No tells",
        "the audit: nothing in the path's shape gives away what a trader couldn't see",
    ),
    (
        "Detectability",
        "how early a good bot can tell the regime, measured on generated sets",
    ),
    (
        "Fairness",
        "what the agent is told, what it can learn from practice, and that the reference stays inside the mandate",
    ),
    (
        "Failure modes, priced",
        "each way a bot can lose, and what it costs",
    ),
    (
        "Predictions",
        "what you expect each model tier to score, before running them",
    ),
    ("Generations", "what each version learned"),
];

fn design(spec: &TaskSpec) -> String {
    let mut out = format!(
        "# Design record: {name}\n\nWritten to `skills/trading-task-design/references/design-record.md`. The\nREADME says what the task is; this says why it's built this way, and what\neach version learned.\n\n## Thesis\n\nA good bot profits here because it {thesis}\n\n- **Wins:** {wins}\n- **Loses:** {loses}\n- **Regimes:** {regimes}\n- **Mandate:** {cap} position cap, {limit} loss limit, {sessions} hidden sessions per grade.\n",
        name = spec.name.trim(),
        thesis = spec.thesis.trim().trim_end_matches('.').to_string() + ".",
        wins = spec.wins.trim(),
        loses = spec.loses.trim(),
        regimes = spec.regimes.trim(),
        cap = cap(spec),
        limit = limit(spec),
        sessions = sessions(spec),
    );
    for (title, prompt) in DESIGN_SECTIONS {
        out.push_str(&format!("\n## {title}\n\n<!-- {TODO}: {prompt}. -->\n"));
    }
    out
}

/// One step of a task's road to publication.
#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub id: &'static str,
    pub title: &'static str,
    /// `done`, `todo`, or `manual` (needs the image or real agents).
    pub state: &'static str,
    /// What the files show.
    pub detail: String,
    /// What to do next, with the command when there is one.
    pub next: String,
}

impl Step {
    fn new(
        id: &'static str,
        title: &'static str,
        done: bool,
        detail: String,
        next: String,
    ) -> Self {
        Step {
            id,
            title,
            state: if done { "done" } else { "todo" },
            detail,
            next: if done { String::new() } else { next },
        }
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn todos(text: &str) -> usize {
    text.matches(TODO).count()
}

/// The task as the lists read it: its `[metadata.mtx]` and reference crate,
/// or what is wrong with them.
fn meta_of(root: &Path, task: &str) -> Result<tasks::Task, String> {
    tasks::load_one(root, task.to_string()).map_err(|e| format!("{e:#}"))
}

/// A reference bot's strategy sources, by file name: everything in its `src`
/// but the process entry point, as `just solution` copies them.
fn strategy_sources(bot_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(bot_dir.join("src"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "main.rs")
        .collect();
    names.sort();
    names
}

/// Which steps are done for a task, and what to do next.
pub fn status(root: &Path, task: &str) -> Result<Vec<Step>> {
    let dir = root.join("bench/tasks").join(task);
    if !dir.join("task.toml").is_file() {
        let known = list(root)
            .iter()
            .map(|t| t["name"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>();
        bail!(
            "no task `{task}` in bench/tasks (tasks: {}). Create one with task_new.",
            known.join(", ")
        );
    }
    let meta = meta_of(root, task);
    let problem = meta.as_ref().err().cloned().unwrap_or_default();
    let meta = meta.ok();
    let family = meta
        .as_ref()
        .map_or_else(|| family_of(task), |t| t.mtx.generator.clone());
    let mut steps = Vec::new();

    let design = read(&dir.join("DESIGN.md"));
    let n = todos(&design);
    steps.push(Step::new(
        "design",
        "Thesis and design record",
        !design.is_empty() && n == 0,
        if design.is_empty() {
            "no DESIGN.md".into()
        } else {
            format!(
                "{n} section{} still to write",
                if n == 1 { "" } else { "s" }
            )
        },
        format!(
            "Fill in bench/tasks/{task}/DESIGN.md, section by section (task_guide step=design)."
        ),
    ));

    let test = read(&dir.join("tests/test.sh"));
    let draw = test
        .split("--draw")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or("")
        .to_string();
    // The task's generator is one a task already draws or one of its own: it
    // is done when task.toml names a registered generator that test.sh draws.
    let own = family_of(task);
    let base_todo = test.contains("TODO(family)");
    let drawn = meta.is_some() && draw == family && !base_todo;
    let anchor = meta
        .as_ref()
        .map_or_else(|| family.clone(), |t| t.mtx.anchor().to_string());
    steps.push(Step::new(
        "family",
        "Scenario family",
        drawn,
        if meta.is_none() {
            format!("task.toml's [metadata.mtx] doesn't load: {problem}")
        } else if base_todo {
            format!("tests/test.sh still grades on the base's `{draw}` family")
        } else if draw != family {
            format!("task.toml's generator is `{family}`, but tests/test.sh draws `{draw}`")
        } else {
            format!("test.sh draws `{family}`")
        },
        format!(
            "Either reuse a registered generator ({}) or write the `{own}` family in crates/grader (task_guide step=family) and register it in `GENERATORS` (crates/grader/src/generators.rs). Then set `generator` in bench/tasks/{task}/task.toml, make tests/test.sh draw it with `--draw`, point --reference at /opt/mtx/anchors/{anchor}, and drop the TODO there.",
            mtx_grader::generators::names().join(", ")
        ),
    ));

    let bot_src = read(&dir.join("reference/src/main.rs"));
    let generated = tasks::stale(root).unwrap_or_default();
    let solution_prefix = format!("bench/tasks/{task}/solution/");
    let solution_stale = generated.iter().any(|p| p.starts_with(&solution_prefix));
    steps.push(Step::new(
        "reference",
        "Reference bot and oracle",
        meta.is_some() && !bot_src.is_empty() && !solution_stale,
        if meta.is_none() {
            format!("the reference doesn't load: {problem}")
        } else if bot_src.is_empty() {
            "reference/src/main.rs is missing".into()
        } else if solution_stale {
            "solution/ differs from reference/".into()
        } else {
            "reference/, and the oracle matches it".to_string()
        },
        "Write the reference strategy in reference/ (task_guide step=reference), then `mtx task sync` to copy it into the oracle.".into(),
    ));

    let v2 = meta
        .as_ref()
        .is_some_and(|t| t.mtx.headline == tasks::Headline::V2);
    if v2 {
        steps.push(anchors_step(root, task));
    }

    let anchored = meta.is_some() && !generated.iter().any(|p| p.ends_with("anchors.txt"));
    steps.push(Step::new(
        "anchor",
        "The verifier's reference (image anchor)",
        anchored,
        if anchored { format!("the image builds {} into /opt/mtx/anchors/{anchor}", meta.as_ref().map(|t| t.reference.package.clone()).unwrap_or_default()) } else { "bench/image/anchors.txt doesn't list this task's reference yet".into() },
        "Run `mtx task sync` (the image builds what bench/image/anchors.txt lists), then `just image`.".to_string(),
    ));

    let practice: Vec<PathBuf> = std::fs::read_dir(dir.join("environment/practice"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    let calibrated = practice
        .iter()
        .filter(|p| read(p).contains("\"reference_pnl_usd\""))
        .count();
    // A task that keeps its reference out of practice (`practice_reference =
    // false`) carries no baseline in its practice files: agents shown the
    // reference's PnL tune toward it.
    let hidden = read(&dir.join("task.toml")).lines().any(|l| {
        l.split_once('=')
            .is_some_and(|(k, v)| k.trim() == "practice_reference" && v.trim() == "false")
    });
    let plural = if practice.len() == 1 { "" } else { "s" };
    // v2 scores practice as it scores the hidden set, against both anchors:
    // every practice file carries the reference's and the informed bot's PnL.
    let informed = practice
        .iter()
        .filter(|p| read(p).contains("\"informed_pnl_usd\""))
        .count();
    steps.push(if v2 {
        Step::new(
            "practice",
            "Practice scenarios, calibrated with both anchors",
            !hidden && !practice.is_empty() && calibrated == practice.len() && informed == practice.len(),
            if hidden {
                "the task hides its reference from practice (`practice_reference = false`), but scoring v2 scores practice against it".to_string()
            } else {
                format!("{} practice scenario{plural}: {calibrated} with the reference's PnL, {informed} with the informed bot's", practice.len())
            },
            format!("Draw a practice set from the family (`mtx scenario generate --family {family} --out bench/tasks/{task}/environment/practice --prefix practice --count 6 --seed <any number>`), then `just calibrate bench/tasks/{task}/reference <files>` and `mtx calibrate --informed --v2-sessions --snapshot snapshots/<snapshot> --bot bench/tasks/{task}/checks/bots/informed <files>`; a v2 task keeps no `practice_reference = false`."),
        )
    } else if hidden {
        Step::new(
            "practice",
            "Practice scenarios, no reference baseline",
            !practice.is_empty() && calibrated == 0,
            format!("{} practice scenario{plural}, {calibrated} with a baseline (the task hides its reference from practice)", practice.len()),
            format!("Draw a practice set from the family (`mtx scenario generate --family {family} --out bench/tasks/{task}/environment/practice --prefix practice --count 6 --seed <any number>`); the task's `practice_reference = false` keeps `reference_pnl_usd` out of the files."),
        )
    } else {
        Step::new(
            "practice",
            "Practice scenarios, calibrated",
            !practice.is_empty() && calibrated == practice.len(),
            format!("{} practice scenario{plural}, {calibrated} calibrated", practice.len()),
            format!("Draw a practice set from the family (`mtx scenario generate --family {family} --out bench/tasks/{task}/environment/practice --prefix practice --count 6 --seed <any number>`), then `just calibrate bench/tasks/{task}/reference <files>`."),
        )
    });

    let instr = read(&dir.join("instruction.md"));
    let n = todos(&instr);
    let budget = instr.contains("You have an hour");
    steps.push(Step::new(
        "instruction",
        "The agent's instruction",
        n == 0 && budget,
        format!(
            "{n} {TODO}{}{}",
            if n == 1 { "" } else { "s" },
            if budget { "" } else { "; no time-budget line" }
        ),
        format!(
            "Write bench/tasks/{task}/instruction.md as a contract (task_guide step=instruction)."
        ),
    ));

    if v2 {
        steps.push(scoring_step(task, &test, &instr));
        steps.push(probes_step(root, task));
    }

    let readme = read(&dir.join("README.md"));
    let sections = [
        "## The skill",
        "## The market",
        "## Scoring",
        "## Baselines",
        "## Realism limits",
        "## More",
    ];
    let missing: Vec<&str> = sections
        .iter()
        .copied()
        .filter(|s| !readme.contains(s))
        .collect();
    let n = todos(&readme);
    steps.push(Step::new(
        "readme",
        "README, to the template",
        missing.is_empty() && n == 0,
        if missing.is_empty() {
            format!("{n} {TODO}{}", if n == 1 { "" } else { "s" })
        } else {
            format!("missing {}", missing.join(", "))
        },
        "Fill in the README's sections (template: \"Task READMEs\" in bench/tasks/README.md)."
            .into(),
    ));

    steps.push(Step::new(
        "registry",
        "Generated lists",
        meta.is_some() && generated.is_empty(),
        if meta.is_none() {
            format!("task.toml doesn't load: {problem}")
        } else if generated.is_empty() {
            "the Justfile, image, dataset and READMEs list every task".into()
        } else {
            format!(
                "stale: {}",
                generated
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
        "Fill in the task's `[metadata.mtx]` in task.toml, then run `mtx task sync`.".into(),
    ));

    let lint = std::process::Command::new("harbor")
        .args(["lint"])
        .arg(&dir)
        .args(["--select", "ALL", "--ignore", "unpinned-image"])
        .output();
    let (lint_ok, lint_detail) = match lint {
        Ok(out) if out.status.success() => (true, "harbor lint passes".to_string()),
        Ok(out) => (
            false,
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .chain(String::from_utf8_lossy(&out.stderr).lines())
                .take(6)
                .collect::<Vec<_>>()
                .join(" · "),
        ),
        Err(_) => (
            false,
            "harbor isn't installed (`uv tool install harbor`)".to_string(),
        ),
    };
    steps.push(Step::new(
        "lint",
        "Harbor lint",
        lint_ok,
        lint_detail,
        format!("harbor lint bench/tasks/{task} --select ALL --ignore unpinned-image"),
    ));

    steps.push(Step {
        id: "verify",
        title: "Checks on the image, the ladder, real agents",
        state: "manual",
        detail: "needs the image and real runs; can't be read from files".into(),
        next: format!(
            "`just image && just harbor-checks` (oracle at skill 1, or at v2's 0.5 before the near-breach charge; nop/cheater/sneaky 0), `just ladder {task}`, `just leak-audit`, `harbor check bench/tasks/{task}`, then real agents (task_guide step=verify)."
        ),
    });
    Ok(steps)
}

/// A task's `checks/ladder.json`, or null.
fn ladder_file(root: &Path, task: &str) -> Value {
    serde_json::from_str(&read(
        &root
            .join("bench/tasks")
            .join(task)
            .join("checks/ladder.json"),
    ))
    .unwrap_or_default()
}

/// Where a task's anchors must live, from the repository's root: the
/// reference is its own `reference/` crate (what the image builds into
/// `/opt/mtx/anchors/<anchor>` for the verifier), the informed bot its
/// `checks/bots/informed`.
pub fn anchor_dirs(task: &str) -> (String, String) {
    (
        format!("bench/tasks/{task}/reference"),
        format!("bench/tasks/{task}/checks/bots/informed"),
    )
}

/// What's wrong with the rungs a v2 task's ladder names as its anchors,
/// from the ladder file alone: each must be a rung, never `may_breach`, the
/// reference the task's own reference crate, run as it is (no `env`, not
/// told), and the
/// informed bot the task's informed crate, told the session's regime
/// (`tell_regime`, the ladder's tell), so a better-tuned bot that reads
/// nothing can't pose as the 1.
pub fn anchor_rung_problems(ladder: &Value, task: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let (reference_dir, informed_dir) = anchor_dirs(task);
    let norm = |d: &str| d.trim_start_matches("./").trim_end_matches('/').to_string();
    for (role, scale, want) in [
        ("reference", "0.5", reference_dir),
        ("informed", "1", informed_dir),
    ] {
        let Some(name) = ladder["anchors"][role].as_str() else {
            problems.push(format!(
                "ladder.json's `anchors.{role}` names no rung (the {scale})"
            ));
            continue;
        };
        let Some(rung) = ladder["rungs"]
            .as_array()
            .and_then(|rungs| rungs.iter().find(|r| r["name"] == name))
        else {
            problems.push(format!(
                "`anchors.{role}` is `{name}`, which isn't a rung in ladder.json"
            ));
            continue;
        };
        let dir = rung["dir"].as_str().map(norm).unwrap_or_default();
        if dir != want {
            problems.push(format!(
                "the {role} anchor `{name}` runs {dir}, not the task's {role} bot, {want}"
            ));
        }
        if rung.get("env").is_some_and(|e| !e.is_null()) {
            problems.push(format!(
                "the {role} anchor `{name}` sets `env`: an anchor runs as it is"
            ));
        }
        // The ladder leaves a `may_breach` rung's breaches out of its clean-run
        // list, which the strength band reads: an anchor never may.
        if rung["may_breach"] == Value::Bool(true) {
            problems.push(format!(
                "the {role} anchor `{name}` is `may_breach`: an anchor stays inside its mandate, and the ladder must report it if it doesn't"
            ));
        }
        let told = rung["tell_regime"] == Value::Bool(true);
        match role {
            "reference" if told => problems.push(format!(
                "the reference anchor `{name}` is told the regime (`tell_regime`): the 0.5 reads the market as a bot does"
            )),
            "informed" if !told => problems.push(format!(
                "the informed anchor `{name}` isn't told the regime (`tell_regime: true`): without the task's tell it isn't the 1"
            )),
            _ => {}
        }
    }
    problems
}

/// The anchors a v2 task's ladder names: the task's own bots, the informed
/// one told the regime and no longer the scaffold's copy of the reference,
/// and the verifier grading against the anchor the image builds from the
/// reference crate. What's wrong, if anything.
pub fn anchor_problems(root: &Path, task: &str) -> Vec<String> {
    let ladder = ladder_file(root, task);
    if ladder.is_null() {
        return vec!["checks/ladder.json is missing or isn't JSON".into()];
    }
    let mut problems = anchor_rung_problems(&ladder, task);
    let (reference, informed) = anchor_dirs(task);
    for (role, dir) in [("reference", &reference), ("informed", &informed)] {
        if !root.join(dir).join("Cargo.toml").is_file() {
            problems.push(format!("the {role} anchor's crate, {dir}, doesn't exist"));
        }
    }
    let unwritten = std::fs::read_dir(root.join(&informed).join("src"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| read(&e.path()).contains(INFORMED_TODO));
    if unwritten {
        problems.push(format!(
            "{informed} is still the scaffold's copy of the reference ({INFORMED_TODO})"
        ));
    }
    // The verifier's 0.5 is the image's build of the reference crate.
    if let Ok(meta) = tasks::load_one(root, task.to_string()) {
        let want = format!("/opt/mtx/anchors/{}", meta.mtx.anchor());
        let test = read(&root.join("bench/tasks").join(task).join("tests/test.sh"));
        if scoring_text::flag_value(&test, "--reference").as_deref() != Some(want.as_str()) {
            problems.push(format!(
                "tests/test.sh doesn't grade against `--reference {want}`, the image's build of {reference}"
            ));
        }
    }
    problems
}

fn anchors_step(root: &Path, task: &str) -> Step {
    let problems = anchor_problems(root, task);
    Step::new(
        "anchors",
        "Scoring v2's anchors: reference (0.5) and informed bot (1)",
        problems.is_empty(),
        if problems.is_empty() {
            "ladder.json names both anchors, and both bots exist".into()
        } else {
            problems.join("; ")
        },
        format!(
            "Write the informed bot in bench/tasks/{task}/checks/bots/informed and name both anchors in checks/ladder.json's `anchors` (task_guide step=anchors)."
        ),
    )
}

/// What a v2 task's verifier and instruction lack: the verifier grades with
/// `--scoring v2-headline` and `--v2-sessions` and every session, and the
/// instruction carries the current scoring rules.
pub fn scoring_problems(test: &str, instruction: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if scoring_text::flag_value(test, "--scoring").as_deref() != Some("v2-headline") {
        problems.push(
            "tests/test.sh doesn't grade with `--scoring v2-headline` (v2 as the one score)"
                .to_string(),
        );
    }
    if !scoring_text::has_flag(test, "--v2-sessions") {
        problems.push("tests/test.sh doesn't pass `--v2-sessions`".to_string());
    }
    if scoring_text::has_flag(test, "--stop-on-disqualify") {
        problems.push(
            "tests/test.sh stops at the first breach (`--stop-on-disqualify`): v2 grades every session"
                .to_string(),
        );
    }
    for flag in ["--ceiling", "--eta"] {
        if scoring_text::has_flag(test, flag) {
            problems.push(format!(
                "tests/test.sh passes `{flag}`: a v2 task's 1 is its informed bot, never a given or proxy ceiling"
            ));
        }
    }
    let leftovers = v1_wording(instruction);
    if !leftovers.is_empty() {
        problems.push(format!(
            "instruction.md still describes v1 scoring ({}): a v2 task has one score",
            leftovers.join(", ")
        ));
    }
    match scoring_text::Settings::of_verifier(test) {
        Err(e) => problems.push(format!("{e:#}")),
        Ok(settings) => match scoring_text::with_block(instruction, &settings) {
            Ok(None) => problems.push("instruction.md has no `mtx:scoring` block".to_string()),
            Ok(Some(text)) if text != instruction => problems.push(
                "instruction.md's scoring rules are stale (`mtx task sync` rewrites them)"
                    .to_string(),
            ),
            Ok(Some(_)) => {}
            Err(e) => problems.push(format!("instruction.md: {e:#}")),
        },
    }
    problems
}

/// The v1 scoring words an instruction holds outside its scoring block:
/// `skill`, the hindsight bound, `reference_ratio`.
pub fn v1_wording(instruction: &str) -> Vec<&'static str> {
    let outside = match (
        instruction.find("<!-- mtx:scoring begin"),
        instruction.find(scoring_text::END),
    ) {
        (Some(b), Some(e)) if e > b => {
            format!("{}{}", &instruction[..b], &instruction[e..])
        }
        _ => instruction.to_string(),
    }
    .to_lowercase();
    let word = |w: &str| {
        outside.match_indices(w).any(|(i, _)| {
            let before = outside[..i].chars().next_back();
            let after = outside[i + w.len()..].chars().next();
            let edge = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
            edge(before) && edge(after)
        })
    };
    [
        ("skill", "`skill`"),
        ("hindsight bound", "the hindsight bound"),
        ("reference_ratio", "`reference_ratio`"),
    ]
    .into_iter()
    .filter(|(w, _)| word(w))
    .map(|(_, label)| label)
    .collect()
}

fn scoring_step(task: &str, test: &str, instruction: &str) -> Step {
    let problems = scoring_problems(test, instruction);
    Step::new(
        "scoring",
        "Scored by v2, and the agent told how",
        problems.is_empty(),
        if problems.is_empty() {
            "the verifier grades by v2, and instruction.md states its rules".into()
        } else {
            problems.join("; ")
        },
        format!(
            "In bench/tasks/{task}: grade with `--scoring v2-headline --v2-sessions` and without `--stop-on-disqualify` in tests/test.sh; put the `mtx:scoring` markers in instruction.md's Scoring section and run `mtx task sync` (task_guide step=scoring)."
        ),
    )
}

/// The probe gate (bench/checks/probes): a v2 task's ladder declares a
/// `probes` section, `{}` or `{"exempt": "why"}`; once the gate is in the
/// repository, a task that doesn't opt out runs its probe rungs.
pub fn probe_problems(root: &Path, task: &str) -> Vec<String> {
    let ladder = ladder_file(root, task);
    let Some(probes) = ladder["probes"].as_object() else {
        return vec!["ladder.json has no `probes` section".into()];
    };
    if let Some(why) = probes.get("exempt") {
        return if why.as_str().is_some_and(|w| !w.trim().is_empty()) {
            Vec::new()
        } else {
            vec!["ladder.json's `probes.exempt` gives no reason".into()]
        };
    }
    let gate = root.join("bench/checks/probes/Cargo.toml").is_file();
    let runs = ladder["rungs"].as_array().into_iter().flatten().any(|r| {
        r["dir"]
            .as_str()
            .is_some_and(|d| d.starts_with("bench/checks/probes"))
    });
    if gate && !runs {
        vec!["ladder.json opts into the probe gate but lists none of its rungs (bench/checks/probes/README.md)".into()]
    } else {
        Vec::new()
    }
}

fn probes_step(root: &Path, task: &str) -> Step {
    let problems = probe_problems(root, task);
    Step::new(
        "probes",
        "The probe gate",
        problems.is_empty(),
        if problems.is_empty() {
            "ladder.json opts into the probe gate".into()
        } else {
            problems.join("; ")
        },
        format!(
            "Add `\"probes\": {{}}` to bench/tasks/{task}/checks/ladder.json, and the probe rungs once bench/checks/probes is in the repository (task_guide step=probes)."
        ),
    )
}

/// The first step not done, if any.
pub fn next(steps: &[Step]) -> Option<&Step> {
    steps.iter().find(|s| s.state == "todo")
}

/// Every task, with how far along it is.
pub fn list(root: &Path) -> Vec<Value> {
    let mut names: Vec<String> = std::fs::read_dir(root.join("bench/tasks"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("task.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let version = read(&root.join("bench/tasks").join(&name).join("task.toml"))
                .lines()
                .find_map(|l| {
                    l.strip_prefix("version = ")
                        .map(|v| v.trim_matches('"').to_string())
                });
            let family =
                meta_of(root, &name).map_or_else(|_| family_of(&name), |t| t.mtx.generator);
            json!({ "name": name, "version": version, "family": family })
        })
        .collect()
}

/// What a step involves: the procedure, and where the skill goes deeper.
pub fn guide(step: &str) -> Option<&'static str> {
    Some(match step {
        "design" => {
            "Write the design record before building. Thesis: one sentence, `a good bot profits here because it ___`. Then, section by section: expert research (who does this for money, their method, where non-experts go wrong, with sources); the central decision and the options that look defensible; variants (each regime flips the best response); levers with how an honest operator detects each; the no-tells audit; detectability; fairness; failure modes priced; predictions per model tier; generations. If you can't write where non-experts go wrong, the task has no failure mode worth measuring. Deeper: skills/trading-task-design/references/design-record.md."
        }
        "family" => {
            "A family is a generator `(seed) -> scenario`, in crates/grader: see `ShockFamily` (scenario.rs) for scripted moves and `MakingFamily` (making.rs) for counterparties. Vary what tests the thesis (timing, depth, legs, recovery, duration); include adversarial draws (a move that doesn't revert, a fake-out, a calm session); make the first move unpredictable (mirror it, 50/50); hide the generator's mechanics (uneven steps and gaps, a random tail, small noise); respect protocol physics (Phoenix's oracle band: steps <= 4%). Give it `draw` and `draw_set` (stratified by the family's odds) and a hindsight bound. Register it in `GENERATORS` (crates/grader/src/generators.rs) with its practice and hidden draws; `mtx grade --draw` and `mtx scenario generate --family` take its name. Before publishing, read the generator as an adversary: if knowing it alone beats the reference, it leaks. Deeper: SKILL.md step 2, references/ladder-and-ceilings.md."
        }
        "reference" => {
            "The reference sets the 1.0 line: a reasonable, simple strategy, not an optimum, sized to the cap and inside the loss limit on every session. Avoid mechanical biases (a bid always placed before the ask). It runs on the same hidden sessions in every grade, so hidden sets need no calibration; practice scenarios do. Keep the oracle identical: `just solution`. Then build the ladder: nop, a blind bet, a naive strategy, the reference, a ceiling, as strict inequalities (`just ladder <family>`). Deeper: SKILL.md step 3, references/ladder-and-ceilings.md."
        }
        "anchors" => {
            "Scoring v2 has three anchors: doing nothing (0), the reference (0.5) and the informed bot (1). The informed bot is a bot, not a hindsight figure: it knows every fact the task's data could tell in principle (the regime, the informed wallets, the fair-value model, the funding process, the cost curves; the ladder hands it the session's regime as MTX_REGIME through `tell_regime`) but not the session's future randomness, and it trades the same market under the same costs, mandate and latency. The scaffold puts a copy of the reference in checks/bots/informed, marked TODO(informed): rewrite it, drop the marker, and keep both named in checks/ladder.json's `anchors` ({\"reference\": \"reference\", \"informed\": \"informed\"}). Freeze both before any model run. Calibrate the practice scenarios with each: `just calibrate` for the reference, then `mtx calibrate --informed --v2-sessions` for the informed bot. The strength band (`mtx gates`): on the ladder's sets the informed bot beats the reference, the reference's weighted PnL is between 0.2 and 0.7 of the informed bot's (`anchors.band` overrides), so it's never `anchor_floored`, and its per-session Sharpe is at least 0.25 (`anchors.min_sharpe`)."
        }
        "scoring" => {
            "A v2 task's verifier grades with `--scoring v2-headline --v2-sessions`, so Harbor's `reward` is v2's clamped score and no v1 score is reported, and it grades every session (no `--stop-on-disqualify`). The grade fails rather than fall back to the hindsight ceiling when a session lacks the informed bot's PnL; `--loss-weight`, `--risk-weight` and `--anchor-floor` only if the task needs other settings than the grader's. The agent is told the rules (D5): instruction.md's Scoring section holds the `mtx:scoring` markers, and `mtx task sync` writes the rules between them from the grader's settings and test.sh's flags. `mtx task check` fails when they're stale. Write only what's specific to the task around them: the hidden set's size and split, the valuation."
        }
        "probes" => {
            "The probe gate (bench/checks/probes, D7) grades bots with no skill or a known small amount (gamblers, naive holds, a martingale, levered references, an occasional breacher, honest-but-noisy bots) and fails the task if v2 pays any of them more than it should. A new task opts in: `\"probes\": {}` in checks/ladder.json, which the scaffold writes. Once the gate is in the repository, add its rungs (the scaffold copies them from the base task when it has them; otherwise follow bench/checks/probes/README.md, \"A task opts in\"). A task that can't run them says why: `\"probes\": {\"exempt\": \"why\"}`."
        }
        "anchor" => {
            "The verifier grades the reference itself on each hidden set, from /opt/mtx/anchors/<family> in the runtime image. `mtx task sync` lists the task's reference in bench/image/anchors.txt, which the image build reads. Rebuild with `just image`."
        }
        "practice" => {
            "Practice scenarios are drawn from the same family as the hidden ones, ship in environment/practice, and carry the reference's PnL so `mtx-practice` can report it: `mtx scenario generate --family <family> --out bench/tasks/<task>/environment/practice --prefix practice --count 6 --seed <any number>`, then `just calibrate bots/<bot> <files>`. Iterating on scored scenarios is overfitting: the hidden set is drawn inside the verifier from the batch's secret."
        }
        "instruction" => {
            "The instruction is a contract. State every rule the grader enforces (env vars, the RPC allowlist and the penalty for refused calls, the stop signal and grace period, the valuation, that the grader doesn't build the bot) and every claim truthfully (paths, tools and timings match the image). Disclose the family's distributions, not its structure: overlapping ranges, independent draws. Keep the time-budget line. The scaffold kept the base's contract sections: revise Scoring for this task. Deeper: references/review-checklist.md, references/anti-cheat.md."
        }
        "readme" => {
            "Every task README has the same sections (template: \"Task READMEs\" in bench/tasks/README.md): a facts table, The skill, The market, Scoring (only what's specific to the task), Baselines (check table, ladder, reference's numbers), Realism limits, More. Anything every task shares belongs in the dataset README."
        }
        "registry" => {
            "Fill in the task's `[metadata.mtx]` in task.toml and run `mtx task sync`: it writes the Justfile's and CI's task lists (bench/tasks/tasks.just), the image's anchors, the dataset's task list and the README tables. `just release` pins digests and publishes."
        }
        "lint" => {
            "`harbor lint bench/tasks/<task> --select ALL --ignore unpinned-image` checks the task's files against Harbor's rules; images are pinned at release."
        }
        "verify" => {
            "On the image: `just image && just harbor-checks` (oracle at skill 1, or for a v2 task at 0.5 before the near-breach charge; nop 0, cheater 0, sneaky 0), `just ladder <task>` (the order rule on fresh hidden sets), `just leak-audit` (the agent's image holds no answer), `harbor check bench/tasks/<task>` (an LLM review against Harbor's rubric). Then real agents, k runs each, and `harbor analyze`. Read the winning bots: agents state their edge in doc comments. Treat a too-clean risk profile (worst session near 0, 90%+ profitable) as a leak. Target: the best model's mean reward between 0.2 and 0.7. Deeper: SKILL.md steps 7 and 8."
        }
        _ => return None,
    })
}

pub const STEPS: &[&str] = &[
    "design",
    "family",
    "reference",
    "anchors",
    "anchor",
    "practice",
    "instruction",
    "scoring",
    "probes",
    "readme",
    "registry",
    "lint",
    "verify",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TaskSpec {
        TaskSpec {
            name: "funding-squeeze-spcx".into(),
            title: "Funding squeeze".into(),
            summary: "Write a bot that trades Phoenix SPCX perps through funding spikes.".into(),
            base: Some(Base::Shocks),
            thesis: "harvests the carry and exits before funding flips against it".into(),
            wins: "sizing the carry to the loss limit".into(),
            loses: "holding the crowded side through a squeeze".into(),
            regimes: "half spike, a quarter decay, a quarter calm".into(),
            max_position: Some(70.0),
            max_drawdown_usd: Some(1000.0),
            hidden_sessions: None,
        }
    }

    #[test]
    fn a_spec_says_everything_wrong_at_once() {
        let root = repo_root();
        let empty = TaskSpec::default();
        assert_eq!(empty.missing().len(), QUESTIONS.len());
        let mut bad = spec();
        bad.name = "Funding".into();
        bad.max_position = Some(-1.0);
        let problems = bad.problems(&root);
        assert_eq!(problems.len(), 2, "{problems:?}");
        let mut taken = spec();
        taken.name = "flash-crash-spcx".into();
        assert!(taken.problems(&root)[0].contains("already exists"));
        assert!(spec().problems(&root).is_empty());
    }

    /// A scratch repository holding flash-crash-spcx (without its checks)
    /// and the files the task lists are written into.
    fn scratch_repo() -> PathBuf {
        let real = repo_root();
        let root =
            std::env::temp_dir().join(format!("mtx-author-{}", uuid::Uuid::new_v4().simple()));
        copy_dir(
            &real.join("bench/tasks/flash-crash-spcx"),
            &root.join("bench/tasks/flash-crash-spcx"),
            &["bin", "target", "practice", "checks"],
        )
        .unwrap();
        std::fs::copy(real.join("Justfile"), root.join("Justfile")).unwrap();
        for file in [
            "README.md",
            "bench/tasks/README.md",
            "bench/tasks/dataset.toml",
        ] {
            std::fs::copy(real.join(file), root.join(file)).unwrap();
        }
        std::fs::create_dir_all(root.join("bench/image")).unwrap();
        root
    }

    fn state_of(steps: &[Step], id: &str) -> (&'static str, String) {
        let step = steps
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("no step {id}"));
        (step.state, step.detail.clone())
    }

    #[test]
    fn a_scaffold_is_a_task_whose_status_points_at_the_family() {
        let root = scratch_repo();
        let out = scaffold(&root, &spec()).unwrap();
        let task = root.join("bench/tasks/funding-squeeze-spcx");
        assert!(
            read(&task.join("task.toml"))
                .contains("name = \"solana-foundation/funding-squeeze-spcx\"")
        );
        assert!(read(&task.join("task.toml")).contains("version = \"0.1.0\""));
        assert!(read(&task.join("instruction.md")).contains("## The bot contract"));
        assert!(read(&task.join("README.md")).contains("harvests the carry"));
        assert!(read(&task.join("reference/Cargo.toml")).contains("mtx-bot-funding-squeeze"));
        assert!(
            read(&root.join("bench/tasks/tasks.just"))
                .contains("funding-squeeze-spcx=bench/tasks/funding-squeeze-spcx/reference")
        );
        assert!(
            read(&root.join("bench/image/anchors.txt"))
                .contains("funding-squeeze mtx-bot-funding-squeeze mtx-funding-squeeze bench/tasks/funding-squeeze-spcx/reference")
        );
        assert!(
            read(&root.join("bench/tasks/dataset.toml"))
                .contains("solana-foundation/funding-squeeze-spcx")
        );
        let steps = out["status"].as_array().unwrap();
        let state = |id: &str| {
            steps.iter().find(|s| s["id"] == id).unwrap()["state"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(state("design"), "todo");
        assert_eq!(state("family"), "todo");
        assert_eq!(state("reference"), "done");
        assert_eq!(state("verify"), "manual");
        assert!(scaffold(&root, &spec()).is_err(), "exists");
        // A task that reuses a registered generator is done with the family
        // once task.toml names it and test.sh draws it.
        let toml = read(&task.join("task.toml"));
        std::fs::write(
            task.join("task.toml"),
            toml.replace("generator = \"flash-crash\"", "generator = \"weekend-gap\""),
        )
        .unwrap();
        let test = read(&task.join("tests/test.sh"))
            .replace("--draw flash-crash", "--draw weekend-gap")
            .replace("TODO(family)", "note");
        std::fs::write(task.join("tests/test.sh"), test).unwrap();
        let steps = status(&root, "funding-squeeze-spcx").unwrap();
        let family = steps.iter().find(|s| s.id == "family").unwrap();
        assert_eq!(family.state, "done", "{}", family.detail);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The template end to end: a dummy task scaffolded, checked step by
    /// step and gated, so the scaffold can't rot.
    #[test]
    fn a_new_task_is_scaffolded_ready_for_scoring_v2() {
        let root = scratch_repo();
        scaffold(&root, &spec()).unwrap();
        let name = "funding-squeeze-spcx";
        let task = root.join("bench/tasks").join(name);

        // v2 is its headline, and its verifier grades by v2, every session.
        let meta = tasks::load_one(&root, name.into()).unwrap();
        assert_eq!(meta.mtx.headline, tasks::Headline::V2);
        let test = read(&task.join("tests/test.sh"));
        assert!(
            test.contains("  --scoring v2-headline \\\n  --v2-sessions \\\n"),
            "{test}"
        );
        assert!(!test.contains("--stop-on-disqualify"));

        // The instruction states v2's rules, as the grader's settings write
        // them, in place of the base's v1 Scoring section.
        let instruction = read(&task.join("instruction.md"));
        let settings = scoring_text::Settings::of_verifier(&test).unwrap();
        assert!(scoring_text::is_current(&instruction, &settings));
        assert!(instruction.contains("## Scoring\n\nYour bot is graded on 72 hidden scenarios"));
        assert!(instruction.contains("**Losses count 1.5 times.**"));
        assert!(
            !instruction.contains("sum of the hindsight bounds"),
            "v1's text is gone"
        );
        assert_eq!(instruction.matches("## Scoring").count(), 1);
        assert!(instruction.contains("## What's in your environment"));
        // Practice is v2's too: the image grades it with the verifier's
        // scoring, and the instruction says so, with no v1 practice line.
        assert!(instruction.contains(PRACTICE_LINE) && instruction.contains(MTX_LINE));
        assert!(!instruction.contains("the reward needs no reference"));
        assert!(
            read(&task.join("environment/Dockerfile"))
                .contains("ENV MTX_PRACTICE_SCORING=\"--scoring v2-headline --v2-sessions\"")
        );
        let (practice, detail) = state_of(&status(&root, name).unwrap(), "practice");
        assert_eq!(practice, "todo");
        assert!(detail.contains("with the informed bot's"), "{detail}");
        assert!(read(&task.join("README.md")).contains("| Anchors | Scoring v2"));

        // Both anchors' slots, and the ladder naming them.
        let informed = task.join("checks/bots/informed");
        let cargo = read(&informed.join("Cargo.toml"));
        assert!(
            cargo.contains("name = \"mtx-bot-funding-squeeze-informed\""),
            "{cargo}"
        );
        assert!(
            cargo.contains("name = \"mtx-funding-squeeze-informed\""),
            "{cargo}"
        );
        assert!(
            cargo.contains("path = \"../../../../../../crates/kit\""),
            "{cargo}"
        );
        assert!(read(&informed.join("Justfile")).contains("--bin mtx-funding-squeeze-informed"));
        assert_eq!(
            meta.check_bots,
            vec![format!("bench/tasks/{name}/checks/bots/informed")]
        );
        let ladder: Value = serde_json::from_str(&read(&task.join("checks/ladder.json"))).unwrap();
        assert_eq!(ladder["family"], "flash-crash");
        assert_eq!(
            ladder["anchors"],
            json!({ "reference": "reference", "informed": "informed" })
        );
        assert_eq!(ladder["probes"], json!({}));
        let names: Vec<&str> = ladder["rungs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["nop", "blind-long", "blind-short", "reference", "informed"]
        );
        let gate: crate::gates::SignalGate =
            serde_json::from_value(ladder["gates"].clone()).unwrap();
        assert_eq!(gate.informed, ["informed"]);

        // Everything generated is current, the scoring rules included.
        assert!(tasks::stale(&root).unwrap().is_empty());

        // Status: the informed bot is still the reference's copy.
        let steps = status(&root, name).unwrap();
        let (anchors, detail) = state_of(&steps, "anchors");
        assert_eq!(anchors, "todo");
        assert!(detail.contains(INFORMED_TODO), "{detail}");
        // The base's contract still mentions v1's scores: flagged until the
        // author rewrites those lines for v2.
        let (scoring, detail) = state_of(&steps, "scoring");
        assert_eq!(scoring, "todo");
        assert!(detail.contains("still describes v1 scoring"), "{detail}");
        let rewritten: String = instruction
            .lines()
            .filter(|l| {
                let l = l.to_lowercase();
                !l.contains("hindsight bound") && !l.contains("skill")
            })
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(task.join("instruction.md"), &rewritten).unwrap();
        let steps = status(&root, name).unwrap();
        assert_eq!(
            state_of(&steps, "scoring").0,
            "done",
            "{}",
            state_of(&steps, "scoring").1
        );
        assert_eq!(state_of(&steps, "probes").0, "done");

        // Written, it is an anchor.
        let lib = informed.join("src/lib.rs");
        let source = read(&lib);
        let body = source.split_once("//!\n").unwrap().1;
        std::fs::write(&lib, body).unwrap();
        assert!(anchor_problems(&root, name).is_empty());
        let steps = status(&root, name).unwrap();
        assert_eq!(
            state_of(&steps, "anchors").0,
            "done",
            "{}",
            state_of(&steps, "anchors").1
        );

        // The anchors are the task's own bots, or none: a `reference` that
        // runs a blind bet, an informed bot that isn't told the regime, or a
        // verifier grading against another anchor is refused.
        let ladder_path = task.join("checks/ladder.json");
        let tamper = |edit: &dyn Fn(&mut Value)| {
            let mut l = ladder.clone();
            edit(&mut l);
            std::fs::write(&ladder_path, l.to_string()).unwrap();
            let problems = anchor_problems(&root, name);
            std::fs::write(&ladder_path, ladder.to_string()).unwrap();
            problems.join("; ")
        };
        let blind = tamper(&|l| l["anchors"]["reference"] = json!("blind-long"));
        assert!(blind.contains("not the task's reference bot"), "{blind}");
        let elsewhere =
            tamper(&|l| l["rungs"][3]["dir"] = json!("bench/tasks/flash-crash-spcx/reference"));
        assert!(
            elsewhere.contains("not the task's reference bot"),
            "{elsewhere}"
        );
        let untold = tamper(&|l| l["rungs"][4]["tell_regime"] = json!(false));
        assert!(untold.contains("isn't told"), "{untold}");
        let tuned = tamper(&|l| l["rungs"][4]["env"] = json!({ "EDGE": "2" }));
        assert!(tuned.contains("sets `env`"), "{tuned}");
        for i in [3, 4] {
            let breaching = tamper(&|l| l["rungs"][i]["may_breach"] = json!(true));
            assert!(breaching.contains("`may_breach`"), "{breaching}");
        }
        assert!(
            test.contains("--reference /opt/mtx/anchors/funding-squeeze "),
            "{test}"
        );
        std::fs::write(
            task.join("tests/test.sh"),
            test.replace(
                "/opt/mtx/anchors/funding-squeeze ",
                "/opt/mtx/anchors/flash-crash ",
            ),
        )
        .unwrap();
        let verifier = anchor_problems(&root, name).join("; ");
        assert!(
            verifier.contains("--reference /opt/mtx/anchors/funding-squeeze"),
            "{verifier}"
        );
        std::fs::write(task.join("tests/test.sh"), &test).unwrap();
        assert!(anchor_problems(&root, name).is_empty());

        // A verifier setting changed: the rules are stale until synced.
        std::fs::write(
            task.join("tests/test.sh"),
            test.replace(
                "  --v2-sessions \\\n",
                "  --v2-sessions \\\n  --loss-weight 2 \\\n",
            ),
        )
        .unwrap();
        let stale = tasks::stale(&root).unwrap();
        assert_eq!(
            stale,
            [
                PathBuf::from(format!("bench/tasks/{name}/environment/Dockerfile")),
                PathBuf::from(format!("bench/tasks/{name}/instruction.md")),
            ]
        );
        let (scoring, detail) = state_of(&status(&root, name).unwrap(), "scoring");
        assert_eq!(scoring, "todo");
        assert!(detail.contains("stale"), "{detail}");
        tasks::sync(&root).unwrap();
        assert!(read(&task.join("instruction.md")).contains("**Losses count 2 times.**"));
        assert!(read(&task.join("environment/Dockerfile")).contains(
            "ENV MTX_PRACTICE_SCORING=\"--scoring v2-headline --loss-weight 2 --v2-sessions\""
        ));
        assert_eq!(state_of(&status(&root, name).unwrap(), "scoring").0, "done");

        // The gates: a reference in the band passes; v1's untouched.
        let summary: crate::gates::LadderSummary = serde_json::from_value(json!({
            "sessions": 4,
            "totals": [{ "reference": 75.0, "informed": 190.0 }],
            "pnls": [{ "reference": [40.0, -10.0, 25.0, 20.0], "informed": [90.0, 30.0, -20.0, 90.0] }],
            "rules": [], "exploits": [], "disqualified": []
        }))
        .unwrap();
        let checks = crate::gates::v2_checks(
            name,
            meta.mtx.headline,
            &ladder,
            Some(&summary),
            &read(&task.join("tests/test.sh")),
            false,
        );
        assert_eq!(checks[0].id, "strength-band");
        assert_eq!(
            checks[0].status,
            crate::gates::Status::Pass,
            "{}",
            checks[0].detail
        );

        // Opting out of the probe gate takes a reason.
        let mut out = ladder.clone();
        out["probes"] = json!({ "exempt": "" });
        std::fs::write(task.join("checks/ladder.json"), out.to_string()).unwrap();
        assert_eq!(probe_problems(&root, name).len(), 1);
        out["probes"] = json!({ "exempt": "no probe crate for spot yet" });
        std::fs::write(task.join("checks/ladder.json"), out.to_string()).unwrap();
        assert!(probe_problems(&root, name).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn v1_wording_is_found_outside_the_scoring_block_only() {
        let block = scoring_text::block(&scoring_text::Settings::default());
        assert!(
            v1_wording(&block).is_empty(),
            "the generated rules are v2's"
        );
        let text = format!("Intro.\n\n{block}\n\nA skillful bot; `skill_x` too.\n");
        assert!(v1_wording(&text).is_empty());
        let text =
            format!("Your `skill` is PnL over the reference's.\n{block}\nThe Hindsight Bound.\n");
        assert_eq!(v1_wording(&text), ["`skill`", "the hindsight bound"]);
        let test = "mtx grade --scoring v2-headline --v2-sessions --ceiling c.json";
        let problems = scoring_problems(test, &block).join("; ");
        assert!(problems.contains("`--ceiling`"), "{problems}");
    }

    #[test]
    fn a_v1_task_has_no_v2_steps() {
        let steps = status(&repo_root(), "flash-crash-spcx").unwrap();
        for id in ["anchors", "scoring", "probes"] {
            assert!(steps.iter().all(|s| s.id != id), "{id}");
        }
    }

    #[test]
    fn every_step_has_a_guide() {
        for step in STEPS {
            assert!(guide(step).is_some(), "{step}");
        }
    }
}
