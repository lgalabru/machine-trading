//! `mth scenario new`: a coding agent authors a Surfpool scenario from a
//! plain-language request.
//!
//! The agent runs over ACP with the Surfpool MCP server attached, pointed at
//! a surfnet restored from the same snapshot `play` uses, so the prices it
//! reads are the prices the scenario will start from. It writes
//! `scenario.json` into a scratch directory; mth checks it and sends the
//! problems back until it passes or the attempts run out.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde_json::{Value, json};
use surfpool_core::scenarios::TemplateRegistry;
use surfpool_types::{AccountAddress, Scenario};
use tracing::info;

use crate::{
    acp::{AgentClient, McpServer, PermissionPolicy, TurnEvent},
    ui,
};

/// The file the agent writes, relative to its working directory.
const SCENARIO_FILE: &str = "scenario.json";
/// Matches mth-core's market moves: Phoenix rejects oracle updates more than
/// 5% from the last one, so each step stays inside 4%.
const MAX_STEP_PCT: f64 = 4.0;
const MIN_SLOTS_BETWEEN_STEPS: u64 = 2;
const MARKET_MOVE: &str = "phoenix-market-move";
const MARKET_UNCROSS: &str = "phoenix-market-uncross";
/// Surfnets here run 250 ms slots, as Surfpool does by default.
pub const SLOTS_PER_SECOND: u64 = 4;

/// Coding agents mth can drive, by their Harbor names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Agent {
    ClaudeCode,
    Codex,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    /// The ACP adapter's executable and the npm package that ships it.
    fn adapter(self) -> (&'static str, &'static str) {
        match self {
            Self::ClaudeCode => ("claude-agent-acp", "@agentclientprotocol/claude-agent-acp"),
            Self::Codex => ("codex-acp", "@agentclientprotocol/codex-acp"),
        }
    }

    /// The adapter from `PATH` if installed, otherwise through `npx`.
    /// `model` takes Harbor's `provider/model` form or a bare model name.
    fn command(self, model: Option<&str>) -> Command {
        let (program, package) = self.adapter();
        let mut command = if on_path(program) {
            Command::new(program)
        } else {
            let mut npx = Command::new("npx");
            npx.args(["-y", package]);
            npx
        };
        if let Some(model) = model {
            let model = model.rsplit('/').next().unwrap_or(model);
            match self {
                Self::ClaudeCode => {
                    command.env("ANTHROPIC_MODEL", model);
                }
                Self::Codex => {
                    command.args(["-c", &format!("model=\"{model}\"")]);
                }
            }
        }
        command
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// What the scenario starts from: the market it trades and its mark on the
/// surfnet the agent can read.
#[derive(Debug, Clone)]
pub struct Market {
    pub symbol: String,
    pub mark_ticks: u64,
    pub mark_usd: f64,
    pub perp_asset_map: String,
    pub surfnet_port: u16,
}

pub struct AuthorOptions<'a> {
    pub agent: Agent,
    pub model: Option<&'a str>,
    pub request: &'a str,
    pub market: &'a Market,
    /// The `surfpool` binary that serves the MCP (`surfpool mcp`).
    pub surfpool: &'a str,
    /// How many times the agent may hand in a scenario that fails the checks.
    pub attempts: usize,
    /// Stream the agent's replies and the adapter's own logs to stderr.
    pub verbose: bool,
}

/// Have the agent write a scenario for `opts.request`, and return it once it
/// passes [`check`].
pub fn author(opts: AuthorOptions<'_>) -> Result<Scenario> {
    let workdir = std::env::temp_dir().join(format!("mth-author-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&workdir).with_context(|| workdir.display().to_string())?;
    let scenario_path = workdir.join(SCENARIO_FILE);
    let log_path = workdir.join("agent.log");
    info!(agent = ?opts.agent, workdir = %workdir.display(), "starting the agent");

    let name = opts.agent.name();
    ui::start(format!("Starting {name}"));
    let stderr = if opts.verbose {
        Stdio::inherit()
    } else {
        std::fs::File::create(&log_path)
            .with_context(|| log_path.display().to_string())?
            .into()
    };
    let client = AgentClient::spawn_with_stderr(
        opts.agent.command(opts.model),
        PermissionPolicy::AllowAll,
        stderr,
    )
    .with_context(|| {
        let (program, package) = opts.agent.adapter();
        format!("starting {program}; install it with `npm install -g {package}`")
    })?;
    let agent = client.initialize().context("ACP initialize")?;
    let surfpool = McpServer {
        name: "surfpool".into(),
        command: opts.surfpool.into(),
        args: vec!["mcp".into()],
    };
    let session = client
        .new_session(&workdir, &[surfpool])
        .context("ACP session/new")?;
    let adapter = agent
        .agent_info
        .map(|info| format!("{} {}", info.name, info.version.unwrap_or_default()))
        .unwrap_or_else(|| opts.agent.adapter().0.into());
    ui::done(format!("{name} connected {}", ui::dim(adapter.trim())));

    let registry = TemplateRegistry::new();
    let mut prompt = instructions(opts.request, opts.market);
    let attempts = opts.attempts.max(1);
    for attempt in 1..=attempts {
        let task = if attempt == 1 {
            format!(
                "Authoring {}",
                ui::bold(format!("\u{201c}{}\u{201d}", opts.request))
            )
        } else {
            format!(
                "Fixing the scenario {}",
                ui::dim(format!("attempt {attempt}/{attempts}"))
            )
        };
        ui::start(&task);
        run_turn(&client, &session, &prompt, &task, opts.verbose).inspect_err(|_| {
            ui::fail(format!(
                "{name} stopped; its log is in {}",
                log_path.display()
            ));
        })?;
        let problems = match load(&scenario_path) {
            Ok(scenario) => {
                let problems = check(&scenario, &registry, opts.market);
                if problems.is_empty() {
                    info!(attempt, name = scenario.name, "scenario accepted");
                    let span = scenario
                        .overrides
                        .iter()
                        .map(|o| o.scenario_relative_slot)
                        .max()
                        .unwrap_or(0);
                    ui::done(format!(
                        "Authored {} {}",
                        ui::bold(&scenario.name),
                        ui::dim(format!(
                            "{} overrides over {}",
                            scenario.overrides.len(),
                            ui::clock(span / SLOTS_PER_SECOND)
                        ))
                    ));
                    if !scenario.description.is_empty() {
                        ui::note(&scenario.description);
                    }
                    return Ok(scenario);
                }
                problems
            }
            Err(error) => vec![format!("{error:#}")],
        };
        info!(attempt, problems = problems.len(), "scenario rejected");
        ui::fail(format!(
            "Attempt {attempt} rejected: {} problem{}",
            problems.len(),
            if problems.len() == 1 { "" } else { "s" }
        ));
        for problem in &problems {
            ui::note(problem);
        }
        prompt = format!(
            "mth rejected ./{SCENARIO_FILE}:\n{}\n\nFix the file, then reply with a one-line summary.",
            problems
                .iter()
                .map(|p| format!("- {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    bail!(
        "the agent's scenario still fails the checks after {attempts} attempts; its last try is in {}",
        scenario_path.display()
    )
}

/// One prompt turn. The spinner follows the agent's tool calls; with
/// `verbose`, its replies stream to stderr too.
fn run_turn(
    client: &AgentClient,
    session: &str,
    prompt: &str,
    task: &str,
    verbose: bool,
) -> Result<()> {
    let mut announced = HashSet::new();
    for event in client.prompt(session, prompt)? {
        match event {
            TurnEvent::Text(chunk) if verbose => eprint!("{chunk}"),
            TurnEvent::Text(_) | TurnEvent::Thought(_) => {}
            TurnEvent::ToolCall { id, title, .. } => {
                if let Some(title) = title
                    && announced.insert(id)
                {
                    if verbose {
                        eprintln!("\n  ▸ {title}");
                    }
                    ui::update(format!("{task} {}", ui::dim(activity(&title))));
                }
            }
            TurnEvent::Done(reason) => {
                if verbose {
                    eprintln!();
                }
                info!(?reason, "agent turn ended");
                return Ok(());
            }
            TurnEvent::Failed(error) => return Err(error).context("agent turn failed"),
        }
    }
    Ok(())
}

/// What a tool call means, in a few words.
fn activity(title: &str) -> String {
    let tool = title.trim_start_matches("mcp__surfpool__");
    let words = match tool {
        "get_override_templates" => "listing override templates",
        "get_override_template" => "reading a template",
        "list_phoenix_markets" => "reading Phoenix markets",
        "search_constant_options" => "looking up constants",
        "call_surfnet_rpc" => "querying the surfnet",
        "ToolSearch" => "loading tools",
        "Terminal" | "Bash" => "running a script",
        _ if tool.starts_with("Write") || tool.starts_with("Edit") => "writing scenario.json",
        _ if tool.starts_with("Read") => "reading a file",
        _ => return tool.replace('_', " ").to_lowercase(),
    };
    words.into()
}

fn instructions(request: &str, market: &Market) -> String {
    let Market {
        symbol,
        mark_ticks,
        mark_usd,
        perp_asset_map,
        surfnet_port,
    } = market;
    format!(
        r#"You are authoring a market scenario for the Machine Trading harness. A scenario is a list of Surfpool account overrides, each applied a number of slots after the scenario starts. mth will replay it on a surfnet and chart the market as it plays.

The request: {request}

A surfnet restored from mainnet is running at http://127.0.0.1:{surfnet_port}. Its {symbol} perp is marked at {mark_ticks} ticks (${mark_usd:.2}); the scenario starts from that mark. The surfpool MCP server is attached:
- `get_override_templates`, then `get_override_template` for each template you use: follow its properties and its llmContext.
- `list_phoenix_markets` with surfnetPort {surfnet_port}, for each market's markTicks, tickSize and baseLotDecimals.
Only read from the surfnet: don't call `create_scenario`, don't register the scenario, and don't send cheatcodes with `call_surfnet_rpc`. mth registers it.

Write the scenario to ./{SCENARIO_FILE}, in Surfpool's Scenario format:
{{
  "id": "<uuid v4>", "name": "<short title>", "description": "<what happens, in a sentence or two>", "tags": ["..."],
  "overrides": [
    {{ "id": "<uuid v4>", "templateId": "{MARKET_MOVE}", "values": {{ "symbol": "{symbol}", "target_ticks": "<decimal string>" }},
       "scenarioRelativeSlot": 10, "label": "<what this step does>", "enabled": true, "fetchBeforeUse": false,
       "account": {{ "pubkey": "{perp_asset_map}" }} }}
  ]
}}

Rules mth checks:
- A slot is 250 ms. Keep the whole scenario within 1,200 slots (5 minutes) unless the request asks for longer.
- Phoenix rejects oracle prices more than 5% from the last one: move the price with `{MARKET_MOVE}` in steps of at most {MAX_STEP_PCT}% from the previous level, at least {MIN_SLOTS_BETWEEN_STEPS} slots apart. Ramp bigger moves over several steps.
- After the last step of each move, add `{MARKET_UNCROSS}` for the same symbol, in the same slot or later, so resting orders the move crossed get matched.
- Phoenix market templates take the PerpAssetMap account {perp_asset_map}. Keep fetchBeforeUse false.
- Every override has a short label: the viewer shows them on the timeline.
Make the path look like a real market unless asked otherwise: uneven steps and pauses, partial rebounds, no perfectly regular ramps.

When ./{SCENARIO_FILE} is written, reply with a short summary of the scenario."#
    )
}

/// Read a scenario file, filling in the fields Surfpool requires but an
/// author can leave out (ids, `enabled`, tags).
pub fn load(path: &Path) -> Result<Scenario> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("{} was not written", path.display()))?;
    let mut value: Value = serde_json::from_str(&contents)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    normalize(&mut value);
    serde_json::from_value(value)
        .with_context(|| format!("{} is not a Surfpool scenario", path.display()))
}

fn normalize(value: &mut Value) {
    let Some(scenario) = value.as_object_mut() else {
        return;
    };
    let uuid = || Value::String(uuid::Uuid::new_v4().to_string());
    scenario.entry("id").or_insert_with(uuid);
    scenario.entry("description").or_insert_with(|| json!(""));
    scenario.entry("tags").or_insert_with(|| json!([]));
    if let Some(overrides) = scenario.get_mut("overrides").and_then(Value::as_array_mut) {
        for o in overrides.iter_mut().filter_map(Value::as_object_mut) {
            o.entry("id").or_insert_with(uuid);
            o.entry("enabled").or_insert_with(|| json!(true));
        }
    }
}

/// [`load`] a scenario file and [`check`] it against `market`.
pub fn load_checked(path: &Path, market: &Market) -> Result<Scenario> {
    let scenario = load(path)?;
    let problems = check(&scenario, &TemplateRegistry::new(), market);
    if !problems.is_empty() {
        bail!(
            "{} fails the checks:\n- {}",
            path.display(),
            problems.join("\n- ")
        );
    }
    Ok(scenario)
}

/// Everything wrong with `scenario`, in words an agent can act on. Empty
/// means it can be registered.
pub fn check(scenario: &Scenario, registry: &TemplateRegistry, market: &Market) -> Vec<String> {
    let mut problems = Vec::new();
    if scenario.name.trim().is_empty() {
        problems.push("the scenario has no name".into());
    }
    let enabled: Vec<_> = scenario.overrides.iter().filter(|o| o.enabled).collect();
    if enabled.is_empty() {
        problems.push("the scenario has no enabled overrides".into());
    }
    let mut ids = HashSet::new();
    for o in &scenario.overrides {
        if !ids.insert(&o.id) {
            problems.push(format!("override id {} is used more than once", o.id));
        }
    }
    for o in &enabled {
        let at = describe(o);
        let Some(template) = registry.get(&o.template_id) else {
            problems.push(format!("{at}: no template {}", o.template_id));
            continue;
        };
        let known: HashSet<&str> = template
            .properties
            .iter()
            .map(|p| p.path.as_str())
            .collect();
        for key in o.values.keys() {
            if !known.contains(key.as_str()) {
                problems.push(format!(
                    "{at}: {} has no property {key} (it has: {})",
                    o.template_id,
                    template
                        .properties
                        .iter()
                        .map(|p| p.path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        if o.fetch_before_use && o.template_id.starts_with("phoenix-") {
            problems.push(format!(
                "{at}: fetchBeforeUse must be false for Phoenix templates"
            ));
        }
        if let (AccountAddress::Pubkey(expected), AccountAddress::Pubkey(actual)) =
            (&template.address, &o.account)
            && expected != actual
        {
            problems.push(format!(
                "{at}: {} applies to account {expected}, not {actual}",
                o.template_id
            ));
        }
    }
    problems.extend(check_moves(&enabled, market));
    problems
}

/// Price moves on the scenario's market: valid ticks, steps Phoenix
/// accepts, and an uncross after the last one.
fn check_moves(overrides: &[&surfpool_types::OverrideInstance], market: &Market) -> Vec<String> {
    let symbol_of = |o: &surfpool_types::OverrideInstance| {
        o.values
            .get("symbol")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let mut moves: Vec<_> = overrides
        .iter()
        .filter(|o| o.template_id == MARKET_MOVE && symbol_of(o).as_deref() == Some(&market.symbol))
        .collect();
    moves.sort_by_key(|o| o.scenario_relative_slot);

    let mut problems = Vec::new();
    let mut level = market.mark_ticks as f64;
    let mut last_slot: Option<u64> = None;
    for o in &moves {
        let at = describe(o);
        let Some(ticks) = target_ticks(o) else {
            problems.push(format!(
                "{at}: target_ticks must be a positive integer as a decimal string"
            ));
            continue;
        };
        let step_pct = (ticks as f64 / level - 1.0) * 100.0;
        if step_pct.abs() > MAX_STEP_PCT + 0.05 {
            problems.push(format!(
                "{at}: a {step_pct:+.2}% step from {level:.0} to {ticks} ticks; split it into steps of at most {MAX_STEP_PCT}%"
            ));
        }
        if let Some(previous) = last_slot
            && o.scenario_relative_slot < previous + MIN_SLOTS_BETWEEN_STEPS
        {
            problems.push(format!(
                "{at}: {} slot(s) after the previous step; space steps at least {MIN_SLOTS_BETWEEN_STEPS} slots apart",
                o.scenario_relative_slot - previous
            ));
        }
        level = ticks as f64;
        last_slot = Some(o.scenario_relative_slot);
    }
    if let Some(last) = last_slot {
        let uncrossed = overrides.iter().any(|o| {
            o.template_id == MARKET_UNCROSS
                && symbol_of(o).as_deref() == Some(&market.symbol)
                && o.scenario_relative_slot >= last
        });
        if !uncrossed {
            problems.push(format!(
                "no {MARKET_UNCROSS} for {} at or after the last move (slot {last})",
                market.symbol
            ));
        }
    }
    problems
}

pub fn target_ticks(o: &surfpool_types::OverrideInstance) -> Option<u64> {
    o.values
        .get("target_ticks")
        .and_then(Value::as_str)
        .and_then(|s| s.trim().parse().ok())
        .filter(|&t| t > 0)
}

fn describe(o: &surfpool_types::OverrideInstance) -> String {
    match &o.label {
        Some(label) => format!(
            "override {} ({label}, slot {})",
            o.id, o.scenario_relative_slot
        ),
        None => format!("override {} (slot {})", o.id, o.scenario_relative_slot),
    }
}

/// Write `scenario` to `path`, refusing to replace a file unless `force`.
pub fn save(scenario: &Scenario, path: &PathBuf, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!("{} exists; pass --force to replace it", path.display());
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    }
    let contents = serde_json::to_string_pretty(scenario)? + "\n";
    std::fs::write(path, contents).with_context(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERP_ASSET_MAP: &str = "2nHGAaEw3D5dd4hVueaUNoygkQFmoeKqRQWnSPqSMFUC";

    fn market() -> Market {
        Market {
            symbol: "SPCX".into(),
            mark_ticks: 10_000,
            mark_usd: 143.0,
            perp_asset_map: PERP_ASSET_MAP.into(),
            surfnet_port: 8899,
        }
    }

    fn scenario(overrides: Value) -> Scenario {
        let mut value = json!({ "name": "crash", "overrides": overrides });
        normalize(&mut value);
        serde_json::from_value(value).unwrap()
    }

    fn step(slot: u64, template: &str, ticks: Option<&str>) -> Value {
        let mut values = json!({ "symbol": "SPCX" });
        if let Some(ticks) = ticks {
            values["target_ticks"] = json!(ticks);
        }
        json!({
            "templateId": template, "values": values, "scenarioRelativeSlot": slot,
            "label": "step", "account": { "pubkey": PERP_ASSET_MAP },
        })
    }

    #[test]
    fn a_ramped_crash_passes() {
        let s = scenario(json!([
            step(4, MARKET_MOVE, Some("9700")),
            step(8, MARKET_MOVE, Some("9350")),
            step(11, MARKET_MOVE, Some("9100")),
            step(11, MARKET_UNCROSS, None),
        ]));
        assert_eq!(
            check(&s, &TemplateRegistry::new(), &market()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn big_steps_tight_spacing_and_no_uncross_are_reported() {
        let s = scenario(json!([
            step(4, MARKET_MOVE, Some("9000")),
            step(5, MARKET_MOVE, Some("8800")),
        ]));
        let problems = check(&s, &TemplateRegistry::new(), &market());
        assert_eq!(problems.len(), 3, "{problems:#?}");
        assert!(problems[0].contains("-10.00% step"));
        assert!(problems[1].contains("1 slot(s) after"));
        assert!(problems[2].contains("no phoenix-market-uncross"));
    }

    #[test]
    fn unknown_templates_properties_and_accounts_are_reported() {
        let mut wrong_account = step(4, MARKET_UNCROSS, None);
        wrong_account["account"] = json!({ "pubkey": "11111111111111111111111111111111" });
        let mut extra = step(6, MARKET_UNCROSS, None);
        extra["values"]["price"] = json!("1");
        let s = scenario(json!([
            step(2, "no-such-template", None),
            wrong_account,
            extra
        ]));
        let problems = check(&s, &TemplateRegistry::new(), &market());
        assert_eq!(problems.len(), 3, "{problems:#?}");
        assert!(problems[0].contains("no template no-such-template"));
        assert!(problems[1].contains("applies to account"));
        assert!(problems[2].contains("no property price"));
    }

    #[test]
    fn models_reach_each_adapter_in_its_own_way() {
        let claude = Agent::ClaudeCode.command(Some("anthropic/claude-sonnet-5-5"));
        let model = claude
            .get_envs()
            .find(|(k, _)| *k == "ANTHROPIC_MODEL")
            .and_then(|(_, v)| v);
        assert_eq!(model, Some(std::ffi::OsStr::new("claude-sonnet-5-5")));

        let codex = Agent::Codex.command(Some("gpt-5"));
        let args: Vec<_> = codex.get_args().collect();
        assert_eq!(args[args.len() - 2..], ["-c", "model=\"gpt-5\""]);
    }

    #[test]
    fn adapters_not_on_path_run_through_npx() {
        let command = Agent::Codex.command(None);
        if command.get_program() == "npx" {
            let args: Vec<_> = command.get_args().collect();
            assert_eq!(args, ["-y", "@agentclientprotocol/codex-acp"]);
        } else {
            assert_eq!(command.get_program(), "codex-acp");
        }
    }

    #[test]
    fn tool_calls_read_as_activities() {
        assert_eq!(
            activity("mcp__surfpool__get_override_template"),
            "reading a template"
        );
        assert_eq!(
            activity("mcp__surfpool__list_phoenix_markets"),
            "reading Phoenix markets"
        );
        assert_eq!(
            activity("Write /tmp/x/scenario.json"),
            "writing scenario.json"
        );
        assert_eq!(activity("Some_New_Tool"), "some new tool");
    }

    #[test]
    fn normalize_fills_what_authors_leave_out() {
        let mut value = json!({ "name": "n", "overrides": [{ "templateId": "t" }] });
        normalize(&mut value);
        assert!(value["id"].as_str().is_some_and(|id| id.len() == 36));
        assert_eq!(value["description"], "");
        assert_eq!(value["tags"], json!([]));
        assert!(value["overrides"][0]["id"].is_string());
        assert_eq!(value["overrides"][0]["enabled"], true);

        // What the author did write stays.
        let mut value = json!({ "id": "mine", "overrides": [{ "id": "o", "enabled": false }] });
        normalize(&mut value);
        assert_eq!(value["id"], "mine");
        assert_eq!(value["overrides"][0]["enabled"], false);
    }

    #[test]
    fn the_prompt_carries_the_market_and_the_rules() {
        let prompt = instructions("simulate a crash", &market());
        for expected in [
            "simulate a crash",
            "http://127.0.0.1:8899",
            "10000 ticks ($143.00)",
            PERP_ASSET_MAP,
            "at most 4%",
            "./scenario.json",
        ] {
            assert!(prompt.contains(expected), "missing {expected:?}");
        }
    }

    #[test]
    fn load_checked_reports_missing_files_and_failing_checks() {
        let dir = std::env::temp_dir().join(format!("mth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = load_checked(&dir.join("nope.json"), &market()).unwrap_err();
        assert!(format!("{missing:#}").contains("was not written"));

        let path = dir.join("scenario.json");
        let bad = json!({ "name": "crash", "overrides": [step(4, MARKET_MOVE, Some("9000"))] });
        std::fs::write(&path, bad.to_string()).unwrap();
        let error = load_checked(&path, &market()).unwrap_err().to_string();
        assert!(
            error.contains("fails the checks") && error.contains("-10.00% step"),
            "{error}"
        );

        let good = json!({ "name": "crash", "overrides": [
            step(4, MARKET_MOVE, Some("9700")),
            step(4, MARKET_UNCROSS, None),
        ] });
        std::fs::write(&path, good.to_string()).unwrap();
        assert_eq!(load_checked(&path, &market()).unwrap().overrides.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_refuses_to_replace_without_force() {
        let dir = std::env::temp_dir().join(format!("mth-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested/scenario.json");
        let s = scenario(json!([step(4, MARKET_UNCROSS, None)]));
        save(&s, &path, false).unwrap();
        assert!(save(&s, &path, false).is_err());
        save(&s, &path, true).unwrap();
        assert_eq!(load(&path).unwrap(), s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn numeric_ticks_are_rejected() {
        let mut s = step(4, MARKET_MOVE, None);
        s["values"]["target_ticks"] = json!(9700);
        let s = scenario(json!([s, step(4, MARKET_UNCROSS, None)]));
        let problems = check(&s, &TemplateRegistry::new(), &market());
        assert!(problems[0].contains("decimal string"), "{problems:#?}");
    }
}
