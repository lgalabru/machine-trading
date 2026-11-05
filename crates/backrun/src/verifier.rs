//! Post-session verification from trusted snapshots and native ledger receipts.
use std::{collections::BTreeSet, fs, path::Path};

use anyhow::{Context, Result, ensure};
use base64::{Engine, prelude::BASE64_STANDARD};
use serde_json::{Value, json};

use crate::score::Score;
pub fn grade(root: &Path, fixture: &Path) -> Result<Value> {
    ensure!(
        !root.join("infrastructure-error.json").exists(),
        "infrastructure failure cannot receive a trading score"
    );
    let hash = crate::build::verify(fixture)?;
    let mut e: Value = serde_json::from_slice(&fs::read(root.join("session.json"))?)?;
    ensure!(
        e["protocol"] == crate::PROTOCOL && e["fixture_hash"] == hash,
        "wrong fixture or protocol"
    );
    if let Some(reason) = e["terminal_disqualification"].as_str() {
        e["participants"] = json!([{"participant":0,"score":{"valid":false,"disqualification":reason,"reward":crate::score::DISQUALIFIED_REWARD}}]);
    } else {
        let schedule = crate::injector::FlowSchedule::load(fixture)?;
        ensure!(
            e["completed_slots"] == schedule.rounds.len()
                && e["timing"]["slots"]
                    .as_array()
                    .is_some_and(|v| v.len() == schedule.rounds.len())
                && e["timing"]["coalesced_slots"] == 0,
            "incomplete or degraded session"
        );
        let initial: Value =
            serde_json::from_slice(&fs::read(root.join("session-snapshot.json"))?)?;
        let final_state: Value =
            serde_json::from_slice(&fs::read(root.join("final-snapshot.json"))?)?;
        let orders: Vec<Value> = serde_json::from_slice(&fs::read(root.join("user-orders.json"))?)?;
        let expected = schedule
            .rounds
            .iter()
            .flat_map(|r| r.orders.iter().map(move |o| (r.round, *o)))
            .collect::<Vec<_>>();
        ensure!(
            orders.len() == expected.len() && e["user_executions"] == orders.len(),
            "missing user orders"
        );
        let records = fs::read_to_string(root.join("trace.jsonl"))?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let submissions: Vec<Value> =
            serde_json::from_slice(&fs::read(root.join("submissions.json"))?)?;
        let submitted = submissions
            .iter()
            .filter_map(|s| {
                Some((
                    crate::gateway::submission_signature(s)?,
                    s["participant"].as_u64()? as usize,
                ))
            })
            .collect::<BTreeSet<_>>();
        let mut signatures = BTreeSet::new();
        let mut fees = [0u64; 4];
        let mut priorities = [0u64; 4];
        let mut executions = [0u64; 4];
        let mut failures = [0u64; 4];
        let mut previous = 0;
        for r in &records {
            let slot = r["slot"].as_u64().context("missing slot")?;
            ensure!(slot >= previous, "reordered slots");
            previous = slot;
            let sig = r["signature"].as_str().context("missing signature")?;
            ensure!(
                signatures.insert(sig.to_owned()),
                "duplicate ledger transaction"
            );
            let tx: solana_transaction::versioned::VersionedTransaction = wincode::deserialize(
                &BASE64_STANDARD.decode(r["transaction"].as_str().context("missing wire")?)?,
            )?;
            tx.verify_and_hash_message()?;
            ensure!(
                tx.signatures[0].to_string() == sig,
                "wire signature mismatch"
            );
            if let Some(i) = r["participant"].as_u64() {
                let i = i as usize;
                ensure!(i < 4, "invalid participant");
                ensure!(
                    tx.message.static_account_keys()[0].to_string()
                        == e["participants"][i]["wallet"]
                            .as_str()
                            .context("missing wallet")?
                        || submitted.contains(&(sig.to_owned(), i)),
                    "payer mismatch"
                );
                let fee = r["receipt"]["meta"]["fee"]
                    .as_u64()
                    .context("missing fee")?;
                fees[i] += fee;
                priorities[i] += crate::score::priority_fee(&tx, fee)?;
                executions[i] += 1;
                if !r["receipt"]["meta"]["err"].is_null() {
                    failures[i] += 1;
                }
            }
        }
        let mut market: crate::fixture::Market =
            serde_json::from_slice(&fs::read(fixture.join("market.json"))?)?;
        market.hydrate();
        let flow: crate::fixture::PrivateFlow =
            serde_json::from_slice(&fs::read(fixture.join("flow.json"))?)?;
        let users: Vec<Vec<u8>> = serde_json::from_slice(&fs::read(fixture.join("users.json"))?)?;
        for (order, (slot, index)) in orders.iter().zip(expected) {
            ensure!(
                order["slot"] == slot && order["order"] == index,
                "fixed flow changed"
            );
            let tx: solana_transaction::versioned::VersionedTransaction = wincode::deserialize(
                &BASE64_STANDARD.decode(
                    order["transaction"]
                        .as_str()
                        .context("missing order wire")?,
                )?,
            )?;
            use solana_signer::Signer;
            let fixed = &flow.orders[index];
            let user = solana_keypair::Keypair::try_from(users[fixed.wallet].as_slice())?;
            let leg = &fixed.route[0];
            let pool = &market.pools[leg.pool];
            let expected_swap = match pool.kind {
                crate::fixture::PoolKind::ConstantProduct => crate::programs::cpmm_swap(
                    pool,
                    user.pubkey(),
                    leg.a_to_b,
                    fixed.amount_in,
                    fixed.min_out,
                ),
                crate::fixture::PoolKind::Concentrated => crate::programs::whirlpool_swap(
                    pool,
                    user.pubkey(),
                    leg.a_to_b,
                    fixed.amount_in,
                    fixed.min_out,
                    0,
                )?,
            };
            ensure!(
                tx.message.instructions().len() == 2
                    && tx.message.static_account_keys()[0] == user.pubkey(),
                "user transaction shape changed"
            );
            let swap = &tx.message.instructions()[1];
            ensure!(
                swap.data == expected_swap.data
                    && tx.message.static_account_keys()[swap.program_id_index as usize]
                        == expected_swap.program_id
                    && swap.accounts.len() == expected_swap.accounts.len(),
                "fixed user swap changed"
            );
            for (n, (actual, expected)) in swap
                .accounts
                .iter()
                .zip(&expected_swap.accounts)
                .enumerate()
            {
                // Whirlpool's three tick-array addresses depend on live state.
                if pool.kind == crate::fixture::PoolKind::Concentrated && (7..10).contains(&n) {
                    continue;
                }
                ensure!(
                    tx.message.static_account_keys()[*actual as usize] == expected.pubkey,
                    "fixed user swap account changed"
                );
            }
            ensure!(
                signatures.contains(&tx.signatures[0].to_string()),
                "user order not in ledger"
            );
        }
        let participants = e["participants"]
            .as_array_mut()
            .context("missing participants")?;
        ensure!(participants.len() == 4, "incomplete participants");
        for (i, p) in participants.iter_mut().enumerate() {
            let wallet = p["wallet"].as_str().context("missing wallet")?;
            let scratch: Vec<String> = serde_json::from_value(p["scratch"].clone())?;
            let start = crate::score::capital(&initial, wallet, &scratch)?;
            let end = crate::score::capital(&final_state, wallet, &scratch)?;
            ensure!(
                p["initial_value_lamports"] == start && p["final_value_lamports"] == end,
                "snapshot capital mismatch"
            );
            ensure!(
                p["fees"] == fees[i]
                    && p["executions"] == executions[i]
                    && p["included_failures"] == failures[i]
                    && p["priority_fees"] == priorities[i],
                "receipt accounting mismatch"
            );
            p["score"] =
                serde_json::to_value(Score::from_capital(start, end, fees[i], priorities[i])?)?;
        }
    }
    e["valid"] = e["participants"][0]["score"]["valid"].clone();
    e["grading_phase"] = json!("post-run");
    e["evidence_sha256"] = json!(crate::fixture::hash_file(&root.join("session.json"))?);
    let reward = e["participants"][0]["score"]["reward"]
        .as_f64()
        .context("missing reward")?;
    fs::write(root.join("report.json"), serde_json::to_vec_pretty(&e)?)?;
    fs::write(root.join("reward.txt"), format!("{reward:.12}\n"))?;
    Ok(json!({"reward":reward,"fixture_hash":hash}))
}
