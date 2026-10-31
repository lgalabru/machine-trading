//! The RPC endpoint bots see: a JSON-RPC proxy in front of the surfnet that
//! only forwards standard Solana methods.
//!
//! A surfnet also answers cheatcodes (`surfnet_setAccount`, time travel,
//! scenario registration), plugin management and `exit`. A bot that could
//! call those could rewrite its balance or the market, so every request
//! naming anything outside [`ALLOWED_METHODS`] is refused and recorded as a
//! violation.
//!
//! HTTP only: there is no websocket endpoint, so bots poll.
//!
//! In a lockstep session (see [`crate::session`]) the world moves only
//! between the bot's turns, so the guard also:
//!
//! - **Holds sends to the bot's turn.** A `sendTransaction` that arrives while
//!   the grader is moving the world is refused with [`TURN_CLOSED`]. It isn't
//!   a violation, and it's counted apart ([`Guard::late_sends`]): the
//!   transaction simply didn't make it into the turn.
//! - **Treats executed as final.** An offline surfnet has one leader and no
//!   forks, but its blocks only come between turns, so a client waiting for
//!   `confirmed` would wait out its own turn. Reads are served at `processed`,
//!   and a transaction's status reads as `finalized` once it has executed.
//!   (Surfpool reports no status at all for a transaction that has executed
//!   but isn't in a block yet; the guard finds it with `getTransaction`.)
//!
//! [`Guard::internal`] serves the grader's own clients the same way, with no
//! allowlist and no turns.

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use axum::{
    Router, body::Bytes, extract::State, http::StatusCode, response::IntoResponse, routing::post,
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use tracing::warn;

use crate::{Result, error::io_error};

pub const ALLOWED_METHODS: &[&str] = &[
    "getAccountInfo",
    "getBalance",
    "getBlock",
    "getBlockCommitment",
    "getBlockHeight",
    "getBlockProduction",
    "getBlockTime",
    "getBlocks",
    "getBlocksWithLimit",
    "getClusterNodes",
    "getEpochInfo",
    "getEpochSchedule",
    "getFeeForMessage",
    "getFirstAvailableBlock",
    "getGenesisHash",
    "getHealth",
    "getHighestSnapshotSlot",
    "getIdentity",
    "getInflationGovernor",
    "getInflationRate",
    "getInflationReward",
    "getLargestAccounts",
    "getLatestBlockhash",
    "getLeaderSchedule",
    "getMaxRetransmitSlot",
    "getMaxShredInsertSlot",
    "getMinimumBalanceForRentExemption",
    "getMultipleAccounts",
    "getProgramAccounts",
    "getRecentPerformanceSamples",
    "getRecentPrioritizationFees",
    "getSignatureStatuses",
    "getSignaturesForAddress",
    "getSlot",
    "getSlotLeader",
    "getSlotLeaders",
    "getStakeMinimumDelegation",
    "getSupply",
    "getTokenAccountBalance",
    "getTokenAccountsByDelegate",
    "getTokenAccountsByOwner",
    "getTokenLargestAccounts",
    "getTokenSupply",
    "getTransaction",
    "getTransactionCount",
    "getVersion",
    "getVoteAccounts",
    "isBlockhashValid",
    "minimumLedgerSlot",
    "sendTransaction",
    "simulateTransaction",
];

/// JSON-RPC "method not found"; a bot sees the same error a real cluster
/// gives for a method it doesn't serve.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_REQUEST: i64 = -32600;
const PARSE_ERROR: i64 = -32700;
/// A send outside the bot's turn: the world is moving.
pub const TURN_CLOSED: i64 = -32099;

/// Methods whose `commitment` is left alone: a real cluster refuses
/// `processed` for them, and the surfnet answers them from what it executed
/// anyway.
const KEEP_COMMITMENT: &[&str] = &[
    "getBlock",
    "getBlocks",
    "getBlocksWithLimit",
    "getSignaturesForAddress",
    "getTransaction",
];

pub struct Guard {
    url: String,
    state: Arc<GuardState>,
    server: JoinHandle<()>,
}

struct GuardState {
    upstream: String,
    client: reqwest::Client,
    /// Whether only [`ALLOWED_METHODS`] pass: off for the grader's own clients.
    allowlist: bool,
    /// Every refused method name, in order, duplicates included.
    refused: Mutex<Vec<String>>,
    /// Whether the bot may send: true outside lockstep sessions.
    turn_open: AtomicBool,
    late_sends: AtomicUsize,
}

impl Guard {
    /// Listen on a free localhost port and forward allowed requests to `upstream`.
    pub async fn start(upstream: impl Into<String>) -> Result<Self> {
        Self::listen(upstream.into(), true).await
    }

    /// A proxy for the grader's own clients: every method passes, served the
    /// way a lockstep session needs (see the module docs).
    pub async fn internal(upstream: impl Into<String>) -> Result<Self> {
        Self::listen(upstream.into(), false).await
    }

    async fn listen(upstream: String, allowlist: bool) -> Result<Self> {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = TcpListener::bind(addr)
            .await
            .map_err(io_error("guard socket"))?;
        let local = listener.local_addr().map_err(io_error("guard socket"))?;
        let state = Arc::new(GuardState {
            upstream,
            client: reqwest::Client::new(),
            allowlist,
            refused: Mutex::default(),
            turn_open: AtomicBool::new(true),
            late_sends: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/", post(handle))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                warn!(%error, "rpc guard stopped");
            }
        });
        Ok(Self {
            url: format!("http://{local}"),
            state,
            server,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn refused(&self) -> Vec<String> {
        self.state.refused.lock().expect("guard lock").clone()
    }

    /// Open or close the bot's turn: while closed, sends are refused.
    pub fn set_turn(&self, open: bool) {
        self.state.turn_open.store(open, Ordering::SeqCst);
    }

    /// Sends refused because they came outside the bot's turn.
    pub fn late_sends(&self) -> usize {
        self.state.late_sends.load(Ordering::SeqCst)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn handle(State(state): State<Arc<GuardState>>, body: Bytes) -> impl IntoResponse {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return reply(error_response(Value::Null, PARSE_ERROR, "parse error")),
    };
    // Refuse a batch as a whole if any member is refused: forwarding the rest
    // would still let a bot learn which of its calls got through.
    let calls: Vec<&Value> = match &request {
        Value::Array(calls) => calls.iter().collect(),
        call => vec![call],
    };
    let refusals: Vec<Value> = calls
        .iter()
        .filter_map(|call| refusal(&state, call).or_else(|| late(&state, call)))
        .collect();
    if !refusals.is_empty() {
        return match request {
            Value::Array(_) => reply(Value::Array(
                calls
                    .iter()
                    .map(|call| {
                        refusal_for(&state, call)
                            .or_else(|| closed_for(&state, call))
                            .unwrap_or_else(|| {
                                error_response(call["id"].clone(), INVALID_REQUEST, "batch refused")
                            })
                    })
                    .collect(),
            )),
            _ => reply(refusals.into_iter().next().expect("one refusal")),
        };
    }

    let mut request = request;
    let statuses = match &mut request {
        Value::Array(calls) => calls.iter_mut().map(at_processed).collect(),
        call => vec![at_processed(call)],
    };
    let upstream = state
        .client
        .post(&state.upstream)
        .header("content-type", "application/json")
        .body(request.to_string())
        .send()
        .await;
    match upstream {
        Ok(response) => {
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match response.bytes().await {
                Ok(bytes) if statuses.iter().any(|s| *s) => {
                    match serde_json::from_slice::<Value>(&bytes) {
                        Ok(mut reply) => {
                            match (&request, &mut reply) {
                                (Value::Array(calls), Value::Array(replies)) => {
                                    for reply in replies.iter_mut() {
                                        if let Some(call) =
                                            calls.iter().find(|c| c["id"] == reply["id"])
                                        {
                                            fill_statuses(&state, call, reply).await;
                                        }
                                    }
                                }
                                (call, reply) => fill_statuses(&state, call, reply).await,
                            }
                            executed_is_final(&mut reply);
                            (status, axum::Json(reply)).into_response()
                        }
                        Err(_) => {
                            (status, [("content-type", "application/json")], bytes).into_response()
                        }
                    }
                }
                Ok(bytes) => {
                    (status, [("content-type", "application/json")], bytes).into_response()
                }
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

/// Serve `call` at `processed`; whether it reads transaction statuses.
fn at_processed(call: &mut Value) -> bool {
    let method = call["method"].as_str().unwrap_or_default().to_string();
    if !KEEP_COMMITMENT.contains(&method.as_str()) {
        processed(&mut call["params"]);
    }
    method == "getSignatureStatuses"
}

fn processed(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if key == "commitment" && field.is_string() {
                    *field = json!("processed");
                } else {
                    processed(field);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(processed),
        _ => {}
    }
}

/// Fill the statuses Surfpool leaves out of a `getSignatureStatuses` reply:
/// transactions that executed after the last block.
async fn fill_statuses(state: &GuardState, call: &Value, reply: &mut Value) {
    if call["method"].as_str() != Some("getSignatureStatuses") {
        return;
    }
    let Some(signatures) = call["params"][0].as_array() else {
        return;
    };
    let Some(Value::Array(statuses)) = reply.pointer_mut("/result/value") else {
        return;
    };
    for (signature, status) in signatures.iter().zip(statuses.iter_mut()) {
        if !status.is_null() {
            continue;
        }
        let lookup = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "getTransaction",
            "params": [signature, { "commitment": "confirmed", "encoding": "json", "maxSupportedTransactionVersion": 0 }],
        });
        let Ok(response) = state
            .client
            .post(&state.upstream)
            .header("content-type", "application/json")
            .body(lookup.to_string())
            .send()
            .await
        else {
            continue;
        };
        let Ok(found) = response.json::<Value>().await else {
            continue;
        };
        let transaction = &found["result"];
        if transaction.is_object() {
            *status = json!({
                "slot": transaction["slot"],
                "confirmations": null,
                "err": transaction["meta"]["err"],
                "status": transaction["meta"]["status"],
                "confirmationStatus": "finalized",
            });
        }
    }
}

/// Every executed transaction's status, in a reply or a batch of them, reads
/// `finalized`.
fn executed_is_final(reply: &mut Value) {
    if let Value::Array(replies) = reply {
        replies.iter_mut().for_each(executed_is_final);
        return;
    }
    if let Some(Value::Array(statuses)) = reply.pointer_mut("/result/value") {
        for status in statuses.iter_mut().filter(|s| s.is_object()) {
            status["confirmationStatus"] = json!("finalized");
            status["confirmations"] = Value::Null;
        }
    }
}

/// Count and build the refusal for a send outside the bot's turn.
fn late(state: &GuardState, call: &Value) -> Option<Value> {
    let response = closed_for(state, call)?;
    state.late_sends.fetch_add(1, Ordering::SeqCst);
    Some(response)
}

fn closed_for(state: &GuardState, call: &Value) -> Option<Value> {
    (call["method"].as_str() == Some("sendTransaction") && !state.turn_open.load(Ordering::SeqCst))
        .then(|| {
            error_response(
                call["id"].clone(),
                TURN_CLOSED,
                "sent outside the bot's turn: the world is moving",
            )
        })
}

/// Record and build the refusal for `call`, if it names a method bots may not use.
fn refusal(state: &GuardState, call: &Value) -> Option<Value> {
    let response = refusal_for(state, call)?;
    let method = call["method"].as_str().unwrap_or("<missing>").to_string();
    warn!(method, "rpc guard refused a call");
    state.refused.lock().expect("guard lock").push(method);
    Some(response)
}

fn refusal_for(state: &GuardState, call: &Value) -> Option<Value> {
    let method = call["method"].as_str();
    match method {
        Some(_) if !state.allowlist => None,
        Some(method) if ALLOWED_METHODS.contains(&method) => None,
        Some(method) => Some(error_response(
            call["id"].clone(),
            METHOD_NOT_FOUND,
            &format!("Method not found: {method}"),
        )),
        None => Some(error_response(
            call["id"].clone(),
            INVALID_REQUEST,
            "missing method",
        )),
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn reply(body: Value) -> axum::response::Response {
    (StatusCode::OK, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(allowlist: bool) -> GuardState {
        GuardState {
            upstream: String::new(),
            client: reqwest::Client::new(),
            allowlist,
            refused: Mutex::default(),
            turn_open: AtomicBool::new(true),
            late_sends: AtomicUsize::new(0),
        }
    }

    #[test]
    fn standard_methods_pass_and_cheats_are_refused() {
        let bot = state(true);
        let call = |method: &str| json!({ "jsonrpc": "2.0", "id": 1, "method": method });
        assert!(refusal_for(&bot, &call("getSlot")).is_none());
        assert!(refusal_for(&bot, &call("sendTransaction")).is_none());
        for cheat in [
            "surfnet_setAccount",
            "surfnet_timeTravel",
            "requestAirdrop",
            "exit",
            "loadPlugin",
        ] {
            let refused = refusal_for(&bot, &call(cheat)).expect(cheat);
            assert_eq!(refused["error"]["code"], METHOD_NOT_FOUND);
            assert_eq!(refused["id"], 1);
            assert!(
                refusal_for(&state(false), &call(cheat)).is_none(),
                "{cheat}: grader"
            );
        }
        assert!(refusal_for(&bot, &json!({ "id": 2 })).is_some());
    }

    #[test]
    fn sends_wait_for_the_turn_and_are_no_violation() {
        let bot = state(true);
        let send = json!({ "jsonrpc": "2.0", "id": 7, "method": "sendTransaction" });
        let read = json!({ "jsonrpc": "2.0", "id": 8, "method": "getSlot" });
        assert!(late(&bot, &send).is_none());
        bot.turn_open.store(false, Ordering::SeqCst);
        assert_eq!(
            late(&bot, &send).expect("closed")["error"]["code"],
            TURN_CLOSED
        );
        assert!(late(&bot, &read).is_none(), "reads pass between turns");
        assert_eq!(bot.late_sends.load(Ordering::SeqCst), 1);
        assert!(bot.refused.lock().unwrap().is_empty());
    }

    #[test]
    fn reads_are_processed_and_executed_is_final() {
        let mut call = json!({
            "method": "getAccountInfo",
            "params": ["x", { "commitment": "confirmed", "encoding": "base64" }],
        });
        assert!(!at_processed(&mut call));
        assert_eq!(call["params"][1]["commitment"], "processed");
        let mut kept =
            json!({ "method": "getTransaction", "params": ["s", { "commitment": "confirmed" }] });
        at_processed(&mut kept);
        assert_eq!(kept["params"][1]["commitment"], "confirmed");

        let mut statuses = json!({ "method": "getSignatureStatuses", "params": [["a", "b"]] });
        assert!(at_processed(&mut statuses));
        let mut reply = json!({ "result": { "value": [
            { "slot": 5, "confirmations": 0, "confirmationStatus": "processed", "err": null },
            null,
        ] } });
        executed_is_final(&mut reply);
        assert_eq!(
            reply["result"]["value"][0]["confirmationStatus"],
            "finalized"
        );
        assert!(reply["result"]["value"][0]["confirmations"].is_null());
        assert!(
            reply["result"]["value"][1].is_null(),
            "unknown stays unknown"
        );
    }
}
