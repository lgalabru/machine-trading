//! Authenticated, bounded proxy to Surfpool's native processed-state RPC.
//! No auction, pool snapshot cache, transaction policy, or fee ordering.
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, State,
        ws::{Message, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::sdk::Context;

pub struct StateData {
    pub ready: [bool; 4],
    pub exited: [bool; 4],
    pub running: bool,
    pub finished: bool,
    pub start_slot: u64,
    pub end_slot: u64,
    epoch: Instant,
    requests: [u32; 4],
    simulations: [u32; 4],
    submissions: [u32; 4],
    pub trace: Vec<Value>,
}
pub struct Gateway {
    pub contexts: Vec<Context>,
    pub tokens: Vec<String>,
    pub state: Mutex<StateData>,
    rpc_url: String,
    ws_url: String,
    http: reqwest::Client,
    permits: Vec<Arc<Semaphore>>,
    sockets: Vec<Arc<Semaphore>>,
    origin: Instant,
}
impl Gateway {
    pub fn new(
        contexts: Vec<Context>,
        tokens: Vec<String>,
        rpc_url: String,
        ws_url: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            contexts,
            tokens,
            rpc_url,
            ws_url,
            state: Mutex::new(StateData {
                ready: [false; 4],
                exited: [false; 4],
                running: false,
                finished: false,
                start_slot: 0,
                end_slot: 0,
                epoch: Instant::now(),
                requests: [0; 4],
                simulations: [0; 4],
                submissions: [0; 4],
                trace: vec![],
            }),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            permits: (0..4).map(|_| Arc::new(Semaphore::new(8))).collect(),
            sockets: (0..4).map(|_| Arc::new(Semaphore::new(2))).collect(),
            origin: Instant::now(),
        })
    }
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/context", get(context))
            .route("/status", get(status))
            .route("/ready", post(ready))
            .route("/exit", post(exited))
            .route("/rpc", post(rpc))
            .route("/ws", get(ws))
            .layer(DefaultBodyLimit::max(16384))
            .with_state(self)
    }
    fn participant(&self, headers: &HeaderMap) -> std::result::Result<usize, StatusCode> {
        let bearer = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        self.tokens
            .iter()
            .position(|t| t == bearer)
            .ok_or(StatusCode::UNAUTHORIZED)
    }
    fn quota(&self, i: usize, method: &str) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        if s.epoch.elapsed() >= Duration::from_secs(1) {
            s.epoch = Instant::now();
            s.requests = [0; 4];
            s.simulations = [0; 4];
            s.submissions = [0; 4];
        }
        s.requests[i] += 1;
        ensure!(s.requests[i] <= 256, "RPC rate limit: 256 requests/second");
        if method == "sendTransaction" || method == "simulateTransaction" {
            ensure!(s.running, "session not running");
            if method == "sendTransaction" {
                s.submissions[i] += 1;
                ensure!(s.submissions[i] <= 32, "submission rate limit: 32/second");
            } else {
                s.simulations[i] += 1;
                ensure!(s.simulations[i] <= 16, "simulation rate limit: 16/second");
            }
        }
        Ok(())
    }
    pub async fn quiesce(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut held = vec![];
            for p in &self.permits {
                held.push(p.clone().acquire_many_owned(8).await?);
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }
}
async fn context(
    State(g): State<Arc<Gateway>>,
    h: HeaderMap,
) -> std::result::Result<Json<Context>, StatusCode> {
    Ok(Json(g.contexts[g.participant(&h)?].clone()))
}
async fn ready(
    State(g): State<Arc<Gateway>>,
    h: HeaderMap,
) -> std::result::Result<Json<Value>, StatusCode> {
    let i = g.participant(&h)?;
    g.state.lock().unwrap().ready[i] = true;
    Ok(Json(json!({"ready":true})))
}
async fn status(
    State(g): State<Arc<Gateway>>,
    h: HeaderMap,
) -> std::result::Result<Json<Value>, StatusCode> {
    g.participant(&h)?;
    let s = g.state.lock().unwrap();
    Ok(Json(
        json!({"running":s.running,"finished":s.finished,"start_slot":s.start_slot,"end_slot":s.end_slot,"slot_ms":crate::SLOT_MS}),
    ))
}

pub fn validate(request: &Value, websocket: bool) -> Result<&str> {
    let method = request["method"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing method"))?;
    let allowed = if websocket {
        matches!(
            method,
            "accountSubscribe"
                | "accountUnsubscribe"
                | "programSubscribe"
                | "programUnsubscribe"
                | "slotSubscribe"
                | "slotUnsubscribe"
                | "logsSubscribe"
                | "logsUnsubscribe"
                | "signatureSubscribe"
                | "signatureUnsubscribe"
        )
    } else {
        matches!(
            method,
            "getAccountInfo"
                | "getMultipleAccounts"
                | "getBalance"
                | "getProgramAccounts"
                | "getTokenAccountBalance"
                | "getTokenAccountsByOwner"
                | "getTokenSupply"
                | "getLatestBlockhash"
                | "isBlockhashValid"
                | "getSlot"
                | "getBlockHeight"
                | "getBlock"
                | "getBlocks"
                | "getBlockTime"
                | "getTransaction"
                | "getSignatureStatuses"
                | "getSignaturesForAddress"
                | "getMinimumBalanceForRentExemption"
                | "getFeeForMessage"
                | "getVersion"
                | "getHealth"
                | "sendTransaction"
                | "simulateTransaction"
        )
    };
    ensure!(allowed, "RPC method not exposed");
    if method == "sendTransaction" {
        // Surfpool's nonstandard skipSigVerify must never be exposed. Unknown
        // extensions fail closed; ordinary Solana send configuration is retained.
        if let Some(config) = request["params"][1].as_object() {
            ensure!(
                config.keys().all(|k| matches!(
                    k.as_str(),
                    "encoding"
                        | "skipPreflight"
                        | "preflightCommitment"
                        | "maxRetries"
                        | "minContextSlot"
                )),
                "nonstandard send configuration"
            );
        }
    }
    Ok(method)
}
async fn rpc(
    State(g): State<Arc<Gateway>>,
    h: HeaderMap,
    Json(request): Json<Value>,
) -> std::result::Result<Json<Value>, StatusCode> {
    let i = g.participant(&h)?;
    let _permit = g.permits[i]
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    let arrival = g.origin.elapsed().as_micros();
    let result = async {
        let method = validate(&request, false)?;
        g.quota(i, method)?;
        let response = g
            .http
            .post(&g.rpc_url)
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        Ok::<_, anyhow::Error>(response)
    }
    .await;
    let response = result.unwrap_or_else(|e| json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":e.to_string()}}));
    if request["method"] == "sendTransaction" {
        g.state.lock().unwrap().trace.push(json!({"kind":"submission","participant":i,"arrival_us":arrival,"completed_us":g.origin.elapsed().as_micros(),"request":request,"response":response}));
    }
    Ok(Json(response))
}
async fn ws(State(g): State<Arc<Gateway>>, h: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let i = match g.participant(&h) {
        Ok(i) => i,
        Err(e) => return e.into_response(),
    };
    let permit = match g.sockets[i].clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return StatusCode::TOO_MANY_REQUESTS.into_response(),
    };
    upgrade.max_message_size(16384).on_upgrade(move |mut socket| async move {
        let _permit=permit;
        let Ok((mut upstream,_))=tokio_tungstenite::connect_async(&g.ws_url).await else { return; };
        let mut subscriptions=0;
        loop {
            tokio::select! {
                incoming=socket.recv()=> {
                    let Some(Ok(Message::Text(text)))=incoming else { break; };
                    let Ok(request)=serde_json::from_str::<Value>(&text) else { break; };
                    let Ok(method)=validate(&request,true) else { break; };
                    if g.quota(i,method).is_err() { break; }
                    // Bound total subscriptions issued per connection, even
                    // across unsubscribe/re-subscribe churn.
                    if method.ends_with("Subscribe") { subscriptions+=1; if subscriptions>32 { break; } }
                    if upstream.send(tokio_tungstenite::tungstenite::Message::Text(text.to_string().into())).await.is_err() { break; }
                }
                outgoing=upstream.next()=> {
                    match outgoing {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => if socket.send(Message::Text(text.to_string().into())).await.is_err() { break; },
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(v))) => { let _=upstream.send(tokio_tungstenite::tungstenite::Message::Pong(v)).await; },
                        Some(Ok(_)) => {}, _=>break,
                    }
                }
            }
        }
    })
}
async fn exited(
    State(g): State<Arc<Gateway>>,
    h: HeaderMap,
) -> std::result::Result<Json<Value>, StatusCode> {
    let i = g.participant(&h)?;
    g.state.lock().unwrap().exited[i] = true;
    Ok(Json(json!({"recorded":true})))
}

/// Identify signed submissions even when an RPC response times out after enqueue.
pub fn submission_signature(record: &Value) -> Option<String> {
    use base64::Engine;
    let encoded = record["request"]["params"][0].as_str()?;
    let bytes = if record["request"]["params"][1]["encoding"] == "base64" {
        base64::prelude::BASE64_STANDARD.decode(encoded).ok()?
    } else {
        bs58::decode(encoded).into_vec().ok()?
    };
    let tx: solana_transaction::versioned::VersionedTransaction =
        wincode::deserialize(&bytes).ok()?;
    tx.signatures.first().map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_transactions_allowed_but_administration_is_not() {
        assert!(validate(&json!({"method":"sendTransaction","params":["wire",{"skipPreflight":true,"encoding":"base64"}]}),false).is_ok());
        for method in ["requestAirdrop", "surfnet_setAccount", "submitBatch"] {
            assert!(validate(&json!({"method":method}), false).is_err());
        }
        assert!(
            validate(
                &json!({"method":"sendTransaction","params":["wire",{"skipSigVerify":true}]}),
                false
            )
            .is_err()
        );
        assert!(validate(&json!({"method":"accountSubscribe"}), true).is_ok());
        assert!(validate(&json!({"method":"sendTransaction"}), true).is_err());
    }
}
