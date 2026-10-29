//! Chain access behind a trait.
//!
//! [`ChainClient`] is the narrow set of node operations this crate needs. The
//! production implementation, [`RpcChainClient`], wraps the non-blocking RPC
//! client at `confirmed` commitment; tests use
//! [`MockChainClient`](crate::chain::mock::MockChainClient). Keeping the
//! surface small is deliberate: every method here is something we have to be
//! able to fake deterministically.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use solana_account::Account;
use solana_account_decoder_client_types::{UiAccount, UiAccountData, UiAccountEncoding};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::{
    client_error::Error as ClientError,
    config::{
        RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSimulateTransactionAccountsConfig,
        RpcSimulateTransactionConfig, RpcTransactionConfig,
    },
    filter::RpcFilterType,
};
use solana_transaction::{Hash, Signature, TransactionError, versioned::VersionedTransaction};
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding, UiTransactionTokenBalance,
    option_serializer::OptionSerializer,
};

use crate::chain::error::ChainError;

/// Result of simulating one transaction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SimulationOutcome {
    /// Compute units the transaction consumed.
    pub units_consumed: Option<u64>,
    /// Program logs.
    pub logs: Vec<String>,
    /// The failure, if the transaction would fail.
    pub error: Option<TransactionError>,
    /// Post-simulation state of the watched accounts, in request order.
    /// `None` means the account would not exist after the transaction.
    pub accounts: Vec<(Pubkey, Option<Account>)>,
}

impl SimulationOutcome {
    /// Post-simulation lamports of a watched account, if it was watched.
    pub fn lamports_of(&self, key: &Pubkey) -> Option<u64> {
        self.accounts
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, acct)| acct.as_ref().map_or(0, |a| a.lamports))
    }
}

/// Token balance of one account before and after a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBalanceChange {
    /// The token account.
    pub account: Pubkey,
    /// Its mint.
    pub mint: Pubkey,
    /// Its owner, when the node reports it.
    pub owner: Option<Pubkey>,
    /// Balance before, in native units. Zero if the account did not exist.
    pub pre_amount: u64,
    /// Balance after, in native units. Zero if the account was closed.
    pub post_amount: u64,
}

/// What a confirmed transaction did, as far as balances are concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionSummary {
    /// The transaction.
    pub signature: Signature,
    /// Slot it landed in.
    pub slot: u64,
    /// Block time, when the node reports it.
    pub block_time: Option<DateTime<Utc>>,
    /// Fee paid by the payer, in lamports.
    pub fee_lamports: u64,
    /// On-chain failure, if the transaction landed but failed.
    pub error: Option<TransactionError>,
    /// Program logs, when the node reports them.
    pub logs: Vec<String>,
    /// Every account the transaction touched, in index order.
    pub account_keys: Vec<Pubkey>,
    /// Lamports per account before.
    pub pre_balances: Vec<u64>,
    /// Lamports per account after.
    pub post_balances: Vec<u64>,
    /// Token balance changes.
    pub token_balance_changes: Vec<TokenBalanceChange>,
}

impl TransactionSummary {
    /// `post − pre` lamports for `key`, if it was in the transaction.
    pub fn lamport_change(&self, key: &Pubkey) -> Option<i128> {
        let index = self.account_keys.iter().position(|k| k == key)?;
        let pre = *self.pre_balances.get(index)?;
        let post = *self.post_balances.get(index)?;
        Some(i128::from(post) - i128::from(pre))
    }

    /// Lamport change for the fee payer with the fee added back: what the
    /// transaction itself moved, independent of the cost of sending it.
    pub fn lamport_change_before_fee(&self, payer: &Pubkey) -> Option<i128> {
        Some(self.lamport_change(payer)? + i128::from(self.fee_lamports))
    }

    /// Net change of `mint` across all token accounts owned by `owner`.
    pub fn token_change(&self, owner: &Pubkey, mint: &Pubkey) -> i128 {
        self.token_balance_changes
            .iter()
            .filter(|c| c.owner.as_ref() == Some(owner) && &c.mint == mint)
            .map(|c| i128::from(c.post_amount) - i128::from(c.pre_amount))
            .sum()
    }
}

/// The node operations this crate depends on.
#[async_trait]
pub trait ChainClient: Send + Sync {
    /// Fetches one account. `Ok(None)` if it does not exist.
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, ChainError>;

    /// Fetches several accounts in one call, preserving order.
    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, ChainError>;

    /// Fetches accounts owned by `program` matching `filters`.
    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filters: Vec<RpcFilterType>,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError>;

    /// Lamport balance of an account (zero if it does not exist).
    async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, ChainError>;

    /// The current slot.
    async fn get_slot(&self) -> Result<u64, ChainError>;

    /// A recent blockhash for building transactions.
    async fn get_latest_blockhash(&self) -> Result<Hash, ChainError>;

    /// Simulates a transaction without signature verification, against the
    /// latest blockhash, returning the post-state of `watch` accounts.
    async fn simulate(
        &self,
        transaction: &VersionedTransaction,
        watch: &[Pubkey],
    ) -> Result<SimulationOutcome, ChainError>;

    /// Sends a signed transaction and waits for confirmation.
    async fn send_and_confirm(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<Signature, ChainError>;

    /// Fetches a confirmed transaction's balance effects. `Ok(None)` if the
    /// node does not have it yet.
    async fn get_transaction(
        &self,
        signature: &Signature,
    ) -> Result<Option<TransactionSummary>, ChainError>;
}

/// [`ChainClient`] over JSON-RPC at `confirmed` commitment.
///
/// `confirmed` rather than the client default `finalized`: on a local test
/// validator or surfnet, waiting for finality costs seconds per transaction
/// for no added safety.
pub struct RpcChainClient {
    rpc: RpcClient,
    url: String,
}

impl RpcChainClient {
    /// Connects to `url`.
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            rpc: RpcClient::new_with_commitment(url.clone(), CommitmentConfig::confirmed()),
            url,
        }
    }

    /// The endpoint URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The underlying RPC client, for calls outside [`ChainClient`].
    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    /// Converts an RPC error, removing the endpoint URL (which often carries
    /// an API key) from its text.
    fn rpc_err(&self, err: ClientError) -> ChainError {
        ChainError::Rpc(err.to_string().replace(&self.url, "<rpc>"))
    }
}

impl std::fmt::Debug for RpcChainClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The URL may carry an API key.
        f.debug_struct("RpcChainClient").finish_non_exhaustive()
    }
}

fn parse_pubkey(what: &'static str, value: &str) -> Result<Pubkey, ChainError> {
    value.parse().map_err(|e| ChainError::Decode {
        what,
        detail: format!("{value:?}: {e}"),
    })
}

/// Decodes an RPC account. `None` for JSON-parsed data, which carries no bytes.
fn ui_to_account(ui: &UiAccount) -> Option<Account> {
    let data = match &ui.data {
        UiAccountData::Json(_) => return None,
        binary => binary.decode()?,
    };
    Some(Account {
        lamports: ui.lamports,
        data,
        owner: ui.owner.parse().ok()?,
        executable: ui.executable,
        rent_epoch: ui.rent_epoch,
    })
}

/// Turns the RPC's encoded transaction into our summary.
pub(crate) fn summarize(
    signature: Signature,
    encoded: EncodedConfirmedTransactionWithStatusMeta,
) -> Result<TransactionSummary, ChainError> {
    let decoded = encoded
        .transaction
        .transaction
        .decode()
        .ok_or(ChainError::Decode {
            what: "transaction",
            detail: "could not decode encoded transaction".into(),
        })?;
    let meta = encoded.transaction.meta.ok_or(ChainError::Decode {
        what: "transaction",
        detail: "missing meta".into(),
    })?;

    let mut account_keys: Vec<Pubkey> = decoded.message.static_account_keys().to_vec();
    if let OptionSerializer::Some(loaded) = &meta.loaded_addresses {
        for key in loaded.writable.iter().chain(loaded.readonly.iter()) {
            account_keys.push(parse_pubkey("loaded address", key)?);
        }
    }

    let pre_token: Vec<UiTransactionTokenBalance> =
        Option::from(meta.pre_token_balances).unwrap_or_default();
    let post_token: Vec<UiTransactionTokenBalance> =
        Option::from(meta.post_token_balances).unwrap_or_default();
    let token_balance_changes = merge_token_balances(&account_keys, &pre_token, &post_token)?;

    Ok(TransactionSummary {
        signature,
        slot: encoded.slot,
        block_time: encoded
            .block_time
            .and_then(|t| DateTime::<Utc>::from_timestamp(t, 0)),
        fee_lamports: meta.fee,
        error: meta.err.map(Into::into),
        logs: Option::from(meta.log_messages).unwrap_or_default(),
        account_keys,
        pre_balances: meta.pre_balances,
        post_balances: meta.post_balances,
        token_balance_changes,
    })
}

fn merge_token_balances(
    account_keys: &[Pubkey],
    pre: &[UiTransactionTokenBalance],
    post: &[UiTransactionTokenBalance],
) -> Result<Vec<TokenBalanceChange>, ChainError> {
    struct Side {
        amount: u64,
        mint: Pubkey,
        owner: Option<Pubkey>,
    }
    fn side(b: &UiTransactionTokenBalance) -> Result<(u8, Side), ChainError> {
        let amount = b
            .ui_token_amount
            .amount
            .parse::<u64>()
            .map_err(|e| ChainError::Decode {
                what: "token balance",
                detail: format!("amount {:?}: {e}", b.ui_token_amount.amount),
            })?;
        let owner = match &b.owner {
            OptionSerializer::Some(o) => Some(parse_pubkey("token owner", o)?),
            _ => None,
        };
        Ok((
            b.account_index,
            Side {
                amount,
                mint: parse_pubkey("token mint", &b.mint)?,
                owner,
            },
        ))
    }

    let mut indices: Vec<u8> = pre
        .iter()
        .chain(post.iter())
        .map(|b| b.account_index)
        .collect();
    indices.sort_unstable();
    indices.dedup();

    let mut changes = Vec::with_capacity(indices.len());
    for index in indices {
        let pre_side = pre
            .iter()
            .find(|b| b.account_index == index)
            .map(side)
            .transpose()?;
        let post_side = post
            .iter()
            .find(|b| b.account_index == index)
            .map(side)
            .transpose()?;
        let (mint, owner) = match (&pre_side, &post_side) {
            (_, Some((_, s))) | (Some((_, s)), None) => (s.mint, s.owner),
            (None, None) => unreachable!("index came from one of the lists"),
        };
        let account = *account_keys
            .get(usize::from(index))
            .ok_or(ChainError::Decode {
                what: "token balance",
                detail: format!("account index {index} out of range"),
            })?;
        changes.push(TokenBalanceChange {
            account,
            mint,
            owner,
            pre_amount: pre_side.map_or(0, |(_, s)| s.amount),
            post_amount: post_side.map_or(0, |(_, s)| s.amount),
        });
    }
    Ok(changes)
}

#[async_trait]
impl ChainClient for RpcChainClient {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, ChainError> {
        Ok(self
            .rpc
            .get_account_with_commitment(pubkey, CommitmentConfig::confirmed())
            .await
            .map_err(|e| self.rpc_err(e))?
            .value)
    }

    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, ChainError> {
        if pubkeys.is_empty() {
            return Ok(Vec::new());
        }
        self.rpc
            .get_multiple_accounts(pubkeys)
            .await
            .map_err(|e| self.rpc_err(e))
    }

    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filters: Vec<RpcFilterType>,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError> {
        let config = RpcProgramAccountsConfig {
            filters: Some(filters),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                ..RpcAccountInfoConfig::default()
            },
            ..RpcProgramAccountsConfig::default()
        };
        let accounts = self
            .rpc
            .get_program_ui_accounts_with_config(program, config)
            .await
            .map_err(|e| self.rpc_err(e))?;
        accounts
            .into_iter()
            .map(|(key, ui)| {
                ui_to_account(&ui)
                    .map(|account| (key, account))
                    .ok_or(ChainError::Decode {
                        what: "program account",
                        detail: format!("{key}: data is not binary"),
                    })
            })
            .collect()
    }

    async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, ChainError> {
        self.rpc
            .get_balance(pubkey)
            .await
            .map_err(|e| self.rpc_err(e))
    }

    async fn get_slot(&self) -> Result<u64, ChainError> {
        self.rpc.get_slot().await.map_err(|e| self.rpc_err(e))
    }

    async fn get_latest_blockhash(&self) -> Result<Hash, ChainError> {
        self.rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| self.rpc_err(e))
    }

    async fn simulate(
        &self,
        transaction: &VersionedTransaction,
        watch: &[Pubkey],
    ) -> Result<SimulationOutcome, ChainError> {
        let config = RpcSimulateTransactionConfig {
            sig_verify: false,
            replace_recent_blockhash: true,
            commitment: Some(CommitmentConfig::confirmed()),
            encoding: Some(UiTransactionEncoding::Base64),
            accounts: (!watch.is_empty()).then(|| RpcSimulateTransactionAccountsConfig {
                encoding: Some(UiAccountEncoding::Base64),
                addresses: watch.iter().map(ToString::to_string).collect(),
            }),
            ..RpcSimulateTransactionConfig::default()
        };
        let result = self
            .rpc
            .simulate_transaction_with_config(transaction, config)
            .await
            .map_err(|e| self.rpc_err(e))?
            .value;
        let accounts = match result.accounts {
            Some(list) => watch
                .iter()
                .zip(list)
                .map(|(key, ui)| (*key, ui.as_ref().and_then(ui_to_account)))
                .collect(),
            None => Vec::new(),
        };
        Ok(SimulationOutcome {
            units_consumed: result.units_consumed,
            logs: result.logs.unwrap_or_default(),
            error: result.err.map(Into::into),
            accounts,
        })
    }

    async fn send_and_confirm(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<Signature, ChainError> {
        self.rpc
            .send_and_confirm_transaction(transaction)
            .await
            .map_err(|e| self.rpc_err(e))
    }

    async fn get_transaction(
        &self,
        signature: &Signature,
    ) -> Result<Option<TransactionSummary>, ChainError> {
        let config = RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        };
        match self
            .rpc
            .get_transaction_with_config(signature, config)
            .await
        {
            Ok(encoded) => summarize(*signature, encoded).map(Some),
            Err(err) => {
                // "not found" surfaces as an RPC error with a null result;
                // treat it as absence rather than failure.
                let text = err.to_string();
                if text.contains("not found") || text.contains("null") {
                    Ok(None)
                } else {
                    Err(self.rpc_err(err))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use solana_account_decoder_client_types::token::UiTokenAmount;

    use super::*;

    fn balance(
        index: u8,
        mint: &Pubkey,
        owner: Option<&Pubkey>,
        amount: u64,
    ) -> UiTransactionTokenBalance {
        UiTransactionTokenBalance {
            account_index: index,
            mint: mint.to_string(),
            ui_token_amount: UiTokenAmount {
                ui_amount: None,
                decimals: 9,
                amount: amount.to_string(),
                ui_amount_string: amount.to_string(),
            },
            owner: owner.map(ToString::to_string).into(),
            program_id: OptionSerializer::None,
        }
    }

    #[test]
    fn merge_token_balances_handles_created_and_closed_accounts() {
        let keys: Vec<Pubkey> = (0..4).map(|_| Pubkey::new_unique()).collect();
        let mint = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let pre = vec![
            balance(1, &mint, Some(&owner), 100),
            balance(2, &mint, Some(&owner), 50),
        ];
        let post = vec![
            balance(1, &mint, Some(&owner), 70),
            balance(3, &mint, Some(&owner), 5),
        ];
        let changes = merge_token_balances(&keys, &pre, &post).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(
            changes[0],
            TokenBalanceChange {
                account: keys[1],
                mint,
                owner: Some(owner),
                pre_amount: 100,
                post_amount: 70
            }
        );
        assert_eq!(
            changes[1],
            TokenBalanceChange {
                account: keys[2],
                mint,
                owner: Some(owner),
                pre_amount: 50,
                post_amount: 0
            }
        );
        assert_eq!(
            changes[2],
            TokenBalanceChange {
                account: keys[3],
                mint,
                owner: Some(owner),
                pre_amount: 0,
                post_amount: 5
            }
        );

        let summary = TransactionSummary {
            signature: Signature::default(),
            slot: 1,
            block_time: None,
            fee_lamports: 5000,
            error: None,
            logs: vec![],
            account_keys: keys.clone(),
            pre_balances: vec![1_000_000, 0, 0, 0],
            post_balances: vec![900_000, 0, 0, 0],
            token_balance_changes: changes,
        };
        assert_eq!(summary.token_change(&owner, &mint), -75);
        assert_eq!(summary.token_change(&Pubkey::new_unique(), &mint), 0);
        assert_eq!(summary.lamport_change(&keys[0]), Some(-100_000));
        assert_eq!(summary.lamport_change_before_fee(&keys[0]), Some(-95_000));
        assert_eq!(summary.lamport_change(&Pubkey::new_unique()), None);
    }

    #[test]
    fn merge_token_balances_rejects_bad_index() {
        let keys = vec![Pubkey::new_unique()];
        let mint = Pubkey::new_unique();
        let err = merge_token_balances(&keys, &[balance(7, &mint, None, 1)], &[]).unwrap_err();
        assert!(matches!(err, ChainError::Decode { .. }));
    }

    #[test]
    fn simulation_outcome_lamports_of_watched_accounts() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let outcome = SimulationOutcome {
            accounts: vec![
                (
                    a,
                    Some(Account {
                        lamports: 42,
                        ..Account::default()
                    }),
                ),
                (b, None),
            ],
            ..SimulationOutcome::default()
        };
        assert_eq!(outcome.lamports_of(&a), Some(42));
        assert_eq!(outcome.lamports_of(&b), Some(0));
        assert_eq!(outcome.lamports_of(&Pubkey::new_unique()), None);
    }

    #[test]
    fn ui_accounts_decode_binary_data_only() {
        let owner = Pubkey::new_unique();
        let ui = UiAccount {
            lamports: 7,
            data: UiAccountData::Binary("AQID".into(), UiAccountEncoding::Base64),
            owner: owner.to_string(),
            executable: false,
            rent_epoch: 0,
            space: Some(3),
        };
        let account = ui_to_account(&ui).unwrap();
        assert_eq!(account.data, vec![1, 2, 3]);
        assert_eq!(account.owner, owner);
        assert_eq!(account.lamports, 7);
    }
}
