use std::{io::IsTerminal, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mtx_grader::{
    generators,
    grade::{self, GradeOptions},
    keys::SessionKeys,
    phoenix, recipe, replay,
    scenario::Scenario,
    snapshot,
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
mod compact;
mod gates;
mod market_cal;
mod mcp;
mod notice;
mod play;
mod publish;
mod rescore;
mod review;
mod run;
mod slim;
mod store;
mod studio;
mod tables;
mod tasks;
mod tools;
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

/// The scenario generators' names, for `--family` and `--draw`: each task
/// names its own in `task.toml` (`[metadata.mtx] generator`).
fn generator_names() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(generators::names())
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
    /// Run a task through the quality bar and write one report (JSON and
    /// markdown): solvable and fair, graded correctly, realistic. `--fast`
    /// runs the cheap checks; without it, the image checks too.
    Gates {
        task: String,
        /// Task files and a one-set ladder on a few sessions; no image checks.
        #[arg(long)]
        fast: bool,
        /// Hidden sets the ladder grades (fast: 1, full: 4).
        #[arg(long)]
        sets: Option<usize>,
        /// Sessions per set (fast: 12, full: the task's own).
        #[arg(long)]
        sessions: Option<usize>,
        /// Where the report goes (default target/gates/<task>).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Use a finished ladder's summary.json instead of running one.
        #[arg(long)]
        ladder_summary: Option<PathBuf>,
    },
    /// Serve the task-authoring tools over MCP (stdio), for Claude Code:
    /// task_new (an interview with forms), task_status, task_guide, task_list.
    Mcp,
    /// Bench results: what `just bench` and Harbor runs leave behind.
    #[command(subcommand)]
    Bench(BenchCommand),
    /// Write your own bot: start one, practise it on a task, package it.
    #[command(subcommand)]
    Bot(BotCommand),
    /// A workbench for a strategy and its scenarios, in the browser: edit
    /// the strategy, ask an agent to change it or the scenario, play it step
    /// by step against the market's chart.
    Studio {
        /// The strategy, as `mtx bot new` makes it: its directory, or its
        /// Cargo.toml.
        #[arg(long, default_value = ".")]
        bot_path: PathBuf,
        /// The task its scenarios come from, as named in bench/tasks
        /// (default: the flash-crash task).
        #[arg(long)]
        task: Option<String>,
        /// Port; 0 picks a free one.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Don't open a browser.
        #[arg(long)]
        no_open: bool,
        /// The address to listen on. Pass 0.0.0.0 to open the studio from
        /// another device on your network (an iPad): it then prints a URL
        /// with a token, and a QR code of it.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// The machine-trading checkout to use (default: the one this mtx was
        /// built from).
        #[arg(long, env = "MTX_REPO")]
        repo: Option<PathBuf>,
    },
    /// Run a bot through scenarios and score it against the reference PnL.
    Grade {
        #[command(flatten)]
        sessions: Sessions,
        /// Draw the scenarios from this generator instead of reading files: a
        /// hidden set, seeded by the secret in `MTX_EVAL_SEED` and never
        /// written anywhere. Reports carry only the secret's SHA-256.
        #[arg(long, value_parser = generator_names(), conflicts_with = "scenarios")]
        draw: Option<String>,
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
        /// Grade the bot only if it is a strategy component (a bot.wasm): the
        /// verifier's rule. The reference can still be a process.
        #[arg(long)]
        components_only: bool,
        /// The bot is trusted, as the reference is (an anchor or a ladder
        /// rung): a process bot gets all the time it needs for each turn.
        #[arg(long)]
        trusted: bool,
        /// Run the grade this many times and fail unless every run's session
        /// reports, the reference's included, are byte for byte the same
        /// (wall times aside). The report is the first run's, with a
        /// `replay` section.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        replay: u32,
        /// While replaying, keep this many threads busy: the replay runs on
        /// a loaded machine.
        #[arg(long, default_value_t = 0)]
        replay_load: usize,
        /// Run the replays with this many sessions at once, the reference's
        /// too (at most --anchor-jobs), instead of --jobs: `--jobs 16
        /// --replay 2 --replay-jobs 1` checks a parallel grade against a
        /// serial one.
        #[arg(long, requires = "replay")]
        replay_jobs: Option<usize>,
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
    /// Score a finished grade again from what it wrote, without running a
    /// session: for when a scoring rule changes. Each session's report and
    /// scenario come from its detail file (`mtx grade --details`). Prints
    /// the new report, and on stderr where it differs from the old one.
    Rescore {
        /// The grade's report (`mtx grade --report`).
        report: PathBuf,
        /// The grade's detail files (`mtx grade --details`; default:
        /// `sessions` beside the report, as the verifier writes them).
        #[arg(long)]
        details: Option<PathBuf>,
        /// Write the new report here.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write Harbor's reward.json for it here.
        #[arg(long)]
        reward: Option<PathBuf>,
        /// Fail unless the new report is the old one: nothing about scoring
        /// changed.
        #[arg(long)]
        check: bool,
    },
    /// Run the reference bot through scenarios and save its PnL in each file.
    Calibrate {
        #[command(flatten)]
        sessions: Sessions,
    },
    /// Market calibration: per-asset files fitted to real data, the market
    /// model they drive, and realism checks against them.
    #[command(subcommand, name = "market-cal")]
    MarketCal(market_cal::MarketCalCommand),
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
    /// Build a bot with `just build`, then grade it on a task's practice
    /// scenarios, exactly as the verifier grades the hidden ones.
    Practice {
        /// The bot directory.
        #[arg(long, default_value = ".")]
        bot: PathBuf,
        /// The task to practise, as named in bench/tasks (default: the
        /// flash-crash task).
        #[arg(long)]
        task: Option<String>,
        /// Grade the bot as it is, without running `just build`.
        #[arg(long)]
        no_build: bool,
        /// Sessions to run at once, each on its own surfnet: 1-4 for a
        /// process bot, up to 32 for a strategy component.
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
    /// Build a bot and play one practice session, showing what its strategy
    /// printed; with --debug, step by step.
    Run {
        /// The bot directory.
        #[arg(long, default_value = ".")]
        bot: PathBuf,
        /// The task, as named in bench/tasks (default: the flash-crash task).
        #[arg(long)]
        task: Option<String>,
        /// A practice scenario by name (`practice-3`), or a scenario file
        /// (default: the task's first practice scenario).
        #[arg(long)]
        scenario: Option<String>,
        /// Run the bot as it is, without running `just build`.
        #[arg(long)]
        no_build: bool,
        /// Step through the session: at each step, the market the strategy
        /// was handed, what it printed, its order sheet and the orders sent.
        #[arg(long)]
        debug: bool,
        /// With --debug: play on at this pace, in milliseconds a step,
        /// instead of waiting for a key.
        #[arg(long, requires = "debug")]
        auto: Option<u64>,
        /// With --debug: show steps from this one on (counting from 1).
        #[arg(long, requires = "debug", default_value_t = 1)]
        from: u64,
        /// The machine-trading checkout to use (default: the one this mtx was
        /// built from).
        #[arg(long, env = "MTX_REPO")]
        repo: Option<PathBuf>,
    },
    /// Check a bot directory against the bot contract's shape: a bot.wasm
    /// that is a component, built from the current source.
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
    /// Bot directory: its `bot.wasm`, or for a process bot its `Justfile`'s
    /// `run`.
    #[arg(long)]
    bot: PathBuf,
    /// Bot output goes to `<logs>/<scenario>.log`.
    #[arg(long, default_value = "logs")]
    logs: PathBuf,
    /// Sessions to run at once, each on its own surfnet: 1-4 for a process
    /// bot (one bot user each), up to 32 for a strategy component. The report
    /// is the same whatever it is; a machine that can't keep up grades
    /// slower, not differently.
    #[arg(long, env = "MTX_JOBS", default_value_t = 1)]
    jobs: usize,
    /// The first bot user (0-3) sessions run their bots as, when run as
    /// root; they use `--jobs` of them from there. Graders running at once
    /// on one machine never share a bot user at the same time (a session's
    /// end kills every process of its user): a session waits for its user,
    /// so ranges that don't overlap keep them from waiting.
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
    /// Write the lists kept from the task folders' `[metadata.mtx]`: the
    /// Justfile's and the image's task lists, the dataset's and the READMEs'
    /// tables.
    Sync,
    /// Fail if `mtx task sync` would change a file.
    Check,
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
    /// Publish a finished batch as a release. It keeps the reports, the
    /// session detail files, the bot's source and the agent's trajectory
    /// (gzipped); never the batch's seed, and nothing that holds it or an API
    /// key. The release goes to the results dataset on Hugging Face (needs
    /// `hf` and HF_TOKEN), and bench/results/<version>/release.json, to
    /// commit, says where it is and holds its scores.
    Publish {
        /// The batch (jobs/bench/<batch>).
        batch: PathBuf,
        /// The release's name: the dataset version it ran, as in 0.9.0.
        version: String,
        /// Replace a release already published under that name.
        #[arg(long)]
        force: bool,
        /// Keep the full release here, as <artifacts>/<version>, outside the
        /// repository, and publish its slim release: the reports, rewards
        /// and bots, without the sessions step by step, logs or
        /// trajectories. Readers find the full copy through MTX_ARTIFACTS.
        #[arg(long)]
        artifacts: Option<PathBuf>,
        /// Write the whole release to bench/results/<version>, and upload
        /// nothing.
        #[arg(long)]
        local: bool,
        #[arg(long)]
        results: Option<PathBuf>,
    },
    /// Make a published release slim: move it whole to
    /// <artifacts>/<version>, outside the repository, and keep only its
    /// slim release in bench/results (see `mtx bench publish --artifacts`).
    Slim {
        /// The release's version (bench/results/<version>).
        version: String,
        #[arg(long)]
        artifacts: PathBuf,
        #[arg(long)]
        results: Option<PathBuf>,
    },
    /// Move a release whose files are in bench/results to the results
    /// dataset: upload them, and leave its release.json, with where they
    /// went and its scores. For a release in the dataset already, upload
    /// its tables (trials.jsonl, sessions.jsonl) and the dataset's card
    /// again.
    Upload {
        /// The release's version, or `all`.
        version: String,
        #[arg(long)]
        results: Option<PathBuf>,
    },
    /// Compact a release published before compacting was part of publishing:
    /// no transaction signatures, sessions' tables as columns, each scenario
    /// and each source file once, logs cut shorter. Readers see the same
    /// sessions. Running it twice changes nothing.
    Compact {
        /// The release's version (bench/results/<version>), or `all`.
        version: String,
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
    /// Score every trial of a batch or release again from its reports and
    /// detail files, without running a session (see `mtx rescore`): each
    /// trial's reward and skill as reported and as scored now.
    Rescore {
        /// A batch (jobs/bench/<batch>), a Harbor job, one trial, or a
        /// published release's version.
        path: String,
        /// Fail unless every trial scores as it was reported.
        #[arg(long)]
        check: bool,
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
    /// Check scenario files as a session would load them: each one's
    /// path, mandate and timing. A problem says how to fix it.
    Check {
        /// The scenario files.
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
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
        /// Which task's generator to draw from.
        #[arg(long, value_parser = generator_names(), default_value = "flash-crash")]
        family: String,
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
    /// Build a snapshot from its recipe: fork, warm up and export. Writes
    /// beside the recipe, with a manifest that records its hash.
    Build {
        /// The recipe, e.g. snapshots/sol-usdc-spot-2026-11-11/recipe.toml.
        recipe: PathBuf,
        /// Write here instead of beside the recipe.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Datasource to fork. May embed an API key; only its host is recorded.
        #[arg(long, env = "MTX_FORK_RPC_URL", default_value = recipe::DEFAULT_FORK_RPC)]
        fork_rpc: String,
        /// Replace a snapshot already in the output directory.
        #[arg(long)]
        force: bool,
        /// Mixed into the keys of simulated markets and never recorded; the
        /// same secret rebuilds the same keys. Random by default.
        #[arg(long, env = "MTX_BUILD_SECRET", hide_env_values = true)]
        secret: Option<String>,
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
    // `mtx bot` prints its own summary, and `mtx studio` its own notices
    // (the page shows the sessions): the grader's logs only if asked.
    let default_filter =
        if interactive || matches!(cli.command, Command::Bot(_) | Command::Studio { .. }) {
            "warn"
        } else {
            "info"
        };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into()),
        )
        .with_writer(ui::LogWriter)
        .event_format(notice::NoticeFormat)
        .init();
    // A terminal reads the progress lines; JSON is for scripts.
    let quiet_stdout = interactive && std::io::stdout().is_terminal();

    let output = match cli.command {
        Command::Mcp => {
            mcp::serve(authoring::repo_root()).await?;
            return Ok(());
        }
        Command::Gates {
            task,
            fast,
            sets,
            sessions,
            out,
            ladder_summary,
        } => {
            let opts = gates::Options {
                fast,
                sets,
                sessions,
                out,
                ladder_summary,
            };
            let (report, dir) = gates::run(&authoring::repo_root(), &task, &opts)?;
            eprintln!(
                "{}\nreport: {}/gates.{{md,json}}",
                report.markdown(),
                dir.display()
            );
            if !report.passed {
                println!("{}", serde_json::to_string_pretty(&report)?);
                std::process::exit(1);
            }
            serde_json::to_string_pretty(&report)?
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
        Command::Task(TaskCommand::Sync) => {
            let changed = tasks::sync(&authoring::repo_root())?;
            serde_json::to_string_pretty(&changed)?
        }
        Command::Task(TaskCommand::Check) => {
            let stale = tasks::stale(&authoring::repo_root())?;
            if !stale.is_empty() {
                anyhow::bail!(
                    "stale, run `mtx task sync`: {}",
                    stale
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            serde_json::to_string_pretty(&stale)?
        }
        Command::Task(TaskCommand::List) => {
            serde_json::to_string_pretty(&authoring::list(&authoring::repo_root()))?
        }
        Command::Bench(BenchCommand::Rescore {
            path,
            check,
            results,
        }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            let summary = rescore::rescore_trials(&publish::resolve(&path, &results)?)?;
            if check && (summary["changed"] != 0 || summary["failed"] != 0) {
                eprintln!("{}", serde_json::to_string_pretty(&summary)?);
                anyhow::bail!(
                    "of {} trials, {} score differently than reported and {} didn't read back",
                    summary["trials"],
                    summary["changed"],
                    summary["failed"]
                );
            }
            serde_json::to_string_pretty(&summary)?
        }
        Command::Rescore {
            report,
            details,
            out,
            reward,
            check,
        } => {
            let details = details.unwrap_or_else(|| {
                report
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .join("sessions")
            });
            let rescored = rescore::rescore(&report, &details, None)?;
            for difference in &rescored.differences {
                eprintln!("{}", rescore::describe(difference));
            }
            eprintln!(
                "{}",
                if rescored.differences.is_empty() {
                    format!("the same as {}", report.display())
                } else {
                    format!(
                        "{} parts differ from {}",
                        rescored.differences.len(),
                        report.display()
                    )
                }
            );
            if check && !rescored.differences.is_empty() {
                anyhow::bail!(
                    "the grade scores differently than {} says",
                    report.display()
                );
            }
            let json = serde_json::to_string_pretty(&rescored.report)?;
            if let Some(path) = out {
                std::fs::write(&path, &json).with_context(|| path.display().to_string())?;
            }
            if let Some(path) = reward {
                let rewards = serde_json::to_string_pretty(&rescored.report.rewards())?;
                std::fs::write(&path, rewards).with_context(|| path.display().to_string())?;
            }
            json
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
        Command::Bench(BenchCommand::Slim {
            version,
            artifacts,
            results,
        }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            let release = results.join(&version);
            if !release.join("release.json").is_file() {
                anyhow::bail!("no release {version} in {}", results.display());
            }
            let summary = slim::slim_in_place(&release, &artifacts)?;
            serde_json::to_string_pretty(&summary)?
        }
        Command::Bench(BenchCommand::Upload { version, results }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            let versions: Vec<String> = if version == "all" {
                std::fs::read_dir(&results)?
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|v| {
                        !v.starts_with('.') && results.join(v).join("release.json").is_file()
                    })
                    .collect()
            } else {
                vec![version]
            };
            let mut out = serde_json::Map::new();
            for version in versions {
                let dir = results.join(&version);
                anyhow::ensure!(
                    dir.join("release.json").is_file(),
                    "{} isn't a published release",
                    dir.display()
                );
                if publish::stored(&dir).is_some() {
                    ui::start(format!("Uploading {version}'s tables"));
                    publish::retable(&dir, &version)?;
                    ui::done(format!("Uploaded {version}'s tables"));
                    out.insert(version, serde_json::json!("tables"));
                    continue;
                }
                ui::start(format!("Uploading {version}"));
                let store = publish::offload(&dir, &dir, &version)?;
                ui::done(format!(
                    "Uploaded {version}: {} files, {}",
                    store.files,
                    store.url()
                ));
                out.insert(version, serde_json::json!(store.url()));
            }
            serde_json::to_string_pretty(&out)?
        }
        Command::Bench(BenchCommand::Publish {
            batch,
            version,
            force,
            artifacts,
            local,
            results,
        }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            ui::start(format!("Publishing {} as {version}", batch.display()));
            let summary = publish::publish(
                &batch,
                &version,
                &results,
                force,
                artifacts.as_deref(),
                local,
            )?;
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
        Command::Bench(BenchCommand::Compact { version, results }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            let releases: Vec<PathBuf> = if version == "all" {
                publish::releases(&results)
                    .into_iter()
                    .map(|r| r.path)
                    .collect()
            } else {
                vec![results.join(&version)]
            };
            let mut out = serde_json::Map::new();
            for release in releases {
                // A release in the dataset was compacted before it went.
                if release.join(store::COMPLETE).is_file() || publish::stored(&release).is_some() {
                    continue;
                }
                anyhow::ensure!(
                    release.join("release.json").is_file(),
                    "{} isn't a published release",
                    release.display()
                );
                ui::start(format!("Compacting {}", release.display()));
                let done = compact::compact_release(&release)?;
                ui::done(format!(
                    "Compacted {}: {:.1} MB → {:.1} MB",
                    release.display(),
                    done.bytes_before as f64 / 1e6,
                    done.bytes_after as f64 / 1e6
                ));
                out.insert(release.display().to_string(), serde_json::to_value(done)?);
            }
            serde_json::to_string_pretty(&out)?
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
        Command::Snapshot(SnapshotCommand::Build {
            recipe,
            out,
            fork_rpc,
            force,
            secret,
        }) => {
            let manifest = recipe::build(
                &recipe,
                recipe::BuildOptions {
                    out_dir: out,
                    fork_rpc_url: fork_rpc,
                    force,
                    secret,
                },
            )
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
        Command::Scenario(ScenarioCommand::Check { files }) => {
            let mut failed = 0;
            for file in &files {
                let loaded = mtx_grader::scenario::Scenario::load(file)
                    .map_err(anyhow::Error::from)
                    .and_then(|scenario| {
                        // A market no snapshot warmed up loads, then never
                        // moves: say so here. Only when the repo is at hand.
                        if let Ok(root) = bot::repo(None)
                            && let Ok(task) = bot::resolve_task(&root, None)
                            && let Ok(snapshot) = bot::task_snapshot(&root, &task)
                            && scenario.spot.is_none()
                        {
                            bot::snapshot_for(&snapshot, &scenario.symbol).map_err(|e| {
                                anyhow::anyhow!(
                                    "{e:#}. Set `symbol`, and the mandate's instrument, to one of those"
                                )
                            })?;
                        }
                        Ok(scenario)
                    });
                match loaded {
                    Ok(scenario) => notice::print(
                        notice::NoticeLevel::Success,
                        &format!("{} is a scenario", file.display()),
                        &format!(
                            "{}: {} steps, {} waypoints",
                            scenario.name,
                            scenario.duration_slots.div_ceil(2),
                            scenario.path.len()
                        ),
                    ),
                    Err(error) => {
                        failed += 1;
                        notice::print(
                            notice::NoticeLevel::Error,
                            &format!("{} isn't a valid scenario", file.display()),
                            &format!("{error:#}"),
                        );
                    }
                }
            }
            if failed > 0 {
                anyhow::bail!("{failed} of {} scenario files have problems", files.len());
            }
            String::new()
        }
        Command::Scenario(ScenarioCommand::Generate {
            out,
            prefix,
            count,
            seed,
            family,
        }) => {
            std::fs::create_dir_all(&out).with_context(|| out.display().to_string())?;
            let generator = generators::generator(&family)
                .with_context(|| format!("no scenario generator `{family}`"))?;
            let scenarios = (generator.practice)(&prefix, count, seed);
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
            components_only,
            trusted,
            replay,
            replay_load,
            replay_jobs,
            reward,
            report,
            details,
        } => {
            let mut regimes: Option<Vec<String>> = None;
            // What each drawn scenario was drawn as, for its detail file.
            let mut annotations: Option<Vec<serde_json::Value>> = None;
            // A drawn set's session keys come from the batch's secret, so
            // every grade of it signs the same; otherwise from a fresh one.
            let mut keys = SessionKeys::random();
            let (scenarios, eval_seed_sha256) = match draw {
                Some(family) => {
                    let (seed, hash, batch_keys) = eval_seed()?;
                    keys = batch_keys;
                    let generator = generators::generator(&family)
                        .with_context(|| format!("no scenario generator `{family}`"))?;
                    let hidden = (generator.hidden)(count, seed, tell_regime)?;
                    annotations = Some(hidden.annotations);
                    regimes = hidden.regimes;
                    let scenarios = hidden.scenarios;
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
            let options = |jobs: usize, anchor_jobs: usize| GradeOptions {
                snapshot_dir: &sessions.snapshot,
                bot_dir: &sessions.bot,
                scenarios: &scenarios,
                log_dir: &sessions.logs,
                jobs,
                first_bot_user: sessions.first_bot_user,
                reference_dir: reference.as_deref(),
                anchor_jobs,
                regimes: regimes.as_deref(),
                stop_on_disqualify,
                detail_dir: details.as_deref(),
                annotations: annotations.as_deref(),
                components_only,
                trusted,
                observer: None,
                keys: &keys,
            };
            let load = (replay > 1 && replay_load > 0).then(|| replay::Load::start(replay_load));
            let mut graded = grade::grade(options(sessions.jobs, anchor_jobs)).await?;
            graded.eval_seed_sha256 = eval_seed_sha256;
            if replay > 1 {
                let first = replay::reports(&graded)?;
                let mut differences = Vec::new();
                for run in 2..=replay as usize {
                    let mut again = grade::grade(match replay_jobs {
                        Some(jobs) => options(jobs, jobs.min(anchor_jobs)),
                        None => options(sessions.jobs, anchor_jobs),
                    })
                    .await?;
                    again.eval_seed_sha256.clone_from(&graded.eval_seed_sha256);
                    differences.extend(replay::compare(&first, &replay::reports(&again)?, run));
                    eprintln!(
                        "replay {run} of {replay}: {}",
                        if differences.is_empty() {
                            "the same so far"
                        } else {
                            "different"
                        }
                    );
                }
                graded.replay = Some(replay::ReplayCheck {
                    runs: replay as usize,
                    load_threads: replay_load,
                    jobs: replay_jobs,
                    reports: first.len(),
                    identical: differences.is_empty(),
                    differences,
                });
            }
            drop(load);
            if let Some(path) = &reward
                && path.exists()
            {
                graded.tamper(format!("a bot wrote {}", path.display()));
            }
            let json = serde_json::to_string_pretty(&graded)?;
            if let Some(path) = report {
                std::fs::write(&path, &json).with_context(|| path.display().to_string())?;
            }
            if let Some(check) = graded.replay.as_ref().filter(|check| !check.identical) {
                let first = check.differences.first().map(ToString::to_string);
                return Err(mtx_grader::Error::Replay(format!(
                    "{} reports differed over {} runs; first: {}",
                    check.differences.len(),
                    check.runs,
                    first.unwrap_or_default()
                ))
                .into());
            }
            if let Some(path) = reward {
                let rewards = serde_json::to_string_pretty(&graded.rewards())?;
                std::fs::write(&path, rewards).with_context(|| path.display().to_string())?;
            }
            json
        }
        Command::Studio {
            bot_path,
            task,
            port,
            no_open,
            host,
            repo,
        } => {
            studio::studio(studio::StudioOptions {
                host: &host,
                dir: &bot_path,
                task: task.as_deref(),
                repo: repo.as_deref(),
                port,
                open: !no_open,
            })
            .await?;
            String::new()
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
            BotCommand::Run {
                bot,
                task,
                scenario,
                no_build,
                debug,
                auto,
                from,
                repo,
            } => {
                run::run(run::Run {
                    dir: &bot,
                    task: task.as_deref(),
                    scenario: scenario.as_deref(),
                    build: !no_build,
                    debug,
                    auto: auto.map(std::time::Duration::from_millis),
                    from: from.saturating_sub(1),
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
        Command::MarketCal(command) => market_cal::run(command)?,
    };
    if !quiet_stdout {
        println!("{output}");
    }
    Ok(())
}

/// The hidden set's seed, from the secret in `MTX_EVAL_SEED`, and the
/// secret's SHA-256 (hex), which is all a report may say about it.
fn eval_seed() -> anyhow::Result<(u64, String, SessionKeys)> {
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
    Ok((
        u64::from_le_bytes(seed[..8].try_into()?),
        hash,
        SessionKeys::from_eval_seed(secret.as_bytes()),
    ))
}

fn remove_if_present(path: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| path.display().to_string())
        }
        _ => Ok(()),
    }
}
