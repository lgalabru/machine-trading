//! Raw sends: transactions a strategy component builds itself, for what its
//! sheet can't say, and the grader signs (`send` in `mtx:strategy@0.2.0`,
//! `docs/rfd/0002-wasm-strategies.md`).
//!
//! A scenario opts in with [`crate::scenario::RawSends`], the programs its
//! raw sends may run. Without it, every send is refused. With it, a send
//! made during a step is taken only if, in order:
//!
//! 1. it is well formed ([`RawTransaction::check`]), and nothing signs but
//!    the bot's authority and the transaction's fresh signers;
//! 2. its fresh signers are made: the session's keys for the role
//!    `fresh/{step}/{id}` ([`FreshKeys`]), from the grader's secret, never
//!    from anything the strategy sees, and never handed to it;
//! 3. its derived addresses resolve, in list order: a program address with
//!    `try_find_program_address` (refused where there is none), an
//!    associated token account the same way;
//! 4. every instruction's program is on the list, and the transaction
//!    compiles, with its lookup tables, into one packet;
//! 5. it succeeds when simulated against the paused chain, running no
//!    program off the list, directly or through another; and
//! 6. the account the venue measures after it (one `simulateBundle`: the
//!    transaction, then Phoenix's own views of the account) breaks no rule
//!    of the mandate: its position cap or floor, its universe, its drawdown
//!    limit.
//!
//! A refused send queues nothing. A taken one gets its number within the
//! step and its fresh signers' addresses. After the step's sheet has gone
//! out, each taken transaction, in the order taken, is simulated and
//! audited again against the chain as the sheet left it, then signed by the
//! authority and its fresh signers and sent through the bot's guard. The
//! next step's market says what became of it: `raw(id)`, with its
//! signature, slot and fee, or why it failed.
//!
//! Everything is a function of the chain and the grade's secret: the same
//! grade makes the same keys and the same transactions, so it replays.

use std::str::FromStr;

use base64::Engine as _;
use mtx_core::{
    Holding,
    mandate::{Breach, InstrumentRef, Mandate},
};
use mtx_kit::{
    Decimal, RawReceipt, RawTransaction, SendSource, SendState, SendStatus,
    chain::{ChainClient, RpcChainClient},
    protocols::USDC_MINT,
    raw::{AccountKey, Derivation, Seed},
};
use phoenix_rise::ix::hawkeye::{
    ViewAssetReturn, ViewBboReturn, ViewMarginReturn, decode_hawkeye_return,
};
use serde_json::{Value, json};
use solana_keypair::Keypair;
use solana_message::{AddressLookupTableAccount, VersionedMessage, v0};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::{
    AccountMeta, Hash, Instruction, Signature, Transaction, versioned::VersionedTransaction,
};

use crate::{keys::FreshKeys, session::grader_f64, wasm::Account};

/// The compute budget program: always allowed, since it only sets the
/// transaction's own compute limit and fee.
const COMPUTE_BUDGET: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");
const ASSOCIATED_TOKEN: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const TOKEN: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_2022: Pubkey = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const LOOKUP_TABLE: Pubkey = Pubkey::from_str_const("AddressLookupTab1e1111111111111111111111111");

/// A lookup table's header, before its addresses.
const LOOKUP_TABLE_META: usize = 56;
/// The most lookup tables a raw transaction may name.
pub const MAX_LOOKUP_TABLES: usize = 32;
/// The most bytes a transaction may take on the wire.
const PACKET_BYTES: usize = 1232;
/// The compute Phoenix's views get when they follow a transaction.
const VIEW_COMPUTE_UNITS: u32 = 1_400_000;
/// How many of a failed simulation's last log lines its refusal quotes.
const LOG_TAIL: usize = 6;

/// What a session's raw sends need from the session.
pub struct RawConfig {
    /// The programs they may run ([`crate::scenario::RawSends::programs`]).
    pub programs: Vec<Pubkey>,
    /// The session's fresh signers.
    pub fresh: FreshKeys,
    /// The grader's own endpoint ([`crate::guard::Guard::internal`]), for
    /// simulations and the account's views: never the bot's guard.
    pub internal_url: String,
    /// The mandate the session audits, and its market.
    pub mandate: Mandate,
    pub instrument: InstrumentRef,
    /// Whether the session values the account with the funding it has
    /// accrued and not settled yet, as its meter does.
    pub unsettled_funding: bool,
}

/// One session's raw sends: what they may do, and the ones the step took.
pub struct RawSender {
    programs: Vec<Pubkey>,
    fresh: FreshKeys,
    rpc_url: String,
    http: reqwest::Client,
    chain: RpcChainClient,
    /// The bot's guard: what the sends go through, like the sheet's.
    guarded: RpcChainClient,
    mandate: Mandate,
    instrument: InstrumentRef,
    unsettled_funding: bool,
    authority: Pubkey,
    phoenix: mtx_kit::Phoenix,
    symbol: String,
    /// The step taking sends, if one is; the account the session measured
    /// for it; and the account the venue showed when it began.
    step: Option<u64>,
    account: Option<Account>,
    start: Option<Measured>,
    queue: Vec<Queued>,
    /// Sends taken this step, for the next one's number.
    taken: u32,
}

/// A transaction the step took, resolved.
struct Queued {
    id: u32,
    step: u64,
    instructions: Vec<Instruction>,
    tables: Vec<AddressLookupTableAccount>,
    fresh: Vec<Keypair>,
}

/// The account as the venue shows it: what the audit compares.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measured {
    /// In base units, positive long.
    pub position: f64,
    /// Markets other than the session's that hold a position or orders.
    pub outside: u32,
    /// Collateral and unrealized PnL (and unsettled funding, where the
    /// session counts it), revalued at the session's price where it isn't
    /// the mark, in USD; and the wallet's USDC, in atoms.
    pub value_usd: f64,
    pub wallet_atoms: u64,
}

impl Measured {
    fn total_usd(&self) -> f64 {
        self.value_usd + self.wallet_atoms as f64 / 1e6
    }
}

/// The first rule of `mandate` the account would break if it went from
/// `start` to `after` during a step that began `drawdown_usd` below its
/// peak, if any.
pub fn audit(
    mandate: &Mandate,
    instrument: &InstrumentRef,
    drawdown_usd: f64,
    start: &Measured,
    after: &Measured,
) -> Option<Breach> {
    if after.outside > 0 {
        return Some(Breach::OutsideUniverse {
            positions: after.outside,
        });
    }
    if let Some(breach) = mandate
        .allowance(instrument)
        .and_then(|allowance| allowance.breach(after.position, after.position))
    {
        return Some(breach);
    }
    let drawn = drawdown_usd + (start.total_usd() - after.total_usd());
    match mandate.limits.max_drawdown_usd {
        Some(limit) if drawn > limit => Some(Breach::Drawdown { drawn, limit }),
        _ => None,
    }
}

impl RawSender {
    /// The session's raw sends, for the bot whose authority is `authority`,
    /// trading `symbol` on the Phoenix exchange `exchange`, through its
    /// guard at `guard_url`.
    pub fn new(
        config: RawConfig,
        authority: Pubkey,
        exchange: mtx_kit::phoenix_rise::types::exchange::ExchangeResponse,
        symbol: &str,
        guard_url: &str,
    ) -> Self {
        let RawConfig {
            programs,
            fresh,
            internal_url,
            mandate,
            instrument,
            unsettled_funding,
        } = config;
        Self {
            programs,
            fresh,
            chain: RpcChainClient::with_timeout(
                internal_url.clone(),
                crate::session::INTERNAL_RPC_TIMEOUT,
            ),
            guarded: RpcChainClient::with_timeout(guard_url, crate::session::INTERNAL_RPC_TIMEOUT),
            phoenix: mtx_kit::Phoenix::new(exchange, internal_url.clone()),
            rpc_url: internal_url,
            http: reqwest::Client::new(),
            mandate,
            instrument,
            unsettled_funding,
            authority,
            symbol: symbol.to_string(),
            step: None,
            account: None,
            start: None,
            queue: Vec::new(),
            taken: 0,
        }
    }

    /// Step `k` begins: sends are taken until [`RawSender::close`].
    /// `account` is the account the session measured for it.
    pub fn begin(&mut self, k: u64, account: Option<Account>) {
        self.step = Some(k);
        self.account = account;
        self.start = None;
        self.taken = 0;
    }

    /// The step returned: it takes no more sends.
    pub fn close(&mut self) {
        self.step = None;
    }

    /// The step failed: what it took is dropped, and said so.
    pub fn discard(&mut self, why: &str) -> Vec<SendStatus> {
        self.close();
        self.queue
            .drain(..)
            .map(|q| status(q.step, q.id, SendState::NotSent(why.to_string())))
            .collect()
    }

    /// Take `tx`, which [`RawTransaction::check`] passed, for after the
    /// step's sheet: its receipt, or why it was refused.
    pub async fn take(&mut self, tx: RawTransaction) -> Result<RawReceipt, String> {
        let Some(step) = self.step else {
            return Err("raw sends go out from `step` only".into());
        };
        let id = self.taken;
        let fresh: Vec<Keypair> = (0..tx.fresh_signers)
            .map(|n| self.fresh.keypair(step, id, n))
            .collect();
        let fresh_keys: Vec<Pubkey> = fresh.iter().map(Signer::pubkey).collect();
        let instructions = resolve(&tx, self.authority, &fresh_keys)?;
        for instruction in &instructions {
            self.allowed(&instruction.program_id)?;
        }
        let tables = self.lookup_tables(&tx.lookup_tables).await?;
        let start = match self.start {
            Some(start) => start,
            None => {
                let start = self.measure_now().await?;
                self.start = Some(start);
                start
            }
        };
        self.check(&instructions, &tables, &start, start.wallet_atoms)
            .await?;
        self.taken += 1;
        self.queue.push(Queued {
            id,
            step,
            instructions,
            tables,
            fresh,
        });
        Ok(RawReceipt {
            id,
            fresh_signers: fresh_keys.iter().map(ToString::to_string).collect(),
        })
    }

    /// Sign what the step took with `signer`, the bot's key, and send it
    /// through the bot's guard, in order, each checked again against the
    /// chain as it is now: what became of each, for the next step's market.
    pub async fn flush(&mut self, signer: &Keypair) -> Vec<SendStatus> {
        let mut statuses = Vec::new();
        for queued in std::mem::take(&mut self.queue) {
            statuses.push(self.send(&queued, signer).await);
        }
        statuses
    }

    async fn send(&self, q: &Queued, signer: &Keypair) -> SendStatus {
        let guarded = &self.guarded;
        let failed = |reason: String, signature: Option<Signature>| SendState::Failed {
            reason,
            signature: signature.map(|s| s.to_string()),
        };
        let checked = async {
            let start = self.start.ok_or("no account measured for the step")?;
            let wallet = self.wallet().await?;
            self.check(&q.instructions, &q.tables, &start, wallet).await
        };
        if let Err(reason) = checked.await {
            return status(q.step, q.id, failed(format!("not sent: {reason}"), None));
        }
        let blockhash = match guarded.get_latest_blockhash().await {
            Ok(blockhash) => blockhash,
            Err(e) => return status(q.step, q.id, failed(format!("not sent: {e}"), None)),
        };
        let tx = match self.signed(q, signer, blockhash) {
            Ok(tx) => tx,
            Err(reason) => {
                return status(q.step, q.id, failed(format!("not sent: {reason}"), None));
            }
        };
        let signature = match guarded.send(&tx).await {
            Ok(signature) => signature,
            Err(e) => return status(q.step, q.id, failed(e.to_string(), None)),
        };
        match self.chain.get_transaction(&signature).await {
            Ok(Some(summary)) => {
                let state = match summary.error {
                    None => SendState::Landed(signature.to_string()),
                    Some(error) => failed(error.to_string(), Some(signature)),
                };
                SendStatus {
                    slot: Some(summary.slot),
                    fee: Some(summary.fee_lamports),
                    ..status(q.step, q.id, state)
                }
            }
            Ok(None) => status(
                q.step,
                q.id,
                failed("it didn't land".into(), Some(signature)),
            ),
            Err(e) => status(q.step, q.id, failed(e.to_string(), Some(signature))),
        }
    }

    /// The transaction, signed by the authority and those of its fresh
    /// signers it names.
    fn signed(
        &self,
        q: &Queued,
        authority: &Keypair,
        blockhash: Hash,
    ) -> Result<VersionedTransaction, String> {
        let message = compile(self.authority, &q.instructions, &q.tables, blockhash)?;
        let required = usize::from(message.header().num_required_signatures);
        let signing =
            &message.static_account_keys()[..required.min(message.static_account_keys().len())];
        let mut signers: Vec<&dyn Signer> = vec![authority];
        signers.extend(
            q.fresh
                .iter()
                .filter(|k| signing.contains(&k.pubkey()))
                .map(|k| k as &dyn Signer),
        );
        VersionedTransaction::try_new(message, &signers).map_err(|e| format!("signing: {e}"))
    }

    fn allowed(&self, program: &Pubkey) -> Result<(), String> {
        if *program == COMPUTE_BUDGET || self.programs.contains(program) {
            Ok(())
        } else {
            Err(format!(
                "{program} isn't a program this session's raw sends may run"
            ))
        }
    }

    /// The tables `names` names, as the chain holds them.
    async fn lookup_tables(
        &self,
        names: &[String],
    ) -> Result<Vec<AddressLookupTableAccount>, String> {
        if names.len() > MAX_LOOKUP_TABLES {
            return Err(format!(
                "{} lookup tables: at most {MAX_LOOKUP_TABLES}",
                names.len()
            ));
        }
        let keys = names
            .iter()
            .map(|name| address(name))
            .collect::<Result<Vec<_>, _>>()?;
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let accounts = self
            .chain
            .get_multiple_accounts(&keys)
            .await
            .map_err(|e| format!("reading the lookup tables: {e}"))?;
        keys.iter()
            .zip(accounts)
            .map(|(key, account)| {
                let account =
                    account.ok_or_else(|| format!("lookup table {key}: no such account"))?;
                if account.owner != LOOKUP_TABLE {
                    return Err(format!("lookup table {key}: not an address lookup table"));
                }
                lookup_table(*key, &account.data)
            })
            .collect()
    }

    /// Simulate the transaction against the chain as it is now, with the
    /// wallet at `wallet` atoms, and audit what it leaves: `Err` with why
    /// it can't be taken.
    async fn check(
        &self,
        instructions: &[Instruction],
        tables: &[AddressLookupTableAccount],
        start: &Measured,
        wallet: u64,
    ) -> Result<(), String> {
        let message = compile(self.authority, instructions, tables, Hash::default())?;
        let required = usize::from(message.header().num_required_signatures);
        let tx = VersionedTransaction {
            signatures: vec![Signature::default(); required.max(1)],
            message,
        };
        let wire = wincode::serialize(&tx).map_err(|e| format!("encoding: {e}"))?;
        if wire.len() > PACKET_BYTES {
            return Err(format!(
                "{} bytes: a transaction is at most {PACKET_BYTES}",
                wire.len()
            ));
        }
        let writable = writable_keys(instructions);
        let mut bundle = vec![encode(&wire)];
        bundle.extend(self.views()?);
        let results = self.simulate_bundle(&bundle, &writable).await?;
        let result = results
            .first()
            .ok_or("the simulation returned nothing for the transaction")?;
        let logs: Vec<String> = result["logs"]
            .as_array()
            .map(|logs| {
                logs.iter()
                    .filter_map(|l| l.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !result["err"].is_null() {
            let tail = &logs[logs.len().saturating_sub(LOG_TAIL)..];
            return Err(format!(
                "the simulation failed: {}; {}",
                result["err"],
                tail.join(" | ")
            ));
        }
        for program in invoked(&logs)? {
            self.allowed(&program).map_err(|e| format!("it runs {e}"))?;
        }
        let delta = wallet_change(
            &self.authority,
            &writable,
            &result["preExecutionAccounts"],
            &result["postExecutionAccounts"],
        )?;
        let wallet_after = u64::try_from(i128::from(wallet) + delta).unwrap_or(0);
        let after = self.measured(&results[1..], wallet_after)?;
        let drawdown = self.account.map_or(0.0, |a| a.drawdown_usd);
        match audit(&self.mandate, &self.instrument, drawdown, start, &after) {
            Some(breach) => Err(format!("it would breach the mandate: {breach}")),
            None => Ok(()),
        }
    }

    /// The account as the venue shows it now, before anything the step
    /// sends.
    async fn measure_now(&self) -> Result<Measured, String> {
        let results = self.simulate_bundle(&self.views()?, &[]).await?;
        self.measured(&results, self.wallet().await?)
    }

    async fn wallet(&self) -> Result<u64, String> {
        mtx_kit::chain::token::owned_amount(&self.chain, &self.authority, &USDC_MINT)
            .await
            .map_err(|e| format!("reading the wallet: {e}"))
    }

    /// Phoenix's views of the account and the market, as transactions to
    /// simulate after another: the account's position, its margin, and the
    /// market's mark.
    fn views(&self) -> Result<Vec<String>, String> {
        let builder = self.phoenix.tx();
        let market = self
            .phoenix
            .exact_market(&self.symbol)
            .map_err(|e| e.to_string())?;
        let trader = self.phoenix.trader_account(&self.authority);
        let views = [
            builder.build_hawkeye_view_margin_for_asset(self.authority, 0, 0, market.asset_id),
            builder.build_hawkeye_view_margin_for_trader(trader),
            builder.build_hawkeye_view_bbo(&self.symbol),
        ];
        views
            .into_iter()
            .map(|view| {
                let mut instructions = vec![compute_limit(VIEW_COMPUTE_UNITS)];
                instructions.extend(view.map_err(|e| format!("Phoenix's views: {e}"))?);
                let tx = VersionedTransaction::from(Transaction::new_with_payer(
                    &instructions,
                    Some(&self.authority),
                ));
                Ok(encode(
                    &wincode::serialize(&tx).map_err(|e| format!("encoding: {e}"))?,
                ))
            })
            .collect()
    }

    /// The account from the results of [`RawSender::views`], with `wallet`
    /// atoms of USDC.
    fn measured(&self, results: &[Value], wallet: u64) -> Result<Measured, String> {
        let [asset, margin, bbo] = results else {
            return Err("Phoenix's views returned nothing".into());
        };
        let asset: ViewAssetReturn = view(asset, "view_margin_for_asset")?;
        let margin: ViewMarginReturn = view(margin, "view_margin")?;
        let bbo: ViewBboReturn = view(bbo, "view_bbo")?;
        let market = self
            .phoenix
            .exact_market(&self.symbol)
            .map_err(|e| e.to_string())?;
        let config = self
            .phoenix
            .market(&self.symbol)
            .map_err(|e| e.to_string())?;
        let position = grader_f64(
            Holding::Perp(
                market.position(asset.base_lots, asset.entry_price_quote_lots_per_base_lot),
            )
            .position(),
        );
        let held =
            asset.has_position_or_orders != 0 && self.mandate.allowance(&self.instrument).is_some();
        let mut quote_lots = margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots;
        if self.unsettled_funding {
            quote_lots += margin.unsettled_funding_quote_lots;
        }
        let mut value_usd = grader_f64(Decimal::new(
            quote_lots,
            u32::from(market.quote_lot_decimals),
        ));
        if let Some(price) = self.account.and_then(|a| a.valued_at_usd) {
            value_usd +=
                position * (price - self.phoenix.ticks_to_usd(config, bbo.mark_price_ticks));
        }
        Ok(Measured {
            position,
            outside: u32::from(margin.position_count.saturating_sub(u16::from(held))),
            value_usd,
            wallet_atoms: wallet,
        })
    }

    /// `simulateBundle` on the grader's endpoint, skipping signatures and
    /// replacing blockhashes, with the first transaction's `watch`
    /// accounts before and after it: each transaction's result.
    async fn simulate_bundle(
        &self,
        bundle: &[String],
        watch: &[Pubkey],
    ) -> Result<Vec<Value>, String> {
        let accounts = |first: Value| {
            let mut configs = vec![first];
            configs.resize(bundle.len(), Value::Null);
            configs
        };
        let watched = (!watch.is_empty()).then(|| {
            json!({
                "encoding": "base64",
                "addresses": watch.iter().map(ToString::to_string).collect::<Vec<_>>(),
            })
        });
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "simulateBundle",
            "params": [
                { "encodedTransactions": bundle },
                {
                    "preExecutionAccountsConfigs": accounts(watched.clone().unwrap_or(Value::Null)),
                    "postExecutionAccountsConfigs": accounts(watched.unwrap_or(Value::Null)),
                    "transactionEncoding": "base64",
                    "skipSigVerify": true,
                    "replaceRecentBlockhash": true,
                },
            ],
        });
        let infra = |e: String| format!("couldn't simulate it: {e}");
        let answer = self
            .http
            .post(&self.rpc_url)
            .timeout(crate::session::INTERNAL_RPC_TIMEOUT)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| infra(e.to_string()))?
            .text()
            .await
            .map_err(|e| infra(e.to_string()))?;
        let answer: Value = serde_json::from_str(&answer).map_err(|e| infra(e.to_string()))?;
        if let Some(error) = answer.get("error").filter(|e| !e.is_null()) {
            return Err(infra(
                error["message"].as_str().unwrap_or("error").to_string(),
            ));
        }
        let value = &answer["result"]["value"];
        let results = value["transactionResults"]
            .as_array()
            .ok_or_else(|| infra("no results".into()))?;
        // A failed transaction stops the bundle: what follows it is empty.
        // The first one's own error says why; a view's means the account
        // couldn't be measured.
        if !value["summary"].is_string()
            && results.first().is_some_and(|first| first["err"].is_null())
        {
            return Err(infra(format!("{}", value["summary"])));
        }
        Ok(results.clone())
    }
}

/// A send's status, for the next step's market.
fn status(step: u64, id: u32, state: SendState) -> SendStatus {
    SendStatus {
        step,
        source: SendSource::Raw(id),
        venue: String::new(),
        client_id: None,
        order: None,
        expires_slot: None,
        state,
        slot: None,
        fee: None,
        tip: None,
    }
}

fn address(name: &str) -> Result<Pubkey, String> {
    Pubkey::from_str(name).map_err(|_| format!("{name} is not a base58 address"))
}

/// `tx`'s instructions, with every key resolved: the fresh signers', the
/// derived addresses' in list order, and the authority's.
pub fn resolve(
    tx: &RawTransaction,
    authority: Pubkey,
    fresh: &[Pubkey],
) -> Result<Vec<Instruction>, String> {
    let mut derived: Vec<Pubkey> = Vec::with_capacity(tx.derived.len());
    let key = |key: &AccountKey, derived: &[Pubkey]| -> Result<Pubkey, String> {
        match key {
            AccountKey::Address(name) => address(name),
            AccountKey::Authority => Ok(authority),
            AccountKey::Fresh(n) => fresh
                .get(usize::from(*n))
                .copied()
                .ok_or_else(|| format!("fresh signer {n}: the transaction has {}", fresh.len())),
            AccountKey::Derived(n) => derived
                .get(usize::from(*n))
                .copied()
                .ok_or_else(|| format!("derived address {n} is named before it is made")),
        }
    };
    for (i, derivation) in tx.derived.iter().enumerate() {
        let (program, seeds) = match derivation {
            Derivation::Pda { program, seeds } => {
                let mut bytes = Vec::with_capacity(seeds.len());
                for seed in seeds {
                    bytes.push(match seed {
                        Seed::Bytes(b) => b.clone(),
                        Seed::Key(k) => key(k, &derived)?.to_bytes().to_vec(),
                    });
                }
                (key(program, &derived)?, bytes)
            }
            Derivation::Ata {
                owner,
                mint,
                token_program,
            } => (
                ASSOCIATED_TOKEN,
                vec![
                    key(owner, &derived)?.to_bytes().to_vec(),
                    key(token_program, &derived)?.to_bytes().to_vec(),
                    key(mint, &derived)?.to_bytes().to_vec(),
                ],
            ),
        };
        let seeds: Vec<&[u8]> = seeds.iter().map(Vec::as_slice).collect();
        let (address, _) = Pubkey::try_find_program_address(&seeds, &program)
            .ok_or_else(|| format!("derived address {i}: no program address"))?;
        derived.push(address);
    }
    tx.instructions
        .iter()
        .map(|ix| {
            Ok(Instruction {
                program_id: address(&ix.program)?,
                accounts: ix
                    .accounts
                    .iter()
                    .map(|a| {
                        Ok(AccountMeta {
                            pubkey: key(&a.key, &derived)?,
                            is_signer: a.signer,
                            is_writable: a.writable,
                        })
                    })
                    .collect::<Result<_, String>>()?,
                data: ix.data.clone(),
            })
        })
        .collect()
}

/// A v0 message paid by `payer`: it compiles any instructions, or says why
/// not, where a legacy message panics on too many keys.
fn compile(
    payer: Pubkey,
    instructions: &[Instruction],
    tables: &[AddressLookupTableAccount],
    blockhash: Hash,
) -> Result<VersionedMessage, String> {
    if instructions.is_empty() {
        return Err("no instructions".into());
    }
    v0::Message::try_compile(&payer, instructions, tables, blockhash)
        .map(VersionedMessage::V0)
        .map_err(|e| format!("compiling it: {e}"))
}

/// A lookup table account's addresses.
fn lookup_table(key: Pubkey, data: &[u8]) -> Result<AddressLookupTableAccount, String> {
    let addresses = data
        .get(LOOKUP_TABLE_META..)
        .filter(|rest| rest.len() % 32 == 0)
        .ok_or_else(|| format!("lookup table {key}: malformed"))?;
    Ok(AddressLookupTableAccount {
        key,
        addresses: addresses
            .chunks_exact(32)
            .map(|bytes| Pubkey::new_from_array(bytes.try_into().expect("32 bytes")))
            .collect(),
    })
}

/// The accounts the instructions may write, lookup tables' included, in
/// the order first named.
fn writable_keys(instructions: &[Instruction]) -> Vec<Pubkey> {
    let mut keys = Vec::new();
    for meta in instructions.iter().flat_map(|ix| &ix.accounts) {
        if meta.is_writable && !keys.contains(&meta.pubkey) {
            keys.push(meta.pubkey);
        }
    }
    keys
}

/// Every program a transaction's logs show it running, at any depth, in
/// the order first seen; refused if the logs were cut short.
pub fn invoked(logs: &[String]) -> Result<Vec<Pubkey>, String> {
    let mut programs = Vec::new();
    for line in logs {
        if line.starts_with("Log truncated") {
            return Err("its logs were cut short, so what it runs can't be checked".into());
        }
        let Some(rest) = line.strip_prefix("Program ") else {
            continue;
        };
        let mut words = rest.split(' ');
        if let (Some(id), Some("invoke")) = (words.next(), words.next())
            && let Ok(program) = Pubkey::from_str(id)
            && !programs.contains(&program)
        {
            programs.push(program);
        }
    }
    Ok(programs)
}

/// How much `owner`'s USDC changes across `watched` (a transaction's
/// writable accounts), from their states before and after it, in atoms.
pub fn wallet_change(
    owner: &Pubkey,
    watched: &[Pubkey],
    before: &Value,
    after: &Value,
) -> Result<i128, String> {
    let owned = |states: &Value| -> Result<i128, String> {
        let Some(states) = states.as_array() else {
            return Ok(0);
        };
        if states.len() != watched.len() {
            return Err("the simulation's account states don't match its accounts".into());
        }
        Ok(states.iter().map(|state| usdc_of(owner, state)).sum())
    };
    Ok(owned(after)? - owned(before)?)
}

/// The USDC `owner` holds in one account, given as JSON-RPC returns it.
fn usdc_of(owner: &Pubkey, state: &Value) -> i128 {
    let program = state["owner"]
        .as_str()
        .and_then(|o| Pubkey::from_str(o).ok());
    if !matches!(program, Some(p) if p == TOKEN || p == TOKEN_2022) {
        return 0;
    }
    let Some(data) = state["data"][0]
        .as_str()
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
    else {
        return 0;
    };
    match (data.get(0..32), data.get(32..64), data.get(64..72)) {
        (Some(mint), Some(holder), Some(amount))
            if mint == USDC_MINT.as_ref() && holder == owner.as_ref() =>
        {
            i128::from(u64::from_le_bytes(amount.try_into().expect("8 bytes")))
        }
        _ => 0,
    }
}

/// One Hawkeye view's return data, from a bundle result: its
/// `returnData`, or, where the node leaves that out of a bundle's results,
/// the `Program return:` line the runtime logs for it.
fn view<T: bytemuck::Pod + Copy>(result: &Value, kind: &'static str) -> Result<T, String> {
    let fail = |why: &str| format!("couldn't measure the account after it: {kind}: {why}");
    if !result["err"].is_null() {
        return Err(fail(&result["err"].to_string()));
    }
    let hawkeye = phoenix_rise::ix::HAWKEYE_PROGRAM_ID.to_string();
    let data = &result["returnData"];
    let encoded = if data["programId"].as_str() == Some(hawkeye.as_str()) {
        data["data"][0].as_str()
    } else {
        let returned = format!("Program return: {hawkeye} ");
        result["logs"].as_array().and_then(|logs| {
            logs.iter()
                .rev()
                .filter_map(Value::as_str)
                .find_map(|line| line.strip_prefix(returned.as_str()))
        })
    };
    let bytes = encoded
        .ok_or_else(|| fail("no view returned"))
        .and_then(|b| {
            base64::engine::general_purpose::STANDARD
                .decode(b)
                .map_err(|_| fail("return data isn't base64"))
        })?;
    decode_hawkeye_return::<T>(&bytes, kind).map_err(|e| fail(&e.to_string()))
}

/// The compute budget's `SetComputeUnitLimit`.
fn compute_limit(units: u32) -> Instruction {
    let mut data = vec![2];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET,
        accounts: Vec::new(),
        data,
    }
}

fn encode(wire: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(wire)
}

#[cfg(test)]
mod tests {
    use mtx_core::Mandate;
    use mtx_kit::{
        TxOptions,
        raw::{RawAccount, RawInstruction},
    };
    use surfpool_sdk::SimnetCommand;

    use super::*;
    use crate::{guard::Guard, keys::SessionKeys, phoenix};

    const SPCX: &str = "SPCX";
    const SYSTEM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

    fn fresh() -> FreshKeys {
        SessionKeys::from_eval_seed(b"a batch secret of at least 32 chars")
            .session("bot/0/raw")
            .fresh()
    }

    fn account(key: AccountKey, signer: bool, writable: bool) -> RawAccount {
        RawAccount {
            key,
            signer,
            writable,
        }
    }

    /// `ix` as a raw instruction: each of `named`'s addresses by its key,
    /// any other by its address.
    fn raw(ix: &Instruction, named: &[(Pubkey, AccountKey)]) -> RawInstruction {
        RawInstruction {
            program: ix.program_id.to_string(),
            accounts: ix
                .accounts
                .iter()
                .map(|meta| {
                    let key = named
                        .iter()
                        .find(|(address, _)| *address == meta.pubkey)
                        .map_or_else(
                            || AccountKey::Address(meta.pubkey.to_string()),
                            |(_, key)| key.clone(),
                        );
                    account(key, meta.is_signer, meta.is_writable)
                })
                .collect(),
            data: ix.data.clone(),
        }
    }

    fn transaction(instructions: Vec<RawInstruction>) -> RawTransaction {
        RawTransaction {
            instructions,
            ..RawTransaction::default()
        }
    }

    #[test]
    fn keys_resolve_in_the_documented_order() {
        let authority = Pubkey::new_unique();
        let fresh: Vec<Pubkey> = (0..2).map(|n| fresh().keypair(4, 1, n).pubkey()).collect();
        let phoenix_program = Pubkey::new_unique();
        let tx = RawTransaction {
            instructions: vec![RawInstruction {
                program: phoenix_program.to_string(),
                accounts: vec![
                    account(AccountKey::Authority, true, true),
                    account(AccountKey::Fresh(1), true, true),
                    account(AccountKey::Derived(0), false, true),
                    account(AccountKey::Derived(1), false, false),
                    account(AccountKey::Derived(2), false, false),
                ],
                data: vec![9],
            }],
            fresh_signers: 2,
            derived: vec![
                // The trader account Phoenix derives for an authority.
                Derivation::Pda {
                    program: AccountKey::Address(phoenix_program.to_string()),
                    seeds: vec![
                        Seed::Bytes(b"trader".to_vec()),
                        Seed::Key(AccountKey::Authority),
                        Seed::Bytes(vec![0, 0]),
                    ],
                },
                Derivation::Ata {
                    owner: AccountKey::Fresh(1),
                    mint: AccountKey::Address(USDC_MINT.to_string()),
                    token_program: AccountKey::Address(TOKEN.to_string()),
                },
                // One made from another.
                Derivation::Pda {
                    program: AccountKey::Address(phoenix_program.to_string()),
                    seeds: vec![Seed::Key(AccountKey::Derived(1))],
                },
            ],
            ..RawTransaction::default()
        };
        tx.check().unwrap();
        let ix = &resolve(&tx, authority, &fresh).unwrap()[0];
        let trader = Pubkey::find_program_address(
            &[b"trader", authority.as_ref(), &[0, 0]],
            &phoenix_program,
        )
        .0;
        let ata = mtx_kit::chain::token::ata(&fresh[1], &USDC_MINT);
        let chained = Pubkey::find_program_address(&[ata.as_ref()], &phoenix_program).0;
        let keys: Vec<_> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert_eq!(keys, [authority, fresh[1], trader, ata, chained]);
        assert!(ix.accounts[1].is_signer && !ix.accounts[2].is_signer);
    }

    #[test]
    fn a_seed_with_no_program_address_is_refused_not_panicked_on() {
        // `check` refuses these; `resolve` must hold up without it too.
        let program = AccountKey::Address(Pubkey::new_unique().to_string());
        for seeds in [
            vec![Seed::Bytes(vec![1; 33])],
            vec![Seed::Bytes(vec![1]); 16],
        ] {
            let tx = RawTransaction {
                instructions: vec![RawInstruction {
                    program: Pubkey::new_unique().to_string(),
                    accounts: vec![account(AccountKey::Derived(0), false, false)],
                    data: vec![],
                }],
                derived: vec![Derivation::Pda {
                    program: program.clone(),
                    seeds,
                }],
                ..RawTransaction::default()
            };
            assert_eq!(
                resolve(&tx, Pubkey::new_unique(), &[]),
                Err("derived address 0: no program address".to_string())
            );
        }
    }

    /// Whatever a strategy sends, the grader's side of a raw send returns:
    /// a few thousand transactions of every shape, well formed or not.
    #[test]
    fn malformed_transactions_never_panic() {
        struct Rng(u64);
        impl Rng {
            fn below(&mut self, n: u64) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0 % n.max(1)
            }
            fn key(&mut self, derived: u64, addresses: &[String]) -> AccountKey {
                match self.below(5) {
                    0 => AccountKey::Authority,
                    1 => AccountKey::Fresh(self.below(10) as u8),
                    2 => AccountKey::Derived(self.below(derived + 2) as u16),
                    _ => AccountKey::Address(
                        addresses[self.below(addresses.len() as u64) as usize].clone(),
                    ),
                }
            }
        }
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let addresses: Vec<String> = (0..6)
            .map(|_| Pubkey::new_unique().to_string())
            .chain(["".into(), "not base58!".into(), "1".repeat(60)])
            .collect();
        let authority = Pubkey::new_unique();
        for _ in 0..3_000 {
            let derived: Vec<Derivation> = (0..rng.below(5))
                .map(|i| {
                    if rng.below(2) == 0 {
                        Derivation::Ata {
                            owner: rng.key(i, &addresses),
                            mint: rng.key(i, &addresses),
                            token_program: rng.key(i, &addresses),
                        }
                    } else {
                        Derivation::Pda {
                            program: rng.key(i, &addresses),
                            seeds: (0..rng.below(18))
                                .map(|_| match rng.below(2) {
                                    0 => Seed::Bytes(vec![7; rng.below(40) as usize]),
                                    _ => Seed::Key(rng.key(i, &addresses)),
                                })
                                .collect(),
                        }
                    }
                })
                .collect();
            let instructions = (0..rng.below(4))
                .map(|_| RawInstruction {
                    program: addresses[rng.below(addresses.len() as u64) as usize].clone(),
                    accounts: (0..rng.below(300))
                        .map(|_| {
                            let key = rng.key(4, &addresses);
                            account(key, rng.below(3) == 0, rng.below(2) == 0)
                        })
                        .collect(),
                    data: vec![1; rng.below(2_000) as usize],
                })
                .collect();
            let tx = RawTransaction {
                instructions,
                lookup_tables: Vec::new(),
                fresh_signers: rng.below(10) as u8,
                derived,
                tx: None,
            };
            let _ = tx.check();
            let fresh: Vec<Pubkey> = (0..tx.fresh_signers.min(8))
                .map(|_| Pubkey::new_unique())
                .collect();
            if let Ok(instructions) = resolve(&tx, authority, &fresh)
                && let Ok(message) = compile(authority, &instructions, &[], Hash::default())
            {
                let tx = VersionedTransaction {
                    signatures: vec![
                        Signature::default();
                        usize::from(message.header().num_required_signatures)
                    ],
                    message,
                };
                let _ = wincode::serialize(&tx);
            }
        }
        // Too many accounts for one message: refused, where a legacy
        // message panics.
        let many = RawInstruction {
            program: SYSTEM.to_string(),
            accounts: (0..300)
                .map(|_| {
                    account(
                        AccountKey::Address(Pubkey::new_unique().to_string()),
                        false,
                        true,
                    )
                })
                .collect(),
            data: vec![],
        };
        let ixs = resolve(&transaction(vec![many]), authority, &[]).unwrap();
        assert!(compile(authority, &ixs, &[], Hash::default()).is_err());
        assert!(lookup_table(authority, &[0; 57]).is_err());
        assert!(lookup_table(authority, &[0; 10]).is_err());
        assert_eq!(
            lookup_table(authority, &[0; 56 + 64])
                .unwrap()
                .addresses
                .len(),
            2
        );
    }

    #[test]
    fn the_programs_a_transaction_runs_come_from_its_logs() {
        let token = TOKEN.to_string();
        let ata = ASSOCIATED_TOKEN.to_string();
        let logs: Vec<String> = [
            format!("Program {ata} invoke [1]"),
            "Program log: Create".into(),
            format!("Program {token} invoke [2]"),
            format!("Program {token} consumed 1000 of 200000 compute units"),
            format!("Program {token} success"),
            format!("Program {SYSTEM} invoke [2]"),
            format!("Program {ata} success"),
        ]
        .into();
        assert_eq!(invoked(&logs).unwrap(), [ASSOCIATED_TOKEN, TOKEN, SYSTEM]);
        let cut = [logs[0].clone(), "Log truncated".to_string()];
        assert!(invoked(&cut).is_err());
    }

    fn token_account(mint: &Pubkey, owner: &Pubkey, amount: u64, program: &Pubkey) -> Value {
        let mut data = mint.to_bytes().to_vec();
        data.extend_from_slice(owner.as_ref());
        data.extend_from_slice(&amount.to_le_bytes());
        data.resize(165, 0);
        json!({ "owner": program.to_string(), "data": [encode(&data), "base64"], "lamports": 1 })
    }

    #[test]
    fn the_wallet_counts_the_owners_usdc_in_any_account_it_writes() {
        let owner = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let watched = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let before = json!([
            token_account(&USDC_MINT, &owner, 100, &TOKEN),
            token_account(&USDC_MINT, &other, 5, &TOKEN),
            Value::Null,
        ]);
        // 60 leaves to another owner's account; 10 goes to a new account
        // the owner holds; a look-alike owned by another program is no
        // token account.
        let after = json!([
            token_account(&USDC_MINT, &owner, 30, &TOKEN),
            token_account(&USDC_MINT, &other, 65, &TOKEN),
            token_account(&USDC_MINT, &owner, 10, &TOKEN),
        ]);
        assert_eq!(wallet_change(&owner, &watched, &before, &after), Ok(-60));
        let fake = json!([
            token_account(&USDC_MINT, &owner, 100, &TOKEN),
            Value::Null,
            token_account(&USDC_MINT, &owner, 1_000, &SYSTEM),
        ]);
        assert_eq!(wallet_change(&owner, &watched, &before, &fake), Ok(0));
        assert!(wallet_change(&owner, &watched[..2], &before, &after).is_err());
    }

    #[test]
    fn the_audit_holds_the_account_to_its_mandate() {
        let mandate = Mandate::perp("phoenix", SPCX, 70.0, 50.0);
        let spcx = mandate.universe[0].instrument.clone();
        let start = Measured {
            position: 10.0,
            outside: 0,
            value_usd: 10_000.0,
            wallet_atoms: 100_000_000,
        };
        let after = |position, value_usd, wallet_atoms, outside| Measured {
            position,
            outside,
            value_usd,
            wallet_atoms,
        };
        let audit = |drawdown, after: Measured| audit(&mandate, &spcx, drawdown, &start, &after);
        assert_eq!(audit(0.0, after(70.0, 9_990.0, 100_000_000, 0)), None);
        assert_eq!(audit(0.0, after(-70.0, 10_000.0, 100_000_000, 0)), None);
        assert!(matches!(
            audit(0.0, after(70.5, 10_000.0, 100_000_000, 0)),
            Some(Breach::Position { cap: 70.0, .. })
        ));
        assert!(matches!(
            audit(0.0, after(-71.0, 10_000.0, 100_000_000, 0)),
            Some(Breach::Position { .. })
        ));
        assert_eq!(
            audit(0.0, after(10.0, 10_000.0, 100_000_000, 1)),
            Some(Breach::OutsideUniverse { positions: 1 })
        );
        // 40 out of the wallet from 15 below the peak: 55, past 50.
        assert_eq!(audit(10.0, after(10.0, 10_000.0, 60_000_000, 0)), None);
        assert!(matches!(
            audit(15.0, after(10.0, 10_000.0, 60_000_000, 0)),
            Some(Breach::Drawdown { limit: 50.0, .. })
        ));
        // A floor.
        let mut long_only = mandate.clone();
        long_only.universe[0].min_position = Some(0.0);
        assert!(matches!(
            super::audit(
                &long_only,
                &spcx,
                0.0,
                &start,
                &after(-1.0, 10_000.0, 100_000_000, 0)
            ),
            Some(Breach::Floor { .. })
        ));
    }

    /// A stepped world as a session starts one: a bot and another trader
    /// onboarded, Phoenix's readings restamped, the world on its first slot.
    struct World {
        _world: crate::snapshot::OfflineWorld,
        _internal: Guard,
        guard: Guard,
        rpc: solana_rpc_client::rpc_client::RpcClient,
        phoenix: mtx_kit::Phoenix,
        bot: Keypair,
        other: Keypair,
    }

    async fn world() -> World {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../snapshots/phoenix-spcx-2026-10-29");
        let world = crate::snapshot::start_offline_stepped(&dir).await.unwrap();
        let internal = Guard::internal(world.surfnet.rpc_url()).await.unwrap();
        let rpc = mtx_kit::rpc_client(internal.url());
        let phoenix = mtx_kit::Phoenix::new(world.exchange.clone(), internal.url());
        let keys = SessionKeys::from_eval_seed(b"a batch secret of at least 32 chars");
        let keyring = keys.session("bot/0/raw");
        let (bot, other) = (keyring.keypair("bot", 0), keyring.keypair("taker", 0));
        let traders = phoenix::onboard_traders_stepped(
            &world.surfnet,
            &phoenix,
            &rpc,
            &[&bot, &other],
            10_000.0,
        )
        .await
        .unwrap();
        let slot = rpc.get_slot().unwrap();
        crate::chain::register_scenario(
            &rpc,
            "readings",
            traders
                .iter()
                .map(|t| phoenix::enable_trader(*t, 1))
                .collect(),
        )
        .unwrap();
        world
            .surfnet
            .send_command(SimnetCommand::SlotForward(None))
            .unwrap();
        crate::session::wait_for_slot(&rpc, slot + 1).await.unwrap();
        let guard = Guard::start(world.surfnet.rpc_url()).await.unwrap();
        World {
            _world: world,
            _internal: internal,
            guard,
            rpc,
            phoenix,
            bot,
            other,
        }
    }

    impl World {
        fn sender(&self, programs: Vec<Pubkey>, mandate: Mandate) -> RawSender {
            let exchange = mtx_kit::load_exchange(
                &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../snapshots/phoenix-spcx-2026-10-29/exchange.json"),
            )
            .unwrap();
            RawSender::new(
                RawConfig {
                    programs,
                    fresh: SessionKeys::from_eval_seed(b"a batch secret of at least 32 chars")
                        .session("bot/0/raw")
                        .fresh(),
                    internal_url: self._internal.url().to_string(),
                    instrument: mandate.universe[0].instrument.clone(),
                    mandate,
                    unsettled_funding: false,
                },
                self.bot.pubkey(),
                exchange,
                SPCX,
                self.guard.url(),
            )
        }

        /// A market order for `units` of SPCX, as the kit builds it for
        /// `owner`, raw: the owner as the authority, its trader account as
        /// a derived address.
        async fn market_order(&self, owner: &Keypair, units: i64) -> RawTransaction {
            let trader = mtx_kit::PhoenixTrader::new(
                self.phoenix.exchange().clone(),
                SPCX,
                owner.pubkey(),
                self._internal.url(),
            )
            .unwrap();
            let side = if units > 0 {
                mtx_kit::core::Side::Buy
            } else {
                mtx_kit::core::Side::Sell
            };
            let plan = trader
                .plan(&mtx_kit::PhoenixOrder::Market {
                    side,
                    quantity: Decimal::from(units.abs()),
                    worst_price: None,
                    all_or_none: false,
                })
                .await
                .unwrap();
            let step = &plan.steps[0];
            let program = step.instructions[0].program_id;
            let named = [
                (owner.pubkey(), AccountKey::Authority),
                (
                    self.phoenix.trader_account(&owner.pubkey()),
                    AccountKey::Derived(0),
                ),
            ];
            let mut instructions = vec![raw(&compute_limit(step.compute_unit_limit), &[])];
            instructions.extend(step.instructions.iter().map(|ix| raw(ix, &named)));
            RawTransaction {
                instructions,
                derived: vec![Derivation::Pda {
                    program: AccountKey::Address(program.to_string()),
                    seeds: vec![
                        Seed::Bytes(b"trader".to_vec()),
                        Seed::Key(AccountKey::Authority),
                        Seed::Bytes(vec![0, 0]),
                    ],
                }],
                tx: Some(TxOptions::default()),
                ..RawTransaction::default()
            }
        }

        async fn position(&self) -> f64 {
            let asset = self
                .phoenix
                .hawkeye(self.bot.pubkey())
                .view_margin_for_asset(
                    self.bot.pubkey(),
                    0,
                    0,
                    self.phoenix.exact_market(SPCX).unwrap().asset_id,
                )
                .await
                .unwrap()
                .value;
            let market = self.phoenix.exact_market(SPCX).unwrap();
            grader_f64(
                Holding::Perp(
                    market.position(asset.base_lots, asset.entry_price_quote_lots_per_base_lot),
                )
                .position(),
            )
        }
    }

    /// What a session's step does with raw sends, against a real chain:
    /// takes or refuses each, then sends what it took after the sheet.
    /// Returns every refusal and status, for a replay to compare.
    async fn scripted_step(world: &World) -> (Vec<Result<RawReceipt, String>>, Vec<SendStatus>) {
        let phoenix_program = world.market_order(&world.bot, 1).await.instructions[1]
            .program
            .parse::<Pubkey>()
            .unwrap();
        let mut sender = world.sender(
            vec![phoenix_program, SYSTEM, TOKEN, ASSOCIATED_TOKEN],
            Mandate::perp("phoenix", SPCX, 5.0, 50.0),
        );
        let usdc = AccountKey::Address(USDC_MINT.to_string());
        let token = AccountKey::Address(TOKEN.to_string());
        let wallet = Derivation::Ata {
            owner: AccountKey::Authority,
            mint: usdc.clone(),
            token_program: token.clone(),
        };
        let usdc_transfer = |to: AccountKey, atoms: u64| {
            let mut data = vec![3];
            data.extend_from_slice(&atoms.to_le_bytes());
            RawInstruction {
                program: TOKEN.to_string(),
                accounts: vec![
                    account(AccountKey::Derived(0), false, true),
                    account(to, false, true),
                    account(AccountKey::Authority, true, false),
                ],
                data,
            }
        };
        let other_wallet = mtx_kit::chain::token::ata(&world.other.pubkey(), &USDC_MINT);
        sender.begin(0, None);
        let mut results = Vec::new();

        // 1. A Phoenix market buy of 2, built raw, its trader account
        //    derived: taken.
        results.push(sender.take(world.market_order(&world.bot, 2).await).await);
        // 2. A buy of 20: past the cap of 5, refused in simulation.
        results.push(sender.take(world.market_order(&world.bot, 20).await).await);
        // 3. A fresh account, created with a fresh signer: taken.
        let mut create = vec![0, 0, 0, 0];
        create.extend_from_slice(&1_000_000u64.to_le_bytes());
        create.extend_from_slice(&0u64.to_le_bytes());
        create.extend_from_slice(SYSTEM.as_ref());
        results.push(
            sender
                .take(RawTransaction {
                    instructions: vec![RawInstruction {
                        program: SYSTEM.to_string(),
                        accounts: vec![
                            account(AccountKey::Authority, true, true),
                            account(AccountKey::Fresh(0), true, true),
                        ],
                        data: create,
                    }],
                    fresh_signers: 1,
                    ..RawTransaction::default()
                })
                .await,
        );
        // 4. A fresh owner's USDC account, made by the associated token
        //    program (which runs the system and token programs), then 1 USDC
        //    into it from the bot's own, both derived: taken.
        let ata_create = RawInstruction {
            program: ASSOCIATED_TOKEN.to_string(),
            accounts: vec![
                account(AccountKey::Authority, true, true),
                account(AccountKey::Derived(1), false, true),
                account(AccountKey::Fresh(0), false, false),
                account(usdc.clone(), false, false),
                account(AccountKey::Address(SYSTEM.to_string()), false, false),
                account(token.clone(), false, false),
            ],
            data: vec![1],
        };
        let to_fresh = RawTransaction {
            instructions: vec![
                ata_create.clone(),
                usdc_transfer(AccountKey::Derived(1), 1_000_000),
            ],
            fresh_signers: 1,
            derived: vec![
                wallet.clone(),
                Derivation::Ata {
                    owner: AccountKey::Fresh(0),
                    mint: usdc.clone(),
                    token_program: token.clone(),
                },
            ],
            ..RawTransaction::default()
        };
        results.push(sender.take(to_fresh.clone()).await);
        // 5. 60 USDC out of the wallet, from a session 0 below its peak
        //    with a limit of 50: refused in simulation.
        let drain = RawTransaction {
            instructions: vec![usdc_transfer(
                AccountKey::Address(other_wallet.to_string()),
                60_000_000,
            )],
            derived: vec![wallet.clone()],
            ..RawTransaction::default()
        };
        results.push(sender.take(drain).await);
        // 6. The other trader's USDC, which it never signed for: the
        //    simulation fails.
        results.push(
            sender
                .take(RawTransaction {
                    instructions: vec![RawInstruction {
                        program: TOKEN.to_string(),
                        accounts: vec![
                            account(AccountKey::Address(other_wallet.to_string()), false, true),
                            account(AccountKey::Derived(0), false, true),
                            account(
                                AccountKey::Address(world.other.pubkey().to_string()),
                                false,
                                false,
                            ),
                        ],
                        data: [vec![3], 1_000_000u64.to_le_bytes().to_vec()].concat(),
                    }],
                    derived: vec![wallet.clone()],
                    ..RawTransaction::default()
                })
                .await,
        );
        // 7. Signing as the other trader: refused before anything runs
        //    (the host runs `check` first, as here).
        let mut as_other = world.market_order(&world.other, 1).await;
        as_other.derived.clear();
        for ix in &mut as_other.instructions {
            for meta in &mut ix.accounts {
                if meta.key == AccountKey::Authority {
                    meta.key = AccountKey::Address(world.other.pubkey().to_string());
                }
            }
        }
        results.push(as_other.check().map(|()| unreachable!()));
        // 8. An order signed by the bot on the other trader's account:
        //    Phoenix refuses an account the signer doesn't own.
        let mut theirs = world.market_order(&world.bot, 1).await;
        theirs.derived.clear();
        let their_account = world
            .phoenix
            .trader_account(&world.other.pubkey())
            .to_string();
        for ix in &mut theirs.instructions {
            for meta in &mut ix.accounts {
                if meta.key == AccountKey::Derived(0) {
                    meta.key = AccountKey::Address(their_account.clone());
                }
            }
        }
        results.push(sender.take(theirs).await);
        sender.close();
        results.push(sender.take(transaction(vec![])).await);

        let statuses = sender.flush(&world.bot).await;
        (results, statuses)
    }

    /// Raw sends against a real chain: what lands, what is refused and why,
    /// and that a second grade of the same session does the same.
    #[tokio::test(flavor = "multi_thread")]
    async fn raw_sends_land_only_within_the_allowlist_and_the_mandate() {
        let first = world().await;
        let (results, statuses) = scripted_step(&first).await;
        let refusal = |i: usize| results[i].clone().unwrap_err();
        let fresh = fresh();
        assert_eq!(results[0].as_ref().unwrap().id, 0);
        assert!(
            refusal(1).contains("over the position cap of 5"),
            "{}",
            refusal(1)
        );
        assert_eq!(
            results[2].as_ref().unwrap().fresh_signers,
            [fresh.keypair(0, 1, 0).pubkey().to_string()]
        );
        assert_eq!(results[3].as_ref().unwrap().id, 2);
        assert!(
            refusal(4).contains("from its peak, over the $50 limit"),
            "{}",
            refusal(4)
        );
        assert!(
            refusal(5).contains("the simulation failed"),
            "{}",
            refusal(5)
        );
        assert!(refusal(6).contains("can't sign"), "{}", refusal(6));
        assert!(
            refusal(7).contains("the simulation failed"),
            "{}",
            refusal(7)
        );
        assert_eq!(refusal(8), "raw sends go out from `step` only");

        // The three taken land, in order, after the sheet.
        assert_eq!(statuses.len(), 3);
        for (id, status) in statuses.iter().enumerate() {
            assert_eq!(status.source, SendSource::Raw(id as u32));
            assert!(matches!(status.state, SendState::Landed(_)), "{status:?}");
            assert!(status.slot.is_some() && status.fee.is_some_and(|fee| fee >= 5_000));
            assert_eq!((status.order.as_ref(), status.venue.as_str()), (None, ""));
        }
        assert!((first.position().await - 2.0).abs() < 1e-9);
        let created = first
            .rpc
            .get_account(&fresh.keypair(0, 1, 0).pubkey())
            .unwrap();
        assert_eq!(created.lamports, 1_000_000);

        // The same session again, from scratch: the same keys, the same
        // transactions, the same signatures.
        let again = world().await;
        assert_eq!(scripted_step(&again).await, (results, statuses));
    }

    /// Programs off the list are refused, at the top level or run by a
    /// listed one.
    #[tokio::test(flavor = "multi_thread")]
    async fn programs_off_the_list_are_refused_directly_or_through_another() {
        let world = world().await;
        let mut sender = world.sender(
            vec![ASSOCIATED_TOKEN],
            Mandate::perp("phoenix", SPCX, 70.0, 50.0),
        );
        sender.begin(3, None);
        let memo = Pubkey::from_str_const("MemoSq4gqABAXKb96qnH83TBF8eB5RvwJGAtnwGk8bv");
        let unlisted = sender
            .take(transaction(vec![RawInstruction {
                program: memo.to_string(),
                accounts: vec![],
                data: b"hi".to_vec(),
            }]))
            .await;
        assert_eq!(
            unlisted,
            Err(format!(
                "{memo} isn't a program this session's raw sends may run"
            ))
        );
        // The associated token program is listed; the system and token
        // programs it runs aren't.
        let through = sender
            .take(RawTransaction {
                instructions: vec![RawInstruction {
                    program: ASSOCIATED_TOKEN.to_string(),
                    accounts: vec![
                        account(AccountKey::Authority, true, true),
                        account(AccountKey::Derived(0), false, true),
                        account(AccountKey::Fresh(0), false, false),
                        account(AccountKey::Address(USDC_MINT.to_string()), false, false),
                        account(AccountKey::Address(SYSTEM.to_string()), false, false),
                        account(AccountKey::Address(TOKEN.to_string()), false, false),
                    ],
                    data: vec![1],
                }],
                derived: vec![Derivation::Ata {
                    owner: AccountKey::Fresh(0),
                    mint: AccountKey::Address(USDC_MINT.to_string()),
                    token_program: AccountKey::Address(TOKEN.to_string()),
                }],
                fresh_signers: 1,
                ..RawTransaction::default()
            })
            .await
            .unwrap_err();
        assert!(
            through.starts_with("it runs ")
                && through.contains("isn't a program this session's raw sends may run"),
            "{through}"
        );
        assert!(sender.flush(&world.bot).await.is_empty());
    }

    /// The probe (`bots/raw-probe`, `just build`) as `MTX_TEST_RAW_PROBE_WASM`,
    /// graded in its scenario twice: the grader answers each of its sends as
    /// the scenario allows, sends what it takes, never disqualifies it for
    /// trying, and the second grade is the first.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a built component"]
    async fn the_raw_probe_is_answered_in_a_session_and_replays() {
        let wasm = std::path::PathBuf::from(std::env::var("MTX_TEST_RAW_PROBE_WASM").unwrap());
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let scenario =
            crate::scenario::Scenario::load(&root.join("bots/raw-probe/scenario.json")).unwrap();
        let keys = SessionKeys::random();
        let mut runs = Vec::new();
        for run in 0..2 {
            let dir =
                std::env::temp_dir().join(format!("mtx-raw-probe-{}-{run}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::copy(&wasm, dir.join("bot.wasm")).unwrap();
            let log = dir.join("bot.log");
            let report = crate::session::run(crate::session::SessionOptions {
                snapshot_dir: &root.join("snapshots/phoenix-spcx-2026-10-29"),
                scenario: &scenario,
                bot_dir: &dir,
                bot_log: &log,
                slot: 0,
                regime: None,
                trusted: false,
                keys: keys.role("bot").session(0, &scenario.name),
                observer: None,
            })
            .await
            .unwrap();
            let log = std::fs::read_to_string(&log).unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            runs.push((report, log));
        }
        let (report, log) = &runs[0];
        let line = |what: &str| {
            log.lines()
                .find(|l| l.starts_with(&format!("{what}: ")))
                .unwrap_or_else(|| panic!("no {what:?} in\n{log}"))
                .to_string()
        };
        assert!(
            log.contains(&format!("programs \"{SYSTEM},{TOKEN},{ASSOCIATED_TOKEN}\"")),
            "{log}"
        );
        assert_eq!(
            line("in start"),
            r#"in start: Err("raw sends go out from `step` only")"#
        );
        assert!(
            line("create").starts_with("create: Ok(RawReceipt { id: 0,"),
            "{log}"
        );
        assert!(
            line("pay").starts_with("pay: Ok(RawReceipt { id: 1,"),
            "{log}"
        );
        assert!(line("unlisted").contains("isn't a program this session's raw sends may run"));
        assert!(line("cpi").starts_with("cpi: Err("), "{}", line("cpi"));
        assert!(
            line("drain").contains("over the $50 limit"),
            "{}",
            line("drain")
        );
        assert!(
            line("as another").contains("can't sign"),
            "{}",
            line("as another")
        );
        assert!(
            line("too big").contains("a transaction is at most 1232"),
            "{}",
            line("too big")
        );
        assert!(line("not an address").contains("is not a base58 address"));
        assert!(
            line("long seed").contains("a seed of 33 bytes"),
            "{}",
            line("long seed")
        );
        assert_eq!(line("empty"), r#"empty: Err("no instructions")"#);
        assert!(line("create again").starts_with("create again: Ok(RawReceipt { id: 0,"));
        assert_ne!(
            line("create").split("fresh_signers").nth(1),
            line("create again").split("fresh_signers").nth(1)
        );
        for taken in [
            "raw 0 of step 0: Landed(",
            "raw 1 of step 0: Landed(",
            "raw 0 of step 1: Landed(",
        ] {
            assert!(log.contains(taken), "no {taken:?} in\n{log}");
        }
        assert!(log.contains("fee Some(10000)"), "{log}");
        assert_eq!(report.turns.stopped, None);
        assert!(report.mandate.audit(&report.observed()).is_none());
        assert!(report.timeline.transactions.len() >= 3);

        // No fresh signer's secret, nor the bot's, is anywhere in what the
        // session wrote or reported.
        let keyring = keys.role("bot").session(0, &scenario.name);
        let fresh = keyring.fresh();
        let mut pairs = vec![keyring.keypair("bot", 0)];
        for step in 0..report.turns.steps {
            for id in 0..4 {
                for n in 0..2 {
                    pairs.push(fresh.keypair(step, id, n));
                }
            }
        }
        let written = format!(
            "{log}{}{}",
            serde_json::to_string(report).unwrap(),
            serde_json::to_string(&report.timeline).unwrap()
        )
        .replace([' ', '\n'], "");
        for pair in &pairs {
            let hex: String = pair.secret_bytes().iter().map(|b| format!("{b:02x}")).collect();
            for secret in [
                pair.to_base58_string(),
                hex,
                format!("{:?}", pair.secret_bytes()).replace(' ', ""),
                format!("{:?}", &pair.to_bytes()[..]).replace(' ', ""),
            ] {
                assert!(!written.contains(&secret), "a secret key in the session's output");
            }
        }

        // The second grade: the same log, the same transactions, the same
        // report but for the wall clock.
        let (again, again_log) = &runs[1];
        assert_eq!(log, again_log);
        let without_clock = |report: &crate::session::SessionReport| {
            let mut value = serde_json::to_value(report).unwrap();
            value["wall_ms"] = Value::Null;
            value["turns"]["wall_ms"] = Value::Null;
            (
                value,
                serde_json::to_value(&report.timeline.transactions).unwrap(),
            )
        };
        assert_eq!(without_clock(report), without_clock(again));
    }
}
