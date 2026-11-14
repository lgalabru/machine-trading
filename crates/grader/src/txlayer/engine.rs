//! One session's transaction layer: the pool, the background demand, the
//! leaders, and what has landed. The bot's guard and the grader submit to
//! it; the session builds and commits each slot through it.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use base64::Engine as _;
use serde_json::{Value, json};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::{Hash, versioned::VersionedTransaction};

use super::{
    RouteKind, TxLayer, builder,
    calibration::Calibration,
    draw,
    fees::FeeLedger,
    ghost::GhostPath,
    jito,
    leader::Leaders,
    normal,
    pool::{Arrival, BASE_FEE, Entry, EntryState, Pool, PooledTx},
    report::{BlockRecord, Landed, Refused, SendRecord, TxLayerReport},
};

/// The sender the bot's entries carry.
pub const BOT: &str = "bot";
/// How many blocks a blockhash stays valid for.
pub const BLOCKHASH_BLOCKS: u64 = 150;

/// A send for the pool: one transaction, or a bundle of them, signed.
#[derive(Debug, Clone)]
pub struct Submission {
    pub wires: Vec<Vec<u8>>,
    pub route: RouteKind,
    pub bundle: bool,
    /// Rebroadcast until this slot; `resign`: the grader signed it and may
    /// sign it again once it expired without landing.
    pub retry_until_slot: Option<u64>,
    pub resign: bool,
    /// Slots the scenario's latency fault adds, and whether its drop fault
    /// loses the delivery.
    pub delay_slots: u64,
    pub dropped: bool,
}

/// What a send gets back.
#[derive(Debug, Clone, PartialEq)]
pub enum Accepted {
    /// A transaction's signature, or a bundle's id.
    Signature(String),
    Bundle(String),
}

/// A refusal, as JSON-RPC answers it.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub code: i64,
    pub message: String,
}

/// Why a stopped-out session's pooled sends failed.
const STOPPED_OUT: &str = "the risk desk closed the account";

/// A rate-limited node's code.
pub const TOO_MANY_REQUESTS: i64 = -32429;
const INVALID_PARAMS: i64 = -32602;
/// A preflight failure: a blockhash not found, or a transaction already
/// processed.
const PREFLIGHT_FAILED: i64 = -32002;

/// What became of a send, as a status reads it.
#[derive(Debug, Clone, PartialEq)]
pub enum Fate {
    Pending,
    Landed {
        slot: u64,
        ok: bool,
        fee: u64,
        tip: u64,
        error: Option<String>,
        signature: String,
    },
    Failed(String),
    Expired,
}

pub struct TxEngine {
    rpc_url: String,
    http: reqwest::Client,
    state: Mutex<State>,
}

struct State {
    layer: TxLayer,
    calibration: Calibration,
    ghost: GhostPath,
    leaders: Leaders,
    base_slot: u64,
    pool: Pool,
    /// The bot's current turn, and the sends made in it so far.
    step: u64,
    step_seq: u64,
    sender_seqs: BTreeMap<String, u64>,
    /// The last slot produced, and the chain's block height there.
    slot: u64,
    height: u64,
    /// Each blockhash the session has seen, and the last block height it is
    /// valid at; the latest one.
    blockhashes: BTreeMap<Hash, u64>,
    latest: Hash,
    /// Lookup tables read so far.
    tables: BTreeMap<Pubkey, Vec<Pubkey>>,
    /// The bot's key, for sends the grader made and may sign again.
    signer: Keypair,
    ledger: FeeLedger,
    blocks: Vec<BlockRecord>,
    /// Errors of landed transactions, by signature.
    errors: BTreeMap<String, String>,
    /// The risk desk took the account over: nothing more is pooled or
    /// lands.
    closed: bool,
}

impl TxEngine {
    /// The layer for a session whose steps start at `base_slot`, through
    /// the grader's own endpoint `rpc_url`. `congestion`: the scenario's
    /// congestion fault, compiled into the background demand.
    pub async fn start(
        layer: &TxLayer,
        rpc_url: &str,
        base_slot: u64,
        end_slot: u64,
        slots_per_step: u64,
        congestion: Option<(u64, u64, u64)>,
        signer: Keypair,
    ) -> crate::Result<Arc<Self>> {
        let calibration =
            Calibration::builtin(&layer.calibration).map_err(crate::Error::TxLayer)?;
        let ghost = GhostPath::new(
            layer,
            &calibration,
            base_slot,
            end_slot,
            slots_per_step,
            congestion,
        );
        let leaders = Leaders::new(layer.seed, layer.validators, &calibration);
        let engine = Arc::new(Self {
            rpc_url: rpc_url.to_string(),
            http: reqwest::Client::new(),
            state: Mutex::new(State {
                layer: layer.clone(),
                calibration,
                ghost,
                leaders,
                base_slot,
                pool: Pool::default(),
                step: 0,
                step_seq: 0,
                sender_seqs: BTreeMap::new(),
                slot: base_slot,
                height: 0,
                blockhashes: BTreeMap::new(),
                latest: Hash::default(),
                tables: BTreeMap::new(),
                signer,
                ledger: FeeLedger::default(),
                blocks: Vec::new(),
                errors: BTreeMap::new(),
                closed: false,
            }),
        });
        engine.observe(base_slot).await?;
        Ok(engine)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("tx layer lock")
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let reply: Value = self
            .http
            .post(&self.rpc_url)
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

    /// Read the chain's block height and latest blockhash at `slot`.
    async fn observe(&self, slot: u64) -> crate::Result<()> {
        let latest = self
            .rpc("getLatestBlockhash", json!([{ "commitment": "processed" }]))
            .await
            .map_err(crate::Error::TxLayer)?;
        let height = self
            .rpc("getBlockHeight", json!([{ "commitment": "processed" }]))
            .await
            .map_err(crate::Error::TxLayer)?
            .as_u64()
            .unwrap_or_default();
        let hash: Hash = latest["value"]["blockhash"]
            .as_str()
            .and_then(|h| h.parse().ok())
            .unwrap_or_default();
        let valid = latest["value"]["lastValidBlockHeight"]
            .as_u64()
            .unwrap_or(height + BLOCKHASH_BLOCKS);
        let mut state = self.state();
        state.slot = slot;
        state.height = height;
        state.latest = hash;
        state.blockhashes.insert(hash, valid);
        Ok(())
    }

    /// The bot's turn `k` begins: its sends count from 0 again.
    pub fn set_step(&self, k: u64) {
        let mut state = self.state();
        state.step = k;
        state.step_seq = 0;
    }

    /// Put the bot's send in the pool. A transaction sent again with the
    /// same signature is a rebroadcast: a lost delivery tries again, and it
    /// never runs twice.
    pub async fn submit(&self, submission: Submission) -> Result<Accepted, Refusal> {
        let refuse = |code, message: String| Refusal { code, message };
        if submission.wires.is_empty() || submission.wires.len() > jito::MAX_BUNDLE {
            return Err(refuse(
                INVALID_PARAMS,
                format!("a bundle holds 1 to {} transactions", jito::MAX_BUNDLE),
            ));
        }
        // Lookup tables the transactions name, read once.
        let mut wanted = Vec::new();
        for wire in &submission.wires {
            if let Ok(tx) = wincode::deserialize::<VersionedTransaction>(wire)
                && let Some(lookups) = tx.message.address_table_lookups()
            {
                let known = self.state();
                wanted.extend(
                    lookups
                        .iter()
                        .map(|l| l.account_key)
                        .filter(|k| !known.tables.contains_key(k)),
                );
            }
        }
        for key in wanted {
            let account = self
                .rpc(
                    "getAccountInfo",
                    json!([key.to_string(), { "encoding": "base64", "commitment": "processed" }]),
                )
                .await
                .map_err(|e| refuse(INVALID_PARAMS, e))?;
            let addresses = account["value"]["data"][0]
                .as_str()
                .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
                .and_then(|data| {
                    data.get(56..).filter(|r| r.len() % 32 == 0).map(|rest| {
                        rest.chunks_exact(32)
                            .map(|b| Pubkey::new_from_array(b.try_into().expect("32 bytes")))
                            .collect::<Vec<_>>()
                    })
                });
            if let Some(addresses) = addresses {
                self.state().tables.insert(key, addresses);
            }
        }
        let txs = {
            let state = self.state();
            let tables = |key: &Pubkey| state.tables.get(key).cloned();
            submission
                .wires
                .iter()
                .map(|wire| PooledTx::parse(wire.clone(), &tables))
                .collect::<Result<Vec<_>, String>>()
                .map_err(|e| refuse(INVALID_PARAMS, e))?
        };
        let mut signatures = std::collections::BTreeSet::new();
        if !txs.iter().all(|t| signatures.insert(&t.signature)) {
            return Err(refuse(
                INVALID_PARAMS,
                "a bundle holds each transaction once".into(),
            ));
        }
        // A blockhash the session hasn't seen is checked with the chain.
        let unknown: Vec<Hash> = {
            let state = self.state();
            txs.iter()
                .map(|t| t.blockhash)
                .filter(|h| !state.blockhashes.contains_key(h))
                .collect()
        };
        for hash in unknown {
            let valid = self
                .rpc(
                    "isBlockhashValid",
                    json!([hash.to_string(), { "commitment": "processed" }]),
                )
                .await
                .ok()
                .and_then(|v| v["value"].as_bool())
                .unwrap_or(false);
            if !valid {
                return Err(refuse(
                    PREFLIGHT_FAILED,
                    "Transaction simulation failed: Blockhash not found".into(),
                ));
            }
            let mut state = self.state();
            let height = state.height;
            state.blockhashes.insert(hash, height + BLOCKHASH_BLOCKS);
        }
        let mut state = self.state();
        state.submit(submission, txs)
    }

    /// Decide slot `slot`'s block, update every entry's fate up to it, and
    /// return what to commit, in order.
    pub fn plan(&self, slot: u64) -> Vec<(u64, bool, Vec<Vec<u8>>)> {
        let mut state = self.state();
        if state.closed {
            return Vec::new();
        }
        state.advance(slot);
        // A send holding a transaction that landed in another lets it go: a
        // signature runs, and is charged, once.
        let landed = state.pool.landed();
        let mut superseded = Vec::new();
        for entry in &mut state.pool.entries {
            if entry.is_pending() && entry.holds_any(&landed) {
                let reason = "a transaction in it already landed".to_string();
                superseded.push(Refused {
                    sender: entry.sender.clone(),
                    reason: reason.clone(),
                });
                entry.state = EntryState::Failed { slot, reason };
            }
        }
        let tips = state.ghost_tips(slot);
        let tip_of = |account: &Pubkey| tips.get(account).copied().unwrap_or(jito::MIN_TIP);
        let block = {
            let s = &*state;
            let eligible: Vec<&Entry> = s
                .pool
                .entries
                .iter()
                .filter(|e| {
                    e.state == EntryState::Pending
                        && e.delivered
                        && !e.dropped
                        && e.ready_slot <= slot
                })
                .collect();
            let leader = s.leaders.leader(slot);
            builder::build(
                &builder::Slot {
                    slot,
                    seed: s.layer.seed,
                    skipped: s.leaders.skipped(slot),
                    jito_leader: leader.jito,
                    ghost: &s.ghost,
                    cu_cap: s.calibration.block.cu_cap,
                    account_cu_cap: s.calibration.block.account_cu_cap,
                    inversion_p: s.calibration.block.inversion_p,
                    ghost_tip: &tip_of,
                },
                &eligible,
            )
        };
        let skipped = state.leaders.skipped(slot);
        let jito_leader = state.leaders.leader(slot).jito;
        let mut record = BlockRecord {
            at: slot.saturating_sub(state.base_slot),
            regime: state.ghost.regime(slot).to_string(),
            price: state.ghost.block(slot).price,
            skipped,
            jito: jito_leader,
            landed: Vec::new(),
            refused: superseded,
        };
        for (id, reason) in &block.invalid {
            if let Some(entry) = state.pool.entries.iter_mut().find(|e| e.id == *id) {
                entry.state = EntryState::Failed {
                    slot,
                    reason: reason.clone(),
                };
                record.refused.push(Refused {
                    sender: entry.sender.clone(),
                    reason: reason.clone(),
                });
            }
        }
        state.blocks.push(record);
        block
            .picks
            .iter()
            .map(|id| {
                let entry = state
                    .pool
                    .entries
                    .iter()
                    .find(|e| e.id == *id)
                    .expect("picked");
                (
                    *id,
                    entry.is_bundle(),
                    entry.txs.iter().map(|t| t.wire.clone()).collect(),
                )
            })
            .collect()
    }

    /// Commit slot `slot`'s picks to the chain, in order, each awaited:
    /// bundles through `sendBundle`, transactions through
    /// `sendTransaction`. Returns the bot's transactions that landed.
    pub async fn commit(&self, slot: u64, picks: Vec<(u64, bool, Vec<Vec<u8>>)>) -> Vec<String> {
        let mut landed_bot = Vec::new();
        let mut bundles = 0u32;
        for (id, is_bundle, wires) in picks {
            // A transaction that already ran isn't sent again: it would be
            // charged, and counted as landed, twice.
            {
                let mut state = self.state();
                let landed = state.pool.landed();
                let entry = state
                    .pool
                    .entries
                    .iter_mut()
                    .find(|e| e.id == id)
                    .expect("picked");
                if entry.holds_any(&landed) {
                    if matches!(entry.state, EntryState::Landed { .. }) {
                        continue;
                    }
                    let reason = "a transaction in it already landed".to_string();
                    let sender = entry.sender.clone();
                    entry.state = EntryState::Failed {
                        slot,
                        reason: reason.clone(),
                    };
                    if let Some(record) = state.blocks.last_mut() {
                        record.refused.push(Refused { sender, reason });
                    }
                    continue;
                }
            }
            let encoded: Vec<String> = wires
                .iter()
                .map(|w| base64::engine::general_purpose::STANDARD.encode(w))
                .collect();
            let sent = if is_bundle {
                self.rpc("sendBundle", json!([encoded, { "encoding": "base64" }]))
                    .await
                    .map(|_| ())
            } else {
                self.rpc(
                    "sendTransaction",
                    json!([encoded[0], { "encoding": "base64", "skipPreflight": true }]),
                )
                .await
                .map(|_| ())
            };
            let signatures: Vec<String> = {
                let state = self.state();
                let entry = state
                    .pool
                    .entries
                    .iter()
                    .find(|e| e.id == id)
                    .expect("picked");
                entry.txs.iter().map(|t| t.signature.clone()).collect()
            };
            if let Err(reason) = sent {
                let mut state = self.state();
                let entry = state
                    .pool
                    .entries
                    .iter_mut()
                    .find(|e| e.id == id)
                    .expect("picked");
                let sender = entry.sender.clone();
                if is_bundle || reason.contains("lockhash") {
                    entry.state = if reason.contains("lockhash") {
                        EntryState::Expired { height: 0 }
                    } else {
                        EntryState::Failed {
                            slot,
                            reason: reason.clone(),
                        }
                    };
                }
                if let Some(record) = state.blocks.last_mut() {
                    record.refused.push(Refused { sender, reason });
                }
                continue;
            }
            // What the chain says of each: its fee and whether it failed.
            let mut metas = Vec::new();
            for signature in &signatures {
                let found = self
                    .rpc(
                        "getTransaction",
                        json!([signature, { "encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }]),
                    )
                    .await
                    .unwrap_or(Value::Null);
                metas.push(found);
            }
            // Accepted, and not run: the node let it in without preflight
            // and dropped it (an invalid compute budget, a fee payer that
            // can't pay). Nothing landed and nothing is paid; the same bytes
            // may go again.
            if metas.iter().all(|m| !m.is_object()) {
                let mut state = self.state();
                let entry = state
                    .pool
                    .entries
                    .iter_mut()
                    .find(|e| e.id == id)
                    .expect("picked");
                let reason = "the chain didn't process it".to_string();
                let sender = entry.sender.clone();
                entry.state = EntryState::Failed {
                    slot,
                    reason: reason.clone(),
                };
                if let Some(record) = state.blocks.last_mut() {
                    record.refused.push(Refused { sender, reason });
                }
                continue;
            }
            let bundle_index = is_bundle.then(|| {
                bundles += 1;
                bundles - 1
            });
            let mut state = self.state();
            let state = &mut *state;
            let bot = state.signer.pubkey();
            let index = state.blocks.last().map_or(0, |b| b.landed.len()) as u32;
            let entry = state
                .pool
                .entries
                .iter_mut()
                .find(|e| e.id == id)
                .expect("picked");
            let mut all_ok = true;
            for (tx, meta) in entry.txs.iter().zip(&metas) {
                if !meta.is_object() {
                    continue;
                }
                let ok = meta["meta"]["err"].is_null();
                all_ok &= ok;
                let fee = meta["meta"]["fee"].as_u64().unwrap_or_else(|| tx.fee());
                let tip = if ok { tx.tip } else { 0 };
                if !ok {
                    state
                        .errors
                        .insert(tx.signature.clone(), meta["meta"]["err"].to_string());
                }
                if entry.sender == BOT {
                    if tx.payer == bot {
                        state.ledger.landed(ok, fee, BASE_FEE * tx.signatures, tip);
                    } else {
                        state.ledger.paid_by_others += fee + tip;
                    }
                    landed_bot.push(tx.signature.clone());
                }
                if let Some(record) = state.blocks.last_mut() {
                    record.landed.push(Landed {
                        sender: entry.sender.clone(),
                        fee,
                        tip,
                        cu: tx.cu_limit,
                        ok,
                        bundle: bundle_index,
                    });
                }
            }
            entry.state = EntryState::Landed {
                slot,
                index,
                ok: all_ok,
            };
        }
        landed_bot
    }

    /// The slot was produced: read the chain's height and blockhash there.
    pub async fn produced(&self, slot: u64) -> crate::Result<()> {
        self.observe(slot).await
    }

    /// What became of the send with `signature` (or a bundle's id).
    pub fn fate(&self, signature: &str) -> Option<Fate> {
        let state = self.state();
        let entry = state
            .pool
            .find(signature)
            .or_else(|| state.pool.bundle(signature).map(|e| state.pool.resolve(e)))?;
        Some(match &entry.state {
            EntryState::Pending | EntryState::Lost | EntryState::Merged { .. } => Fate::Pending,
            EntryState::Landed { slot, .. } => {
                let tx = entry
                    .txs
                    .iter()
                    .find(|t| t.signature == signature)
                    .unwrap_or(&entry.txs[entry.txs.len() - 1]);
                let error = state.errors.get(&tx.signature).cloned();
                Fate::Landed {
                    slot: *slot,
                    ok: error.is_none(),
                    fee: tx.fee(),
                    // A bundle's tip shows on its first transaction.
                    tip: if error.is_none() && tx.signature == entry.txs[0].signature {
                        entry.tip()
                    } else {
                        0
                    },
                    error,
                    signature: tx.signature.clone(),
                }
            }
            EntryState::Failed { reason, .. } => Fate::Failed(reason.clone()),
            EntryState::Expired { .. } => Fate::Expired,
        })
    }

    /// `getRecentPrioritizationFees` for `filter`, as a cluster answers it.
    pub fn recent_fees(&self, filter: &[Pubkey]) -> Value {
        let state = self.state();
        Value::Array(
            state
                .ghost
                .recent(state.slot, filter)
                .into_iter()
                .map(|(slot, fee)| json!({ "slot": slot, "prioritizationFee": fee }))
                .collect(),
        )
    }

    /// A component's fee view: recent prices on the hot accounts, the tip
    /// floor and the next leaders.
    pub fn fee_view(&self) -> mtx_kit::FeeView {
        let state = self.state();
        let hot: Vec<Pubkey> = state
            .layer
            .hot_accounts
            .iter()
            .filter_map(|h| h.address.parse().ok())
            .collect();
        mtx_kit::FeeView {
            cu_prices: state.ghost.percentiles(state.slot, &hot),
            tip_floor: Some(state.calibration.tips.p50),
            leaders: (1..=8)
                .map(|i| state.leaders.leader(state.slot + i).identity.to_string())
                .collect(),
        }
    }

    /// The tip floor, as Jito's REST endpoint serves it.
    pub fn tip_floor(&self) -> Value {
        jito::tip_floor(&self.state().calibration.tips)
    }

    /// `getSlotLeader`, `getSlotLeaders`, `getLeaderSchedule` and
    /// `getBlockProduction`: the synthetic validators' answers, or `None`
    /// for another method.
    pub fn leader_answer(&self, method: &str, params: &Value) -> Option<Value> {
        let state = self.state();
        let now = state.slot;
        let window = state.base_slot..state.ghost_end();
        Some(match method {
            "getSlotLeader" => json!(state.leaders.leader(now).identity.to_string()),
            "getSlotLeaders" => {
                let start = params[0].as_u64().unwrap_or(now);
                let limit = params[1].as_u64().unwrap_or(1).min(5_000);
                json!(
                    (start..start + limit)
                        .map(|s| state.leaders.leader(s).identity.to_string())
                        .collect::<Vec<_>>()
                )
            }
            "getLeaderSchedule" => state
                .leaders
                .schedule(window.start, window.start, window.end),
            "getBlockProduction" => {
                let mut by_identity: BTreeMap<String, (u64, u64)> = BTreeMap::new();
                for slot in window.start..=now {
                    let counts = by_identity
                        .entry(state.leaders.leader(slot).identity.to_string())
                        .or_default();
                    counts.0 += 1;
                    if !state.leaders.skipped(slot) {
                        counts.1 += 1;
                    }
                }
                json!({
                    "context": { "slot": now },
                    "value": {
                        "byIdentity": by_identity
                            .into_iter()
                            .map(|(k, (leader, produced))| (k, json!([leader, produced])))
                            .collect::<serde_json::Map<_, _>>(),
                        "range": { "firstSlot": window.start, "lastSlot": now },
                    }
                })
            }
            _ => return None,
        })
    }

    /// `getBundleStatuses`: each bundle that landed, or null.
    pub fn bundle_statuses(&self, ids: &[String]) -> Value {
        let state = self.state();
        let value: Vec<Value> = ids
            .iter()
            .map(|id| match state.pool.bundle(id).map(|e| state.pool.resolve(e)) {
                Some(entry) => match entry.state {
                    EntryState::Landed { slot, .. } => json!({
                        "bundle_id": id,
                        "transactions": entry.txs.iter().map(|t| t.signature.clone()).collect::<Vec<_>>(),
                        "slot": slot,
                        "confirmation_status": "finalized",
                        "err": { "Ok": null },
                    }),
                    _ => Value::Null,
                },
                None => Value::Null,
            })
            .collect();
        json!({ "context": { "slot": state.slot }, "value": value })
    }

    /// `getInflightBundleStatuses`: `Pending`, `Landed`, `Failed` or
    /// `Invalid` for each.
    pub fn inflight_statuses(&self, ids: &[String]) -> Value {
        let state = self.state();
        let value: Vec<Value> = ids
            .iter()
            .map(
                |id| match state.pool.bundle(id).map(|e| state.pool.resolve(e)) {
                    Some(entry) => {
                        let (status, slot) = match &entry.state {
                            EntryState::Landed { slot, .. } => ("Landed", Some(*slot)),
                            EntryState::Pending | EntryState::Lost | EntryState::Merged { .. } => {
                                ("Pending", None)
                            }
                            EntryState::Failed { .. } | EntryState::Expired { .. } => {
                                ("Failed", None)
                            }
                        };
                        json!({ "bundle_id": id, "status": status, "landed_slot": slot })
                    }
                    None => json!({ "bundle_id": id, "status": "Invalid", "landed_slot": null }),
                },
            )
            .collect();
        json!({ "context": { "slot": state.slot }, "value": value })
    }

    /// Whether the scenario offers a staked route.
    pub fn offers_staked(&self) -> bool {
        self.state().layer.routes.staked.is_some()
    }

    /// The risk desk took the account over (see [`crate::risk`]): every
    /// send of the bot's still pooled fails unpaid, and the layer takes and
    /// lands nothing more, so none of them can reopen the position during
    /// the close. Returns how many it dropped.
    pub fn stop_out(&self) -> usize {
        let mut state = self.state();
        state.closed = true;
        let slot = state.slot;
        let mut dropped = 0;
        for entry in &mut state.pool.entries {
            if entry.sender == BOT && entry.is_pending() {
                entry.state = EntryState::Failed {
                    slot,
                    reason: STOPPED_OUT.into(),
                };
                dropped += 1;
            }
        }
        dropped
    }

    /// Lamports the risk desk's close took from the bot's wallet in fees:
    /// it lands past the layer, so the ledger books it here.
    pub fn forced_close_paid(&self, lamports: u64) {
        self.state().ledger.forced_close += lamports;
    }

    /// The SOL a route charges per send, counted in the ledger.
    pub fn charge_route(&self, lamports: u64) {
        self.state().ledger.route_fees += lamports;
    }

    /// The session's report, its ledger checked against the wallet's SOL
    /// `change` and the lamports moved into accounts it can close.
    pub fn report(&self, wallet: Option<(i64, i64)>) -> TxLayerReport {
        let state = self.state();
        let mut fees = state.ledger.clone();
        if let Some((change, closable)) = wallet {
            fees.reconcile(change, closable);
        }
        let base = state.base_slot;
        let sends = state
            .pool
            .entries
            .iter()
            .filter(|e| e.sender == BOT)
            .map(|e| {
                let (state_name, at, reason) = match &e.state {
                    EntryState::Pending => ("pending", None, None),
                    EntryState::Lost => ("lost", None, None),
                    EntryState::Landed { slot, ok, .. } => (
                        if *ok { "landed" } else { "failed" },
                        Some(slot - base),
                        None,
                    ),
                    EntryState::Failed { slot, reason } => (
                        "failed",
                        Some(slot.saturating_sub(base)),
                        Some(reason.clone()),
                    ),
                    EntryState::Expired { .. } => ("expired", None, None),
                    EntryState::Merged { into } => (
                        "merged",
                        None,
                        Some(format!(
                            "signed again, it became the transactions of pool entry {into}"
                        )),
                    ),
                };
                let landed = matches!(e.state, EntryState::Landed { .. });
                SendRecord {
                    step: e.arrival.step,
                    seq: e.arrival.seq,
                    route: e.route,
                    transactions: e.txs.len(),
                    bundle: e.is_bundle(),
                    state: state_name.to_string(),
                    at,
                    fee: if landed { e.fee() } else { 0 },
                    tip: if matches!(e.state, EntryState::Landed { ok: true, .. }) {
                        e.tip()
                    } else {
                        0
                    },
                    attempts: e.attempts,
                    resigned: e.resigned_from.len() as u64,
                    reason,
                }
            })
            .collect();
        TxLayerReport {
            calibration: state.layer.calibration.clone(),
            blocks: state
                .blocks
                .iter()
                .filter(|b| b.skipped || !b.landed.is_empty() || !b.refused.is_empty())
                .cloned()
                .collect(),
            sends,
            fees,
            pending_at_end: state.pool.entries.iter().filter(|e| e.is_pending()).count(),
        }
    }

    /// What the bot's routes charged so far, off chain, in lamports.
    pub fn route_fees(&self) -> u64 {
        self.state().ledger.route_fees
    }

    /// What the bot paid so far, in lamports, on chain and off.
    pub fn paid(&self) -> u64 {
        self.state().ledger.total()
    }
}

impl State {
    fn ghost_end(&self) -> u64 {
        self.slot + 1
    }

    fn route(&self, kind: RouteKind) -> super::Route {
        match kind {
            RouteKind::Rpc => self.layer.routes.rpc.clone(),
            RouteKind::Staked => self
                .layer
                .routes
                .staked
                .clone()
                .unwrap_or_else(super::Route::staked),
            RouteKind::Jito => self.layer.routes.jito.clone(),
        }
    }

    fn submit(&mut self, submission: Submission, txs: Vec<PooledTx>) -> Result<Accepted, Refusal> {
        if self.closed {
            return Err(Refusal {
                code: INVALID_PARAMS,
                message: STOPPED_OUT.into(),
            });
        }
        let signatures: Vec<String> = txs.iter().map(|t| t.signature.clone()).collect();
        let key = if submission.bundle {
            jito::bundle_id(&signatures)
        } else {
            signatures[0].clone()
        };
        let answer = |key: &str| {
            if submission.bundle {
                Accepted::Bundle(key.to_string())
            } else {
                Accepted::Signature(key.to_string())
            }
        };
        // The same send again: a rebroadcast. One that failed without
        // running (a bundle the leader refused, a send the chain dropped)
        // goes again, as a searcher resends once the state may let it
        // land; a transaction that ran never does. The same transaction
        // alone and as a bundle are two sends, and race.
        let route = self.route(submission.route);
        let now = self.slot;
        let seed = self.layer.seed;
        let landed = self.pool.landed();
        let pending = self.pool.pending(BOT);
        let max_pending = self.layer.max_pending;
        if let Some(entry) = self.pool.entries.iter_mut().find(|e| {
            e.is_bundle() == submission.bundle
                && e.txs.iter().map(|t| &t.signature).eq(signatures.iter())
        }) {
            if matches!(entry.state, EntryState::Failed { .. }) {
                if entry.holds_any(&landed) {
                    return Err(Refusal {
                        code: PREFLIGHT_FAILED,
                        message: "Transaction simulation failed: This transaction has already been processed".into(),
                    });
                }
                if pending >= max_pending {
                    return Err(Refusal {
                        code: TOO_MANY_REQUESTS,
                        message: format!("Too many requests: {max_pending} sends are pending"),
                    });
                }
                entry.state = EntryState::Lost;
            }
            if entry.state == EntryState::Lost || entry.dropped {
                entry.attempts += 1;
                entry.state = EntryState::Pending;
                entry.dropped = submission.dropped;
                entry.delivered = false;
                let latency = latency(
                    seed,
                    &route,
                    &entry.sender,
                    entry.sender_seq,
                    entry.attempts,
                );
                entry.ready_slot = now + 1 + latency + submission.delay_slots;
            }
            return Ok(answer(&key));
        }
        // A transaction that already landed, alone or in a bundle, is
        // refused as a node refuses one it has processed. One still pending
        // in another send may go again: whichever lands first takes it.
        if self.pool.landed().iter().any(|s| signatures.contains(s)) {
            return Err(Refusal {
                code: PREFLIGHT_FAILED,
                message:
                    "Transaction simulation failed: This transaction has already been processed"
                        .into(),
            });
        }
        if self.pool.pending(BOT) >= self.layer.max_pending {
            return Err(Refusal {
                code: TOO_MANY_REQUESTS,
                message: format!(
                    "Too many requests: {} sends are pending",
                    self.layer.max_pending
                ),
            });
        }
        let expiry_height = txs
            .iter()
            .map(|t| self.blockhashes.get(&t.blockhash).copied().unwrap_or(0))
            .min()
            .unwrap_or(0);
        let sender = BOT.to_string();
        let sender_seq = {
            let seq = self.sender_seqs.entry(sender.clone()).or_default();
            *seq += 1;
            *seq - 1
        };
        let arrival = Arrival {
            step: self.step,
            seq: self.step_seq,
        };
        self.step_seq += 1;
        let latency = latency(seed, &route, &sender, sender_seq, 1);
        if route.fee_lamports > 0 {
            self.ledger.route_fees += route.fee_lamports;
        }
        let id = self.pool.entries.len() as u64;
        self.pool.entries.push(Entry {
            id,
            sender,
            sender_seq,
            arrival,
            route: submission.route,
            bundle: submission.bundle.then(|| key.clone()),
            txs,
            ready_slot: now + 1 + latency + submission.delay_slots,
            expiry_height,
            retry_until_slot: submission.retry_until_slot,
            resign: submission.resign,
            attempts: 1,
            delivered: false,
            dropped: submission.dropped,
            state: EntryState::Pending,
            resigned_from: Vec::new(),
        });
        Ok(answer(&key))
    }

    /// Bring every entry's fate up to slot `slot`: deliveries drawn, lost
    /// ones rebroadcast where they retry, expired ones let go or signed
    /// again.
    fn advance(&mut self, slot: u64) {
        let seed = self.layer.seed;
        // The block that takes slot `slot` is one above the last.
        let height = self.height + 1;
        let latest = self.latest;
        let block_util = self.ghost.block(slot).utilization;
        let mut charges = Vec::new();
        let routes: BTreeMap<RouteKind, super::Route> =
            [RouteKind::Rpc, RouteKind::Staked, RouteKind::Jito]
                .into_iter()
                .map(|k| (k, self.route(k)))
                .collect();
        let valid_until = self
            .blockhashes
            .get(&latest)
            .copied()
            .unwrap_or(height + BLOCKHASH_BLOCKS);
        for i in 0..self.pool.entries.len() {
            let entry = &self.pool.entries[i];
            if !entry.is_pending() {
                continue;
            }
            let route = &routes[&entry.route];
            if height > entry.expiry_height {
                let retrying = entry.retry_until_slot.is_some_and(|until| slot <= until);
                if retrying && entry.resign && latest != Hash::default() {
                    // Signed again on the latest blockhash, now that the old
                    // one can no longer land.
                    if let Some(resigned) = resign(&entry.txs, &self.signer, latest) {
                        // Signing is deterministic: the bot may have sent
                        // these very transactions itself. Then they are one
                        // send, which runs once: this entry merges into it,
                        // and sending the same bytes again rebroadcasts it.
                        let id = entry.id;
                        let same = |e: &Entry| {
                            e.id != id
                                && (e.is_pending() || matches!(e.state, EntryState::Landed { .. }))
                                && e.txs
                                    .iter()
                                    .map(|t| &t.signature)
                                    .eq(resigned.iter().map(|t| &t.signature))
                        };
                        if let Some(j) = self.pool.entries.iter().position(same) {
                            let old: Vec<String> =
                                entry.txs.iter().map(|t| t.signature.clone()).collect();
                            let twin = &mut self.pool.entries[j];
                            twin.resigned_from.extend(old);
                            if twin.state == EntryState::Lost || twin.dropped {
                                twin.attempts += 1;
                                twin.state = EntryState::Pending;
                                twin.delivered = false;
                                twin.dropped = false;
                                twin.ready_slot = slot
                                    + latency(
                                        seed,
                                        &routes[&twin.route],
                                        &twin.sender,
                                        twin.sender_seq,
                                        twin.attempts,
                                    );
                            }
                            let into = twin.id;
                            let entry = &mut self.pool.entries[i];
                            entry.state = EntryState::Merged { into };
                            entry.dropped = false;
                            continue;
                        }
                        let entry = &mut self.pool.entries[i];
                        entry
                            .resigned_from
                            .extend(entry.txs.iter().map(|t| t.signature.clone()));
                        entry.txs = resigned;
                        entry.expiry_height = valid_until;
                        entry.attempts += 1;
                        entry.state = EntryState::Pending;
                        entry.delivered = false;
                        entry.dropped = false;
                        entry.ready_slot = slot
                            + latency(seed, route, &entry.sender, entry.sender_seq, entry.attempts);
                        if entry.sender == BOT && route.fee_lamports > 0 {
                            charges.push(route.fee_lamports);
                        }
                        continue;
                    }
                }
                self.pool.entries[i].state = EntryState::Expired { height };
                continue;
            }
            let entry = &mut self.pool.entries[i];
            // A lost delivery, rebroadcast each slot while it retries.
            if entry.state == EntryState::Lost
                && entry.retry_until_slot.is_some_and(|until| slot <= until)
            {
                entry.attempts += 1;
                entry.state = EntryState::Pending;
                entry.delivered = false;
                entry.dropped = false;
                entry.ready_slot =
                    slot + latency(seed, route, &entry.sender, entry.sender_seq, entry.attempts);
            }
            if entry.state != EntryState::Pending || entry.ready_slot > slot || entry.delivered {
                continue;
            }
            // The delivery reaches the leader, or is lost on the way: more
            // often in a full block when it is priced below what clears it.
            let writes: Vec<Pubkey> = entry.writable().copied().collect();
            let below = entry.bid() < self.ghost.price_to_land(slot, &writes);
            let loss = if below {
                route.loss + route.loss_congested * block_util
            } else {
                route.loss
            };
            let lost = entry.dropped
                || draw(
                    seed,
                    "loss",
                    &[
                        super::hash(0, "sender", &[], entry.sender.as_bytes()),
                        entry.sender_seq,
                        entry.attempts,
                    ],
                ) < loss;
            entry.delivered = true;
            if lost {
                entry.state = EntryState::Lost;
                entry.dropped = false;
                // Retrying, it goes again next slot.
                if entry.retry_until_slot.is_some_and(|until| slot < until) {
                    entry.attempts += 1;
                    entry.state = EntryState::Pending;
                    entry.delivered = false;
                    entry.ready_slot = slot
                        + 1
                        + latency(seed, route, &entry.sender, entry.sender_seq, entry.attempts);
                }
            }
        }
        self.ledger.route_fees += charges.iter().sum::<u64>();
    }

    /// The ghosts' tip on each contended hot account in `slot`.
    fn ghost_tips(&self, slot: u64) -> BTreeMap<Pubkey, u64> {
        let tips = &self.calibration.tips;
        self.layer
            .hot_accounts
            .iter()
            .enumerate()
            .filter_map(|(i, hot)| {
                let key: Pubkey = hot.address.parse().ok()?;
                let contended = self.ghost.account(&key, slot).price > 0;
                let tip = if contended {
                    let z = normal(self.layer.seed, "ghost-tip", &[i as u64, slot]);
                    ((tips.log_mean + tips.log_sd * z).exp() as u64).max(jito::MIN_TIP)
                } else {
                    jito::MIN_TIP
                };
                Some((key, tip))
            })
            .collect()
    }
}

/// A route's latency for a sender's delivery, in slots.
fn latency(seed: u64, route: &super::Route, sender: &str, seq: u64, attempt: u64) -> u64 {
    let (lo, hi) = route.latency_slots;
    let u = draw(
        seed,
        "latency",
        &[
            super::hash(0, "sender", &[], sender.as_bytes()),
            seq,
            attempt,
        ],
    );
    lo + ((u * (hi - lo + 1) as f64) as u64).min(hi - lo)
}

/// The same transactions signed again by `signer` on `blockhash`, if it is
/// their only signer.
fn resign(txs: &[PooledTx], signer: &Keypair, blockhash: Hash) -> Option<Vec<PooledTx>> {
    txs.iter()
        .map(|t| {
            let mut tx: VersionedTransaction = wincode::deserialize(&t.wire).ok()?;
            if tx.message.header().num_required_signatures != 1
                || tx.message.static_account_keys().first() != Some(&signer.pubkey())
            {
                return None;
            }
            tx.message.set_recent_blockhash(blockhash);
            let signed = VersionedTransaction::try_new(tx.message, &[signer]).ok()?;
            let wire = wincode::serialize(&signed).ok()?;
            let mut again = t.clone();
            again.signature = signed.signatures[0].to_string();
            again.wire = wire;
            again.blockhash = blockhash;
            Some(again)
        })
        .collect()
}
