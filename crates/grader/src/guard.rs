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
//! A scenario with [`Faults`](crate::faults::Faults) adds execution faults to
//! the bot's guard (see [`Guard::set_faults`]): sends the guard holds, drops or
//! refuses, statuses it hides, reads it fails with a 429.
//!
//! [`Guard::internal`] serves the grader's own clients the same way, with no
//! allowlist and no turns.
//!
//! A session may also give the guard a [`SendHook`]: it runs before and
//! after each of the bot's transactions is forwarded, for actors who act on
//! the bot's orders as they arrive (see [`crate::actors`]). While a hooked
//! send runs, the bot's other calls wait for it, and a batch's other calls
//! are answered first: nothing the bot reads sees the market between the two
//! halves. Sessions without a hook forward as before.
//!
//! The bot's guard also keeps a [`CallLog`]: every method it was asked for,
//! and each send and refusal with the turn it came in, for the session's
//! review.
//!
//! The grader trades a component's sheet for it through the same guard, at
//! [`Guard::host_url`]: the same rules and the same faults on its sends, but
//! its reads of the market on the component's behalf don't count as the
//! bot's, so a fault numbered by the bot's reads fires on the bot's own.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    body::Bytes,
    extract::{RawQuery, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use tracing::warn;

use crate::{
    Result,
    error::io_error,
    faults::{FaultPlan, FaultState, Faults, FaultsReport, SendAction},
};

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

/// Sends a [`CallLog`] keeps one by one; past this it only counts them, so a
/// bot that floods the guard can't grow the grader's memory without bound.
const MAX_LOGGED_SENDS: usize = 50_000;
/// [`GuardState::step`] before the first turn.
const NO_STEP: u64 = u64::MAX;

/// What a bot asked the guard for in a session.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CallLog {
    /// Calls by method, refused ones included.
    pub methods: BTreeMap<String, u64>,
    /// Every `sendTransaction` forwarded, in order (up to [`MAX_LOGGED_SENDS`]).
    pub sends: Vec<SendRecord>,
    /// Sends past [`MAX_LOGGED_SENDS`], counted but not kept.
    pub sends_dropped: u64,
    /// Refused calls, in order: methods bots may not use.
    pub refused: Vec<CallRecord>,
    /// Sends refused because they came while the world moved.
    pub late: Vec<CallRecord>,
}

/// A `sendTransaction` the guard forwarded, and what the surfnet answered.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SendRecord {
    /// The turn it came in, if it came in one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// The surfnet's error, if it refused the transaction (a failed
    /// preflight, say).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A call the guard refused.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CallRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<u64>,
    pub method: String,
}

/// Something that acts around each of the bot's transactions: `before`
/// runs once the guard has accepted a send and before it is forwarded, with
/// the `sendTransaction` calls it carries, `after` once the surfnet has
/// answered it.
pub trait SendHook: Send + Sync {
    fn before<'a>(&'a self, sends: &'a [Value]) -> futures::future::BoxFuture<'a, ()>;
    fn after(&self) -> futures::future::BoxFuture<'_, ()>;
}

pub struct Guard {
    url: String,
    /// The path the grader's host side calls at: a secret, so a process bot
    /// on the same machine can't call it.
    host_path: String,
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
    /// The bot's current turn, or [`NO_STEP`].
    step: AtomicU64,
    log: Mutex<CallLog>,
    /// Set by [`Guard::set_send_hook`]; with it, `calls` orders the bot's
    /// calls around hooked sends.
    hook: Mutex<Option<Arc<dyn SendHook>>>,
    calls: tokio::sync::RwLock<()>,
    /// The scenario's execution faults, if it has any.
    faults: Mutex<Option<FaultState>>,
    /// The session's router, if it has one: its API is served beside the
    /// RPC methods.
    router: Mutex<Option<Arc<crate::router::Router>>>,
    /// The step the bot's router calls are counted in, and how many.
    router_calls: Mutex<(u64, u32)>,
}

impl GuardState {
    fn step(&self) -> Option<u64> {
        Some(self.step.load(Ordering::SeqCst)).filter(|k| *k != NO_STEP)
    }

    fn log(&self) -> std::sync::MutexGuard<'_, CallLog> {
        self.log.lock().expect("guard lock")
    }

    fn faults(&self) -> std::sync::MutexGuard<'_, Option<FaultState>> {
        self.faults.lock().expect("guard lock")
    }

    fn router(&self) -> Option<Arc<crate::router::Router>> {
        self.router.lock().expect("guard lock").clone()
    }
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
            step: AtomicU64::new(NO_STEP),
            log: Mutex::default(),
            hook: Mutex::default(),
            calls: tokio::sync::RwLock::new(()),
            faults: Mutex::default(),
            router: Mutex::default(),
            router_calls: Mutex::default(),
        });
        let host_path = format!("/host/{}", uuid::Uuid::new_v4().simple());
        let app = Router::new()
            .route("/", post(handle))
            .route(&host_path, post(handle_host))
            .route("/quote", get(handle_quote))
            .route("/swap-instructions", post(handle_swap_instructions))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                warn!(%error, "rpc guard stopped");
            }
        });
        Ok(Self {
            url: format!("http://{local}"),
            host_path,
            state,
            server,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Where the grader calls on the bot's behalf: the bot's guard, whose
    /// faults count the bot's own reads, not these (see the module docs).
    pub fn host_url(&self) -> String {
        format!("{}{}", self.url, self.host_path)
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

    /// The turn calls count toward from now on, in the [`CallLog`].
    pub fn set_step(&self, k: u64) {
        self.state.step.store(k, Ordering::SeqCst);
    }

    /// Run `hook` around each of the bot's transactions from now on.
    pub fn set_send_hook(&self, hook: Arc<dyn SendHook>) {
        *self.state.hook.lock().expect("guard lock") = Some(hook);
    }

    /// What the bot asked for so far.
    pub fn call_log(&self) -> CallLog {
        self.state.log().clone()
    }

    /// Put the bot's calls under the scenario's `faults` from now on (a
    /// session calls this once, with `None` for a scenario without them).
    /// `base_slot` is the slot the session's steps count from,
    /// `slots_per_step` their length.
    pub fn set_faults(&self, faults: Option<&Faults>, base_slot: u64, slots_per_step: u64) {
        *self.state.faults() =
            faults.map(|faults| FaultState::new(FaultPlan::new(faults), base_slot, slots_per_step));
    }

    /// Forward the sends held for the block of `step`, in the order they were
    /// sent. The session calls it once the step's blocks have landed.
    pub async fn release(&self, step: u64) {
        let due = match &mut *self.state.faults() {
            Some(faults) => faults.take_due(step),
            None => return,
        };
        for held in due {
            let reply = forward_call(&self.state, held.call, false).await;
            land(&self.state, &reply, step);
        }
    }

    /// Serve `router`'s API to the bot from now on: Jupiter's `GET /quote`
    /// and `POST /swap-instructions` at its URL, and the JSON-RPC methods
    /// [`ROUTER_METHODS`]. Without a router, neither exists.
    pub fn set_router(&self, router: Arc<crate::router::Router>) {
        *self.state.router.lock().expect("guard lock") = Some(router);
    }

    /// What the faults did, if the bot ran under any.
    pub fn faults_report(&self) -> Option<FaultsReport> {
        self.state.faults().as_ref().map(FaultState::report)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn handle(State(state): State<Arc<GuardState>>, body: Bytes) -> impl IntoResponse {
    handle_as(&state, body, false).await
}

/// A request the grader makes for the bot: its reads don't count toward the
/// bot's faults.
async fn handle_host(State(state): State<Arc<GuardState>>, body: Bytes) -> impl IntoResponse {
    handle_as(&state, body, true).await
}

async fn handle_as(state: &GuardState, body: Bytes, host: bool) -> axum::response::Response {
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
    if state.allowlist {
        let mut log = state.log();
        for call in &calls {
            let method = call["method"].as_str().unwrap_or("<missing>");
            *log.methods.entry(method.to_string()).or_default() += 1;
        }
    }
    let is_router = |call: &Value| {
        call["method"]
            .as_str()
            .is_some_and(|m| ROUTER_METHODS.contains(&m))
    };
    // Router calls are no refusal, so a bot that probes for the router, or
    // shares one client across tasks with and without it, is never disqualified.
    if let Value::Object(_) = &request
        && is_router(&request)
    {
        return reply(router_answer(state, &request).await);
    }
    let rest: Vec<&Value> = calls.iter().copied().filter(|c| !is_router(c)).collect();
    let refusals: Vec<Value> = rest
        .iter()
        .filter_map(|call| refusal(state, call).or_else(|| late(state, call)))
        .collect();
    if !refusals.is_empty() {
        return match request {
            Value::Array(_) => reply(Value::Array(
                calls
                    .iter()
                    .map(|call| {
                        refusal_for(state, call)
                            .or_else(|| closed_for(state, call))
                            .unwrap_or_else(|| {
                                error_response(call["id"].clone(), INVALID_REQUEST, "batch refused")
                            })
                    })
                    .collect(),
            )),
            _ => reply(refusals.into_iter().next().expect("one refusal")),
        };
    }
    if let Value::Array(_) = &request
        && rest.len() < calls.len()
    {
        return mixed_batch(state, &calls, host).await;
    }

    let hook = state.hook.lock().expect("guard lock").clone();
    let Some(hook) = hook else {
        return serve(state, request, host).await;
    };
    if !calls.iter().any(|c| is_send(c)) {
        let _reading = state.calls.read().await;
        return serve(state, request, host).await;
    }
    // A hooked send: nothing else the bot asks runs until both halves have.
    let _sending = state.calls.write().await;
    match request {
        Value::Array(calls) => {
            let (sends, others): (Vec<Value>, Vec<Value>) = calls.into_iter().partition(is_send);
            let mut replies = Vec::new();
            if !others.is_empty() {
                replies.extend(batch_replies(serve(state, Value::Array(others), host).await).await);
            }
            hook.before(&sends).await;
            let sent = batch_replies(serve(state, Value::Array(sends), host).await).await;
            hook.after().await;
            replies.extend(sent);
            reply(Value::Array(replies))
        }
        call => {
            hook.before(std::slice::from_ref(&call)).await;
            let response = serve(state, call, host).await;
            hook.after().await;
            response
        }
    }
}

/// The members of a forwarded batch's reply.
async fn batch_replies(response: axum::response::Response) -> Vec<Value> {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    match serde_json::from_slice(&bytes) {
        Ok(Value::Array(replies)) => replies,
        Ok(reply) => vec![reply],
        Err(_) => Vec::new(),
    }
}

/// Serve an accepted request: under the scenario's faults if it has any,
/// otherwise forward it at once. `host`: the grader's, on the bot's behalf.
async fn serve(state: &GuardState, request: Value, host: bool) -> axum::response::Response {
    if state.faults().is_some() {
        return faulted(state, request, host).await;
    }
    forward(state, request, true).await
}

/// Send `request` to the surfnet and give its reply the way the guard serves
/// it: at `processed`, executed as final. `log_sends` keeps the bot's sends in
/// the [`CallLog`].
async fn forward(state: &GuardState, request: Value, log_sends: bool) -> axum::response::Response {
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
            let bytes = response.bytes().await;
            if let Ok(bytes) = &bytes
                && log_sends
            {
                record_sends(state, &request, bytes);
            }
            match bytes {
                Ok(bytes) if statuses.iter().any(|s| *s) => {
                    match serde_json::from_slice::<Value>(&bytes) {
                        Ok(mut reply) => {
                            match (&request, &mut reply) {
                                (Value::Array(calls), Value::Array(replies)) => {
                                    for reply in replies.iter_mut() {
                                        if let Some(call) =
                                            calls.iter().find(|c| c["id"] == reply["id"])
                                        {
                                            fill_statuses(state, call, reply).await;
                                        }
                                    }
                                }
                                (call, reply) => fill_statuses(state, call, reply).await,
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

/// HTTP 429 and the JSON-RPC code clusters answer a rate-limited caller with.
const TOO_MANY_REQUESTS: i64 = -32429;

/// Serve a request under the scenario's faults: each call on its own, since a
/// fault can hold, refuse or answer one call of a batch and not the next.
/// The grader's own reads on the bot's behalf (`host`) count toward no fault,
/// and none fails; its sends are the bot's.
async fn faulted(state: &GuardState, request: Value, host: bool) -> axum::response::Response {
    let step = state.step().unwrap_or(0);
    let batch = request.is_array();
    let calls: Vec<Value> = match request {
        Value::Array(calls) => calls,
        call => vec![call],
    };
    // Every read counts, so none is skipped once one fails.
    let unavailable: Vec<bool> = if host {
        Vec::new()
    } else {
        let mut faults = state.faults();
        let faults = faults.as_mut().expect("faults are on");
        calls
            .iter()
            .filter(|call| !is_send(call))
            .map(|call| faults.on_read(call["method"].as_str().unwrap_or_default(), step))
            .collect()
    };
    if unavailable.contains(&true) {
        let body = error_response(Value::Null, TOO_MANY_REQUESTS, "Too many requests");
        return (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
    }
    let mut replies = Vec::with_capacity(calls.len());
    for call in calls {
        replies.push(if is_send(&call) {
            send_under_faults(state, call, step).await
        } else {
            read_under_faults(state, call, step).await
        });
    }
    reply(if batch {
        Value::Array(replies)
    } else {
        replies.remove(0)
    })
}

fn is_send(call: &Value) -> bool {
    call["method"].as_str() == Some("sendTransaction")
}

/// The reply to a send the plan lets through, held or refused.
async fn send_under_faults(state: &GuardState, call: Value, step: u64) -> Value {
    let action = state
        .faults()
        .as_mut()
        .expect("faults are on")
        .on_send(&call, step);
    let id = call["id"].clone();
    let mut record = SendRecord {
        step: Some(step),
        signature: None,
        error: None,
    };
    let answer = match action {
        SendAction::Forward => {
            let reply = forward_call(state, call, true).await;
            land(state, &reply, step);
            return reply;
        }
        SendAction::Expired => {
            record.error = Some("Blockhash not found".into());
            error_response(
                id,
                BLOCKHASH_NOT_FOUND,
                "Transaction simulation failed: Blockhash not found",
            )
        }
        SendAction::Dropped(signature) | SendAction::Held(signature) => {
            record.signature = Some(signature.clone());
            json!({ "jsonrpc": "2.0", "id": id, "result": signature })
        }
    };
    push_send(&mut state.log(), record);
    answer
}

/// JSON-RPC code of a send whose blockhash the node doesn't know.
const BLOCKHASH_NOT_FOUND: i64 = -32002;

/// The reply to a read, with the faults' say in it.
async fn read_under_faults(state: &GuardState, call: Value, step: u64) -> Value {
    let id = call["id"].clone();
    let method = call["method"].as_str().unwrap_or_default().to_string();
    if method == "getRecentPrioritizationFees" {
        let fees = state
            .faults()
            .as_ref()
            .filter(|faults| faults.plan().congested())
            .map(|faults| faults.recent_fees(step));
        if let Some(fees) = fees {
            return json!({ "jsonrpc": "2.0", "id": id, "result": fees });
        }
    }
    let params = call["params"].clone();
    let mut reply = forward_call(state, call, true).await;
    let faults = state.faults();
    let faults = faults.as_ref().expect("faults are on");
    match method.as_str() {
        "getSignatureStatuses" => {
            let signatures = params[0].as_array().cloned().unwrap_or_default();
            if let Some(Value::Array(statuses)) = reply.pointer_mut("/result/value") {
                for (signature, status) in signatures.iter().zip(statuses.iter_mut()) {
                    if signature.as_str().is_some_and(|s| faults.hides(s, step)) {
                        *status = Value::Null;
                    }
                }
            }
        }
        "getTransaction" if params[0].as_str().is_some_and(|s| faults.hides(s, step)) => {
            reply["result"] = Value::Null;
        }
        _ => {}
    }
    reply
}

/// One call through [`forward`], as a JSON-RPC reply.
async fn forward_call(state: &GuardState, call: Value, log_sends: bool) -> Value {
    let id = call["id"].clone();
    let response = forward(state, call, log_sends).await;
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| error_response(id, -32603, "the surfnet didn't answer"))
}

/// A send the chain ran (it has a signature): count it, and hide its status
/// if the plan loses its confirmation.
fn land(state: &GuardState, reply: &Value, step: u64) {
    if let (Some(signature), Some(faults)) = (reply["result"].as_str(), &mut *state.faults()) {
        faults.on_landing(signature, step);
    }
}

fn push_send(log: &mut CallLog, record: SendRecord) {
    if log.sends.len() >= MAX_LOGGED_SENDS {
        log.sends_dropped += 1;
    } else {
        log.sends.push(record);
    }
}

/// Log the bot's sends in `request` with what `reply` says of them.
fn record_sends(state: &GuardState, request: &Value, reply: &[u8]) {
    let is_send = |call: &Value| call["method"].as_str() == Some("sendTransaction");
    if !state.allowlist {
        return;
    }
    let sends: Vec<&Value> = match request {
        Value::Array(calls) => calls.iter().filter(|c| is_send(c)).collect(),
        call if is_send(call) => vec![call],
        _ => return,
    };
    if sends.is_empty() {
        return;
    }
    let reply: Value = serde_json::from_slice(reply).unwrap_or(Value::Null);
    let answer = |call: &Value| -> Value {
        match &reply {
            Value::Array(replies) => replies
                .iter()
                .find(|r| r["id"] == call["id"])
                .cloned()
                .unwrap_or(Value::Null),
            reply => reply.clone(),
        }
    };
    let step = state.step();
    let mut log = state.log();
    for call in sends {
        let answer = answer(call);
        push_send(
            &mut log,
            SendRecord {
                step,
                signature: answer["result"].as_str().map(str::to_string),
                error: answer.get("error").map(|e| {
                    e["message"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| e.to_string())
                }),
            },
        );
    }
}

/// Where each read's config object goes in its params, for the reads that
/// take a commitment: a call that leaves it out is served at `processed`
/// too, not at the node's default, which trails the world.
const CONFIG_AT: &[(&str, usize)] = &[
    ("getAccountInfo", 1),
    ("getBalance", 1),
    ("getBlockHeight", 0),
    ("getEpochInfo", 0),
    ("getFeeForMessage", 1),
    ("getLatestBlockhash", 0),
    ("getMinimumBalanceForRentExemption", 1),
    ("getMultipleAccounts", 1),
    ("getProgramAccounts", 1),
    ("getSlot", 0),
    ("getTokenAccountBalance", 1),
    ("getTokenAccountsByDelegate", 2),
    ("getTokenAccountsByOwner", 2),
    ("getTokenSupply", 1),
    ("simulateTransaction", 1),
];

/// Serve `call` at `processed`; whether it reads transaction statuses.
fn at_processed(call: &mut Value) -> bool {
    let method = call["method"].as_str().unwrap_or_default().to_string();
    if !KEEP_COMMITMENT.contains(&method.as_str()) {
        processed(&mut call["params"]);
        if let Some(&(_, at)) = CONFIG_AT.iter().find(|(m, _)| *m == method) {
            add_processed(&mut call["params"], at);
        }
    }
    method == "getSignatureStatuses"
}

/// Give a call that names no commitment one: `processed`, in its config
/// object at `at` (made, if the call has none and stops just before it).
fn add_processed(params: &mut Value, at: usize) {
    if params.is_null() {
        *params = json!([]);
    }
    let Some(params) = params.as_array_mut() else {
        return;
    };
    if params.len() == at {
        params.push(json!({}));
    }
    match params.get_mut(at) {
        Some(config @ Value::Null) => *config = json!({ "commitment": "processed" }),
        Some(Value::Object(config)) => {
            config
                .entry("commitment")
                .or_insert_with(|| json!("processed"));
        }
        _ => {}
    }
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
    state.log().late.push(CallRecord {
        step: state.step(),
        method: "sendTransaction".into(),
    });
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
    state.log().refused.push(CallRecord {
        step: state.step(),
        method: method.clone(),
    });
    state.refused.lock().expect("guard lock").push(method);
    Some(response)
}

fn refusal_for(state: &GuardState, call: &Value) -> Option<Value> {
    let method = call["method"].as_str();
    match method {
        Some(_) if !state.allowlist => None,
        Some(method) if ALLOWED_METHODS.contains(&method) => None,
        Some(method) if ROUTER_METHODS.contains(&method) => None,
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

/// The router's JSON-RPC methods: Jupiter's quote and swap instructions,
/// each taking the same object as its first parameter.
pub use mtx_kit::ROUTER_METHODS;

/// One router call, counted against [`crate::router::CALLS_PER_STEP`]:
/// its answer, or a JSON-RPC error code and why.
async fn router_call(
    state: &GuardState,
    router: &crate::router::Router,
    method: &str,
    params: &Value,
) -> std::result::Result<Value, (i64, String)> {
    {
        let mut calls = state.router_calls.lock().expect("guard lock");
        let step = state.step().unwrap_or(NO_STEP);
        if calls.0 != step {
            *calls = (step, 0);
        }
        calls.1 += 1;
        if calls.1 > crate::router::CALLS_PER_STEP {
            return Err((
                TOO_MANY_REQUESTS,
                format!(
                    "more than {} router calls this step",
                    crate::router::CALLS_PER_STEP
                ),
            ));
        }
    }
    let answer = if method == ROUTER_METHODS[0] {
        router.quote_api(params).await
    } else {
        router.swap_instructions_api(params).await
    };
    answer.map_err(|e| (INVALID_REQUEST, e))
}

/// A router call's JSON-RPC answer: the router's, or a plain error, never a
/// refusal, in a session without one.
async fn router_answer(state: &GuardState, call: &Value) -> Value {
    let id = call["id"].clone();
    let Some(router) = state.router() else {
        return error_response(
            id,
            METHOD_NOT_FOUND,
            "Method not found: this session has no router",
        );
    };
    let method = call["method"].as_str().unwrap_or_default();
    let params = call["params"].get(0).cloned().unwrap_or(Value::Null);
    match router_call(state, &router, method, &params).await {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error_response(id, code, &message),
    }
}

/// A batch of router calls and others: the router's own are answered here,
/// the rest are served as a batch, and the replies come back together.
async fn mixed_batch(state: &GuardState, calls: &[&Value], host: bool) -> axum::response::Response {
    let mut replies = Vec::with_capacity(calls.len());
    let mut rest = Vec::new();
    for call in calls {
        if call["method"]
            .as_str()
            .is_some_and(|m| ROUTER_METHODS.contains(&m))
        {
            replies.push(router_answer(state, call).await);
        } else {
            rest.push((*call).clone());
        }
    }
    if rest.is_empty() {
        return reply(Value::Array(replies));
    }
    let served = serve(state, Value::Array(rest), host).await;
    let (parts, body) = served.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Array(served)) => replies.extend(served),
        // The upstream refused the batch as a whole, with one error object.
        Ok(error @ Value::Object(_)) => replies.push(error),
        // Nothing to add: notifications alone are answered with no body.
        _ => {}
    }
    (parts.status, axum::Json(Value::Array(replies))).into_response()
}

/// A router call over HTTP, as Jupiter's API answers: the JSON, or 400 with
/// `{"error": ...}`; 404 in a session without a router.
async fn router_http(state: &GuardState, method: &str, params: Value) -> axum::response::Response {
    let Some(router) = state.router() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if state.allowlist {
        *state.log().methods.entry(method.to_string()).or_default() += 1;
    }
    match router_call(state, &router, method, &params).await {
        Ok(answer) => (StatusCode::OK, axum::Json(answer)).into_response(),
        Err((code, error)) => {
            let status = if code == TOO_MANY_REQUESTS {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, axum::Json(json!({ "error": error }))).into_response()
        }
    }
}

async fn handle_quote(
    State(state): State<Arc<GuardState>>,
    RawQuery(query): RawQuery,
) -> impl IntoResponse {
    router_http(
        &state,
        ROUTER_METHODS[0],
        query_params(query.as_deref().unwrap_or("")),
    )
    .await
}

/// A URL query's parameters as a JSON object of strings, `+` and `%XX`
/// decoded; the last of a repeated name wins.
fn query_params(query: &str) -> Value {
    let decode = |text: &str| {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < bytes.len() => {
                    let hex = |b: u8| (b as char).to_digit(16);
                    match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                        (Some(high), Some(low)) => {
                            out.push((high * 16 + low) as u8);
                            i += 2;
                        }
                        _ => out.push(b'%'),
                    }
                }
                byte => out.push(byte),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    Value::Object(
        query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                (decode(name), Value::String(decode(value)))
            })
            .collect(),
    )
}

async fn handle_swap_instructions(
    State(state): State<Arc<GuardState>>,
    body: Bytes,
) -> impl IntoResponse {
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "the body is not JSON" })),
        )
            .into_response();
    };
    router_http(&state, ROUTER_METHODS[1], body).await
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
            step: AtomicU64::new(NO_STEP),
            log: Mutex::default(),
            hook: Mutex::default(),
            calls: tokio::sync::RwLock::new(()),
            faults: Mutex::default(),
            router: Mutex::default(),
            router_calls: Mutex::default(),
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
    fn the_log_keeps_sends_and_refusals_by_turn() {
        let bot = state(true);
        bot.step.store(3, Ordering::SeqCst);
        let send = |id: u64| json!({ "jsonrpc": "2.0", "id": id, "method": "sendTransaction" });
        record_sends(
            &bot,
            &send(1),
            br#"{"jsonrpc":"2.0","id":1,"result":"sig-a"}"#,
        );
        let batch = json!([send(2), { "jsonrpc": "2.0", "id": 3, "method": "getSlot" }, send(4)]);
        record_sends(
            &bot,
            &batch,
            br#"[{"id":2,"result":"sig-b"},{"id":3,"result":7},{"id":4,"error":{"code":-32002,"message":"preflight failed"}}]"#,
        );
        assert!(refusal(&bot, &json!({ "id": 5, "method": "surfnet_setAccount" })).is_some());
        let log = bot.log().clone();
        let signatures: Vec<_> = log.sends.iter().map(|s| s.signature.as_deref()).collect();
        assert_eq!(signatures, [Some("sig-a"), Some("sig-b"), None]);
        assert_eq!(log.sends[2].error.as_deref(), Some("preflight failed"));
        assert!(log.sends.iter().all(|s| s.step == Some(3)));
        assert_eq!(
            log.refused,
            [CallRecord {
                step: Some(3),
                method: "surfnet_setAccount".into()
            }]
        );
        // The grader's own guard keeps nothing.
        let grader = state(false);
        record_sends(&grader, &send(1), br#"{"id":1,"result":"sig"}"#);
        assert!(grader.log().sends.is_empty());
    }

    #[test]
    fn reads_are_processed_and_executed_is_final() {
        let mut call = json!({
            "method": "getAccountInfo",
            "params": ["x", { "commitment": "confirmed", "encoding": "base64" }],
        });
        assert!(!at_processed(&mut call));
        assert_eq!(call["params"][1]["commitment"], "processed");
        // A read that names no commitment is served at processed too.
        let mut bare = json!({ "method": "getSlot", "params": [] });
        at_processed(&mut bare);
        assert_eq!(bare["params"][0]["commitment"], "processed");
        let mut unnamed =
            json!({ "method": "getAccountInfo", "params": ["x", { "encoding": "base64" }] });
        at_processed(&mut unnamed);
        assert_eq!(unnamed["params"][1]["commitment"], "processed");
        let mut no_params = json!({ "method": "getSlot" });
        at_processed(&mut no_params);
        assert_eq!(no_params["params"][0]["commitment"], "processed");
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

    /// An upstream that notes each method it's asked for, in order.
    async fn noting_upstream(seen: Arc<Mutex<Vec<String>>>) -> String {
        async fn answer(
            State(seen): State<Arc<Mutex<Vec<String>>>>,
            body: Bytes,
        ) -> axum::response::Response {
            let request: Value = serde_json::from_slice(&body).unwrap();
            let one = |call: &Value| {
                seen.lock()
                    .unwrap()
                    .push(call["method"].as_str().unwrap().to_string());
                json!({ "jsonrpc": "2.0", "id": call["id"], "result": null })
            };
            reply(match &request {
                Value::Array(calls) => Value::Array(calls.iter().map(one).collect()),
                call => one(call),
            })
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/", post(answer)).with_state(seen);
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    struct NotingHook(Arc<Mutex<Vec<String>>>);

    impl SendHook for NotingHook {
        fn before<'a>(&'a self, sends: &'a [Value]) -> futures::future::BoxFuture<'a, ()> {
            Box::pin(async move {
                assert!(sends.iter().all(is_send));
                self.0.lock().unwrap().push("before".into());
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            })
        }
        fn after(&self) -> futures::future::BoxFuture<'_, ()> {
            Box::pin(async move { self.0.lock().unwrap().push("after".into()) })
        }
    }

    #[tokio::test]
    async fn a_hooked_send_runs_alone_between_its_halves() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let guard = Guard::start(noting_upstream(seen.clone()).await)
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let call = |id: u64, method: &str| json!({ "jsonrpc": "2.0", "id": id, "method": method });
        let post = |body: Value| client.post(guard.url()).json(&body).send();

        // Without a hook, a batch goes up as it came.
        post(json!([call(1, "getSlot"), call(2, "sendTransaction")]))
            .await
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), ["getSlot", "sendTransaction"]);
        seen.lock().unwrap().clear();

        guard.set_send_hook(Arc::new(NotingHook(seen.clone())));
        let batch = json!([
            call(1, "getSlot"),
            call(2, "sendTransaction"),
            call(3, "getBalance")
        ]);
        let replies: Value = post(batch).await.unwrap().json().await.unwrap();
        let ids: Vec<u64> = replies
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, [1, 3, 2]);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "getSlot",
                "getBalance",
                "before",
                "sendTransaction",
                "after"
            ],
            "the batch's reads are answered first"
        );
        seen.lock().unwrap().clear();

        // A read that comes in while a hooked send runs waits for it.
        let send = tokio::spawn(post(call(4, "sendTransaction")));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        post(call(5, "getSlot")).await.unwrap();
        send.await.unwrap().unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            ["before", "sendTransaction", "after", "getSlot"]
        );
    }

    /// A surfnet that answers every send with the signature in its params
    /// (as the surfnet does) and knows no statuses; it counts what reached it.
    async fn upstream() -> (String, Arc<Mutex<Vec<String>>>) {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        let app = Router::new().route(
            "/",
            post(move |body: Bytes| {
                let log = log.clone();
                async move {
                    let answer = |call: &Value| {
                        let method = call["method"].as_str().unwrap().to_string();
                        log.lock().unwrap().push(method.clone());
                        let result = match method.as_str() {
                            "sendTransaction" => json!(
                                crate::faults::WireTransaction::parse(call)
                                    .unwrap()
                                    .signature
                            ),
                            "getSignatureStatuses" => json!({ "value": [{
                                "slot": 1, "confirmations": null, "err": null,
                                "confirmationStatus": "processed" }] }),
                            _ => json!(1),
                        };
                        json!({ "jsonrpc": "2.0", "id": call["id"], "result": result })
                    };
                    axum::Json(match serde_json::from_slice::<Value>(&body).unwrap() {
                        Value::Array(calls) => Value::Array(calls.iter().map(answer).collect()),
                        call => answer(&call),
                    })
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    /// A router with no pools: every quote is an error of its own, so a call
    /// it answers is told apart from one the guard turned away.
    fn bare_router(url: &str) -> Arc<crate::router::Router> {
        Arc::new(crate::router::Router::new(
            Vec::new(),
            &[],
            Arc::new(mtx_kit::chain::RpcChainClient::new(url)),
            solana_pubkey::Pubkey::new_unique(),
            2,
            50,
        ))
    }

    fn router_call_body(id: u64) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": ROUTER_METHODS[0], "params": [{}] })
    }

    #[tokio::test]
    async fn a_batched_router_call_is_answered_and_no_refusal() {
        let (url, seen) = upstream().await;
        let guard = Guard::start(url.clone()).await.unwrap();
        guard.set_router(bare_router(&url));
        let read = json!({ "jsonrpc": "2.0", "id": 2, "method": "getSlot" });
        let (status, reply) = call(&guard, json!([router_call_body(1), read])).await;
        assert_eq!(status, StatusCode::OK);
        let replies = reply.as_array().expect("a batch reply");
        assert_eq!(replies.len(), 2);
        let router = replies.iter().find(|r| r["id"] == 1).unwrap();
        assert_ne!(router["error"]["code"], METHOD_NOT_FOUND);
        assert!(
            !router["error"]["message"]
                .as_str()
                .unwrap()
                .contains("batch")
        );
        assert_eq!(replies.iter().find(|r| r["id"] == 2).unwrap()["result"], 1);
        assert_eq!(seen.lock().unwrap().as_slice(), ["getSlot"]);
        assert!(guard.refused().is_empty());
        assert!(guard.call_log().refused.is_empty());
    }

    #[tokio::test]
    async fn router_calls_without_a_router_are_errors_and_no_refusal() {
        let (url, seen) = upstream().await;
        let guard = Guard::start(url).await.unwrap();
        let (_, single) = call(&guard, router_call_body(1)).await;
        assert_eq!(single["error"]["code"], METHOD_NOT_FOUND);
        let read = json!({ "jsonrpc": "2.0", "id": 2, "method": "getSlot" });
        let (_, batch) = call(&guard, json!([router_call_body(3), read])).await;
        let batch = batch.as_array().expect("a batch reply");
        assert_eq!(batch.len(), 2);
        assert_eq!(
            batch.iter().find(|r| r["id"] == 3).unwrap()["error"]["code"],
            METHOD_NOT_FOUND
        );
        assert_eq!(batch.iter().find(|r| r["id"] == 2).unwrap()["result"], 1);
        assert_eq!(seen.lock().unwrap().as_slice(), ["getSlot"]);
        assert!(guard.refused().is_empty());
        assert!(guard.call_log().refused.is_empty());
    }

    #[tokio::test]
    async fn a_forbidden_method_still_refuses_beside_a_router_call() {
        let (url, seen) = upstream().await;
        let guard = Guard::start(url.clone()).await.unwrap();
        guard.set_router(bare_router(&url));
        let cheat = json!({ "jsonrpc": "2.0", "id": 2, "method": "surfnet_setAccount" });
        let (_, reply) = call(&guard, json!([router_call_body(1), cheat.clone()])).await;
        let replies = reply.as_array().expect("a batch reply");
        assert_eq!(
            replies.iter().find(|r| r["id"] == 2).unwrap()["error"]["code"],
            METHOD_NOT_FOUND
        );
        let (_, single) = call(&guard, cheat).await;
        assert_eq!(single["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(
            guard.refused(),
            ["surfnet_setAccount", "surfnet_setAccount"]
        );
        assert!(seen.lock().unwrap().is_empty());
    }

    /// A surfnet that answers any request with one error object and a body of
    /// nothing at all for a request of notifications only.
    async fn rejecting_upstream() -> String {
        let app = Router::new().route(
            "/",
            post(|body: Bytes| async move {
                let calls = serde_json::from_slice::<Value>(&body).unwrap();
                let silent = calls
                    .as_array()
                    .is_some_and(|c| c.iter().all(|c| c.get("id").is_none()));
                if silent {
                    return Bytes::new().into_response();
                }
                axum::Json(error_response(
                    Value::Null,
                    INVALID_REQUEST,
                    "Invalid request",
                ))
                .into_response()
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn a_batch_of_router_calls_alone_is_answered_without_the_surfnet() {
        let (url, seen) = upstream().await;
        let guard = Guard::start(url.clone()).await.unwrap();
        guard.set_router(bare_router(&url));
        let batch = json!([router_call_body(1), router_call_body(2)]);
        let check = |reply: Value| {
            let replies = reply.as_array().expect("a batch reply").clone();
            assert_eq!(replies.len(), 2);
            for id in [1, 2] {
                let r = replies.iter().find(|r| r["id"] == id).unwrap();
                assert_ne!(r["error"]["code"], METHOD_NOT_FOUND);
            }
        };
        let (status, reply) = call(&guard, batch.clone()).await;
        assert_eq!(status, StatusCode::OK);
        check(reply);
        guard.set_faults(
            Some(&faults(|f| {
                f.rpc_unavailable = Some(crate::faults::RpcUnavailable {
                    reads: vec![1],
                    count: 5,
                })
            })),
            100,
            2,
        );
        guard.set_step(0);
        let (status, reply) = call(&guard, batch).await;
        assert_eq!(status, StatusCode::OK);
        check(reply);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_router_call_beside_a_notification_is_still_answered() {
        let url = rejecting_upstream().await;
        let guard = Guard::start(url.clone()).await.unwrap();
        guard.set_router(bare_router(&url));
        let notification = json!({ "jsonrpc": "2.0", "method": "getSlot" });
        let (status, reply) = call(&guard, json!([router_call_body(1), notification])).await;
        assert_eq!(status, StatusCode::OK);
        let replies = reply.as_array().expect("a batch reply");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["id"], 1);
    }

    #[tokio::test]
    async fn router_answers_survive_an_upstream_that_rejects_the_rest() {
        let url = rejecting_upstream().await;
        let guard = Guard::start(url.clone()).await.unwrap();
        guard.set_router(bare_router(&url));
        let read = json!({ "jsonrpc": "2.0", "id": 2, "method": "getSlot" });
        let (_, reply) = call(&guard, json!([router_call_body(1), read])).await;
        let replies = reply.as_array().expect("a batch reply");
        assert_eq!(replies.len(), 2);
        assert!(replies.iter().any(|r| r["id"] == 1));
        assert!(
            replies
                .iter()
                .any(|r| r["id"].is_null() && r["error"]["code"] == INVALID_REQUEST)
        );
    }

    #[tokio::test]
    async fn a_rate_limited_mixed_batch_still_spends_the_router_cap() {
        let (url, _) = upstream().await;
        let guard = Guard::start(url.clone()).await.unwrap();
        guard.set_router(bare_router(&url));
        guard.set_faults(
            Some(&faults(|f| {
                f.rpc_unavailable = Some(crate::faults::RpcUnavailable {
                    reads: vec![1],
                    count: 3,
                })
            })),
            100,
            2,
        );
        guard.set_step(0);
        let read = json!({ "jsonrpc": "2.0", "id": 2, "method": "getSlot" });
        let (status, reply) = call(&guard, json!([router_call_body(1), read])).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(reply.as_array().unwrap().iter().any(|r| r["id"] == 1));
        assert_eq!(guard.state.router_calls.lock().unwrap().1, 1);
        let (status, _) = call(&guard, router_call_body(3)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(guard.state.router_calls.lock().unwrap().1, 2);
    }

    async fn call(guard: &Guard, body: Value) -> (StatusCode, Value) {
        call_at(guard.url(), body).await
    }

    async fn call_at(url: &str, body: Value) -> (StatusCode, Value) {
        let response = reqwest::Client::new()
            .post(url)
            .json(&body)
            .send()
            .await
            .unwrap();
        (
            StatusCode::from_u16(response.status().as_u16()).unwrap(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    fn faults(f: impl FnOnce(&mut crate::faults::Faults)) -> Faults {
        let mut faults = crate::faults::Faults {
            seed: 1,
            latency: None,
            congestion: None,
            blockhash_expiry: None,
            drop: None,
            lost_confirmation: None,
            rpc_unavailable: None,
        };
        f(&mut faults);
        faults
    }

    #[tokio::test]
    async fn without_a_plan_the_guard_forwards_everything_at_once() {
        let (url, seen) = upstream().await;
        let guard = Guard::start(url).await.unwrap();
        guard.set_step(0);
        let (status, reply) = call(&guard, crate::faults::tests::transaction(None, 1)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(reply["result"].is_string());
        assert_eq!(seen.lock().unwrap().as_slice(), ["sendTransaction"]);
        assert!(guard.faults_report().is_none());
        assert_eq!(guard.call_log().sends.len(), 1);
    }

    /// A signed v0 transaction whose accounts come from a lookup table, with
    /// a compute-unit price, as a bot sends it.
    async fn v0_transaction(price: u64) -> (Value, String) {
        use base64::Engine;
        use mtx_kit::chain::{AddressLookupTableAccount, InstructionGroup, TransactionSigner};
        use solana_signer::Signer;
        let payer = solana_keypair::Keypair::new();
        let to: Vec<solana_pubkey::Pubkey> = (0..3)
            .map(|_| solana_pubkey::Pubkey::new_unique())
            .collect();
        let mut instructions = vec![mtx_kit::Instruction::new_with_bytes(
            "ComputeBudget111111111111111111111111111111"
                .parse()
                .unwrap(),
            &[[3u8].as_slice(), &price.to_le_bytes()].concat(),
            vec![],
        )];
        for key in &to {
            instructions.push(mtx_kit::Instruction::new_with_bytes(
                solana_pubkey::pubkey!("11111111111111111111111111111111"),
                &[2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
                vec![
                    solana_transaction::AccountMeta::new(payer.pubkey(), true),
                    solana_transaction::AccountMeta::new(*key, false),
                ],
            ));
        }
        let table = AddressLookupTableAccount {
            key: solana_pubkey::Pubkey::new_unique(),
            addresses: to,
        };
        let plan = mtx_kit::chain::pack_with_lookup_tables(
            &payer.pubkey(),
            &[InstructionGroup::new("v0", instructions)],
            200_000,
            &[table],
        )
        .unwrap();
        let mut tx = plan.steps[0]
            .versioned(&payer.pubkey(), mtx_kit::Hash::new_unique())
            .unwrap();
        assert!(
            tx.message
                .address_table_lookups()
                .is_some_and(|l| l.len() == 1)
        );
        payer.sign_transaction(&mut tx).await.unwrap();
        let wire =
            base64::engine::general_purpose::STANDARD.encode(wincode::serialize(&tx).unwrap());
        (
            json!({ "jsonrpc": "2.0", "id": 9, "method": "sendTransaction",
                    "params": [wire, { "encoding": "base64" }] }),
            tx.signatures[0].to_string(),
        )
    }

    #[tokio::test]
    async fn v0_transactions_are_read_forwarded_and_logged() {
        let (send, signature) = v0_transaction(4_321).await;
        let parsed = crate::faults::WireTransaction::parse(&send).unwrap();
        assert_eq!(parsed.signature, signature);
        assert_eq!(parsed.unit_price, 4_321);

        let (url, seen) = upstream().await;
        let guard = Guard::start(url).await.unwrap();
        guard.set_step(0);
        guard.set_turn(true);
        let (status, reply) = call(&guard, send).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reply["result"], signature);
        assert_eq!(seen.lock().unwrap().as_slice(), ["sendTransaction"]);
        let log = guard.call_log();
        assert_eq!(log.sends[0].signature.as_deref(), Some(signature.as_str()));
        assert!(log.refused.is_empty());
    }

    #[tokio::test]
    async fn drops_expiries_and_holds_follow_the_plan() {
        use crate::faults::{Latency, SendNumbers};
        let (url, seen) = upstream().await;
        let guard = Guard::start(url).await.unwrap();
        guard.set_faults(
            Some(&faults(|f| {
                f.drop = Some(SendNumbers { sends: vec![1] });
                f.blockhash_expiry = Some(SendNumbers { sends: vec![2] });
                f.latency = Some(Latency {
                    slots: (3, 3),
                    spike: None,
                });
            })),
            100,
            2,
        );
        guard.set_step(0);
        let (_, dropped) = call(&guard, crate::faults::tests::transaction(None, 1)).await;
        assert!(dropped["result"].is_string(), "a dropped send is accepted");
        let (_, expired) = call(&guard, crate::faults::tests::transaction(None, 2)).await;
        assert_eq!(expired["error"]["code"], BLOCKHASH_NOT_FOUND);
        assert!(
            expired["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Blockhash not found")
        );
        let (_, held) = call(&guard, crate::faults::tests::transaction(None, 3)).await;
        let signature = held["result"].as_str().unwrap().to_string();
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing has been forwarded"
        );
        // Three slots are two steps; nothing lands before.
        guard.release(1).await;
        assert!(seen.lock().unwrap().is_empty());
        guard.release(2).await;
        assert_eq!(seen.lock().unwrap().as_slice(), ["sendTransaction"]);
        guard.release(3).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "a held send lands once");
        let report = guard.faults_report().unwrap();
        assert_eq!(
            (report.sends, report.landings, report.held_at_end),
            (3, 1, 0)
        );
        let fired: Vec<_> = report.fired.iter().map(|f| f.fault).collect();
        assert_eq!(fired, ["drop", "blockhash_expiry", "latency"]);
        // All three are in the log with their turn, the held one once.
        let sends = guard.call_log().sends;
        assert_eq!(sends.len(), 3);
        assert_eq!(sends[2].signature.as_deref(), Some(signature.as_str()));
        assert!(sends.iter().all(|s| s.step == Some(0)));
    }

    #[tokio::test]
    async fn the_graders_reads_for_the_bot_count_toward_none_of_its_faults() {
        use crate::faults::{LostConfirmation, RpcUnavailable};
        let (url, _) = upstream().await;
        let guard = Guard::start(url).await.unwrap();
        guard.set_faults(
            Some(&faults(|f| {
                f.lost_confirmation = Some(LostConfirmation {
                    landings: vec![1],
                    hide_steps: 2,
                });
                f.rpc_unavailable = Some(RpcUnavailable {
                    reads: vec![1],
                    count: 1,
                });
            })),
            100,
            2,
        );
        guard.set_step(5);
        let host = guard.host_url();
        assert_ne!(host, guard.url());
        // The host's send is the bot's: it lands, and its confirmation is
        // the one the plan loses.
        let tx = crate::faults::tests::transaction(None, 1);
        let (_, sent) = call_at(&host, tx).await;
        let signature = sent["result"].as_str().unwrap().to_string();
        let statuses = |id| json!({ "jsonrpc": "2.0", "id": id, "method": "getSignatureStatuses", "params": [[signature]] });
        for id in [1, 2, 3] {
            let (status, hidden) = call_at(&host, statuses(id)).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "the host's read {id} isn't the bot's"
            );
            assert!(hidden["result"]["value"][0].is_null(), "still lost");
        }
        let (status, _) = call(&guard, statuses(4)).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "the bot's own first read"
        );
        let (status, _) = call(&guard, statuses(5)).await;
        assert_eq!(status, StatusCode::OK);
        let report = guard.faults_report().unwrap();
        assert_eq!(
            (report.sends, report.reads),
            (1, 2),
            "the host's reads uncounted"
        );
    }

    #[tokio::test]
    async fn a_lost_confirmation_and_the_429s_reach_the_bot() {
        use crate::faults::{LostConfirmation, RpcUnavailable};
        let (url, _) = upstream().await;
        let guard = Guard::start(url).await.unwrap();
        guard.set_faults(
            Some(&faults(|f| {
                f.lost_confirmation = Some(LostConfirmation {
                    landings: vec![1],
                    hide_steps: 2,
                });
                f.rpc_unavailable = Some(RpcUnavailable {
                    reads: vec![3],
                    count: 2,
                });
            })),
            100,
            2,
        );
        guard.set_step(5);
        let tx = crate::faults::tests::transaction(None, 1);
        let (_, sent) = call(&guard, tx).await;
        let signature = sent["result"].as_str().unwrap().to_string();
        let statuses = |id| json!({ "jsonrpc": "2.0", "id": id, "method": "getSignatureStatuses", "params": [[signature]] });
        let (_, hidden) = call(&guard, statuses(1)).await;
        assert!(
            hidden["result"]["value"][0].is_null(),
            "landed, reads unknown"
        );
        let (status, _) = call(&guard, statuses(2)).await;
        assert_eq!(status, StatusCode::OK, "the second read");
        for id in [3, 4] {
            let (status, _) = call(&guard, statuses(id)).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "read {id}");
        }
        guard.set_step(7);
        let (status, shown) = call(&guard, statuses(5)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            shown["result"]["value"][0]["confirmationStatus"], "finalized",
            "back after two steps"
        );
    }

    #[tokio::test]
    async fn congestion_holds_a_cheap_send_and_the_fees_show_the_floor() {
        use crate::faults::Congestion;
        let (url, seen) = upstream().await;
        let guard = Guard::start(url).await.unwrap();
        guard.set_faults(
            Some(&faults(|f| {
                f.congestion = Some(Congestion {
                    from_step: 2,
                    to_step: 4,
                    floor_micro_lamports: (7_000, 7_000),
                })
            })),
            1_000,
            2,
        );
        guard.set_step(1);
        let (_, cheap) = call(&guard, crate::faults::tests::transaction(Some(6_999), 1)).await;
        assert!(cheap["result"].is_string());
        let (_, paid) = call(&guard, crate::faults::tests::transaction(Some(7_000), 2)).await;
        assert!(paid["result"].is_string());
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "only the one that pays lands"
        );
        let fees = json!({ "jsonrpc": "2.0", "id": 9, "method": "getRecentPrioritizationFees", "params": [] });
        guard.set_step(2);
        let (_, reply) = call(&guard, fees).await;
        let fees = reply["result"].as_array().unwrap();
        assert_eq!(fees.len(), 150);
        assert!(
            fees.iter()
                .any(|f| f["prioritizationFee"].as_u64().unwrap() >= 7_000)
        );
        guard.release(3).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "still congested");
        guard.release(4).await;
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}
