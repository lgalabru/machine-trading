//! Deterministic in-memory [`ChainClient`] for tests.
//!
//! Accounts are a map; transactions are recorded, not executed. Tests script
//! simulation results and receipts per transaction so that execution flows
//! can be exercised end to end without a node.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

use async_trait::async_trait;
use solana_account::Account;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::filter::RpcFilterType;
use solana_transaction::{Hash, Signature, TransactionError, versioned::VersionedTransaction};

use crate::chain::{
    client::{ChainClient, SimulationOutcome, TransactionSummary},
    error::ChainError,
};

#[derive(Default)]
struct State {
    accounts: HashMap<Pubkey, Account>,
    blockhash: Hash,
    slot: u64,
    sent: Vec<VersionedTransaction>,
    simulated: Vec<VersionedTransaction>,
    simulations: VecDeque<SimulationOutcome>,
    receipts: HashMap<Signature, TransactionSummary>,
    receipt_requests: usize,
    send_failures: VecDeque<TransactionError>,
}

/// Scriptable chain.
#[derive(Default)]
pub struct MockChainClient {
    state: Mutex<State>,
}

impl MockChainClient {
    /// An empty chain with a zero blockhash at slot zero.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("mock state lock poisoned")
    }

    /// Inserts or replaces an account.
    pub fn set_account(&self, key: Pubkey, account: Account) -> &Self {
        self.state().accounts.insert(key, account);
        self
    }

    /// Removes an account.
    pub fn remove_account(&self, key: &Pubkey) -> &Self {
        self.state().accounts.remove(key);
        self
    }

    /// Sets the blockhash returned by [`ChainClient::get_latest_blockhash`].
    pub fn set_blockhash(&self, hash: Hash) -> &Self {
        self.state().blockhash = hash;
        self
    }

    /// Sets the slot returned by [`ChainClient::get_slot`].
    pub fn set_slot(&self, slot: u64) -> &Self {
        self.state().slot = slot;
        self
    }

    /// Queues a simulation outcome; outcomes are consumed in order. When the
    /// queue is empty, simulations succeed with no logs.
    pub fn push_simulation(&self, outcome: SimulationOutcome) -> &Self {
        self.state().simulations.push_back(outcome);
        self
    }

    /// Queues a send failure for the next [`ChainClient::send_and_confirm`].
    pub fn push_send_failure(&self, error: TransactionError) -> &Self {
        self.state().send_failures.push_back(error);
        self
    }

    /// Registers the receipt returned for a signature.
    pub fn set_receipt(&self, summary: TransactionSummary) -> &Self {
        self.state().receipts.insert(summary.signature, summary);
        self
    }

    /// Transactions sent so far.
    pub fn sent(&self) -> Vec<VersionedTransaction> {
        self.state().sent.clone()
    }

    /// Transactions simulated so far.
    pub fn simulated(&self) -> Vec<VersionedTransaction> {
        self.state().simulated.clone()
    }

    /// How many times a receipt was requested.
    pub fn receipt_requests(&self) -> usize {
        self.state().receipt_requests
    }
}

#[async_trait]
impl ChainClient for MockChainClient {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, ChainError> {
        Ok(self.state().accounts.get(pubkey).cloned())
    }

    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, ChainError> {
        let s = self.state();
        Ok(pubkeys.iter().map(|k| s.accounts.get(k).cloned()).collect())
    }

    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filters: Vec<RpcFilterType>,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError> {
        let s = self.state();
        let mut out: Vec<(Pubkey, Account)> = s
            .accounts
            .iter()
            .filter(|(_, a)| &a.owner == program)
            .filter(|(_, a)| {
                filters.iter().all(|f| match f {
                    RpcFilterType::DataSize(size) => a.data.len() as u64 == *size,
                    RpcFilterType::Memcmp(memcmp) => memcmp.bytes_match(&a.data),
                    RpcFilterType::TokenAccountState => false,
                })
            })
            .map(|(k, a)| (*k, a.clone()))
            .collect();
        out.sort_by_key(|(k, _)| k.to_bytes());
        Ok(out)
    }

    async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, ChainError> {
        Ok(self.state().accounts.get(pubkey).map_or(0, |a| a.lamports))
    }

    async fn get_slot(&self) -> Result<u64, ChainError> {
        Ok(self.state().slot)
    }

    async fn get_latest_blockhash(&self) -> Result<Hash, ChainError> {
        Ok(self.state().blockhash)
    }

    async fn simulate(
        &self,
        transaction: &VersionedTransaction,
        watch: &[Pubkey],
    ) -> Result<SimulationOutcome, ChainError> {
        let mut s = self.state();
        s.simulated.push(transaction.clone());
        let mut outcome = s.simulations.pop_front().unwrap_or_default();
        if outcome.accounts.is_empty() {
            outcome.accounts = watch
                .iter()
                .map(|k| (*k, s.accounts.get(k).cloned()))
                .collect();
        }
        Ok(outcome)
    }

    async fn send_and_confirm(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<Signature, ChainError> {
        let mut s = self.state();
        if let Some(error) = s.send_failures.pop_front() {
            return Err(ChainError::Failed {
                label: String::new(),
                signature: transaction.signatures[0],
                error,
            });
        }
        s.sent.push(transaction.clone());
        Ok(transaction.signatures[0])
    }

    async fn get_transaction(
        &self,
        signature: &Signature,
    ) -> Result<Option<TransactionSummary>, ChainError> {
        let mut s = self.state();
        s.receipt_requests += 1;
        Ok(s.receipts.get(signature).cloned())
    }
}
