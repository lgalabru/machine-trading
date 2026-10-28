use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ott_core::{
    grade::{self, GradeOptions},
    phoenix,
    scenario::{CrashFamily, Scenario},
    snapshot,
};
use tracing_subscriber::EnvFilter;

/// Tooling for the ott Solana trading benchmark. Results go to stdout as JSON,
/// logs to stderr.
#[derive(Parser)]
#[command(name = "ott", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
    #[arg(long, env = "OTT_JOBS", default_value_t = 1)]
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
    /// Draw flash-crash scenarios from the SOL family, one file per scenario.
    /// Sets are stratified: each aftermath (rebound, stall, slide) appears in
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
    },
}

#[derive(Subcommand)]
enum SnapshotCommand {
    /// Fork mainnet, warm up the Phoenix accounts a task touches, and export them.
    Prepare {
        /// Output directory, e.g. snapshots/phoenix-sol-2026-10-27.
        #[arg(long)]
        out: PathBuf,
        /// Datasource to fork. May embed an API key; only its host is recorded.
        #[arg(
            long,
            env = "OTT_FORK_RPC_URL",
            default_value = "https://api.mainnet-beta.solana.com"
        )]
        fork_rpc: String,
        #[arg(long, default_value = phoenix::DEFAULT_API_URL)]
        phoenix_api: String,
        #[arg(long, default_value = "SOL")]
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
            env = "OTT_FORK_RPC_URL",
            default_value = "https://api.mainnet-beta.solana.com"
        )]
        fork_rpc: String,
        #[arg(long, default_value = phoenix::DEFAULT_API_URL)]
        phoenix_api: String,
        #[arg(long, default_value = "SOL")]
        symbol: String,
        #[arg(long, default_value_t = 8.0)]
        crash_pct: f64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    let output = match Cli::parse().command {
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
        }) => {
            std::fs::create_dir_all(&out).with_context(|| out.display().to_string())?;
            let family = CrashFamily::sol();
            let mut written = Vec::new();
            for scenario in family.sample_set(&prefix, count, seed) {
                let path = out.join(format!("{}.json", scenario.name));
                scenario.save(&path)?;
                written.push(path);
            }
            serde_json::to_string_pretty(&written)?
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
    println!("{output}");
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
