use std::{io::IsTerminal, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mth_core::{
    grade::{self, GradeOptions},
    making::MakingFamily,
    phoenix,
    scenario::{Scenario, ShockFamily},
    snapshot,
};
use tracing_subscriber::EnvFilter;

mod acp;
mod author;
mod charts;
mod play;
mod ui;

/// The snapshot `scenario new` and `play` start from: the checkout's, by
/// absolute path, so an installed `mth` finds it from any directory.
const DEFAULT_SNAPSHOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../snapshots/phoenix-spcx-2026-10-29"
);

/// Tooling for the mth Solana trading benchmark. Results go to stdout as JSON,
/// logs to stderr.
#[derive(Parser)]
#[command(name = "mth", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The scenario families, one per task.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Family {
    /// phoenix-flash-crash: shocks that crash or squeeze, then revert or not.
    FlashCrash,
    /// phoenix-market-making: a walking, trending, jumping market, competing
    /// makers, and taker flow, some of it informed.
    MarketMaking,
}

#[derive(Subcommand)]
enum Command {
    /// Pinned chain state for offline tasks.
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Market scenarios: the price paths sessions replay.
    #[command(subcommand)]
    Scenario(ScenarioCommand),
    /// Run a bot through scenarios and score it against the reference PnL.
    Grade {
        #[command(flatten)]
        sessions: Sessions,
        /// Write Harbor's reward.json here.
        #[arg(long)]
        reward: Option<PathBuf>,
        /// Write the full per-scenario report here (it also goes to stdout).
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Run the reference bot through scenarios and save its PnL in each file.
    Calibrate {
        #[command(flatten)]
        sessions: Sessions,
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
    #[arg(long, env = "MTH_JOBS", default_value_t = 1)]
    jobs: usize,
    /// Scenario files, run in order.
    #[arg(required = true)]
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
enum ScenarioCommand {
    /// Draw shock scenarios (crashes and squeezes) from the SPCX family, one
    /// file per scenario.
    /// Sets are stratified: each direction and aftermath appears in
    /// proportion to its odds.
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
    #[arg(long, env = "MTH_SNAPSHOT", default_value = DEFAULT_SNAPSHOT)]
    snapshot: PathBuf,
    /// Market the scenario trades; defaults to the snapshot's.
    #[arg(long)]
    symbol: Option<String>,
    /// The `surfpool` binary the agent runs as its MCP server.
    #[arg(long, env = "MTH_SURFPOOL", default_value = "surfpool")]
    surfpool: String,
    /// Scenarios the agent may hand in before mth gives up.
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
            env = "MTH_FORK_RPC_URL",
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
            env = "MTH_FORK_RPC_URL",
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
    let default_filter = if interactive { "warn" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into()),
        )
        .with_writer(ui::LogWriter)
        .init();
    // A terminal reads the progress lines; JSON is for scripts.
    let quiet_stdout = interactive && std::io::stdout().is_terminal();

    let output = match cli.command {
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
            reward,
            report,
        } => {
            let scenarios: Vec<Scenario> = sessions.load()?.into_iter().map(|(_, s)| s).collect();
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
            })
            .await?;
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
        Command::Calibrate { sessions } => {
            let mut scenarios = sessions.load()?;
            let reports = grade::calibrate(
                &sessions.snapshot,
                &sessions.bot,
                &mut scenarios,
                &sessions.logs,
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

fn remove_if_present(path: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| path.display().to_string())
        }
        _ => Ok(()),
    }
}
