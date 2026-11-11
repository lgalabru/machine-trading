//! Guided task authoring: what `mtx task` and the `mtx mcp` tools share.
//!
//! A new task goes through the same steps every time (see the
//! `trading-task-design` skill): a thesis and a design record, a scenario
//! family, a reference bot, an image anchor, practice scenarios, the agent's
//! instruction, the README, CI and the dataset, lint, then the checks that
//! need the image. This module scaffolds a task from the closest existing one,
//! and reads a task's files to say which steps are done and what to do next.
//! Nothing here keeps state of its own: the files are the state.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Family;

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
    fn bot(self) -> &'static str {
        match self {
            Base::Shocks => "reference",
            Base::Making => "maker",
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
            } else if root.join("bots").join(family_of(name)).exists() {
                out.push(format!(
                    "`name` is `{name}`, whose bot directory bots/{} already exists: pick another name.",
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

/// Create bench/tasks/<name> and bots/<family> from the spec's base, and
/// register the pair in the Justfile. Returns what it wrote.
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
        &["practice", "README.md", "DESIGN.md", "instruction.md"],
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
    std::fs::write(&toml_path, toml)?;

    let test_path = to.join("tests/test.sh");
    let test = std::fs::read_to_string(&test_path)?;
    let test = test.replacen(
        "set -euo pipefail\n",
        &format!(
            "set -euo pipefail\n\n# {TODO}(family): until the `{family}` family exists, this grades on the\n# {base}'s family. See `mtx task status {name}`.\n",
            base = base.task()
        ),
        1,
    );
    std::fs::write(&test_path, test)?;

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
    std::fs::write(
        to.join("instruction.md"),
        instruction(spec, &canary, contract),
    )?;
    std::fs::write(to.join("README.md"), readme(spec))?;
    std::fs::write(to.join("DESIGN.md"), design(spec))?;
    written.push(format!("bench/tasks/{name}/ (from {})", base.task()));

    // The reference bot: the base's, under the family's name, so it builds
    // beside it; the oracle is a copy of it.
    let bot_from = root.join("bots").join(base.bot());
    let bot_to = root.join("bots").join(&family);
    copy_dir(&bot_from, &bot_to, &["bin", "target"])?;
    let old_bin = format!("mtx-{}", base.bot());
    let new_bin = format!("mtx-{family}");
    let cargo = std::fs::read_to_string(bot_to.join("Cargo.toml"))?
        .replace(
            &format!("\"mtx-bot-{}\"", base.bot()),
            &format!("\"mtx-bot-{family}\""),
        )
        .replace(&format!("\"{old_bin}\""), &format!("\"{new_bin}\""));
    std::fs::write(bot_to.join("Cargo.toml"), cargo)?;
    let justfile = std::fs::read_to_string(bot_to.join("Justfile"))?
        .replace(&format!("--bin {old_bin}"), &format!("--bin {new_bin}"));
    std::fs::write(bot_to.join("Justfile"), justfile)?;
    std::fs::copy(bot_to.join("src/main.rs"), to.join("solution/src/main.rs"))?;
    written.push(format!(
        "bots/{family}/ (from bots/{}, package mtx-bot-{family})",
        base.bot()
    ));

    // The Justfile's task-to-bot pairs, which `just solution` and the checks read.
    let justfile = root.join("Justfile");
    let just = std::fs::read_to_string(&justfile)?;
    let just = just
        .lines()
        .map(|l| {
            if l.starts_with("references := \"") && !l.contains(&format!("{name}=")) {
                l.trim_end_matches('"').to_string() + &format!(" {name}={family}\"")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&justfile, just)?;
    written.push(format!("Justfile: references += {name}={family}"));

    Ok(json!({ "task": name, "family": family, "written": written, "status": status(root, name)? }))
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

fn instruction(spec: &TaskSpec, canary: &str, contract: &str) -> String {
    format!(
        "{canary}\n{summary}\n\n**You have an hour** (at the standard time limit). When time is up you're stopped, and whatever is in `/app/bot` is graded as it stands, with the `bin/bot` it last built. Keep a working, built bot there all along, and replace it only with a version you've checked is better: an experiment or a parameter sweep still running when time runs out counts for nothing.\n\n## Where it trades\n\n<!-- {TODO}: the market, the snapshot, and the scenario family the hidden sessions are drawn from: every range and odd the grader uses, as distributions, not structure. Regimes: {regimes} -->\n\n{contract}",
        summary = spec.summary.trim(),
        regimes = spec.regimes.trim(),
    )
}

fn readme(spec: &TaskSpec) -> String {
    let name = spec.name.trim();
    format!(
        "# solana-foundation/{name}\n\n{summary}\n\n| | |\n| --- | --- |\n| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |\n| Sessions | {sessions} hidden per grade; <!-- {TODO}: how they split by regime --> practice |\n| Mandate | {cap} position cap; {limit} loss limit |\n| Bound | <!-- {TODO}: what the reward divides by --> |\n| Reference | <!-- {TODO}: its reward, and what it does in one line --> |\n\nHow every task is scored, and how risk is enforced, is in the\n[dataset README](../README.md#how-a-bot-is-judged).\n\n## The skill\n\nA good bot profits here because it {thesis}\n\n- **Wins:** {wins}\n- **Loses:** {loses}\n\n## The market\n\n<!-- {TODO}: the scenario family: what moves, how often, what the agent can observe. Regimes: {regimes} -->\n\n## Scoring\n\n<!-- {TODO}: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->\n\n## Baselines\n\n<!-- {TODO}: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->\n\n## Realism limits\n\n<!-- {TODO}: what the simulation doesn't model. -->\n\n## More\n\n- [DESIGN.md](DESIGN.md): the design record.\n- [instruction.md](instruction.md): what the agent is told.\n",
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

/// The task's bot, from the Justfile's `references`.
fn bot_of(root: &Path, task: &str) -> Option<String> {
    let just = read(&root.join("Justfile"));
    let line = just.lines().find(|l| l.starts_with("references := "))?;
    line.trim_start_matches("references := ")
        .trim_matches('"')
        .split_whitespace()
        .find_map(|pair| pair.strip_prefix(&format!("{task}=")).map(str::to_string))
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
    let family = family_of(task);
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
    let families: Vec<String> = Family::value_variants()
        .iter()
        .filter_map(|f| f.to_possible_value().map(|v| v.get_name().to_string()))
        .collect();
    let exists = families.contains(&family);
    steps.push(Step::new(
        "family",
        "Scenario family",
        exists && draw == family,
        if !exists {
            format!("no `{family}` family in mtx yet (families: {}); test.sh draws `{draw}`", families.join(", "))
        } else if draw != family {
            format!("the `{family}` family exists, but tests/test.sh still draws `{draw}`")
        } else {
            format!("test.sh draws `{family}`")
        },
        if !exists {
            format!("Write the `{family}` family in crates/grader (task_guide step=family), add it to `Family` in crates/cli/src/main.rs and to `mtx grade --draw`, then make tests/test.sh draw it.")
        } else {
            format!("In bench/tasks/{task}/tests/test.sh, draw `--draw {family}` and point --reference at /opt/mtx/anchors/{family}; drop the TODO there.")
        },
    ));

    let bot = bot_of(root, task);
    let solution = read(&dir.join("solution/src/main.rs"));
    let bot_src = bot
        .as_ref()
        .map(|b| read(&root.join("bots").join(b).join("src/main.rs")))
        .unwrap_or_default();
    steps.push(Step::new(
        "reference",
        "Reference bot and oracle",
        bot.is_some() && !bot_src.is_empty() && solution == bot_src,
        match &bot {
            None => "the Justfile's `references` has no bot for this task".into(),
            Some(b) if bot_src.is_empty() => format!("bots/{b}/src/main.rs is missing"),
            Some(b) if solution != bot_src => format!("solution/src/main.rs differs from bots/{b}/src/main.rs"),
            Some(b) => format!("bots/{b}, and the oracle matches it"),
        },
        "Write the reference strategy in its bot (task_guide step=reference), then `just solution` to copy it into the oracle.".into(),
    ));

    let image = read(&root.join("bench/image/Dockerfile"));
    let anchored = bot.as_ref().is_some_and(|b| {
        image.contains(&format!("{family}:{b}")) && image.contains(&format!("-p mtx-bot-{b}"))
    });
    steps.push(Step::new(
        "anchor",
        "The verifier's reference (image anchor)",
        anchored,
        if anchored { format!("the image builds mtx-bot-{} into /opt/mtx/anchors/{family}", bot.clone().unwrap_or_default()) } else { "the image doesn't build this task's reference into /opt/mtx/anchors yet".into() },
        format!(
            "In bench/image/Dockerfile, add `-p mtx-bot-{b}` to the cargo build and `{family}:{b}` to the anchors loop, then `just image`.",
            b = bot.clone().unwrap_or_else(|| family.clone())
        ),
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
    steps.push(Step::new(
        "practice",
        "Practice scenarios, calibrated",
        !practice.is_empty() && calibrated == practice.len(),
        format!("{} practice scenario{}, {calibrated} calibrated", practice.len(), if practice.len() == 1 { "" } else { "s" }),
        format!("Draw a practice set from the family (`mtx scenario generate --family {family} --out bench/tasks/{task}/environment/practice --prefix practice --count 6 --seed <any number>`), then `just calibrate bots/{} <files>`.", bot.clone().unwrap_or_default()),
    ));

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

    let ci = read(&root.join(".github/workflows/ci.yml"));
    let in_ci = bot
        .as_ref()
        .is_some_and(|b| ci.contains(&format!("{task}={b}")));
    let in_dataset = read(&root.join("bench/tasks/dataset.toml"))
        .contains(&format!("solana-foundation/{task}\""));
    steps.push(Step::new(
        "registry",
        "CI and the dataset",
        in_ci && in_dataset,
        format!("CI oracle check: {}; dataset.toml: {}", if in_ci { "yes" } else { "no" }, if in_dataset { "yes" } else { "no" }),
        format!(
            "Add `{task}={}` to the oracle check in .github/workflows/ci.yml; add the task to bench/tasks/dataset.toml (`harbor dataset add solana-foundation/{task}`, or at `just release`).",
            bot.clone().unwrap_or_default()
        ),
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
            "`just image && just harbor-checks` (oracle at skill 1, nop/cheater/sneaky 0), `just ladder {family}`, `just leak-audit`, `harbor check bench/tasks/{task}`, then real agents (task_guide step=verify)."
        ),
    });
    Ok(steps)
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
            json!({ "name": name, "version": version, "family": family_of(&name) })
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
            "A family is a generator `(seed) -> scenario`, in crates/grader: see `ShockFamily` (scenario.rs) for scripted moves and `MakingFamily` (making.rs) for counterparties. Vary what tests the thesis (timing, depth, legs, recovery, duration); include adversarial draws (a move that doesn't revert, a fake-out, a calm session); make the first move unpredictable (mirror it, 50/50); hide the generator's mechanics (uneven steps and gaps, a random tail, small noise); respect protocol physics (Phoenix's oracle band: steps <= 4%). Give it `draw` and `draw_set` (stratified by the family's odds) and a hindsight bound. Register it in `Family` (crates/cli/src/main.rs) and `mtx grade --draw`, and in `mtx scenario generate --family`. Before publishing, read the generator as an adversary: if knowing it alone beats the reference, it leaks. Deeper: SKILL.md step 2, references/ladder-and-ceilings.md."
        }
        "reference" => {
            "The reference sets the 1.0 line: a reasonable, simple strategy, not an optimum, sized to the cap and inside the loss limit on every session. Avoid mechanical biases (a bid always placed before the ask). It runs on the same hidden sessions in every grade, so hidden sets need no calibration; practice scenarios do. Keep the oracle identical: `just solution`. Then build the ladder: nop, a blind bet, a naive strategy, the reference, a ceiling, as strict inequalities (`just ladder <family>`). Deeper: SKILL.md step 3, references/ladder-and-ceilings.md."
        }
        "anchor" => {
            "The verifier grades the reference itself on each hidden set, from /opt/mtx/anchors/<family> in the runtime image. In bench/image/Dockerfile: add `-p mtx-bot-<bot>` to the `cargo build` line and `<family>:<bot>` to the anchors loop. Rebuild with `just image`."
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
            "Add the task to CI's oracle check (`<task>=<bot>` in .github/workflows/ci.yml, mirroring the Justfile's `references`) and to the dataset (bench/tasks/dataset.toml; `just release` pins digests and publishes)."
        }
        "lint" => {
            "`harbor lint bench/tasks/<task> --select ALL --ignore unpinned-image` checks the task's files against Harbor's rules; images are pinned at release."
        }
        "verify" => {
            "On the image: `just image && just harbor-checks` (oracle at skill 1, nop 0, cheater 0, sneaky 0), `just ladder <family>` (the order rule on fresh hidden sets), `just leak-audit` (the agent's image holds no answer), `harbor check bench/tasks/<task>` (an LLM review against Harbor's rubric). Then real agents, k runs each, and `harbor analyze`. Read the winning bots: agents state their edge in doc comments. Treat a too-clean risk profile (worst session near 0, 90%+ profitable) as a leak. Target: the best model's mean reward between 0.2 and 0.7. Deeper: SKILL.md steps 7 and 8."
        }
        _ => return None,
    })
}

pub const STEPS: &[&str] = &[
    "design",
    "family",
    "reference",
    "anchor",
    "practice",
    "instruction",
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

    #[test]
    fn a_scaffold_is_a_task_whose_status_points_at_the_family() {
        let real = repo_root();
        let root =
            std::env::temp_dir().join(format!("mtx-author-{}", uuid::Uuid::new_v4().simple()));
        for dir in ["bench/tasks/flash-crash-spcx", "bots/reference"] {
            copy_dir(
                &real.join(dir),
                &root.join(dir),
                &["bin", "target", "practice"],
            )
            .unwrap();
        }
        std::fs::copy(real.join("Justfile"), root.join("Justfile")).unwrap();
        let out = scaffold(&root, &spec()).unwrap();
        let task = root.join("bench/tasks/funding-squeeze-spcx");
        assert!(
            read(&task.join("task.toml"))
                .contains("name = \"solana-foundation/funding-squeeze-spcx\"")
        );
        assert!(read(&task.join("task.toml")).contains("version = \"0.1.0\""));
        assert!(read(&task.join("instruction.md")).contains("## The bot contract"));
        assert!(read(&task.join("README.md")).contains("harvests the carry"));
        assert!(
            read(&root.join("bots/funding-squeeze/Cargo.toml")).contains("mtx-bot-funding-squeeze")
        );
        assert!(read(&root.join("Justfile")).contains("funding-squeeze-spcx=funding-squeeze"));
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
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn every_step_has_a_guide() {
        for step in STEPS {
            assert!(guide(step).is_some(), "{step}");
        }
    }
}
