use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_keypair::Keypair;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::{Instruction, Signature, Transaction};
use tracing::info;

use crate::{Error, Result};

/// Phoenix instructions routinely exceed the 200k default.
const COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// An RPC client that waits for `confirmed`, not the default `finalized`,
/// which on a surfnet costs ~8s per transaction.
pub fn rpc_client(url: impl Into<String>) -> RpcClient {
    RpcClient::new_with_commitment(url.into(), CommitmentConfig::confirmed())
}

/// Sign, simulate, send and confirm `instructions` with a raised compute limit.
pub fn send(
    rpc: &RpcClient,
    payer: &Keypair,
    instructions: Vec<Instruction>,
    label: &str,
) -> Result<Signature> {
    let mut all = vec![ComputeBudgetInstruction::set_compute_unit_limit(
        COMPUTE_UNIT_LIMIT,
    )];
    all.extend(instructions);
    let blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(&all, Some(&payer.pubkey()), &[payer], blockhash);

    // Simulate first: a failed send only says "failed", the simulation logs say why.
    let simulation = rpc.simulate_transaction(&tx)?.value;
    if let Some(err) = simulation.err {
        let logs = simulation.logs.unwrap_or_default();
        let tail = logs[logs.len().saturating_sub(12)..].join("\n");
        return Err(Error::Transaction {
            label: label.to_string(),
            reason: format!("{err}\n{tail}"),
        });
    }
    let signature = rpc.send_and_confirm_transaction(&tx)?;
    info!(%signature, label, "transaction confirmed");
    Ok(signature)
}
