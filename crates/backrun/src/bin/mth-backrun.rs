use clap::Parser;
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: mth_backrun::cli::Command,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("{}", mth_backrun::cli::run(Args::parse().command).await?);
    Ok(())
}
