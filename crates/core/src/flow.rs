//! The taker flow of a market-making session (see [`crate::making`]).
//!
//! Before the cheatcodes lock, the grader onboards a pool of taker accounts
//! and widens the snapshot's makers to the scenario's spread. While the bot
//! runs, it sends each listed order at its slot as an ordinary market order,
//! from its account, through the surfnet's own RPC: the same Phoenix
//! transaction any trader sends, filled against whatever quotes are best.

use std::{path::PathBuf, time::Duration};

use futures::{StreamExt, stream::FuturesUnordered};
use mth_botkit::{BotEnv, Side, Trader};
use serde::Serialize;
use solana_keypair::Keypair;
use solana_rpc_client::rpc_client::RpcClient;
use tracing::{info, warn};

use crate::{
    Result,
    error::io_error,
    making::{Making, TAKER_ACCOUNTS, TakerSide, level_at},
    phoenix::{self, Phoenix},
    scenario::Scenario,
    snapshot::OfflineWorld,
};

/// Each taker's collateral: far more than its flow needs, so no order fails
/// for margin.
const TAKER_DEPOSIT_USDC: f64 = 100_000.0;
/// How far from the market's level a taker's order may fill. Wide enough to
/// reach the competitor at its widest, so a taker order always fills.
const TAKER_SLIPPAGE: f64 = 0.02;

/// What the grader's takers did in a session.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct FlowReport {
    /// Orders the scenario lists, and how many of them were sent and failed.
    pub orders: usize,
    pub sent: usize,
    pub failed: usize,
    /// Taker volume sent, in base units.
    pub volume: f64,
    /// Ticks the makers were pushed out by, to the scenario's half-spread.
    pub makers_widened_ticks: u64,
}

pub struct Takers {
    traders: Vec<Trader>,
    widened_ticks: u64,
    /// Grader-only: the bot can't read the takers' keypairs.
    dir: PathBuf,
}

impl Drop for Takers {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Takers {
    /// Onboard the taker pool and widen the makers. Setup: cheatcodes must
    /// still be unlocked.
    pub async fn prepare(
        world: &OfflineWorld,
        phoenix: &Phoenix,
        scenario: &Scenario,
        making: &Making,
        start_mark_ticks: u64,
    ) -> Result<Self> {
        let half_spread_ticks =
            (start_mark_ticks as f64 * making.competitor_bps / 10_000.0).round() as u64;
        let widened_ticks =
            phoenix::widen_makers(&world.surfnet, phoenix, &scenario.symbol, half_spread_ticks)?;

        let dir = std::env::temp_dir().join(format!("mth-takers-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).map_err(io_error(&dir))?;
        let keypairs: Vec<Keypair> = (0..TAKER_ACCOUNTS).map(|_| Keypair::new()).collect();
        let onboarded = futures::future::join_all(keypairs.iter().map(|keypair| {
            phoenix::onboard_trader(&world.surfnet, phoenix, keypair, TAKER_DEPOSIT_USDC)
        }))
        .await;
        let mut traders = Vec::with_capacity(keypairs.len());
        for (i, (keypair, onboarded)) in keypairs.iter().zip(onboarded).enumerate() {
            onboarded?;
            let path = dir.join(format!("taker-{i}.json"));
            let bytes = serde_json::to_string(&keypair.to_bytes().to_vec())?;
            std::fs::write(&path, bytes).map_err(io_error(&path))?;
            traders.push(Trader::connect(&BotEnv {
                rpc_url: world.surfnet.rpc_url().to_string(),
                keypair_path: path,
                exchange_path: world.exchange_path.clone(),
                symbol: scenario.symbol.clone(),
                end_slot: u64::MAX,
                max_position: f64::MAX,
                max_drawdown_usd: f64::MAX,
                mandate_path: None,
            })?);
        }
        info!(
            takers = traders.len(),
            widened_ticks,
            competitor_bps = making.competitor_bps,
            "takers ready"
        );
        Ok(Self {
            traders,
            widened_ticks,
            dir,
        })
    }

    /// Send every order at its slot, counted from `base_slot`, until
    /// `end_slot`. Orders are sent without waiting on the ones before them,
    /// so a slow transaction doesn't hold the flow back.
    pub async fn run(
        &self,
        rpc: &RpcClient,
        scenario: &Scenario,
        making: &Making,
        start_mark_usd: f64,
        base_slot: u64,
        end_slot: u64,
    ) -> FlowReport {
        let mut report = FlowReport {
            orders: making.takers.len(),
            makers_widened_ticks: self.widened_ticks,
            ..FlowReport::default()
        };
        let mut next = 0;
        let mut inflight = FuturesUnordered::new();
        loop {
            let slot = match rpc.get_slot() {
                Ok(slot) => slot,
                Err(error) => {
                    warn!("reading the slot for the flow: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if slot >= end_slot {
                break;
            }
            let at = slot.saturating_sub(base_slot);
            while let Some(order) = making.takers.get(next).filter(|o| o.slot <= at) {
                next += 1;
                let level = start_mark_usd * (1.0 + level_at(&scenario.path, at) / 100.0);
                let (side, limit) = match order.side {
                    TakerSide::Buy => (Side::Bid, level * (1.0 + TAKER_SLIPPAGE)),
                    TakerSide::Sell => (Side::Ask, level * (1.0 - TAKER_SLIPPAGE)),
                };
                let trader = &self.traders[order.account as usize];
                let size = order.size;
                report.sent += 1;
                report.volume += size;
                inflight.push(async move {
                    trader
                        .place_market(side, size, (limit * 100.0).round() / 100.0)
                        .await
                });
            }
            // Collect what has landed while waiting for the next slot.
            let tick = tokio::time::sleep(Duration::from_millis(80));
            tokio::pin!(tick);
            loop {
                tokio::select! {
                    Some(result) = inflight.next() => {
                        if let Err(error) = result {
                            report.failed += 1;
                            warn!("a taker order failed: {error}");
                        }
                    }
                    () = &mut tick => break,
                }
            }
        }
        // Orders still in flight at the end are left to land or not: the
        // account is valued at the last slot either way.
        report
    }
}
