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

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
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

pub struct Guard {
    url: String,
    state: Arc<GuardState>,
    server: JoinHandle<()>,
}

struct GuardState {
    upstream: String,
    client: reqwest::Client,
    /// Every refused method name, in order, duplicates included.
    refused: Mutex<Vec<String>>,
}

impl Guard {
    /// Listen on a free localhost port and forward allowed requests to `upstream`.
    pub async fn start(upstream: impl Into<String>) -> Result<Self> {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = TcpListener::bind(addr)
            .await
            .map_err(io_error("guard socket"))?;
        let local = listener.local_addr().map_err(io_error("guard socket"))?;
        let state = Arc::new(GuardState {
            upstream: upstream.into(),
            client: reqwest::Client::new(),
            refused: Mutex::default(),
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
        .filter_map(|call| refusal(&state, call))
        .collect();
    if !refusals.is_empty() {
        return match request {
            Value::Array(_) => reply(Value::Array(
                calls
                    .iter()
                    .map(|call| {
                        refusal_for(call).unwrap_or_else(|| {
                            error_response(call["id"].clone(), INVALID_REQUEST, "batch refused")
                        })
                    })
                    .collect(),
            )),
            _ => reply(refusals.into_iter().next().expect("one refusal")),
        };
    }

    let upstream = state
        .client
        .post(&state.upstream)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await;
    match upstream {
        Ok(response) => {
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match response.bytes().await {
                Ok(bytes) => {
                    (status, [("content-type", "application/json")], bytes).into_response()
                }
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

/// Record and build the refusal for `call`, if it names a method bots may not use.
fn refusal(state: &GuardState, call: &Value) -> Option<Value> {
    let response = refusal_for(call)?;
    let method = call["method"].as_str().unwrap_or("<missing>").to_string();
    warn!(method, "rpc guard refused a call");
    state.refused.lock().expect("guard lock").push(method);
    Some(response)
}

fn refusal_for(call: &Value) -> Option<Value> {
    let method = call["method"].as_str();
    match method {
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

    #[test]
    fn standard_methods_pass_and_cheats_are_refused() {
        let call = |method: &str| json!({ "jsonrpc": "2.0", "id": 1, "method": method });
        assert!(refusal_for(&call("getSlot")).is_none());
        assert!(refusal_for(&call("sendTransaction")).is_none());
        for cheat in [
            "surfnet_setAccount",
            "surfnet_timeTravel",
            "requestAirdrop",
            "exit",
            "loadPlugin",
        ] {
            let refused = refusal_for(&call(cheat)).expect(cheat);
            assert_eq!(refused["error"]["code"], METHOD_NOT_FOUND);
            assert_eq!(refused["id"], 1);
        }
        assert!(refusal_for(&json!({ "id": 2 })).is_some());
    }
}
