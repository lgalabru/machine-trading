use std::{io::IsTerminal, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mtx_grader::{
    carry::CarryFamily,
    grade::{self, GradeOptions},
    making::MakingFamily,
    phoenix,
    scenario::{Scenario, ShockFamily},
    snapshot,
    weekend::WeekendFamily,
};
use tracing_subscriber::EnvFilter;

/// Every session starts and drops a whole surfnet, hundreds of megabytes on a
/// few hundred threads. glibc's allocator keeps the memory a session freed in
/// per-thread arenas, so a verifier grading 144 sessions grows past its
/// limit; mimalloc returns it.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod acp;
mod author;
mod authoring;
mod bot;
mod charts;
mod mcp;
mod play;
mod publish;
mod review;
mod ui;

/// The snapshot `scenario new` and `play` start from: the checkout's, by
/// absolute path, so an installed `mtx` finds it from any directory.
const DEFAULT_SNAPSHOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../snapshots/phoenix-spcx-2026-10-29"
);

/// Tooling for the mtx Solana trading benchmark. Results go to stdout as JSON,
/// logs to stderr.
#[derive(Parser)]
#[command(name = "mtx", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The scenario families, one per task.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum Family {
    /// flash-crash-spcx: shocks that crash or squeeze, then revert or not.
    FlashCrash,
    /// market-making-spcx: a walking, trending, jumping market, competing
    /// makers, and taker flow: noise, fake-out bursts, and informed orders
    /// ahead of most jumps.
    MarketMaking,
    /// funding-carry-spcx: book premiums over the index, some the crowd's
    /// positioning opened and some the index follows, and Phoenix's funding.
    FundingCarry,
    /// weekend-gap-spcx: a weekend with SPCX's own market closed, flow that
    /// may know where it reopens, and the reopen.
    WeekendGap,
}

#[derive(Subcommand)]
enum Command {
    /// Pinned chain state for offline tasks.
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Market scenarios: the price paths sessions replay.
    #[command(subcommand)]
    Scenario(ScenarioCommand),
    /// Make a new task, guided: scaffold it, then see what's left to do.
    #[command(subcommand)]
    Task(TaskCommand),
    /// Serve the task-authoring tools over MCP (stdio), for Claude Code:
    /// task_new (an interview with forms), task_status, task_guide, task_list.
    Mcp,
    /// Bench results: what `just bench` and Harbor runs leave behind.
    #[command(subcommand)]
    Bench(BenchCommand),
    /// Write your own bot: start one, practise it on a task, package it.
    #[command(subcommand)]
    Bot(BotCommand),
    /// Run a bot through scenarios and score it against the reference PnL.
    Grade {
        #[command(flatten)]
        sessions: Sessions,
        /// Draw the scenarios from this family instead of reading files: a
        /// hidden set, seeded by the secret in `MTX_EVAL_SEED` and never
        /// written anywhere. Reports carry only the secret's SHA-256.
        #[arg(long, value_enum, conflicts_with = "scenarios")]
        draw: Option<Family>,
        /// How many scenarios to draw, stratified as the family's odds.
        #[arg(long, default_value_t = 12, requires = "draw")]
        count: usize,
        /// Run this reference bot on the same scenarios first, and score
        /// against its PnL rather than what the scenario files carry.
        #[arg(long)]
        reference: Option<PathBuf>,
        /// Reference sessions to run at once (1-4).
        #[arg(long, default_value_t = 4)]
        anchor_jobs: usize,
        /// Hand the bot each drawn session's regime as MTX_REGIME: the
        /// shock's shape (flash-crash), or the informed taker orders
        /// (market-making). For the ladder's oracles, never a graded bot.
        #[arg(long, requires = "draw", hide = true)]
        tell_regime: bool,
        /// Stop at the bot's first disqualifying session (the grade is 0
        /// whatever the rest do); the report covers the sessions up to it.
        #[arg(long)]
        stop_on_disqualify: bool,
        /// Write Harbor's reward.json here.
        #[arg(long)]
        reward: Option<PathBuf>,
        /// Write the full per-scenario report here (it also goes to stdout).
        #[arg(long)]
        report: Option<PathBuf>,
        /// Write each session step by step here, for `mtx bench review`:
        /// `bot/<scenario>.json.gz` and `reference/<scenario>.json.gz`.
        #[arg(long)]
        details: Option<PathBuf>,
    },
    /// Run the reference bot through scenarios and save its PnL in each file.
    Calibrate {
        #[command(flatten)]
        sessions: Sessions,
    },
}

#[derive(Subcommand)]
enum BotCommand {
    /// Make a bot that connects, watches the market and never trades: the
    /// plumbing works, the strategy is yours.
    New {
        /// Where to make it: an empty or new directory, outside this checkout.
        dir: PathBuf,
        /// Depend on the bot SDK by git, at the commit this mtx was built
        /// from, instead of by path to this checkout.
        #[arg(long)]
        git: bool,
        /// The machine-trading checkout to use (default: the one this mtx was
        /// built from).
        #[arg(long, env = "MTX_REPO")]
        repo: Option<PathBuf>,
    },
    /// Build a bot with its build.sh, then grade it on a task's practice
    /// scenarios, exactly as the verifier grades the hidden ones.
    Practice {
        /// The bot directory.
        #[arg(long, default_value = ".")]
        bot: PathBuf,
        /// The task to practise, as named in bench/tasks (default: the
        /// flash-crash task).
        #[arg(long)]
        task: Option<String>,
        /// Grade bin/bot as it is, without running build.sh.
        #[arg(long)]
        no_build: bool,
        /// Sessions to run at once, each on its own surfnet (1-4).
        #[arg(long, env = "MTX_JOBS", default_value_t = 1)]
        jobs: usize,
        /// Print the grader's full JSON report instead of a summary.
        #[arg(long)]
        json: bool,
        /// The machine-trading checkout to use (default: the one this mtx was
        /// built from).
        #[arg(long, env = "MTX_REPO")]
        repo: Option<PathBuf>,
        /// Scenario files to grade on instead of the task's practice set,
        /// such as ones `mtx scenario generate` drew.
        scenarios: Vec<PathBuf>,
    },
    /// Check a bot directory against the bot contract's shape: run.sh, an
    /// executable bin/bot, built from the current source.
    Check {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Package a bot's source (no builds, no logs) into one archive, with a
    /// manifest naming its task and the mtx it was made with.
    Pack {
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The task the bot is for, as named in bench/tasks (default: the
        /// flash-crash task).
        #[arg(long)]
        task: Option<String>,
        /// The machine-trading checkout to use (default: the one this mtx was
        /// built from).
        #[arg(long, env = "MTX_REPO")]
        repo: Option<PathBuf>,
        /// The archive to write.
        #[arg(long, short, default_value = "bot.tar.gz")]
        out: PathBuf,
    },
}

#[derive(clap::Args)]
struct Sessions {
    /// Snapshot directory the sessions start from.
    #[arg(long)]
    snapshot: PathBuf,
    /// Bot directory: `run.sh` is its entry point.
    #[arg(long)]
    bot: PathBuf,
    /// Bot output goes to `<logs>/<scenario>.log`.
    #[arg(long, default_value = "logs")]
    logs: PathBuf,
    /// Sessions to run at once, each on its own surfnet (1-4). A machine
    /// that can't keep up fails the run on late slots rather than skewing it.
    #[arg(long, env = "MTX_JOBS", default_value_t = 1)]
    jobs: usize,
    /// The first bot user (0-3) sessions run their bots as, when run as
    /// root; they use `--jobs` of them from there. Graders running at once
    /// on one machine need ranges that don't overlap: a session's end kills
    /// every process of its bot user.
    #[arg(long, default_value_t = 0)]
    first_bot_user: usize,
    /// Scenario files, run in order.
    scenarios: Vec<PathBuf>,
}

impl Sessions {
    fn load(&self) -> Result<Vec<(PathBuf, Scenario)>> {
        self.scenarios
            .iter()
            .map(|path| Ok((path.clone(), Scenario::load(path)?)))
            .collect()
    }
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Scaffold bench/tasks/<name> and its reference bot from the closest
    /// existing task. Every field the interview asks is a flag; leave any out
    /// and mtx lists what's missing.
    New {
        #[command(flatten)]
        spec: TaskArgs,
    },
    /// Where a task stands: each step to publication, done or not, with what
    /// to do next.
    Status { task: String },
    /// The procedure for one step (design, family, reference, anchor,
    /// practice, instruction, readme, registry, lint, verify).
    Guide { step: String },
    /// The repository's tasks.
    List,
}

#[derive(clap::Args)]
struct TaskArgs {
    /// The task's id, the skill then the instrument: funding-squeeze-spcx.
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    title: Option<String>,
    /// One sentence: what the agent's bot does, on which market.
    #[arg(long)]
    summary: Option<String>,
    /// Start from `shocks` (flash-crash-spcx) or `making`
    /// (market-making-spcx).
    #[arg(long, value_enum)]
    base: Option<authoring::Base>,
    /// A good bot profits here because it ___.
    #[arg(long)]
    thesis: Option<String>,
    #[arg(long)]
    wins: Option<String>,
    #[arg(long)]
    loses: Option<String>,
    #[arg(long)]
    regimes: Option<String>,
    /// Position cap in SPCX.
    #[arg(long)]
    max_position: Option<f64>,
    /// Loss limit per session, USD.
    #[arg(long)]
    max_drawdown_usd: Option<f64>,
    #[arg(long)]
    hidden_sessions: Option<u32>,
}

#[derive(Subcommand)]
enum BenchCommand {
    /// Browse a bench's results in a web UI: the leaderboard, each trial's
    /// sessions against the reference's, every session step by step, the
    /// agent's trajectory and the bot it wrote.
    Review {
        /// A batch (jobs/bench/<batch>), a Harbor job, one trial, or a
        /// published release's version (bench/results/<version>).
        path: String,
        /// Server port; 0 picks a free one.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Don't open a browser.
        #[arg(long)]
        no_open: bool,
        /// Where published releases are; defaults to this checkout's
        /// bench/results.
        #[arg(long)]
        results: Option<PathBuf>,
    },
    /// Publish a finished batch as a release, small enough to commit:
    /// bench/results/<version>. It keeps the reports, the session detail
    /// files, the bot's source and the agent's trajectory (gzipped); never
    /// the batch's seed, and nothing that holds it or an API key.
    Publish {
        /// The batch (jobs/bench/<batch>).
        batch: PathBuf,
        /// The release's name: the dataset version it ran, as in 0.9.0.
        version: String,
        /// Replace a release already published under that name.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        results: Option<PathBuf>,
    },
    /// Write every published release as a static site: the review page and
    /// its data, for GitHub Pages. The browser inflates the gzipped files.
    Export {
        /// The site's directory, as in specs/site/_site/bench.
        out: PathBuf,
        /// Where the site is served, as in
        /// https://lgalabru.github.io/machine-trading: link previews (og:
        /// tags) point at <site>/social.jpg.
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        results: Option<PathBuf>,
    },
    /// Compare two releases (or batches), task by task: each model's mean
    /// skill on both, and the change. As in `mtx bench compare 0.9.0...0.8.0`.
    Compare {
        /// <a>...<b>: versions or paths.
        range: String,
        /// Open the comparison in the review page too.
        #[arg(long)]
        open: bool,
        /// Server port for --open; 0 picks a free one.
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long)]
        results: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ScenarioCommand {
    /// Draw scenarios from a task's family (`--family`), one file per
    /// scenario.
    /// Sets are stratified: each kind of session (a shock's direction and
    /// aftermath, a market's regime) appears in proportion to its odds.
    Generate {
        #[arg(long)]
        out: PathBuf,
        /// Names are `<prefix>-<n>`.
        #[arg(long)]
        prefix: String,
        #[arg(long, default_value_t = 3)]
        count: usize,
        /// Mixed into each scenario's seed. Keep hidden sets' seeds private.
        #[arg(long)]
        seed: u64,
        /// Which task's family to draw from.
        #[arg(long, value_enum, default_value_t = Family::FlashCrash)]
        family: Family,
    },
    /// Have a coding agent author a Surfpool scenario from a request, e.g.
    /// "simulate a crash for SPCX", and save it.
    New {
        /// What should happen, in plain language.
        request: String,
        #[command(flatten)]
        authoring: Authoring,
        /// Where to save the scenario.
        #[arg(long, short, default_value = "scenario.json")]
        out: PathBuf,
        /// Replace `--out` if it exists.
        #[arg(long)]
        force: bool,
    },
    /// Author a scenario like `new` (or load one: pass a .json file), then
    /// play it on a surfnet and chart it live in a web UI.
    Play {
        /// A request for the agent, or a scenario file to replay.
        request: String,
        #[command(flatten)]
        authoring: Authoring,
        /// Also save the authored scenario here.
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Replace `--out` if it exists.
        #[arg(long)]
        force: bool,
        /// Viewer port; 0 picks a free one.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Register the scenario right away instead of waiting for Play.
        #[arg(long)]
        autoplay: bool,
        /// Don't open a browser.
        #[arg(long)]
        no_open: bool,
    },
}

#[derive(clap::Args)]
struct Authoring {
    /// The coding agent that authors the scenario, over ACP.
    #[arg(long, short, value_enum, default_value = "claude-code")]
    agent: author::Agent,
    /// The agent's model, e.g. `anthropic/claude-sonnet-5-5`.
    #[arg(long, short)]
    model: Option<String>,
    /// Snapshot the surfnet starts from; the agent reads prices from it.
    #[arg(long, env = "MTX_SNAPSHOT", default_value = DEFAULT_SNAPSHOT)]
    snapshot: PathBuf,
    /// Market the scenario trades; defaults to the snapshot's.
    #[arg(long)]
    symbol: Option<String>,
    /// The `surfpool` binary the agent runs as its MCP server.
    #[arg(long, env = "MTX_SURFPOOL", default_value = "surfpool")]
    surfpool: String,
    /// Scenarios the agent may hand in before mtx gives up.
    #[arg(long, default_value_t = 3)]
    attempts: usize,
    /// Show every log, the agent's replies and its adapter's output, instead
    /// of the progress lines. `RUST_LOG` also turns logs on.
    #[arg(long, short)]
    verbose: bool,
}

impl Authoring {
    async fn world(&self) -> Result<play::World> {
        play::World::start(&self.snapshot, self.symbol.as_deref(), true).await
    }

    /// Run the agent against `world`'s surfnet, off the async runtime.
    async fn author(&self, request: &str, world: &play::World) -> Result<surfpool_types::Scenario> {
        let markets = world.markets.clone();
        let (agent, model, request, surfpool, attempts, verbose) = (
            self.agent,
            self.model.clone(),
            request.to_string(),
            self.surfpool.clone(),
            self.attempts,
            self.verbose,
        );
        tokio::task::spawn_blocking(move || {
            author::author(author::AuthorOptions {
                agent,
                model: model.as_deref(),
                request: &request,
                markets: &markets,
                surfpool: &surfpool,
                attempts,
                verbose,
            })
        })
        .await?
    }
}

#[derive(Subcommand)]
enum SnapshotCommand {
    /// Fork mainnet, warm up the Phoenix accounts a task touches, and export them.
    Prepare {
        /// Output directory, e.g. snapshots/phoenix-spcx-2026-10-29.
        #[arg(long)]
        out: PathBuf,
        /// Datasource to fork. May embed an API key; only its host is recorded.
        #[arg(
            long,
            env = "MTX_FORK_RPC_URL",
            default_value = "https://api.mainnet-beta.solana.com"
        )]
        fork_rpc: String,
        #[arg(long, default_value = phoenix::DEFAULT_API_URL)]
        phoenix_api: String,
        #[arg(long, default_value = "SPCX")]
        symbol: String,
    },
    /// Restart offline from a snapshot and trade through a scripted crash.
    Verify {
        #[arg(long)]
        dir: PathBuf,
        /// How far the scenario drops the market, in percent.
        #[arg(long, default_value_t = 8.0)]
        crash_pct: f64,
    },
    /// Run the verify crash test against a live mainnet fork instead of a snapshot.
    VerifyFork {
        #[arg(
            long,
            env = "MTX_FORK_RPC_URL",
            default_value = "https://api.mainnet-beta.solana.com"
        )]
        fork_rpc: String,
        #[arg(long, default_value = phoenix::DEFAULT_API_URL)]
        phoenix_api: String,
        #[arg(long, default_value = "SPCX")]
        symbol: String,
        #[arg(long, default_value_t = 8.0)]
        crash_pct: f64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let result = run().await;
    if result.is_err() {
        // Don't leave a spinner line under the error.
        ui::clear();
    }
    result
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    // `scenario new` and `play` show progress lines instead of logs, unless
    // asked for logs.
    let interactive = match &cli.command {
        Command::Scenario(
            ScenarioCommand::New { authoring, .. } | ScenarioCommand::Play { authoring, .. },
        ) => !authoring.verbose,
        _ => false,
    };
    // `mtx bot` prints its own summary: the grader's logs only if asked.
    let default_filter = if interactive || matches!(cli.command, Command::Bot(_)) {
        "warn"
    } else {
        "info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into()),
        )
        .with_writer(ui::LogWriter)
        .init();
    // A terminal reads the progress lines; JSON is for scripts.
    let quiet_stdout = interactive && std::io::stdout().is_terminal();

    let output = match cli.command {
        Command::Mcp => {
            mcp::serve(authoring::repo_root()).await?;
            return Ok(());
        }
        Command::Task(TaskCommand::New { spec: a }) => {
            let spec = authoring::TaskSpec {
                name: a.name.unwrap_or_default(),
                title: a.title.unwrap_or_default(),
                summary: a.summary.unwrap_or_default(),
                base: a.base,
                thesis: a.thesis.unwrap_or_default(),
                wins: a.wins.unwrap_or_default(),
                loses: a.loses.unwrap_or_default(),
                regimes: a.regimes.unwrap_or_default(),
                max_position: a.max_position,
                max_drawdown_usd: a.max_drawdown_usd,
                hidden_sessions: a.hidden_sessions,
            };
            let out = authoring::scaffold(&authoring::repo_root(), &spec)?;
            ui::done(format!("Created bench/tasks/{}", spec.name.trim()));
            for w in out["written"].as_array().into_iter().flatten() {
                ui::note(w.as_str().unwrap_or_default());
            }
            serde_json::to_string_pretty(&out)?
        }
        Command::Task(TaskCommand::Status { task }) => {
            let steps = authoring::status(&authoring::repo_root(), &task)?;
            for s in &steps {
                let mark = match s.state {
                    "done" => ui::accent("✓"),
                    "manual" => ui::dim("…"),
                    _ => "○".to_string(),
                };
                eprintln!("{mark} {:<44} {}", s.title, ui::dim(&s.detail));
            }
            if let Some(n) = authoring::next(&steps) {
                eprintln!("\n{} {}\n  {}", ui::bold("Next:"), n.title, n.next);
            }
            serde_json::to_string_pretty(&steps)?
        }
        Command::Task(TaskCommand::Guide { step }) => {
            authoring::guide(&step).map(str::to_string).ok_or_else(|| {
                anyhow::anyhow!("no step `{step}`; steps: {}", authoring::STEPS.join(", "))
            })?
        }
        Command::Task(TaskCommand::List) => {
            serde_json::to_string_pretty(&authoring::list(&authoring::repo_root()))?
        }
        Command::Bench(BenchCommand::Review {
            path,
            port,
            no_open,
            results,
        }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            let first = publish::resolve(&path, &results)?;
            let summary = review::review(
                publish::roots(first, &results),
                review::ReviewOptions {
                    port,
                    open: !no_open,
                    start: None,
                },
            )
            .await?;
            serde_json::to_string_pretty(&summary)?
        }
        Command::Bench(BenchCommand::Publish {
            batch,
            version,
            force,
            results,
        }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            ui::start(format!("Publishing {} as {version}", batch.display()));
            let summary = publish::publish(&batch, &version, &results, force)?;
            ui::done(format!(
                "Published {} trial{} to {} ({:.1} MB)",
                summary["summary"]["trials"],
                if summary["summary"]["trials"] == 1 {
                    ""
                } else {
                    "s"
                },
                summary["path"].as_str().unwrap_or_default(),
                summary["bytes"].as_f64().unwrap_or(0.0) / 1e6
            ));
            for skipped in summary["summary"]["skipped"]
                .as_array()
                .into_iter()
                .flatten()
            {
                ui::note(format!(
                    "left out, too large: {}",
                    skipped.as_str().unwrap_or_default()
                ));
            }
            serde_json::to_string_pretty(&summary)?
        }
        Command::Bench(BenchCommand::Export { out, site, results }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            let releases = publish::releases(&results);
            if releases.is_empty() {
                anyhow::bail!("no published release in {}", results.display());
            }
            ui::start(format!(
                "Exporting {} release{}",
                releases.len(),
                if releases.len() == 1 { "" } else { "s" }
            ));
            let summary = review::export(releases, &out, site.as_deref())?;
            ui::done(format!("Exported to {}", out.display()));
            serde_json::to_string_pretty(&summary)?
        }
        Command::Bench(BenchCommand::Compare {
            range,
            open,
            port,
            results,
        }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            let (a, b) = publish::split_range(&range)?;
            let (a, b) = (
                publish::resolve(&a, &results)?,
                publish::resolve(&b, &results)?,
            );
            let cmp = publish::compare(&a, &b);
            publish::print_compare(&cmp);
            if open {
                let start = format!("compare/{}...{}", a.label, b.label);
                let mut roots = publish::roots(a, &results);
                if !roots.iter().any(|r| r.path == b.path) {
                    roots.push(b);
                }
                review::review(
                    roots,
                    review::ReviewOptions {
                        port,
                        open: true,
                        start: Some(start),
                    },
                )
                .await?;
            }
            serde_json::to_string_pretty(&cmp)?
        }
        Command::Snapshot(SnapshotCommand::Prepare {
            out,
            fork_rpc,
            phoenix_api,
            symbol,
        }) => {
            let manifest = snapshot::prepare(snapshot::PrepareOptions {
                out_dir: out,
                fork_rpc_url: fork_rpc,
                phoenix_api_url: phoenix_api,
                symbol,
            })
            .await?;
            serde_json::to_string_pretty(&manifest)?
        }
        Command::Snapshot(SnapshotCommand::VerifyFork {
            fork_rpc,
            phoenix_api,
            symbol,
            crash_pct,
        }) => {
            let report = snapshot::verify_fork(&fork_rpc, &phoenix_api, &symbol, crash_pct).await?;
            serde_json::to_string_pretty(&report)?
        }
        Command::Snapshot(SnapshotCommand::Verify { dir, crash_pct }) => {
            serde_json::to_string_pretty(&snapshot::verify(&dir, crash_pct).await?)?
        }
        Command::Scenario(ScenarioCommand::Generate {
            out,
            prefix,
            count,
            seed,
            family,
        }) => {
            std::fs::create_dir_all(&out).with_context(|| out.display().to_string())?;
            let scenarios = match family {
                Family::FlashCrash => ShockFamily::spcx().sample_set(&prefix, count, seed),
                Family::MarketMaking => MakingFamily::spcx().sample_set(&prefix, count, seed),
                Family::FundingCarry => CarryFamily::practice().sample_set(&prefix, count, seed),
                Family::WeekendGap => WeekendFamily::spcx().sample_set(&prefix, count, seed),
            };
            let mut written = Vec::new();
            for scenario in scenarios {
                let path = out.join(format!("{}.json", scenario.name));
                scenario.save(&path)?;
                written.push(path);
            }
            serde_json::to_string_pretty(&written)?
        }
        Command::Scenario(ScenarioCommand::New {
            request,
            authoring,
            out,
            force,
        }) => {
            if out.exists() && !force {
                anyhow::bail!("{} exists; pass --force to replace it", out.display());
            }
            let world = authoring.world().await?;
            let scenario = authoring.author(&request, &world).await?;
            author::save(&scenario, &out, force)?;
            ui::done(format!("Saved {}", ui::accent(out.display().to_string())));
            serde_json::to_string_pretty(&serde_json::json!({
                "path": out,
                "name": scenario.name,
                "description": scenario.description,
                "overrides": scenario.overrides.len(),
            }))?
        }
        Command::Scenario(ScenarioCommand::Play {
            request,
            authoring,
            out,
            force,
            port,
            autoplay,
            no_open,
        }) => {
            if let Some(out) = &out
                && out.exists()
                && !force
            {
                anyhow::bail!("{} exists; pass --force to replace it", out.display());
            }
            let world = authoring.world().await?;
            let file = PathBuf::from(&request);
            let scenario = if request.ends_with(".json") && file.is_file() {
                let scenario = author::load_checked(&file, &world.markets)?;
                ui::done(format!(
                    "Loaded {} {}",
                    ui::bold(&scenario.name),
                    ui::dim(format!("{} overrides", scenario.overrides.len()))
                ));
                scenario
            } else {
                authoring.author(&request, &world).await?
            };
            if let Some(out) = &out {
                author::save(&scenario, out, force)?;
                ui::done(format!("Saved {}", ui::accent(out.display().to_string())));
            }
            let summary = play::play(
                world,
                scenario,
                authoring.snapshot.clone(),
                play::PlayOptions {
                    port,
                    open: !no_open,
                    autoplay,
                },
            )
            .await?;
            serde_json::to_string_pretty(&summary)?
        }
        Command::Grade {
            sessions,
            draw,
            count,
            reference,
            anchor_jobs,
            tell_regime,
            stop_on_disqualify,
            reward,
            report,
            details,
        } => {
            let mut regimes: Option<Vec<String>> = None;
            // What each drawn scenario was drawn as, for its detail file.
            let mut annotations: Option<Vec<serde_json::Value>> = None;
            let (scenarios, eval_seed_sha256) = match draw {
                Some(family) => {
                    let (seed, hash) = eval_seed()?;
                    let scenarios = match family {
                        Family::FlashCrash => shaped(
                            ShockFamily::spcx().draw_set("hidden", count, seed),
                            tell_regime,
                            &mut annotations,
                            &mut regimes,
                        )?,
                        Family::MarketMaking => {
                            let drawn =
                                MakingFamily::spcx().sample_set_labeled("hidden", count, seed);
                            annotations = Some(
                                drawn
                                    .iter()
                                    .map(
                                        |(_, informed)| serde_json::json!({ "informed": informed }),
                                    )
                                    .collect(),
                            );
                            if tell_regime {
                                // The informed orders, by slot and side: what
                                // a perfect reader of the tape would know.
                                regimes = Some(
                                    drawn
                                        .iter()
                                        .map(|(scenario, informed)| {
                                            let orders: Vec<_> = scenario
                                                .making
                                                .iter()
                                                .flat_map(|m| m.takers.iter())
                                                .zip(informed)
                                                .filter(|(_, informed)| **informed)
                                                .map(|(o, _)| {
                                                    serde_json::json!({"slot": o.slot, "side": o.side})
                                                })
                                                .collect();
                                            serde_json::to_string(&orders)
                                        })
                                        .collect::<Result<_, _>>()?,
                                );
                            }
                            drawn
                                .into_iter()
                                .map(|(scenario, _)| scenario)
                                .collect::<Vec<_>>()
                        }
                        Family::FundingCarry => shaped(
                            CarryFamily::spcx().draw_set("hidden", count, seed),
                            tell_regime,
                            &mut annotations,
                            &mut regimes,
                        )?,
                        Family::WeekendGap => shaped(
                            WeekendFamily::spcx().draw_set("hidden", count, seed),
                            tell_regime,
                            &mut annotations,
                            &mut regimes,
                        )?,
                    };
                    (scenarios, Some(hash))
                }
                None if sessions.scenarios.is_empty() => {
                    anyhow::bail!("give scenario files, or --draw a family")
                }
                None => (sessions.load()?.into_iter().map(|(_, s)| s).collect(), None),
            };
            // Bots run on the same file system, and some mounts (Docker
            // Desktop's) ignore permissions: start from no reward file, and
            // treat one that appears while bots run as tampering.
            if let Some(path) = &reward {
                remove_if_present(path)?;
            }
            let mut graded = grade::grade(GradeOptions {
                snapshot_dir: &sessions.snapshot,
                bot_dir: &sessions.bot,
                scenarios: &scenarios,
                log_dir: &sessions.logs,
                jobs: sessions.jobs,
                first_bot_user: sessions.first_bot_user,
                reference_dir: reference.as_deref(),
                anchor_jobs,
                regimes: regimes.as_deref(),
                stop_on_disqualify,
                detail_dir: details.as_deref(),
                annotations: annotations.as_deref(),
            })
            .await?;
            graded.eval_seed_sha256 = eval_seed_sha256;
            if let Some(path) = &reward
                && path.exists()
            {
                graded.tamper(format!("a bot wrote {}", path.display()));
            }
            let json = serde_json::to_string_pretty(&graded)?;
            if let Some(path) = report {
                std::fs::write(&path, &json).with_context(|| path.display().to_string())?;
            }
            if let Some(path) = reward {
                let rewards = serde_json::to_string_pretty(&graded.rewards())?;
                std::fs::write(&path, rewards).with_context(|| path.display().to_string())?;
            }
            json
        }
        Command::Bot(command) => match command {
            BotCommand::New { dir, git, repo } => bot::new(&dir, git, repo.as_deref())?,
            BotCommand::Practice {
                bot,
                task,
                no_build,
                jobs,
                json,
                repo,
                scenarios,
            } => {
                bot::practice(bot::Practice {
                    dir: &bot,
                    task: task.as_deref(),
                    scenarios: &scenarios,
                    build: !no_build,
                    jobs,
                    json,
                    repo: repo.as_deref(),
                })
                .await?
            }
            BotCommand::Check { dir } => {
                let checked = bot::check(&dir);
                let mut lines: Vec<String> = checked
                    .problems
                    .iter()
                    .map(|p| format!("problem: {p}"))
                    .collect();
                lines.extend(checked.notes.iter().map(|n| format!("note: {n}")));
                if checked.problems.is_empty() {
                    lines.insert(0, format!("{} is ready to grade", dir.display()));
                }
                let out = lines.join("\n");
                if !checked.problems.is_empty() {
                    anyhow::bail!(out);
                }
                out
            }
            BotCommand::Pack {
                dir,
                task,
                repo,
                out,
            } => bot::pack(&dir, task.as_deref(), repo.as_deref(), &out)?,
        },
        Command::Calibrate { sessions } => {
            let mut scenarios = sessions.load()?;
            let reports = grade::calibrate(
                &sessions.snapshot,
                &sessions.bot,
                &mut scenarios,
                &sessions.logs,
                sessions.first_bot_user,
                sessions.jobs,
            )
            .await?;
            serde_json::to_string_pretty(&reports)?
        }
    };
    if !quiet_stdout {
        println!("{output}");
    }
    Ok(())
}

/// A hidden set drawn with each scenario's shape: annotated `{"shape": ..}`
/// and, when told, the shape as the regime.
fn shaped<S: serde::Serialize>(
    drawn: Vec<(Scenario, S)>,
    tell_regime: bool,
    annotations: &mut Option<Vec<serde_json::Value>>,
    regimes: &mut Option<Vec<String>>,
) -> Result<Vec<Scenario>> {
    *annotations = Some(
        drawn
            .iter()
            .map(|(_, shape)| serde_json::json!({ "shape": shape }))
            .collect(),
    );
    if tell_regime {
        *regimes = Some(
            drawn
                .iter()
                .map(|(_, shape)| serde_json::to_string(shape))
                .collect::<Result<_, _>>()?,
        );
    }
    Ok(drawn.into_iter().map(|(scenario, _)| scenario).collect())
}

/// The hidden set's seed, from the secret in `MTX_EVAL_SEED`, and the
/// secret's SHA-256 (hex), which is all a report may say about it.
fn eval_seed() -> anyhow::Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    let secret = std::env::var("MTX_EVAL_SEED").unwrap_or_default();
    if secret.len() < 32 {
        anyhow::bail!(
            "MTX_EVAL_SEED must hold the batch's secret (at least 32 characters): \
             hidden sets are drawn from it and never shipped"
        );
    }
    let digest = Sha256::digest(secret.as_bytes());
    let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    // The seed is a different hash than the one reported, so the report
    // doesn't hand anyone the seed itself.
    let seed = Sha256::digest([b"mtx-eval-seed:".as_slice(), secret.as_bytes()].concat());
    Ok((u64::from_le_bytes(seed[..8].try_into()?), hash))
}

fn remove_if_present(path: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| path.display().to_string())
        }
        _ => Ok(()),
    }
}
