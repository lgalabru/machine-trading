use std::{
    collections::BTreeMap,
    io::IsTerminal,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
#[cfg(feature = "generate")]
use mtx_grader::generators;
use mtx_grader::{
    grade::{self, GradeOptions},
    keys::SessionKeys,
    replay,
    scenario::Scenario,
};
#[cfg(feature = "host")]
use mtx_grader::{phoenix, recipe, snapshot};
use tracing_subscriber::EnvFilter;

/// Every session starts and drops a whole surfnet, hundreds of megabytes on a
/// few hundred threads. glibc's allocator keeps the memory a session freed in
/// per-thread arenas, so a verifier grading 144 sessions grows past its
/// limit; mimalloc returns it.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "host")]
mod acp;
#[cfg(feature = "generate")]
mod anchors;
#[cfg(feature = "host")]
mod author;
#[cfg(feature = "generate")]
mod authoring;
#[cfg(feature = "host")]
mod bot;
mod bot_check;
#[cfg(feature = "host")]
mod charts;
#[cfg(feature = "host")]
mod compact;
#[cfg(feature = "generate")]
mod gates;
#[cfg(all(feature = "host", unix))]
mod live;
#[cfg(feature = "generate")]
mod market_cal;
#[cfg(feature = "generate")]
mod mcp;
mod notice;
#[cfg(feature = "host")]
mod play;
#[cfg(feature = "host")]
mod publish;
#[cfg(feature = "host")]
mod rescore;
#[cfg(feature = "host")]
mod review;
#[cfg(feature = "host")]
mod run;
mod scoring_args;
#[cfg(feature = "generate")]
mod scoring_text;
#[cfg(feature = "host")]
mod slim;
#[cfg(feature = "host")]
mod store;
#[cfg(feature = "host")]
mod studio;
#[cfg(feature = "host")]
mod tables;
#[cfg(feature = "generate")]
mod tasks;
#[cfg(feature = "host")]
mod tools;
#[cfg(feature = "host")]
mod ui;
mod versions;

#[cfg(feature = "host")]
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
#[cfg(feature = "generate")]
fn generator_names() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(generators::names())
}

// Parsed once per run: a large variant costs nothing worth boxing for.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Pinned chain state for offline tasks.
    #[cfg(feature = "host")]
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Market scenarios: the price paths sessions replay.
    #[command(subcommand)]
    Scenario(ScenarioCommand),
    /// Make a new task, guided: scaffold it, then see what's left to do.
    #[cfg(feature = "generate")]
    #[command(subcommand)]
    Task(TaskCommand),
    /// Run a task through the quality bar and write one report (JSON and
    /// markdown): solvable and fair, graded correctly, realistic. `--fast`
    /// runs the cheap checks; without it, the image checks too.
    #[cfg(feature = "generate")]
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
    #[cfg(feature = "generate")]
    Mcp,
    /// A Claude Code agent speaking ACP on stdio: what mtx itself starts to
    /// talk to Claude Code (the studio's composer, `scenario new`). It drives
    /// the `claude` command, with no Node adapter.
    #[cfg(feature = "host")]
    #[command(hide = true)]
    ClaudeAcp,
    /// The same for Codex: it drives `codex app-server`.
    #[cfg(feature = "host")]
    #[command(hide = true)]
    CodexAcp,
    /// Bench results: what `just bench` and Harbor runs leave behind.
    #[cfg(feature = "host")]
    #[command(subcommand)]
    Bench(BenchCommand),
    // The agent's build keeps `check` alone, so its help says only that.
    #[cfg_attr(
        feature = "host",
        doc = "Write your own bot: start one, practise it on a task, package it."
    )]
    #[cfg_attr(
        not(feature = "host"),
        doc = "Check a bot directory before it is graded."
    )]
    #[command(subcommand)]
    Bot(BotCommand),
    /// Control a live runtime while it runs: add, pause, resume and remove
    /// strategies, change a mandate, or stop everything (`docs/rfd/0004`).
    #[cfg(all(feature = "host", unix))]
    Live {
        /// The runtime's control socket (its configuration's
        /// `control.socket`).
        #[arg(long, env = "MTX_LIVE_SOCKET")]
        socket: PathBuf,
        /// Print the strategies as JSON.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: live::LiveCommand,
    },
    /// A workbench for a strategy and its scenarios, in the browser: edit
    /// the strategy, ask an agent to change it or the scenario, play it step
    /// by step against the market's chart.
    #[cfg(feature = "host")]
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
        #[cfg(feature = "generate")]
        #[arg(long, value_parser = generator_names(), conflicts_with = "scenarios")]
        draw: Option<String>,
        /// How many scenarios to draw, stratified as the family's odds.
        #[cfg(feature = "generate")]
        #[arg(long, default_value_t = 12, requires = "draw")]
        count: usize,
        /// Run this reference bot on the same scenarios first, and score
        /// against its PnL rather than what the scenario files carry.
        #[arg(long)]
        reference: Option<PathBuf>,
        /// Run this informed bot on the same scenarios after the reference,
        /// handed each drawn session's latent state as MTX_REGIME (what the
        /// task's data could tell, never the session's future; the bot is
        /// handed nothing), and anchor scoring v2's 1 to its PnL rather than
        /// what the scenario files carry.
        #[arg(long)]
        informed: Option<PathBuf>,
        /// The anchors' PnL and drawdowns per scenario, as `mtx calibrate`
        /// saves them in scenario files, for a drawn set, which has none: a
        /// JSON object from scenario name to `reference_pnl_usd`,
        /// `reference_max_drawdown_usd`, `informed_pnl_usd` and
        /// `informed_max_drawdown_usd` (each optional). Every scenario must
        /// be in it. A bot run with --reference or --informed replaces its
        /// part.
        #[arg(long)]
        anchors: Option<PathBuf>,
        /// Reference sessions to run at once (1-4); the informed bot's too.
        #[arg(long, default_value_t = 4)]
        anchor_jobs: usize,
        /// Hand the bot each drawn session's regime as MTX_REGIME: the
        /// shock's shape (flash-crash), or the informed taker orders
        /// (market-making). For the ladder's oracles, never a graded bot.
        #[cfg(feature = "generate")]
        #[arg(long, requires = "draw", hide = true)]
        tell_regime: bool,
        /// Hand the bot each drawn session's latent state as MTX_REGIME, as
        /// --informed hands it to the informed bot: for the ladder's
        /// informed rung, never a graded bot.
        #[cfg(feature = "generate")]
        #[arg(long, requires = "draw", conflicts_with = "tell_regime", hide = true)]
        tell_latent: bool,
        /// Stop at the bot's first disqualifying session (the grade is 0
        /// whatever the rest do); the report covers the sessions up to it.
        #[arg(long)]
        stop_on_disqualify: bool,
        /// Run the sessions under scoring v2's rules: positions are sampled
        /// every slot, the bot's seed in each session is salted with the
        /// grade's secret (as in every drawn grade), so it can't tell which
        /// session it is in, and a session that starts holding something is
        /// also run by a bot that does nothing, whose PnL is v2's 0 there.
        /// (A breach's stop-out is the scenario's own `stop_out`.) For a
        /// task scored by v2; off, sessions run as they always have.
        #[arg(long)]
        v2_sessions: bool,
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
        /// The task's fingerprint, written into the report with the scoring
        /// spec's hash (default: the file `mtx task sync` ships to the
        /// verifier, /tests/task.fingerprint, if it is there).
        #[arg(long, env = "MTX_TASK_FINGERPRINT")]
        task_fingerprint: Option<String>,
        #[command(flatten)]
        scoring: scoring_args::ScoringArgs,
    },
    /// Score a finished grade again from what it wrote, without running a
    /// session: for when a scoring rule changes. Each session's report and
    /// scenario come from its detail file (`mtx grade --details`). Prints
    /// the new report, and on stderr where it differs from the old one.
    #[cfg(feature = "host")]
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
        /// changed (v2 aside, when the old one had none).
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        scoring: scoring_args::ScoringArgs,
    },
    /// Run the reference bot through scenarios and save its PnL in each file.
    #[cfg(feature = "host")]
    Calibrate {
        #[command(flatten)]
        sessions: Sessions,
        /// The bot is the task's informed bot: save its PnL as the
        /// scenario's `informed_pnl_usd`, scoring v2's 1, instead.
        #[arg(long)]
        informed: bool,
        /// Run the sessions under v2's rules, as `mtx grade --v2-sessions`
        /// does, so the anchors saved are measured as a v2 grade measures
        /// them.
        #[arg(long)]
        v2_sessions: bool,
        /// Draw the scenarios from this generator instead of reading files,
        /// seeded by the secret in `MTX_EVAL_SEED` as `mtx grade --draw`
        /// draws them (the informed bot is handed each one's latent state),
        /// and save the PnL in `--anchors`, not in the scenarios.
        #[cfg(feature = "generate")]
        #[arg(long, value_parser = generator_names(), conflicts_with = "scenarios", requires = "anchors")]
        draw: Option<String>,
        /// How many scenarios to draw.
        #[cfg(feature = "generate")]
        #[arg(long, default_value_t = 12, requires = "draw")]
        count: usize,
        /// The anchors file a drawn set's PnL goes to (`mtx grade
        /// --anchors`), by scenario name. What it already holds for other
        /// anchors is kept, so the reference and the informed bot can be
        /// calibrated into one file in turn.
        #[cfg(feature = "generate")]
        #[arg(long, requires = "draw")]
        anchors: Option<PathBuf>,
        /// Draw a task's practice set from this generator, as `mtx scenario
        /// generate` does, and calibrate it into `--out`: each file carries
        /// the anchor's PnL and drawdown. The informed bot is handed each
        /// session's latent state (who is informed, which wallets) as it is
        /// in a grade, host-side, and the files carry none of it: only
        /// the PnL reaches the agent's image, and the description is a
        /// neutral one (the generator's states the draw in prose).
        /// `--informed` needs `--v2-sessions`. What `--out` already holds
        /// for the other anchor is kept.
        #[cfg(feature = "generate")]
        #[arg(
            long,
            value_parser = generator_names(),
            conflicts_with_all = ["scenarios", "draw"],
            requires_all = ["out", "practice_seed"]
        )]
        practice: Option<String>,
        /// Where the practice files are written, as `mtx scenario generate
        /// --out` writes them.
        #[cfg(feature = "generate")]
        #[arg(long, requires = "practice")]
        out: Option<PathBuf>,
        /// Names are `<prefix>-<n>`.
        #[cfg(feature = "generate")]
        #[arg(long, default_value = "practice", requires = "practice")]
        practice_prefix: String,
        #[cfg(feature = "generate")]
        #[arg(long, default_value_t = 6, requires = "practice")]
        practice_count: usize,
        /// The seed the practice set was generated with (`mtx scenario
        /// generate --seed`): the same scenarios, so the files match.
        #[cfg(feature = "generate")]
        #[arg(long, requires = "practice")]
        practice_seed: Option<u64>,
    },
    /// Market calibration: per-asset files fitted to real data, the market
    /// model they drive, and realism checks against them.
    #[cfg(feature = "generate")]
    #[command(subcommand, name = "market-cal")]
    MarketCal(market_cal::MarketCalCommand),
}

#[derive(Subcommand)]
enum BotCommand {
    /// Make a bot that connects, watches the market and never trades: the
    /// plumbing works, the strategy is yours.
    #[cfg(feature = "host")]
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
    #[cfg(feature = "host")]
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
    #[cfg(feature = "host")]
    Run {
        /// The bot directory.
        #[arg(long, default_value = ".")]
        bot: PathBuf,
        /// The task, as named in bench/tasks (default: the flash-crash task).
        #[arg(long)]
        task: Option<String>,
        /// The studio's `practice-101` (the default) or `practice-102`, one of
        /// the task's practice scenarios by name (`practice-3`), or a
        /// scenario file.
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
    #[cfg(feature = "host")]
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
#[cfg(feature = "generate")]
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
    /// The procedure for one step (design, family, reference, anchors,
    /// anchor, practice, instruction, scoring, probes, readme, registry,
    /// lint, verify).
    Guide { step: String },
    /// The repository's tasks.
    List,
    /// A task's fingerprint: a hash over what changes what it measures (its
    /// spec, scenario family, practice files, anchors, mandate and grader
    /// version, and what the engine does to its anchors), as every grade of
    /// it records.
    Fingerprint { task: String },
    /// Write the lists kept from the task folders' `[metadata.mtx]`: the
    /// Justfile's and the image's task lists, the dataset's and the READMEs'
    /// tables, each task's `solution/` and its fingerprint. A task whose
    /// anchors' inputs changed gets a `pending` anchor digest (a warning
    /// says so; `check` and CI refuse it) unless `--anchors` plays them.
    Sync {
        /// Build and play every task's anchors and write their digests
        /// (Linux, `just` and Surfpool installed: see `anchor-digest`).
        #[arg(long)]
        anchors: bool,
    },
    /// Fail if `mtx task sync` would change a file.
    Check,
    /// What the engine does to a task's anchors: play the reference (and
    /// the informed bot) on a fixed public draw and hash the sessions. With
    /// `--check`, fail when the digest differs from the committed
    /// `tests/anchor.digest`: an engine change altered what the task does.
    /// Builds the anchors; needs Linux, `just` and Surfpool.
    AnchorDigest {
        /// The tasks (default: every task).
        tasks: Vec<String>,
        /// Compare with the committed digests instead of printing them.
        #[arg(long)]
        check: bool,
    },
}

#[derive(clap::Args)]
#[cfg(feature = "generate")]
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
    /// (market-making-cbrs).
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
#[cfg(feature = "host")]
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
        /// Go on although the trials mix task fingerprints or scoring
        /// hashes (their scores aren't on one scale); the output lists the
        /// mismatch.
        #[arg(long)]
        allow_version_mismatch: bool,
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
        /// Go on although the trials mix task fingerprints or scoring
        /// hashes (their scores aren't on one scale); the output lists the
        /// mismatch.
        #[arg(long)]
        allow_version_mismatch: bool,
        /// Publish trials without an agent trajectory (refused otherwise).
        #[arg(long)]
        allow_missing_trajectories: bool,
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
        /// Go on although the trials mix task fingerprints or scoring
        /// hashes (their scores aren't on one scale); the output lists the
        /// mismatch.
        #[arg(long)]
        allow_version_mismatch: bool,
        #[arg(long)]
        results: Option<PathBuf>,
        #[command(flatten)]
        scoring: scoring_args::ScoringArgs,
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
        /// Go on although the trials mix task fingerprints or scoring
        /// hashes (their scores aren't on one scale); the output lists the
        /// mismatch.
        #[arg(long)]
        allow_version_mismatch: bool,
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
    /// Not in the agent's build (`--no-default-features`).
    #[cfg(feature = "generate")]
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
    #[cfg(feature = "host")]
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
    #[cfg(feature = "host")]
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
#[cfg(feature = "host")]
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

#[cfg(feature = "host")]
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
#[cfg(feature = "host")]
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
    // Don't leave a spinner line under the error.
    #[cfg(feature = "host")]
    if result.is_err() {
        ui::clear();
    }
    result
}

async fn run() -> Result<()> {
    // The ACP agent owns stdout for its protocol, and runs on its own
    // threads: before anything of mtx's writes there.
    #[cfg(feature = "host")]
    {
        let serve: Option<fn() -> Result<()>> = match std::env::args_os().nth(1) {
            Some(arg) if arg == "claude-acp" => Some(mtx_acp::claude::serve_stdio),
            Some(arg) if arg == "codex-acp" => Some(mtx_acp::codex::serve_stdio),
            _ => None,
        };
        if let Some(serve) = serve {
            return tokio::task::spawn_blocking(serve).await?;
        }
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // Help and version exit 0; a usage error exits 64, so it is never
            // mistaken for a failed check (2).
            if e.use_stderr() {
                eprint!("{e}");
                std::process::exit(64);
            }
            e.exit()
        }
    };
    // `scenario new` and `play` show progress lines instead of logs, unless
    // asked for logs.
    #[cfg(feature = "host")]
    let interactive = match &cli.command {
        Command::Scenario(
            ScenarioCommand::New { authoring, .. } | ScenarioCommand::Play { authoring, .. },
        ) => !authoring.verbose,
        _ => false,
    };
    // The agent's build has neither.
    #[cfg(not(feature = "host"))]
    let interactive = false;
    // `mtx bot` prints its own summary, and `mtx studio` its own notices
    // (the page shows the sessions): the grader's logs only if asked.
    #[cfg(feature = "host")]
    let prints_its_own = matches!(cli.command, Command::Bot(_) | Command::Studio { .. });
    #[cfg(not(feature = "host"))]
    let prints_its_own = matches!(cli.command, Command::Bot(_));
    let default_filter = if interactive || prints_its_own {
        "warn"
    } else {
        "info"
    };
    let logs = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into()),
        )
        .event_format(notice::NoticeFormat);
    // The host's commands draw a spinner, which a log line clears first; the
    // agent's build draws none.
    #[cfg(feature = "host")]
    logs.with_writer(ui::LogWriter).init();
    #[cfg(not(feature = "host"))]
    logs.with_writer(std::io::stderr).init();
    // A terminal reads the progress lines; JSON is for scripts.
    let quiet_stdout = interactive && std::io::stdout().is_terminal();

    let output = match cli.command {
        #[cfg(feature = "generate")]
        Command::Mcp => {
            mcp::serve(authoring::repo_root()).await?;
            return Ok(());
        }
        // Served above, before the arguments were parsed.
        #[cfg(feature = "host")]
        Command::ClaudeAcp | Command::CodexAcp => return Ok(()),
        #[cfg(feature = "generate")]
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
        #[cfg(feature = "generate")]
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
        #[cfg(feature = "generate")]
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
        #[cfg(feature = "generate")]
        Command::Task(TaskCommand::Guide { step }) => {
            authoring::guide(&step).map(str::to_string).ok_or_else(|| {
                anyhow::anyhow!("no step `{step}`; steps: {}", authoring::STEPS.join(", "))
            })?
        }
        #[cfg(feature = "generate")]
        Command::Task(TaskCommand::Sync { anchors }) => {
            let root = authoring::repo_root();
            if anchors {
                tasks::play_anchor_digests(&root)?;
            }
            let changed = tasks::sync(&root)?;
            let pending = tasks::pending_digests(&root)?;
            if !pending.is_empty() {
                eprintln!(
                    "warning: tests/anchor.digest is pending for {}: CI refuses it. Run `mtx task sync --anchors` on Linux, or commit the digest CI prints",
                    pending.join(", ")
                );
            }
            serde_json::to_string_pretty(&changed)?
        }
        #[cfg(feature = "generate")]
        Command::Task(TaskCommand::AnchorDigest { tasks, check }) => {
            let root = authoring::repo_root();
            let names: Vec<String> = if tasks.is_empty() {
                tasks::load(&root)?.into_iter().map(|t| t.dir).collect()
            } else {
                tasks
            };
            let mut out = serde_json::Map::new();
            let mut failed = Vec::new();
            for name in &names {
                if check {
                    match anchors::check(&root, name)? {
                        None => {
                            out.insert(name.clone(), "unchanged".into());
                        }
                        Some(why) => {
                            eprintln!("{why}");
                            failed.push(name.clone());
                        }
                    }
                } else {
                    let digest = anchors::compute(&root, name)?;
                    out.insert(name.clone(), serde_json::json!(digest.digest));
                }
            }
            if !failed.is_empty() {
                anyhow::bail!("anchor digest differs for: {}", failed.join(", "));
            }
            serde_json::to_string_pretty(&out)?
        }
        #[cfg(feature = "generate")]
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
            let pending = tasks::pending_digests(&authoring::repo_root())?;
            if !pending.is_empty() {
                anyhow::bail!(
                    "tests/anchor.digest is pending for {}: run `mtx task sync --anchors` on Linux",
                    pending.join(", ")
                );
            }
            serde_json::to_string_pretty(&stale)?
        }
        #[cfg(feature = "generate")]
        Command::Task(TaskCommand::Fingerprint { task }) => {
            let root = authoring::repo_root();
            let fingerprint =
                versions::fingerprint(&root, &task, &|p| std::fs::read(root.join(p)).ok())?;
            serde_json::to_string_pretty(
                &serde_json::json!({ "task": task, "fingerprint": fingerprint }),
            )?
        }
        #[cfg(feature = "generate")]
        Command::Task(TaskCommand::List) => {
            serde_json::to_string_pretty(&authoring::list(&authoring::repo_root()))?
        }
        #[cfg(feature = "host")]
        Command::Bench(BenchCommand::Rescore {
            path,
            check,
            allow_version_mismatch,
            results,
            scoring,
        }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            let summary = rescore::rescore_trials(
                &publish::resolve(&path, &results)?,
                &scoring,
                allow_version_mismatch,
            )?;
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
        #[cfg(feature = "host")]
        Command::Rescore {
            report,
            details,
            out,
            reward,
            check,
            scoring,
        } => {
            let v2 = scoring.params(None)?;
            let details = details.unwrap_or_else(|| {
                report
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .join("sessions")
            });
            let rescored = rescore::rescore(&report, &details, None, v2.as_ref())?;
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
        #[cfg(feature = "host")]
        Command::Bench(BenchCommand::Review {
            path,
            port,
            no_open,
            allow_version_mismatch,
            results,
        }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            let first = publish::resolve(&path, &results)?;
            let summary = review::review(
                publish::roots(first, &results),
                review::ReviewOptions {
                    allow_version_mismatch,
                    port,
                    open: !no_open,
                    start: None,
                },
            )
            .await?;
            serde_json::to_string_pretty(&summary)?
        }
        #[cfg(feature = "host")]
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
        #[cfg(feature = "host")]
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
        #[cfg(feature = "host")]
        Command::Bench(BenchCommand::Publish {
            batch,
            version,
            force,
            artifacts,
            local,
            allow_version_mismatch,
            allow_missing_trajectories,
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
                allow_version_mismatch,
                allow_missing_trajectories,
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
        #[cfg(feature = "host")]
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
        #[cfg(feature = "host")]
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
        #[cfg(feature = "host")]
        Command::Bench(BenchCommand::Compare {
            range,
            open,
            port,
            allow_version_mismatch,
            results,
        }) => {
            let results = results.unwrap_or_else(publish::results_dir);
            let (a, b) = publish::split_range(&range)?;
            let (a, b) = (
                publish::resolve(&a, &results)?,
                publish::resolve(&b, &results)?,
            );
            let cmp = publish::compare_checked(&a, &b, allow_version_mismatch)?;
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
                        allow_version_mismatch,
                        port,
                        open: true,
                        start: Some(start),
                    },
                )
                .await?;
            }
            serde_json::to_string_pretty(&cmp)?
        }
        #[cfg(feature = "host")]
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
        #[cfg(feature = "host")]
        Command::Snapshot(SnapshotCommand::VerifyFork {
            fork_rpc,
            phoenix_api,
            symbol,
            crash_pct,
        }) => {
            let report = snapshot::verify_fork(&fork_rpc, &phoenix_api, &symbol, crash_pct).await?;
            serde_json::to_string_pretty(&report)?
        }
        #[cfg(feature = "host")]
        Command::Snapshot(SnapshotCommand::Verify { dir, crash_pct }) => {
            serde_json::to_string_pretty(&snapshot::verify(&dir, crash_pct).await?)?
        }
        Command::Scenario(ScenarioCommand::Check { files }) => {
            let mut failed = 0;
            for file in &files {
                let loaded = Scenario::load(file).map_err(anyhow::Error::from);
                // A market no snapshot warmed up loads, then never moves: say
                // so here. Only when the repo is at hand, which it never is
                // in an agent's image.
                #[cfg(feature = "host")]
                let loaded = loaded.and_then(|scenario| {
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
                            scenario
                                .market_scenarios()
                                .iter()
                                .map(|m| m.path.len())
                                .sum::<usize>()
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
        #[cfg(feature = "generate")]
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
            let scenarios = generator.practice(&prefix, count, seed);
            let mut written = Vec::new();
            for scenario in scenarios {
                let path = out.join(format!("{}.json", scenario.name));
                // A family with pools draws scenarios that have no v1 form.
                scenario.save_any(&path)?;
                written.push(path);
            }
            serde_json::to_string_pretty(&written)?
        }
        #[cfg(feature = "host")]
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
        #[cfg(feature = "host")]
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
            #[cfg(feature = "generate")]
            draw,
            #[cfg(feature = "generate")]
            count,
            reference,
            informed,
            anchors,
            anchor_jobs,
            #[cfg(feature = "generate")]
            tell_regime,
            #[cfg(feature = "generate")]
            tell_latent,
            stop_on_disqualify,
            v2_sessions,
            components_only,
            trusted,
            replay,
            replay_load,
            replay_jobs,
            reward,
            report,
            details,
            task_fingerprint,
            scoring,
        } => {
            let v2 = scoring.params(None)?;
            let versions = mtx_grader::versions::Versions {
                task_fingerprint: versions::recorded_fingerprint(
                    task_fingerprint,
                    std::path::Path::new(versions::VERIFIER_FINGERPRINT),
                ),
                scoring_hash: Some(match &v2 {
                    Some(params) => mtx_grader::scoring_v2::spec_hash(params),
                    None => mtx_grader::grade::v1_spec_hash(),
                }),
            };
            v2_scores_every_session(stop_on_disqualify, v2.as_ref())?;
            #[cfg_attr(not(feature = "generate"), allow(unused_mut))]
            let mut regimes: Option<Vec<String>> = None;
            #[cfg_attr(not(feature = "generate"), allow(unused_mut))]
            let mut latent: Option<Vec<String>> = None;
            // What each drawn scenario was drawn as, for its detail file.
            #[cfg_attr(not(feature = "generate"), allow(unused_mut))]
            let mut annotations: Option<Vec<serde_json::Value>> = None;
            // A drawn set's session keys come from the batch's secret, so
            // every grade of it signs the same; otherwise from a fresh one.
            #[cfg_attr(not(feature = "generate"), allow(unused_mut))]
            let mut keys = SessionKeys::random();
            #[cfg(feature = "generate")]
            let drawn_set = draw.is_some();
            #[cfg(not(feature = "generate"))]
            let drawn_set = false;
            #[cfg(feature = "generate")]
            let (scenarios, eval_seed_sha256) = match draw {
                Some(family) => {
                    let (seed, hash, batch_keys) = eval_seed()?;
                    keys = batch_keys;
                    let generator = generators::generator(&family)
                        .with_context(|| format!("no scenario generator `{family}`"))?;
                    let hidden = (generator.hidden)(count, seed, tell_regime)?;
                    annotations = Some(hidden.annotations);
                    regimes = hidden.regimes;
                    latent = Some(hidden.latent);
                    let scenarios = hidden.scenarios;
                    (scenarios, Some(hash))
                }
                None if sessions.scenarios.is_empty() => {
                    anyhow::bail!("give scenario files, or --draw a family")
                }
                None => (sessions.load()?.into_iter().map(|(_, s)| s).collect(), None),
            };
            // The agent's build draws nothing: only the scenario files it is given.
            #[cfg(not(feature = "generate"))]
            let (scenarios, eval_seed_sha256) = {
                if sessions.scenarios.is_empty() {
                    anyhow::bail!("give scenario files")
                }
                let scenarios: Vec<Scenario> =
                    sessions.load()?.into_iter().map(|(_, s)| s).collect();
                (scenarios, None::<String>)
            };
            let mut scenarios = scenarios;
            if let Some(path) = &anchors {
                set_anchors(&mut scenarios, path)?;
            }
            // Bots run on the same file system, and some mounts (Docker
            // Desktop's) ignore permissions: start from no reward file, and
            // treat one that appears while bots run as tampering.
            if let Some(path) = &reward {
                remove_if_present(path)?;
            }
            keys = grade_keys(keys, drawn_set, v2_sessions);
            let options = |jobs: usize, anchor_jobs: usize| GradeOptions {
                snapshot_dir: &sessions.snapshot,
                bot_dir: &sessions.bot,
                scenarios: &scenarios,
                log_dir: &sessions.logs,
                jobs,
                first_bot_user: sessions.first_bot_user,
                reference_dir: reference.as_deref(),
                informed: informed.as_deref().map(|dir| grade::Informed {
                    dir,
                    latent: latent.as_deref(),
                }),
                anchor_jobs,
                // The bot is handed nothing, but on the ladder: its oracles
                // the regime, its informed rung the latent state.
                #[cfg(feature = "generate")]
                regimes: if tell_latent {
                    latent.as_deref()
                } else {
                    regimes.as_deref()
                },
                #[cfg(not(feature = "generate"))]
                regimes: regimes.as_deref(),
                stop_on_disqualify,
                detail_dir: details.as_deref(),
                annotations: annotations.as_deref(),
                components_only,
                trusted,
                observer: None,
                keys: &keys,
                v2_sessions,
                host: None,
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
            // v2 was asked for: a grade it can't score (no reference session
            // for its ceiling, say) fails and writes no reward. Beside v1,
            // the report is written with its v1 scores first; as the one
            // score, nothing is: a v1 score never stands in for it.
            let v2_failed = v2.as_ref().and_then(|params| graded.score_v2(params).err());
            graded.versions = versions;
            let json = serde_json::to_string_pretty(&graded)?;
            if let Some(path) = report {
                match report_to_write(v2.as_ref(), v2_failed.is_some(), &json) {
                    Some(text) => {
                        std::fs::write(&path, text).with_context(|| path.display().to_string())?
                    }
                    None => match std::fs::remove_file(&path) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                            return Err(e).with_context(|| path.display().to_string());
                        }
                        _ => {}
                    },
                }
            }
            if let Some(error) = v2_failed {
                return Err(anyhow::Error::from(error).context("--scoring v2"));
            }
            match (&graded.headline, &graded.v2) {
                (Some(_), Some(v2)) => {
                    tracing::info!(reward = graded.reward, raw = v2.raw, "scored by v2")
                }
                _ => tracing::info!(
                    reward = graded.reward,
                    reference_ratio = graded.reference_ratio,
                    "scored"
                ),
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
        #[cfg(feature = "host")]
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
        #[cfg(all(feature = "host", unix))]
        Command::Live {
            socket,
            json,
            command,
        } => {
            live::run(&socket, command, json)?;
            String::new()
        }
        Command::Bot(command) => match command {
            #[cfg(feature = "host")]
            BotCommand::New { dir, git, repo } => bot::new(&dir, git, repo.as_deref())?,
            #[cfg(feature = "host")]
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
            #[cfg(feature = "host")]
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
                let checked = bot_check::check(&dir);
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
            #[cfg(feature = "host")]
            BotCommand::Pack {
                dir,
                task,
                repo,
                out,
            } => bot::pack(&dir, task.as_deref(), repo.as_deref(), &out)?,
        },
        #[cfg(feature = "host")]
        Command::Calibrate {
            sessions,
            informed,
            v2_sessions,
            #[cfg(feature = "generate")]
            draw,
            #[cfg(feature = "generate")]
            count,
            #[cfg(feature = "generate")]
            anchors,
            #[cfg(feature = "generate")]
            practice,
            #[cfg(feature = "generate")]
            out,
            #[cfg(feature = "generate")]
            practice_prefix,
            #[cfg(feature = "generate")]
            practice_count,
            #[cfg(feature = "generate")]
            practice_seed,
        } => {
            let mut keys = SessionKeys::random();
            #[cfg_attr(not(feature = "generate"), allow(unused_mut))]
            let mut latent: Option<Vec<String>> = None;
            #[cfg(feature = "generate")]
            let practice_dir = out;
            #[cfg(feature = "generate")]
            let mut scenarios: Vec<(Option<PathBuf>, Scenario)> = match (&draw, &practice) {
                (None, Some(family)) => {
                    let generator = generators::generator(family)
                        .with_context(|| format!("no scenario generator `{family}`"))?;
                    let seed = practice_seed.context("--practice needs --practice-seed")?;
                    let dir = practice_dir.as_deref().context("--practice needs --out")?;
                    if informed && !v2_sessions {
                        anyhow::bail!(
                            "--practice --informed needs --v2-sessions: the practice anchors must be measured under the rules a v2 grade runs by"
                        );
                    }
                    let drawn = (generator.practice_drawn)(&practice_prefix, practice_count, seed)?;
                    latent = informed.then_some(drawn.latent);
                    let mut scenarios = Vec::new();
                    for mut scenario in drawn.scenarios {
                        // The generator's description states the session's
                        // latent state in prose, and the file goes into the
                        // agent's image: it carries a neutral one.
                        scenario.description = generator.neutral_description(&scenario.symbol);
                        let path = dir.join(format!("{}.json", scenario.name));
                        if path.exists() {
                            keep_other_anchors(&mut scenario, &Scenario::load(&path)?)?;
                        }
                        scenarios.push((None, scenario));
                    }
                    scenarios
                }
                (Some(family), _) => {
                    let (seed, _, batch_keys) = eval_seed()?;
                    keys = batch_keys;
                    let generator = generators::generator(family)
                        .with_context(|| format!("no scenario generator `{family}`"))?;
                    // The informed bot is handed each session's latent
                    // state, as a grade hands it; the reference nothing.
                    let hidden = (generator.hidden)(count, seed, false)?;
                    latent = informed.then_some(hidden.latent);
                    hidden.scenarios.into_iter().map(|s| (None, s)).collect()
                }
                (None, None) => sessions
                    .load()?
                    .into_iter()
                    .map(|(path, s)| (Some(path), s))
                    .collect(),
            };
            // The agent's build draws nothing: only the scenario files it is given.
            #[cfg(not(feature = "generate"))]
            let mut scenarios: Vec<(Option<PathBuf>, Scenario)> = sessions
                .load()?
                .into_iter()
                .map(|(path, s)| (Some(path), s))
                .collect();
            if v2_sessions {
                keys = keys.salt_strategy_seeds();
            }
            let reports = grade::calibrate(
                grade::Calibration {
                    snapshot_dir: &sessions.snapshot,
                    bot_dir: &sessions.bot,
                    log_dir: &sessions.logs,
                    first_bot_user: sessions.first_bot_user,
                    jobs: sessions.jobs,
                    informed,
                    latent: latent.as_deref(),
                    keys: &keys,
                    v2_sessions,
                },
                &mut scenarios,
            )
            .await?;
            #[cfg(feature = "generate")]
            if let Some(path) = &anchors {
                save_anchors(path, scenarios.iter().map(|(_, s)| s))?;
            }
            #[cfg(feature = "generate")]
            if let Some(dir) = &practice_dir {
                std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
                for (_, scenario) in &scenarios {
                    save_practice(scenario, &dir.join(format!("{}.json", scenario.name)))?;
                }
            }
            serde_json::to_string_pretty(&reports)?
        }
        #[cfg(feature = "generate")]
        Command::MarketCal(command) => market_cal::run(command)?,
    };
    if !quiet_stdout {
        println!("{output}");
    }
    Ok(())
}

#[cfg(feature = "generate")]
/// Save a practice file without the per-account role maps a family draws
/// for the host-side informed run: the file goes into the agent's image. It
/// is v1 where the scenario has a v1 form, and v2 where it has pools or
/// several markets, which v1 can't say (a v1 file would drop them).
fn save_practice(scenario: &Scenario, path: &Path) -> Result<()> {
    let mut value = if scenario.amm.is_some() || !scenario.markets.is_empty() {
        let v2 = mtx_grader::scenario_v2::ScenarioV2::try_from(scenario)
            .map_err(|reason| anyhow::anyhow!("{}: {reason}", path.display()))?;
        serde_json::to_value(v2)?
    } else {
        serde_json::to_value(scenario)?
    };
    generators::strip_identity_roles(&mut value);
    let text = serde_json::to_string_pretty(&value)? + "\n";
    std::fs::write(path, text).with_context(|| path.display().to_string())
}

#[cfg(feature = "generate")]
/// A practice file that is already in `--out` keeps its anchors the
/// calibration doesn't run, if it is the scenario the family draws at the
/// seed (whatever its description, which a calibration rewrites): a file
/// from another seed or family is an error, not overwritten.
fn keep_other_anchors(drawn: &mut Scenario, existing: &Scenario) -> anyhow::Result<()> {
    let bare = |s: &Scenario| {
        let mut s = s.clone();
        s.description.clear();
        s.reference_pnl_usd = None;
        s.reference_max_drawdown_usd = None;
        s.informed_pnl_usd = None;
        s.informed_max_drawdown_usd = None;
        let mut value = serde_json::to_value(s)?;
        generators::strip_identity_roles(&mut value);
        Ok::<_, serde_json::Error>(value)
    };
    if bare(drawn)? != bare(existing)? {
        anyhow::bail!(
            "{} already in --out isn't what the family draws at this seed",
            drawn.name
        );
    }
    drawn.reference_pnl_usd = existing.reference_pnl_usd;
    drawn.reference_max_drawdown_usd = existing.reference_max_drawdown_usd;
    drawn.informed_pnl_usd = existing.informed_pnl_usd;
    drawn.informed_max_drawdown_usd = existing.informed_max_drawdown_usd;
    Ok(())
}

/// One scenario's anchors in an `mtx grade --anchors` file: what
/// `mtx calibrate` saves in a scenario file.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioAnchors {
    reference_pnl_usd: Option<f64>,
    reference_max_drawdown_usd: Option<f64>,
    informed_pnl_usd: Option<f64>,
    informed_max_drawdown_usd: Option<f64>,
}

/// Give each scenario the anchors `path` holds for it, as if its file had
/// been calibrated. Every scenario must be there, and nothing else.
fn set_anchors(scenarios: &mut [Scenario], path: &Path) -> Result<()> {
    let mut anchors: BTreeMap<String, ScenarioAnchors> =
        serde_json::from_slice(&std::fs::read(path).with_context(|| path.display().to_string())?)
            .with_context(|| format!("{}: not a map of scenario anchors", path.display()))?;
    for scenario in scenarios.iter_mut() {
        let given = anchors
            .remove(&scenario.name)
            .with_context(|| format!("{}: no anchors for {}", path.display(), scenario.name))?;
        for (value, slot) in [
            (given.reference_pnl_usd, &mut scenario.reference_pnl_usd),
            (
                given.reference_max_drawdown_usd,
                &mut scenario.reference_max_drawdown_usd,
            ),
            (given.informed_pnl_usd, &mut scenario.informed_pnl_usd),
            (
                given.informed_max_drawdown_usd,
                &mut scenario.informed_max_drawdown_usd,
            ),
        ] {
            if let Some(value) = value {
                anyhow::ensure!(
                    value.is_finite(),
                    "{}: {}'s anchors must be finite",
                    path.display(),
                    scenario.name
                );
                *slot = Some(value);
            }
        }
    }
    if let Some(name) = anchors.keys().next() {
        anyhow::bail!("{}: {name} isn't a scenario of this grade", path.display());
    }
    Ok(())
}

/// Write the anchors `scenarios` carry into the anchors file at `path`
/// (see [`set_anchors`]), keeping what it holds for any anchor they don't.
#[cfg(feature = "generate")]
fn save_anchors<'a>(path: &Path, scenarios: impl Iterator<Item = &'a Scenario>) -> Result<()> {
    let mut file: BTreeMap<String, serde_json::Map<String, serde_json::Value>> =
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{}: not a map of scenario anchors", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(anyhow::Error::from(e).context(path.display().to_string())),
        };
    for scenario in scenarios {
        let entry = file.entry(scenario.name.clone()).or_default();
        for (key, value) in [
            ("reference_pnl_usd", scenario.reference_pnl_usd),
            (
                "reference_max_drawdown_usd",
                scenario.reference_max_drawdown_usd,
            ),
            ("informed_pnl_usd", scenario.informed_pnl_usd),
            (
                "informed_max_drawdown_usd",
                scenario.informed_max_drawdown_usd,
            ),
        ] {
            if let Some(value) = value {
                entry.insert(key.to_string(), value.into());
            }
        }
    }
    std::fs::write(path, serde_json::to_string_pretty(&file)?)
        .with_context(|| path.display().to_string())
}

/// The hidden set's seed, from the secret in `MTX_EVAL_SEED`, and the
/// secret's SHA-256 (hex), which is all a report may say about it.
#[cfg(feature = "generate")]
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

/// The keys a grade runs its sessions with: a bot's seed is a hash of its
/// session's name unless the grade's secret salts it, and a name says which
/// session of a drawn set it is.
fn grade_keys(keys: SessionKeys, drawn: bool, v2_sessions: bool) -> SessionKeys {
    if drawn || v2_sessions {
        keys.salt_strategy_seeds()
    } else {
        keys
    }
}

/// Scoring v2 scores every session of the set: a grade that stops at the
/// bot's first breach would leave it a truncated one.
fn v2_scores_every_session(
    stop_on_disqualify: bool,
    v2: Option<&mtx_grader::scoring_v2::Params>,
) -> anyhow::Result<()> {
    if stop_on_disqualify && v2.is_some() {
        anyhow::bail!(
            "--stop-on-disqualify ends the grade at the bot's first breach, and scoring v2 \
             scores every session: drop one of them"
        );
    }
    Ok(())
}

fn remove_if_present(path: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| path.display().to_string())
        }
        _ => Ok(()),
    }
}

/// What `mtx grade` writes as its report: the grade, unless v2 is its one
/// score and v2 couldn't score it. Then nothing: the grade's only scores
/// would be v1's, and a reader would take the trial for graded.
fn report_to_write<'a>(
    v2: Option<&mtx_grader::scoring_v2::Params>,
    v2_failed: bool,
    json: &'a str,
) -> Option<&'a str> {
    (!(v2_failed && v2.is_some_and(|p| p.headline))).then_some(json)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "generate")]
    use clap::Parser;
    use mtx_grader::keys::SessionKeys;
    #[cfg(feature = "generate")]
    use mtx_grader::scenario::ShockFamily;

    #[cfg(feature = "generate")]
    use super::{Cli, keep_other_anchors, save_anchors, set_anchors};
    use super::{grade_keys, v2_scores_every_session};

    #[test]
    fn a_drawn_grade_salts_what_its_bots_seed_says_of_their_session() {
        let seed = |keys: &SessionKeys| {
            keys.role("bot")
                .session(2, "hidden-3")
                .strategy_seed("hidden-3")
        };
        let named = mtx_core::turns::scenario_seed("hidden-3");
        let fresh = || SessionKeys::from_eval_seed(b"a batch secret of at least 32 bytes..");
        assert_ne!(seed(&grade_keys(fresh(), true, false)), named);
        assert_ne!(seed(&grade_keys(fresh(), true, true)), named);
        assert_ne!(seed(&grade_keys(fresh(), false, true)), named);
        // Scenario files graded by hand keep the seed their name gives.
        assert_eq!(seed(&grade_keys(fresh(), false, false)), named);
        // And a drawn set's salt is the batch's: the same in every grade.
        assert_eq!(
            seed(&grade_keys(fresh(), true, false)),
            seed(&grade_keys(fresh(), true, true))
        );
    }

    #[test]
    fn v2_grades_every_session_so_it_refuses_a_grade_that_stops_at_the_first_breach() {
        let v2 = mtx_grader::scoring_v2::Params::new(mtx_grader::scoring_v2::Ceiling::Informed {
            eta_override: None,
        });
        let refused = v2_scores_every_session(true, Some(&v2))
            .unwrap_err()
            .to_string();
        assert!(refused.contains("--stop-on-disqualify"), "{refused}");
        assert!(v2_scores_every_session(false, Some(&v2)).is_ok());
        // Under v1 the flag is the verifiers' own.
        assert!(v2_scores_every_session(true, None).is_ok());
    }

    /// The commands under `mtx <path>`, hidden ones too, sorted.
    #[cfg(not(feature = "generate"))]
    fn offered(path: &[&str]) -> Vec<String> {
        let mut command = <super::Cli as clap::CommandFactory>::command();
        for name in path {
            command = command
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("no `mtx {}`", path.join(" ")))
                .clone();
        }
        let mut names: Vec<String> = command
            .get_subcommands()
            .map(|sub| sub.get_name().to_owned())
            .collect();
        names.sort();
        names
    }

    /// Whether `mtx grade` takes `--<flag>`.
    #[cfg(not(feature = "generate"))]
    fn grade_takes(flag: &str) -> bool {
        <super::Cli as clap::CommandFactory>::command()
            .find_subcommand("grade")
            .expect("mtx grade is in every build")
            .get_arguments()
            .any(|argument| argument.get_long() == Some(flag))
    }

    // The agent's build: `just leak-audit` reads the same lists off the
    // image's binary (`agent_commands` and its two neighbours, less `help`),
    // so a command added without the `host` gate fails here before an image
    // is built.
    #[cfg(not(feature = "host"))]
    #[test]
    fn the_agents_build_grades_and_checks_and_does_nothing_else() {
        use clap::Parser;

        assert_eq!(offered(&[]), ["bot", "grade", "scenario"]);
        assert_eq!(offered(&["bot"]), ["check"]);
        assert_eq!(offered(&["scenario"]), ["check"]);
        assert!(!grade_takes("draw"));
        // What `mtx-practice` runs in the agent's container still parses.
        assert!(grade_takes("snapshot"));
        let practice = [
            "mtx",
            "grade",
            "--jobs",
            "3",
            "--snapshot",
            "s",
            "--bot",
            "/app/bot",
            "--components-only",
            "--logs",
            "/tmp/mtx-practice-logs",
            "--scoring",
            "v2-headline",
            "/opt/mtx/practice/p1.json",
        ];
        assert!(super::Cli::try_parse_from(practice).is_ok());
        // And what an agent might reach for is not a command at all.
        for gone in [
            &["scenario", "generate"][..],
            &["scenario", "new"],
            &["scenario", "play"],
            &["bot", "practice"],
            &["studio"],
            &["bench", "show"],
            &["calibrate"],
            &["rescore"],
            &["snapshot", "create"],
            &["claude-acp"],
            &["codex-acp"],
            &["live", "status"],
        ] {
            let args = [&["mtx"][..], gone].concat();
            assert!(super::Cli::try_parse_from(args).is_err(), "{gone:?}");
        }
    }

    // A host's build without the generators keeps its own commands and still
    // draws nothing from a task's family.
    #[cfg(all(feature = "host", not(feature = "generate")))]
    #[test]
    fn a_hosts_build_without_the_generators_draws_no_scenario() {
        assert_eq!(
            offered(&[]),
            [
                "bench",
                "bot",
                "calibrate",
                "claude-acp",
                "codex-acp",
                "grade",
                "live",
                "rescore",
                "scenario",
                "snapshot",
                "studio"
            ]
        );
        assert_eq!(
            offered(&["bot"]),
            ["check", "new", "pack", "practice", "run"]
        );
        assert_eq!(offered(&["scenario"]), ["check", "new", "play"]);
        assert!(!grade_takes("draw"));
    }

    #[cfg(feature = "generate")]
    #[test]
    fn an_anchors_file_calibrates_a_drawn_set_and_names_every_scenario() {
        let dir = std::env::temp_dir().join(format!("mtx-anchors-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("anchors.json");
        let _ = std::fs::remove_file(&path);
        let mut drawn = ShockFamily::spcx().sample_set("hidden", 2, 7);
        // The reference, then the informed bot, saved in turn into one file.
        drawn[0].reference_pnl_usd = Some(10.0);
        drawn[1].reference_pnl_usd = Some(-4.5);
        save_anchors(&path, drawn.iter()).unwrap();
        let mut informed = ShockFamily::spcx().sample_set("hidden", 2, 7);
        informed[0].informed_pnl_usd = Some(30.0);
        informed[0].informed_max_drawdown_usd = Some(120.0);
        informed[1].informed_pnl_usd = Some(12.0);
        save_anchors(&path, informed.iter()).unwrap();

        let mut fresh = ShockFamily::spcx().sample_set("hidden", 2, 7);
        set_anchors(&mut fresh, &path).unwrap();
        assert_eq!(fresh[0].reference_pnl_usd, Some(10.0));
        assert_eq!(fresh[1].reference_pnl_usd, Some(-4.5));
        assert_eq!(fresh[0].informed_pnl_usd, Some(30.0));
        assert_eq!(fresh[0].informed_max_drawdown_usd, Some(120.0));
        assert_eq!(fresh[1].informed_max_drawdown_usd, None);

        // A scenario the file doesn't name, or one it names that the grade
        // doesn't have, is refused.
        let mut more = ShockFamily::spcx().sample_set("hidden", 3, 7);
        assert!(set_anchors(&mut more, &path).is_err());
        let mut fewer = ShockFamily::spcx().sample_set("hidden", 1, 7);
        assert!(set_anchors(&mut fewer, &path).is_err());
        // And so is a field it doesn't know.
        std::fs::write(&path, r#"{"hidden-1": {"reference_pnl": 1.0}}"#).unwrap();
        assert!(set_anchors(&mut fewer, &path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(feature = "generate")]
    #[test]
    fn the_informed_bot_runs_beside_the_reference_and_a_drawn_calibration_needs_an_anchors_file() {
        let grade = |extra: &[&str]| {
            let mut args = vec!["mtx", "grade", "--snapshot", "s", "--bot", "b"];
            args.extend_from_slice(extra);
            Cli::try_parse_from(args)
        };
        assert!(grade(&["--scoring", "v2-headline", "--informed", "i", "x.json"]).is_ok());
        assert!(grade(&["--informed", "i", "--draw", "flash-crash"]).is_ok());
        // The ladder's informed rung is told the latent state, an oracle the
        // regime: never both, and never without a draw.
        assert!(grade(&["--tell-latent", "--draw", "flash-crash"]).is_ok());
        assert!(grade(&["--tell-latent", "--tell-regime", "--draw", "flash-crash"]).is_err());
        // There is one headline mechanism: `--scoring v2-headline`.
        assert!(grade(&["--v2-headline", "x.json"]).is_err());
        let calibrate = |extra: &[&str]| {
            let mut args = vec!["mtx", "calibrate", "--snapshot", "s", "--bot", "b"];
            args.extend_from_slice(extra);
            Cli::try_parse_from(args)
        };
        // A drawn calibration has no files to save in: it needs --anchors.
        assert!(calibrate(&["--draw", "flash-crash"]).is_err());
        assert!(calibrate(&["--draw", "flash-crash", "--anchors", "a.json", "--informed"]).is_ok());
        assert!(calibrate(&["--anchors", "a.json"]).is_err());
        // A practice calibration saves in the files it is drawn into, and
        // draws the same set as the files were generated from.
        let practice = [
            "--practice",
            "funding-carry",
            "--out",
            "d",
            "--practice-seed",
            "7",
        ];
        assert!(calibrate(&[&practice[..], &["--informed"]].concat()).is_ok());
        assert!(calibrate(&["--practice", "funding-carry", "--out", "d"]).is_err());
        assert!(calibrate(&["--practice", "funding-carry", "--practice-seed", "7"]).is_err());
        assert!(
            calibrate(
                &[
                    &practice[..],
                    &["--draw", "flash-crash", "--anchors", "a.json"]
                ]
                .concat()
            )
            .is_err()
        );
        assert!(calibrate(&[&practice[..], &["x.json"]].concat()).is_err());
    }

    #[cfg(feature = "generate")]
    #[test]
    fn a_practice_file_keeps_the_anchor_that_was_not_run_and_refuses_another_draw() {
        let mut drawn = ShockFamily::spcx().sample_set("practice", 1, 7).remove(0);
        let mut existing = drawn.clone();
        existing.reference_pnl_usd = Some(3.0);
        existing.reference_max_drawdown_usd = Some(9.0);
        keep_other_anchors(&mut drawn, &existing).unwrap();
        assert_eq!(drawn.reference_pnl_usd, Some(3.0));
        assert_eq!(drawn.reference_max_drawdown_usd, Some(9.0));
        assert_eq!(drawn.informed_pnl_usd, None);
        let mut other = ShockFamily::spcx().sample_set("practice", 1, 8).remove(0);
        assert!(keep_other_anchors(&mut other, &existing).is_err());
        // A file from `mtx scenario generate` has the generator's description
        // and the calibration's has the neutral one: the same scenario.
        let mut renamed = existing.clone();
        renamed.description = "SPCX practice session".into();
        let mut again = ShockFamily::spcx().sample_set("practice", 1, 7).remove(0);
        keep_other_anchors(&mut again, &renamed).unwrap();
        assert_eq!(again.reference_pnl_usd, Some(3.0));
    }

    #[cfg(feature = "generate")]
    #[test]
    fn a_saved_practice_file_has_no_role_map() {
        let scenario = ShockFamily::spcx().sample_set("practice", 1, 7).remove(0);
        let dir = std::env::temp_dir().join(format!("mtx-save-practice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.json");
        super::save_practice(&scenario, &path).unwrap();
        assert_eq!(
            mtx_grader::scenario::Scenario::load(&path).unwrap(),
            scenario
        );
        // The same data with a c1-shaped role map compares equal once the
        // maps are stripped, as keep_other_anchors needs.
        let mut with_roles = serde_json::to_value(&scenario).unwrap();
        with_roles["making"] = serde_json::json!({"roles": {"informed": [7]}});
        mtx_grader::generators::strip_identity_roles(&mut with_roles);
        assert!(with_roles["making"].as_object().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_v2_headline_grade_writes_no_v1_report() {
        use mtx_grader::scoring_v2::{Ceiling, Params};
        let beside = Params::new(Ceiling::Informed { eta_override: None });
        let one = Params {
            headline: true,
            ..beside.clone()
        };
        let json = r#"{"reward": 0.13, "skill": 1.0}"#;
        // Beside v1, a failed v2 leaves the v1 report, as before.
        assert_eq!(
            super::report_to_write(Some(&beside), true, json),
            Some(json)
        );
        assert_eq!(super::report_to_write(None, false, json), Some(json));
        // As the one score, a failed v2 writes nothing; a scored one, the report.
        assert_eq!(super::report_to_write(Some(&one), true, json), None);
        assert_eq!(super::report_to_write(Some(&one), false, json), Some(json));
    }

    #[cfg(feature = "generate")]
    #[test]
    fn check_joint_takes_the_calibration_flag_repeated_or_with_several_files() {
        let base = ["mtx", "market-cal", "check-joint", "c.toml"];
        for tail in [
            vec!["--calibration", "a.toml", "--calibration", "b.toml"],
            vec!["--calibration", "a.toml", "b.toml"],
        ] {
            let args: Vec<&str> = base.iter().copied().chain(tail).collect();
            let cli = Cli::try_parse_from(args).unwrap();
            let super::Command::MarketCal(super::market_cal::MarketCalCommand::CheckJoint {
                calibration,
                ..
            }) = cli.command
            else {
                panic!("not check-joint");
            };
            assert_eq!(calibration.len(), 2);
        }
        assert!(Cli::try_parse_from(base).is_err());
    }
}
