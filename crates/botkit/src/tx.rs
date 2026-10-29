use luce::{
    chain::{InstructionGroup, TransactionPlan},
    protocols::phoenix::COMPUTE_UNIT_LIMIT,
};
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::{Instruction, Signature};
use tracing::info;

use crate::{Error, Result};

/// An RPC client that waits for `confirmed`, not the default `finalized`,
/// which on a surfnet costs ~8s per transaction.
pub fn rpc_client(url: impl Into<String>) -> RpcClient {
    RpcClient::new_with_commitment(url.into(), CommitmentConfig::confirmed())
}

/// Sign, simulate, send and confirm `instructions` with a raised compute
/// limit, on the blocking RPC client.
///
/// The transaction is built by luce's [`TransactionPlan`]; this is the
/// blocking counterpart of [`luce::chain::execute`], for callers without an
/// async runtime at hand (the grader's chain setup, [`Trader::cancel_all`](crate::Trader::cancel_all)).
pub fn send(
    rpc: &RpcClient,
    payer: &Keypair,
    instructions: Vec<Instruction>,
    label: &str,
) -> Result<Signature> {
    let plan = TransactionPlan::from_group(
        &InstructionGroup::new(label, instructions),
        COMPUTE_UNIT_LIMIT,
    );
    let blockhash = rpc.get_latest_blockhash()?;
    let mut tx = plan.transaction(&payer.pubkey(), blockhash);
    tx.sign(&[payer], blockhash);

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
