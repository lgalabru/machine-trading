use std::path::Path;

use anyhow::Result;
use solana_signer::Signer;
use surfpool_sdk::cheatcodes::builders::DeployProgram;

use crate::{
    fixture::Market, runner::REFERENCE_EXECUTOR, sdk::Context, wallet::Wallet, world::World,
};

pub async fn run(market: &Path, snapshot: &Path, programs: &Path, context: &Path) -> Result<()> {
    let mut market: Market = serde_json::from_slice(&std::fs::read(market)?)?;
    market.validate()?;
    market.hydrate();
    let world = World::restore(snapshot).await?;
    world
        .surfnet
        .cheatcodes()
        .fund_sol(&world.payer().pubkey(), 1_000_000_000_000_000_000)?;
    world
        .surfnet
        .cheatcodes()
        .time_travel_to_slot_at(100_000, 1_800_000_000_000)?;
    world.surfnet.cheatcodes().deploy(
        DeployProgram::new(REFERENCE_EXECUTOR).so_path(programs.join("mth_backrun_executor.so")),
    )?;
    world.make_immutable(REFERENCE_EXECUTOR)?;
    let session = Wallet::prepare(&world, &market, REFERENCE_EXECUTOR).await?;
    let addresses = crate::wallet::lookup_addresses(&market, &session);
    let mut tables = vec![];
    for chunk in addresses.chunks(256) {
        tables.push(world.lookup_table(chunk.to_vec()).await?);
    }
    let context_value = Context::new(0, market, &session, &tables);
    std::fs::write(context, serde_json::to_vec_pretty(&context_value)?)?;
    println!(
        "{}",
        serde_json::json!({"mode":"unscored_pool_playground","rpc_url":world.surfnet.rpc_url(),"context":context,"ws_url":world.surfnet.ws_url(),"reset":"restart this command"})
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}
