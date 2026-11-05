//! Development-only CLI: no verifier, reference bots or scoring entry points.
use clap::{Parser, Subcommand};
use std::path::PathBuf;
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Playground {
        #[arg(long)] market: PathBuf,
        #[arg(long)] snapshot: PathBuf,
        #[arg(long)] programs: PathBuf,
        #[arg(long)] context: PathBuf,
    },
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Args::parse().command {
        Command::Playground {market,snapshot,programs,context} =>
            mth_backrun::playground::run(&market,&snapshot,&programs,&context).await,
    }
}
