//! A component's sends under the transaction layer: the chain client its
//! seats send through. Reads go to the bot's guard as before; a send takes
//! the line's transaction options (WIT 0.2.0 `tx-options`) and goes to the
//! pool through the guard's host side, without waiting for it to land.
//!
//! - `cu-limit` sets the transaction's compute-unit limit; none, with other
//!   options set, is what the simulation consumed plus a margin.
//! - `cu-price` sets its priority fee.
//! - `tip` adds a transaction paying one of Jito's tip accounts, and makes
//!   the line a bundle on the `jito` route.
//! - `bundle`: lines with the same id go as one bundle, in sheet order, at
//!   most five transactions, tip included.
//! - `route`: `rpc`, `staked` (where the scenario offers it) or `jito`;
//!   `bundle-only` sends a lone transaction as a bundle, `dont-front` adds
//!   a `jitodontfront` key, read-only, to the bundle's first transaction.
//! - `retry-until-slot`: rebroadcast the same transaction each slot until
//!   then; once its blockhash expired, and it never landed, the grader
//!   signs it again on a new one.
//!
//! The grader signs: the strategy never holds a key.

use std::{collections::BTreeMap, sync::Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use mtx_kit::{
    Route, TxOptions,
    chain::{ChainClient, ChainError, RpcChainClient, SimulationOutcome, TransactionSummary},
};
use serde_json::{Value, json};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{VersionedMessage, compiled_instruction::CompiledInstruction};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::filter::RpcFilterType;
use solana_signer::Signer;
use solana_transaction::{Hash, Signature, versioned::VersionedTransaction};

use super::{jito, pool};

/// The margin on a simulation's units, for a limit the options leave out.
const UNITS_MARGIN: f64 = 0.1;
const UNITS_FLOOR: u64 = 1_000;

/// A seat's chain under the transaction layer.
pub struct PoolClient {
    inner: RpcChainClient,
    host_url: String,
    http: reqwest::Client,
    signer: Keypair,
    state: Mutex<ClientState>,
}

#[derive(Default)]
struct ClientState {
    /// The options of the line being sent.
    options: Option<TxOptions>,
    /// The units the last simulation consumed.
    units: Option<u64>,
    /// Bundles being gathered, by the sheet's bundle id: each transaction's
    /// wire and its lines' tips.
    bundles: BTreeMap<u32, Gathered>,
    /// Tips paid so far, to pick each tip account in turn.
    tips: u64,
    /// A raw send: send it as signed.
    keep_budget: bool,
}

#[derive(Default)]
struct Gathered {
    wires: Vec<Vec<u8>>,
    tip: u64,
    retry_until_slot: Option<u64>,
}

impl PoolClient {
    /// A client for the bot `signer`, through its guard's host side.
    pub fn new(host_url: &str, signer: Keypair) -> Self {
        Self {
            inner: RpcChainClient::with_timeout(host_url, crate::session::INTERNAL_RPC_TIMEOUT),
            host_url: host_url.to_string(),
            http: reqwest::Client::new(),
            signer,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ClientState> {
        self.state.lock().expect("pool client lock")
    }

    /// The next sends are the line's, with its options.
    pub fn set_options(&self, options: Option<TxOptions>) {
        let mut state = self.state();
        state.options = options;
        state.keep_budget = false;
    }

    /// The next send is a raw transaction's, with its options: its compute
    /// budget is already its own, and others may have signed it.
    pub fn set_raw_options(&self, options: Option<TxOptions>) {
        let mut state = self.state();
        state.options = options;
        state.keep_budget = true;
    }

    /// Send the bundles the sheet gathered, in the order of their ids.
    /// Returns, for each, its transactions' signatures and why it failed,
    /// if it did.
    pub async fn flush_bundles(&self) -> Vec<(Vec<String>, Option<String>)> {
        let bundles = std::mem::take(&mut self.state().bundles);
        let mut out = Vec::new();
        for (_, gathered) in bundles {
            let signatures = signatures_of(&gathered.wires);
            let sent = self
                .bundle(gathered.wires, gathered.tip, gathered.retry_until_slot)
                .await;
            out.push((signatures, sent.err()));
        }
        out
    }

    /// A tip transaction paying `lamports` to the next tip account.
    async fn tip_transaction(&self, lamports: u64) -> Result<Vec<u8>, String> {
        let blockhash = self
            .inner
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let n = {
            let mut state = self.state();
            state.tips += 1;
            state.tips - 1
        };
        let to: Pubkey = jito::TIP_ACCOUNTS[(n % 8) as usize]
            .parse()
            .expect("a tip account");
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend(lamports.to_le_bytes());
        let ix = solana_transaction::Instruction::new_with_bytes(
            pool::SYSTEM,
            &data,
            vec![
                solana_transaction::AccountMeta::new(self.signer.pubkey(), true),
                solana_transaction::AccountMeta::new(to, false),
            ],
        );
        let message = solana_message::Message::new_with_blockhash(
            &[ix],
            Some(&self.signer.pubkey()),
            &blockhash,
        );
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&self.signer])
            .map_err(|e| e.to_string())?;
        wincode::serialize(&tx).map_err(|e| e.to_string())
    }

    /// Send `wires`, a tip after them, as one bundle through the host side.
    async fn bundle(
        &self,
        mut wires: Vec<Vec<u8>>,
        tip: u64,
        retry_until_slot: Option<u64>,
    ) -> Result<(), String> {
        if tip > 0 {
            wires.push(self.tip_transaction(tip).await?);
        }
        if wires.len() > jito::MAX_BUNDLE {
            return Err(format!(
                "a bundle of {} transactions, tip included: at most {}",
                wires.len(),
                jito::MAX_BUNDLE
            ));
        }
        let encoded: Vec<String> = wires.iter().map(|w| encode(w)).collect();
        self.call(
            "sendBundle",
            json!([encoded, { "encoding": "base64", "mtx": { "route": "jito", "retryUntilSlot": retry_until_slot, "resign": true } }]),
        )
        .await
        .map(|_| ())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let reply: Value = self
            .http
            .post(&self.host_url)
            .timeout(crate::session::INTERNAL_RPC_TIMEOUT)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        match reply.get("error") {
            Some(error) => Err(error["message"]
                .as_str()
                .map_or_else(|| error.to_string(), str::to_string)),
            None => Ok(reply["result"].clone()),
        }
    }

    fn sign(&self, message: VersionedMessage) -> Result<Vec<u8>, String> {
        if message.header().num_required_signatures != 1
            || message.static_account_keys().first() != Some(&self.signer.pubkey())
        {
            return Err(
                "transaction options apply only to transactions the bot alone signs".into(),
            );
        }
        let tx =
            VersionedTransaction::try_new(message, &[&self.signer]).map_err(|e| e.to_string())?;
        wincode::serialize(&tx).map_err(|e| e.to_string())
    }

    /// Send `transaction` to the pool with the line's options.
    async fn pooled(&self, transaction: &VersionedTransaction) -> Result<Signature, ChainError> {
        let fail = |e: String| ChainError::Rpc(e);
        let (options, units, keep_budget) = {
            let state = self.state();
            (state.options, state.units, state.keep_budget)
        };
        let Some(options) = options else {
            let wire = wincode::serialize(transaction).map_err(|e| fail(e.to_string()))?;
            self.call(
                "sendTransaction",
                json!([encode(&wire), { "encoding": "base64", "skipPreflight": true }]),
            )
            .await
            .map_err(fail)?;
            return Ok(transaction.signatures[0]);
        };
        let (bundle_only, dont_front, route) = match options.route {
            Some(Route::Jito {
                bundle_only,
                dont_front,
            }) => (bundle_only, dont_front, "jito"),
            Some(Route::Staked) => (false, false, "staked"),
            Some(Route::Rpc) | None => (false, false, "rpc"),
        };
        // `dont-front` marks the bundle's first transaction.
        let first = options.bundle.is_none_or(|id| {
            self.state()
                .bundles
                .get(&id)
                .is_none_or(|g| g.wires.is_empty())
        });
        let front = dont_front && first;
        let wire = if keep_budget && !front {
            wincode::serialize(transaction).map_err(|e| fail(e.to_string()))?
        } else {
            let mut message = transaction.message.clone();
            if !keep_budget {
                let limit = match options.cu_limit {
                    Some(limit) => Some(limit as u64),
                    None => units.map(|u| {
                        ((u as f64 * (1.0 + UNITS_MARGIN)) as u64 + UNITS_FLOOR)
                            .min(pool::MAX_COMPUTE_UNITS)
                    }),
                };
                set_budget(&mut message, limit, options.cu_price).map_err(fail)?;
            }
            if front {
                ensure_key(&mut message, jito::DONT_FRONT).map_err(fail)?;
            }
            self.sign(message).map_err(fail)?
        };
        let signature = signatures_of(std::slice::from_ref(&wire))[0]
            .parse()
            .expect("a signature");
        let tip = options.tip.unwrap_or(0);
        if let Some(id) = options.bundle {
            let mut state = self.state();
            let gathered = state.bundles.entry(id).or_default();
            gathered.wires.push(wire);
            gathered.tip += tip;
            gathered.retry_until_slot = gathered.retry_until_slot.max(options.retry_until_slot);
            return Ok(signature);
        }
        if tip > 0 || bundle_only || dont_front {
            self.bundle(vec![wire], tip, options.retry_until_slot)
                .await
                .map_err(fail)?;
            return Ok(signature);
        }
        self.call(
            "sendTransaction",
            json!([encode(&wire), { "encoding": "base64", "skipPreflight": true,
                "mtx": { "route": route, "retryUntilSlot": options.retry_until_slot, "resign": true } }]),
        )
        .await
        .map_err(fail)?;
        Ok(signature)
    }
}

#[async_trait]
impl ChainClient for PoolClient {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, ChainError> {
        self.inner.get_account(pubkey).await
    }

    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, ChainError> {
        self.inner.get_multiple_accounts(pubkeys).await
    }

    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filters: Vec<RpcFilterType>,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError> {
        self.inner.get_program_accounts(program, filters).await
    }

    async fn get_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError> {
        self.inner.get_token_accounts_by_owner(owner, mint).await
    }

    async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, ChainError> {
        self.inner.get_balance(pubkey).await
    }

    async fn get_slot(&self) -> Result<u64, ChainError> {
        self.inner.get_slot().await
    }

    async fn get_latest_blockhash(&self) -> Result<Hash, ChainError> {
        self.inner.get_latest_blockhash().await
    }

    async fn simulate(
        &self,
        transaction: &VersionedTransaction,
        watch: &[Pubkey],
    ) -> Result<SimulationOutcome, ChainError> {
        let outcome = self.inner.simulate(transaction, watch).await?;
        self.state().units = outcome.units_consumed;
        Ok(outcome)
    }

    async fn send_and_confirm(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<Signature, ChainError> {
        self.pooled(transaction).await
    }

    async fn send(&self, transaction: &VersionedTransaction) -> Result<Signature, ChainError> {
        self.pooled(transaction).await
    }

    async fn get_transaction(
        &self,
        signature: &Signature,
    ) -> Result<Option<TransactionSummary>, ChainError> {
        self.inner.get_transaction(signature).await
    }

    async fn signatures_for_address(
        &self,
        address: &Pubkey,
        after_slot: u64,
    ) -> Result<Vec<(Signature, u64)>, ChainError> {
        self.inner.signatures_for_address(address, after_slot).await
    }
}

/// An actor's chain under the transaction layer, for its seat: reads go to
/// the grader's endpoint, and a send goes to the pool as the actor's
/// (`sender`), at the priority fee last set, to land in the slot being
/// built. Before the session binds it to the layer, a send goes straight to
/// the chain.
pub struct ActorChain {
    inner: RpcChainClient,
    sender: String,
    signer: Keypair,
    engine: std::sync::OnceLock<std::sync::Arc<super::TxEngine>>,
    /// Micro-lamports per compute unit its next send pays.
    price: Mutex<u64>,
}

impl ActorChain {
    pub fn new(rpc_url: &str, sender: String, signer: Keypair) -> Self {
        Self {
            inner: RpcChainClient::with_timeout(rpc_url, crate::session::INTERNAL_RPC_TIMEOUT),
            sender,
            signer,
            engine: std::sync::OnceLock::new(),
            price: Mutex::new(0),
        }
    }

    /// Send through `engine`'s pool from now on, registered as the actor.
    pub fn bind(&self, engine: &std::sync::Arc<super::TxEngine>) {
        engine.register_actor(&self.sender, self.signer.pubkey());
        let _ = self.engine.set(engine.clone());
    }

    /// The priority fee its next sends pay.
    pub fn set_price(&self, micro_lamports: u64) {
        *self.price.lock().expect("actor chain lock") = micro_lamports;
    }

    async fn pooled(&self, transaction: &VersionedTransaction) -> Result<Signature, ChainError> {
        let Some(engine) = self.engine.get() else {
            return self.inner.send(transaction).await;
        };
        let fail = |e: String| ChainError::Rpc(e);
        let price = *self.price.lock().expect("actor chain lock");
        let mut message = transaction.message.clone();
        set_budget(&mut message, None, Some(price)).map_err(fail)?;
        let signed = VersionedTransaction::try_new(message, &[&self.signer])
            .map_err(|e| fail(e.to_string()))?;
        let wire = wincode::serialize(&signed).map_err(|e| fail(e.to_string()))?;
        engine
            .submit_actor(
                &self.sender,
                true,
                super::engine::Submission {
                    wires: vec![wire],
                    route: super::RouteKind::Rpc,
                    bundle: false,
                    retry_until_slot: None,
                    resign: false,
                    delay_slots: 0,
                    dropped: false,
                },
            )
            .await
            .map_err(|refusal| fail(refusal.message))?;
        Ok(signed.signatures[0])
    }
}

#[async_trait]
impl ChainClient for ActorChain {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, ChainError> {
        self.inner.get_account(pubkey).await
    }

    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, ChainError> {
        self.inner.get_multiple_accounts(pubkeys).await
    }

    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filters: Vec<RpcFilterType>,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError> {
        self.inner.get_program_accounts(program, filters).await
    }

    async fn get_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError> {
        self.inner.get_token_accounts_by_owner(owner, mint).await
    }

    async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, ChainError> {
        self.inner.get_balance(pubkey).await
    }

    async fn get_slot(&self) -> Result<u64, ChainError> {
        self.inner.get_slot().await
    }

    async fn get_latest_blockhash(&self) -> Result<Hash, ChainError> {
        self.inner.get_latest_blockhash().await
    }

    async fn simulate(
        &self,
        transaction: &VersionedTransaction,
        watch: &[Pubkey],
    ) -> Result<SimulationOutcome, ChainError> {
        self.inner.simulate(transaction, watch).await
    }

    async fn send_and_confirm(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<Signature, ChainError> {
        if self.engine.get().is_none() {
            return self.inner.send_and_confirm(transaction).await;
        }
        self.pooled(transaction).await
    }

    async fn send(&self, transaction: &VersionedTransaction) -> Result<Signature, ChainError> {
        self.pooled(transaction).await
    }

    async fn get_transaction(
        &self,
        signature: &Signature,
    ) -> Result<Option<TransactionSummary>, ChainError> {
        self.inner.get_transaction(signature).await
    }

    async fn signatures_for_address(
        &self,
        address: &Pubkey,
        after_slot: u64,
    ) -> Result<Vec<(Signature, u64)>, ChainError> {
        self.inner.signatures_for_address(address, after_slot).await
    }
}

fn encode(wire: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(wire)
}

/// The signatures of signed transactions' wires.
fn signatures_of(wires: &[Vec<u8>]) -> Vec<String> {
    wires
        .iter()
        .filter_map(|w| wincode::deserialize::<VersionedTransaction>(w).ok())
        .map(|tx| tx.signatures[0].to_string())
        .collect()
}

/// `key`'s index in the message's static keys, added read-only (and
/// unsigned) at their end if it isn't there. A v0 message's lookup-table
/// accounts are indexed after the static keys, so their indices move up.
pub fn ensure_key(message: &mut VersionedMessage, key: Pubkey) -> Result<u8, String> {
    if let Some(i) = message.static_account_keys().iter().position(|k| *k == key) {
        return Ok(i as u8);
    }
    let loaded: usize = message.address_table_lookups().map_or(0, |lookups| {
        lookups
            .iter()
            .map(|l| l.writable_indexes.len() + l.readonly_indexes.len())
            .sum()
    });
    let (keys, header, instructions, v0) = match message {
        VersionedMessage::Legacy(m) => (
            &mut m.account_keys,
            &mut m.header,
            &mut m.instructions,
            false,
        ),
        VersionedMessage::V0(m) => (
            &mut m.account_keys,
            &mut m.header,
            &mut m.instructions,
            true,
        ),
        _ => return Err("only legacy and v0 messages take transaction options".into()),
    };
    let at = keys.len();
    if at + loaded >= 256 {
        return Err("no room for another account".into());
    }
    keys.push(key);
    header.num_readonly_unsigned_accounts += 1;
    if v0 {
        for ix in instructions.iter_mut() {
            if ix.program_id_index as usize >= at {
                ix.program_id_index += 1;
            }
            for account in ix.accounts.iter_mut() {
                if *account as usize >= at {
                    *account += 1;
                }
            }
        }
    }
    Ok(at as u8)
}

/// Set the message's compute-unit limit and price, where given, adding the
/// compute budget's instructions it lacks.
pub fn set_budget(
    message: &mut VersionedMessage,
    limit: Option<u64>,
    price: Option<u64>,
) -> Result<(), String> {
    if limit.is_none() && price.is_none() {
        return Ok(());
    }
    let program = ensure_key(message, pool::COMPUTE_BUDGET)?;
    let instructions = match message {
        VersionedMessage::Legacy(m) => &mut m.instructions,
        VersionedMessage::V0(m) => &mut m.instructions,
        _ => return Err("only legacy and v0 messages take transaction options".into()),
    };
    let mut set = |tag: u8, data: Vec<u8>| {
        let found = instructions
            .iter_mut()
            .find(|ix| ix.program_id_index == program && ix.data.first() == Some(&tag));
        match found {
            Some(ix) => ix.data = data,
            None => instructions.insert(
                0,
                CompiledInstruction {
                    program_id_index: program,
                    accounts: Vec::new(),
                    data,
                },
            ),
        }
    };
    if let Some(limit) = limit {
        let mut data = vec![2];
        data.extend((limit.min(pool::MAX_COMPUTE_UNITS) as u32).to_le_bytes());
        set(2, data);
    }
    if let Some(price) = price {
        let mut data = vec![3];
        data.extend(price.to_le_bytes());
        set(3, data);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use solana_message::{AddressLookupTableAccount, v0};
    use solana_transaction::{AccountMeta, Instruction};

    use super::*;
    use crate::txlayer::pool::PooledTx;

    fn ix(payer: &Pubkey, writes: &[Pubkey]) -> Instruction {
        let mut accounts = vec![AccountMeta::new(*payer, true)];
        accounts.extend(writes.iter().map(|w| AccountMeta::new(*w, false)));
        Instruction::new_with_bytes(Pubkey::new_unique(), &[1, 2, 3], accounts)
    }

    #[test]
    fn options_set_the_budget_on_legacy_and_v0_messages() {
        let payer = Keypair::new();
        let writes: Vec<Pubkey> = (0..3).map(|_| Pubkey::new_unique()).collect();
        let legacy = VersionedMessage::Legacy(solana_message::Message::new_with_blockhash(
            &[ix(&payer.pubkey(), &writes)],
            Some(&payer.pubkey()),
            &Hash::new_unique(),
        ));
        let table = AddressLookupTableAccount {
            key: Pubkey::new_unique(),
            addresses: writes.clone(),
        };
        let v0 = VersionedMessage::V0(
            v0::Message::try_compile(
                &payer.pubkey(),
                &[ix(&payer.pubkey(), &writes)],
                std::slice::from_ref(&table),
                Hash::new_unique(),
            )
            .unwrap(),
        );
        for mut message in [legacy, v0] {
            set_budget(&mut message, Some(321_000), Some(7_777)).unwrap();
            ensure_key(&mut message, jito::DONT_FRONT).unwrap();
            let tx = VersionedTransaction::try_new(message, &[&payer]).unwrap();
            let wire = wincode::serialize(&tx).unwrap();
            let tables = |k: &Pubkey| (*k == table.key).then(|| table.addresses.clone());
            let parsed = PooledTx::parse(wire, &tables).unwrap();
            assert_eq!((parsed.cu_limit, parsed.cu_price), (321_000, 7_777));
            assert!(parsed.dont_front);
            // The instruction still writes the same accounts.
            for w in &writes {
                assert!(parsed.writable.contains(w), "{w} writable");
            }
            assert!(parsed.readonly.contains(&jito::DONT_FRONT));
        }
    }
}
