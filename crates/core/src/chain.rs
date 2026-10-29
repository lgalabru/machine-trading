//! Thin helpers over a surfnet's JSON-RPC: transactions, scenarios, snapshots.

use std::{collections::BTreeMap, time::Duration};

use serde_json::{Value, json};
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::request::RpcRequest;
use surfpool_sdk::{AccountSnapshot, SimnetEvent, Surfnet};
use tracing::{debug, error, info, warn};

use crate::{Error, Result};

const LAST_RESTART_SLOT_SYSVAR: &str = "SysvarLastRestartS1ot1111111111111111111111";

/// A client for `surfnet` that confirms at `confirmed` (see [`mth_botkit::rpc_client`]).
pub fn rpc_client(surfnet: &Surfnet) -> RpcClient {
    mth_botkit::rpc_client(surfnet.rpc_url())
}

pub fn call(rpc: &RpcClient, method: &'static str, params: Value) -> Result<Value> {
    Ok(rpc.send(RpcRequest::Custom { method }, params)?)
}

/// One account override in a Surfpool scenario, applied `slot` slots after registration.
pub struct Override {
    pub template_id: &'static str,
    pub account: Pubkey,
    pub values: Value,
    pub slot: u64,
    pub label: String,
}

pub fn register_scenario(rpc: &RpcClient, name: &str, overrides: Vec<Override>) -> Result<()> {
    let overrides: Vec<Value> = overrides
        .into_iter()
        .map(|o| {
            json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "templateId": o.template_id,
                "account": { "pubkey": o.account.to_string() },
                "values": o.values,
                "scenarioRelativeSlot": o.slot,
                "enabled": true,
                "fetchBeforeUse": false,
                "label": o.label,
            })
        })
        .collect();
    let scenario = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "name": name,
        "description": name,
        "tags": ["mth"],
        "overrides": overrides,
    });
    call(rpc, "surfnet_registerScenario", json!([scenario]))?;
    info!(name, "scenario registered");
    Ok(())
}

/// Every account the surfnet holds, including program accounts, which
/// `surfnet_exportSnapshot` leaves out by default: without them an offline
/// surfnet can't execute Phoenix or Hawkeye. Sysvars are left out: mainnet's
/// Clock and SlotHashes would fight the restored surfnet's own clock. The one
/// exception is LastRestartSlot: Phoenix treats the exchange as halted unless
/// it matches the restart slot its GlobalConfiguration acknowledged.
pub fn export_snapshot(rpc: &RpcClient) -> Result<BTreeMap<String, Option<AccountSnapshot>>> {
    let response = call(
        rpc,
        "surfnet_exportSnapshot",
        json!([{
            "scope": "network",
            "filter": {
                "includeProgramAccounts": true,
                "excludeSysvars": true,
                "includeAccounts": [LAST_RESTART_SLOT_SYSVAR],
            },
        }]),
    )?;
    // The RPC wraps the snapshot in `{ context, value }`; snapshot files hold `value`.
    Ok(serde_json::from_value(response["value"].clone())?)
}

pub async fn wait_for<T>(
    what: &str,
    timeout: Duration,
    mut check: impl AsyncFnMut() -> Result<Option<T>>,
) -> Result<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Some(value) = check().await? {
            return Ok(value);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(Error::Timeout(what.to_string()))
}

/// Surfpool reports skipped scenario overrides and runtime problems on its
/// event channel, not through `tracing`; mirror them so they show up in logs.
pub fn forward_surfnet_logs(surfnet: &Surfnet) {
    let events = surfnet.events().clone();
    std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match event {
                SimnetEvent::ErrorLog(_, message) => error!(target: "surfnet", "{message}"),
                SimnetEvent::WarnLog(_, message) => warn!(target: "surfnet", "{message}"),
                SimnetEvent::InfoLog(_, message) => debug!(target: "surfnet", "{message}"),
                _ => {}
            }
        }
    });
}
