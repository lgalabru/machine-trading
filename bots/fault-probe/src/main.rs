//! A bot that meets execution faults and recovers, for the fault smoke.
//!
//! It sends a self-transfer and reads its status the way a careful client
//! does. A send that is accepted but whose status stays unknown once its
//! turn has passed was dropped: it sends again. A send refused as an expired
//! blockhash is sent again on a fresh one. A status that reads unknown after
//! the transaction landed is waited out, not sent a third time. Every
//! finding is logged as a `probe:` line, which the smoke checks.

use mtx_kit::{
    Keypair, RpcClient, Signature, Signer,
    chain::{InstructionGroup, TransactionPlan},
    prelude::*,
    rpc_client,
};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_system_interface::instruction::transfer;
use tracing::info;

enum Phase {
    /// Nothing sent yet.
    Fresh,
    /// A send awaits its status: unknown now means it was dropped.
    Sent(Signature),
    /// The replacement was sent, and its status is awaited.
    Resent(Signature),
    Done,
}

struct Probe {
    rpc: Option<RpcClient>,
    payer: Option<Keypair>,
    phase: Phase,
    /// Lamports of the next transfer: each send is a different transaction.
    amount: u64,
}

impl Probe {
    fn send(&mut self) -> Result<Signature, String> {
        let (rpc, payer) = (self.rpc.as_ref().unwrap(), self.payer.as_ref().unwrap());
        self.amount += 1;
        let mut instructions = vec![ComputeBudgetInstruction::set_compute_unit_price(5_000)];
        instructions.push(transfer(&payer.pubkey(), &payer.pubkey(), self.amount));
        let plan =
            TransactionPlan::from_group(&InstructionGroup::new("probe", instructions), 20_000);
        let blockhash = rpc.get_latest_blockhash().map_err(|e| e.to_string())?;
        let mut tx = plan.transaction(&payer.pubkey(), blockhash);
        tx.sign(&[payer], blockhash);
        rpc.send_transaction(&tx).map_err(|e| e.to_string())
    }

    /// Whether the chain knows `signature`.
    fn known(&self, signature: &Signature) -> bool {
        self.rpc
            .as_ref()
            .unwrap()
            .get_signature_statuses(&[*signature])
            .map(|r| r.value[0].is_some())
            .unwrap_or(false)
    }
}

impl Strategy for Probe {
    async fn start(&mut self, session: &Session) -> BotResult {
        self.rpc = Some(rpc_client(&session.env.rpc_url));
        self.payer = Some(session.env.keypair()?);
        Ok(())
    }

    async fn step(&mut self, market: &Market, _orders: &mut Orders<'_>) -> BotResult {
        let k = market.k;
        match std::mem::replace(&mut self.phase, Phase::Done) {
            Phase::Fresh => {
                let signature = self.send()?;
                info!(k, %signature, "probe: sent");
                self.phase = Phase::Sent(signature);
            }
            Phase::Sent(signature) if self.known(&signature) => {
                info!(k, %signature, "probe: landed");
            }
            Phase::Sent(signature) => {
                info!(k, %signature, "probe: dropped send seen, status still unknown");
                let mut tries = 0;
                loop {
                    tries += 1;
                    match self.send() {
                        Ok(signature) => {
                            info!(k, %signature, tries, "probe: resent");
                            self.phase = Phase::Resent(signature);
                            break;
                        }
                        Err(error) if error.contains("Blockhash not found") && tries < 4 => {
                            info!(
                                k,
                                tries, "probe: expired blockhash seen, trying a fresh one"
                            );
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            Phase::Resent(signature) if self.known(&signature) => {
                info!(k, %signature, "probe: recovered, status known");
            }
            Phase::Resent(signature) => {
                info!(k, %signature, "probe: status unknown, waiting");
                self.phase = Phase::Resent(signature);
            }
            Phase::Done => {}
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Probe {
        rpc: None,
        payer: None,
        phase: Phase::Fresh,
        amount: 0,
    })
}
