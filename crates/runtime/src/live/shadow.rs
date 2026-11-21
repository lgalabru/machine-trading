//! Shadow sends (`docs/rfd/0004`, step 6): a strategy that steps, plans and
//! signs its sheets on the real chain, and sends nothing. Each transaction
//! it would have sent is written down instead, with the slot and the time,
//! so a shadow run says what the strategy would have done, and how long
//! from a slot to a signed transaction.

use std::{
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use mtx_kit::chain::{
    ChainClient, ChainError, RpcChainClient, SimulationOutcome, TransactionSummary,
};
use serde::Serialize;
use solana_account::Account;
use solana_pubkey::Pubkey;
use solana_transaction::{Hash, Signature, versioned::VersionedTransaction};

/// One transaction a shadow strategy would have sent: a line of its
/// journal.
#[derive(Debug, Serialize)]
struct WouldSend {
    /// The slot the node was at when it was signed.
    slot: Option<u64>,
    /// When, in milliseconds since the Unix epoch.
    unix_ms: u128,
    signature: String,
    /// Its size on the wire, in bytes.
    bytes: usize,
    instructions: usize,
    /// Why the node's simulation of it failed, if it did: in a shadow run
    /// a key may hold no SOL and have no account on the venue, so its
    /// transactions are written down whatever the simulation says.
    simulation_error: Option<String>,
}

/// The chain a shadow strategy trades through: its reads are the node's,
/// its sends are written to a journal and go nowhere.
pub struct ShadowChain {
    rpc: RpcChainClient,
    /// The strategy whose sends these are, for their metrics.
    strategy: String,
    journal: Arc<Mutex<std::fs::File>>,
    /// The last simulation's failure, for the send that follows it.
    simulated: Mutex<Option<String>>,
}

impl ShadowChain {
    /// `strategy`'s shadow sends, written to `journal`, appended to.
    pub fn new(rpc: RpcChainClient, journal: &Path, strategy: &str) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal)
            .map_err(|e| format!("{}: {e}", journal.display()))?;
        Ok(Self {
            rpc,
            strategy: strategy.to_string(),
            journal: Arc::new(Mutex::new(file)),
            simulated: Mutex::new(None),
        })
    }

    async fn write_down(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<Signature, ChainError> {
        let signature = *transaction
            .signatures
            .first()
            .ok_or_else(|| ChainError::Rpc("an unsigned transaction".into()))?;
        let line = WouldSend {
            // The slot is what the node says now: a read that fails leaves
            // it out rather than failing a send that goes nowhere.
            slot: self.rpc.get_slot().await.ok(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis()),
            signature: signature.to_string(),
            bytes: wincode::serialize(transaction).map_or(0, |wire| wire.len()),
            instructions: transaction.message.instructions().len(),
            simulation_error: self
                .simulated
                .lock()
                .map_err(|_| ChainError::Rpc("the shadow chain was poisoned".into()))?
                .take(),
        };
        let mut text = serde_json::to_string(&line).map_err(|e| ChainError::Rpc(e.to_string()))?;
        text.push('\n');
        self.journal
            .lock()
            .map_err(|_| ChainError::Rpc("the shadow journal was poisoned".into()))?
            .write_all(text.as_bytes())
            .map_err(|e| ChainError::Rpc(format!("the shadow journal: {e}")))?;
        tracing::info!(
            counter.mtx_shadow_sends = 1_u64,
            strategy = %self.strategy,
            simulated = line.simulation_error.is_none(),
            signature = %line.signature,
            bytes = line.bytes,
            "a shadow send, written down"
        );
        Ok(signature)
    }
}

#[async_trait]
impl ChainClient for ShadowChain {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, ChainError> {
        self.rpc.get_account(pubkey).await
    }

    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, ChainError> {
        self.rpc.get_multiple_accounts(pubkeys).await
    }

    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filters: Vec<solana_rpc_client_api::filter::RpcFilterType>,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError> {
        self.rpc.get_program_accounts(program, filters).await
    }

    async fn get_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError> {
        self.rpc.get_token_accounts_by_owner(owner, mint).await
    }

    async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, ChainError> {
        self.rpc.get_balance(pubkey).await
    }

    async fn get_slot(&self) -> Result<u64, ChainError> {
        self.rpc.get_slot().await
    }

    async fn get_latest_blockhash(&self) -> Result<Hash, ChainError> {
        self.rpc.get_latest_blockhash().await
    }

    async fn simulate(
        &self,
        transaction: &VersionedTransaction,
        watch: &[Pubkey],
    ) -> Result<SimulationOutcome, ChainError> {
        // The node simulates it for real; a failure is written down with the
        // transaction rather than stopping it, since a shadow key may hold
        // nothing the transaction needs.
        let (outcome, failure) = match self.rpc.simulate(transaction, watch).await {
            Ok(outcome) => {
                let failure = outcome.error.as_ref().map(ToString::to_string);
                (
                    SimulationOutcome {
                        error: None,
                        ..outcome
                    },
                    failure,
                )
            }
            Err(error) => (
                SimulationOutcome {
                    units_consumed: None,
                    logs: Vec::new(),
                    error: None,
                    accounts: Vec::new(),
                },
                Some(error.to_string()),
            ),
        };
        *self
            .simulated
            .lock()
            .map_err(|_| ChainError::Rpc("the shadow chain was poisoned".into()))? = failure;
        Ok(outcome)
    }

    /// Written down, never sent: it "confirms" at once, so the desk goes on
    /// planning as if it had.
    async fn send_and_confirm(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<Signature, ChainError> {
        self.write_down(transaction).await
    }

    async fn send(&self, transaction: &VersionedTransaction) -> Result<Signature, ChainError> {
        self.write_down(transaction).await
    }

    /// Nothing was sent, so there is nothing to find.
    async fn get_transaction(
        &self,
        _signature: &Signature,
    ) -> Result<Option<TransactionSummary>, ChainError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use solana_keypair::Keypair;
    use solana_signer::Signer;
    use solana_transaction::Transaction;

    use super::*;

    #[tokio::test]
    async fn a_shadow_send_is_written_down_and_goes_nowhere() {
        let dir = std::env::temp_dir().join(format!("mtx-shadow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let journal = dir.join("shadow.jsonl");
        // A node that isn't there: a shadow send needs none.
        let chain =
            ShadowChain::new(RpcChainClient::new("http://127.0.0.1:1"), &journal, "test").unwrap();
        let payer = Keypair::new();
        let transaction = VersionedTransaction::from(Transaction::new_signed_with_payer(
            &[],
            Some(&payer.pubkey()),
            &[&payer],
            Hash::default(),
        ));
        let signature = chain.send_and_confirm(&transaction).await.unwrap();
        assert_eq!(signature, transaction.signatures[0]);
        let text = std::fs::read_to_string(&journal).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(line["signature"], signature.to_string());
        assert_eq!(line["slot"], serde_json::Value::Null);
        assert!(line["bytes"].as_u64().unwrap() > 0);
        assert_eq!(chain.get_transaction(&signature).await.unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
