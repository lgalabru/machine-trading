use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum Command {
    /// Replay a private recorded trace without latency races or a new score.
    Replay {
        #[arg(long)]
        trace: PathBuf,
    },
    #[command(subcommand)]
    Fixture(FixtureCommand),
    /// Restore only public pool state; no user schedule or scoring endpoint.
    Playground {
        #[arg(long)]
        market: PathBuf,
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        programs: PathBuf,
        #[arg(long)]
        context: PathBuf,
    },
    /// Verify a completed session and calculate its final reward.
    Grade {
        #[arg(long)]
        evidence: PathBuf,
        #[arg(long)]
        fixture: PathBuf,
    },
    /// Inject the fixed user flow and run a continuous Surfpool trading session. Produces no reward.
    Run {
        #[arg(long)]
        fixture: PathBuf,
        #[arg(long)]
        programs: PathBuf,
        #[arg(long)]
        artifact: PathBuf,
        #[arg(long)]
        bootstrap: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "0.0.0.0:8000")]
        listen: String,
    },
    #[command(hide = true)]
    Reference {
        #[arg(long)]
        gateway: String,
        #[arg(long, env = "MTH_BACKRUN_TOKEN")]
        token: String,
        #[arg(long, default_value = "0")]
        strategy: usize,
        #[arg(long)]
        idle: bool,
        /// Maintainer smoke policy; fixed competitors use the pinned defaults.
        #[arg(long, value_parser = clap::value_parser!(u64).range(0..=10000))]
        keep_basis_points: Option<u64>,
    },
}
#[derive(Subcommand)]
pub enum FixtureCommand {
    /// Preserve an existing sealed market and user flow; change execution policy only.
    MigrateExecution {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Build {
        #[arg(long)]
        programs: PathBuf,
        #[arg(long)]
        cpmm_admin: PathBuf,
        #[arg(long)]
        orca_admin: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Verify {
        #[arg(long)]
        fixture: PathBuf,
    },
    /// Export a filtered pool-only playground; never exports flow or user keys.
    Public {
        #[arg(long)]
        fixture: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
}
pub async fn run(command: Command) -> Result<String> {
    let output = match command {
        Command::Replay { trace } => crate::replay::replay(&trace).await?,
        Command::Fixture(FixtureCommand::MigrateExecution { source, out }) => {
            serde_json::json!({"fixture_hash":crate::build::migrate_execution(&source,&out)?})
        }
        Command::Grade { evidence, fixture } => crate::verifier::grade(&evidence, &fixture)?,
        Command::Fixture(FixtureCommand::Build {
            programs,
            cpmm_admin,
            orca_admin,
            out,
        }) => {
            serde_json::json!({"fixture_hash":crate::build::build(&programs,&cpmm_admin,&orca_admin,&out).await?})
        }
        Command::Fixture(FixtureCommand::Verify { fixture }) => {
            serde_json::json!({"fixture_hash":crate::build::verify(&fixture)?})
        }
        Command::Fixture(FixtureCommand::Public { fixture, out }) => {
            crate::build::public(&fixture, &out)?;
            serde_json::json!({"public_assets":out})
        }
        Command::Run {
            fixture,
            programs,
            artifact,
            bootstrap,
            out,
            listen,
        } => {
            crate::runner::run_session(crate::runner::Options {
                fixture,
                programs,
                artifact,
                bootstrap,
                out,
                listen,
            })
            .await?
        }
        Command::Reference {
            gateway,
            token,
            strategy,
            idle,
            keep_basis_points,
        } => {
            let mut strategy = crate::reference::strategies()
                .get(strategy)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown reference strategy"))?;
            if let Some(keep) = keep_basis_points {
                strategy.keep_basis_points = keep;
            }
            tokio::task::spawn_blocking(move || {
                crate::reference::run(gateway, token, strategy, idle)
            })
            .await??;
            serde_json::json!({"finished":true})
        }
        Command::Playground {
            market,
            snapshot,
            programs,
            context,
        } => {
            crate::playground::run(&market, &snapshot, &programs, &context).await?;
            serde_json::json!({"stopped":true})
        }
    };
    Ok(serde_json::to_string_pretty(&output)?)
}
