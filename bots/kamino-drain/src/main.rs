//! A check of a Kamino lending session (`bench/checks/kamino/drain.json`):
//! a process bot that sends away all its SOL on its first turn, so its own
//! transactions can no longer pay a fee. The grade must still complete and
//! value its account.

use anyhow::Result;
use mtx_kit::{
    BotEnv, Keypair, Signer, Turns,
    chain::{InstructionGroup, TransactionPlan},
    rpc_client,
};
use solana_system_interface::instruction::transfer;
use tracing::info;

/// The fee of a one-signature transaction, in lamports.
const FEE: u64 = 5_000;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let env = BotEnv::from_env()?;
    let payer = env.keypair()?;
    let rpc = rpc_client(&env.rpc_url);
    let mut turns = Turns::connect()?;
    let mut drained = false;
    while let Some(step) = turns.next()? {
        if std::mem::replace(&mut drained, true) {
            continue;
        }
        let balance = rpc.get_balance(&payer.pubkey())?;
        let to = Keypair::new().pubkey();
        let group = InstructionGroup::new(
            "drain",
            vec![transfer(&payer.pubkey(), &to, balance.saturating_sub(FEE))],
        );
        let plan = TransactionPlan::from_group(&group, 20_000);
        let blockhash = rpc.get_latest_blockhash()?;
        let mut tx = plan.transaction(&payer.pubkey(), blockhash);
        tx.sign(&[&payer], blockhash);
        let signature = rpc.send_transaction(&tx)?;
        info!(step = step.k, %signature, balance, "drain: sent all SOL away");
    }
    Ok(())
}
