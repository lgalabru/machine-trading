//! Maintainer-only exact replay. Reuses signed transactions in their recorded
//! order, with signature/blockhash checks enabled and no simulated fills.
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, prelude::BASE64_STANDARD};
use serde_json::{Value, json};

use crate::world::World;

pub async fn replay(root: &Path) -> Result<Value> {
    let metadata: Value = serde_json::from_slice(&fs::read(root.join("replay.json"))?)?;
    let base = metadata["base_slot"]
        .as_u64()
        .context("missing replay slot")?;
    let timestamp = metadata["timestamp_ms"]
        .as_u64()
        .context("missing replay timestamp")?;
    let records = BufReader::new(fs::File::open(root.join("trace.jsonl"))?)
        .lines()
        .map(|line| Ok(serde_json::from_str::<Value>(&line?)?))
        .collect::<Result<Vec<_>>>()?;
    let first = records
        .iter()
        .find_map(|r| r["transaction"].as_str())
        .context("empty replay trace")?;
    let first_tx: solana_transaction::versioned::VersionedTransaction =
        wincode::deserialize(&BASE64_STANDARD.decode(first)?)?;
    fn index(hash: &str) -> Result<u64> {
        ensure!(
            hash.starts_with("SURFNETxSAFEHASHx") && hash.len() >= 8,
            "unexpected pinned runtime blockhash"
        );
        Ok(u64::from_str_radix(
            &hash[hash.len() - 8..].replace('x', "0"),
            16,
        )?)
    }
    let target = index(
        metadata["initial_blockhash"]
            .as_str()
            .unwrap_or(&first_tx.message.recent_blockhash().to_string()),
    )?;
    ensure!(
        (2..100_000).contains(&target),
        "replay setup block count out of bounds"
    );
    let world = World::restore(&root.join("session-snapshot.json")).await?;
    // Surfpool's synthetic hash counts produced blocks, separately from the
    // logical slot after time travel. Reconstruct that count with empty blocks;
    // retain signature and recent-blockhash validation throughout replay.
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::UpdateBlockProductionMode(
            surfpool_types::BlockProductionMode::Manual,
        ))?;
    while index(&world.rpc.get_latest_blockhash()?.to_string())? < target - 2 {
        world.advance_slot().await?;
    }
    world
        .surfnet
        .cheatcodes()
        .time_travel_to_slot_at(base, timestamp)?;
    world.advance_slot().await?;
    world
        .surfnet
        .cheatcodes()
        .lock(surfpool_sdk::CheatcodeFilter::all())?;
    let mut slot = 0;
    let mut count = 0;
    let mut fees = 0u64;
    for record in records {
        let Some(encoded) = record["transaction"].as_str() else {
            continue;
        };
        let next = record["slot"].as_u64().context("missing logical slot")?;
        ensure!(next >= slot, "reordered replay slots");
        while slot < next {
            world.advance_slot().await?;
            slot += 1;
        }
        let transaction: solana_transaction::versioned::VersionedTransaction =
            wincode::deserialize(&BASE64_STANDARD.decode(encoded)?)?;
        let receipt = world.execute(&transaction).await?;
        for field in [
            "err",
            "fee",
            "preBalances",
            "postBalances",
            "preTokenBalances",
            "postTokenBalances",
        ] {
            ensure!(
                receipt["meta"][field] == record["receipt"]["meta"][field],
                "replay mismatch at transaction {count}, field {field}"
            );
        }
        fees += receipt["meta"]["fee"].as_u64().context("missing fee")?;
        count += 1;
    }
    ensure!(count > 0, "empty replay trace");
    Ok(
        json!({"verified":true,"transactions":count,"fees_lamports":fees,"fixture_hash":metadata["fixture_hash"]}),
    )
}
