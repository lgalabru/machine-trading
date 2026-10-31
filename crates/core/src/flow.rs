//! The taker flow of a market-making session (see [`crate::making`]).
//!
//! Before the cheatcodes lock, the grader onboards a pool of taker accounts
//! and widens the snapshot's makers to the scenario's spread. While the bot
//! runs, at each step it sends the orders due by then as ordinary market
//! orders, from their accounts, through the grader's own RPC: the same Phoenix
//! transaction any trader sends, filled against whatever quotes are best.
//! They go one at a time, in the scenario's order, between the bot's turns,
//! so the same session sends the same flow into the same book every time.

use std::path::PathBuf;

use mth_botkit::{BotEnv, Side, Trader};
use serde::Serialize;
use solana_keypair::Keypair;
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
    /// still be unlocked. The takers trade through `rpc_url`.
    pub async fn prepare(
        world: &OfflineWorld,
        phoenix: &Phoenix,
        scenario: &Scenario,
        making: &Making,
        start_mark_ticks: u64,
        rpc_url: &str,
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
                rpc_url: rpc_url.to_string(),
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

    /// The flow's state through a session, starting before its first order.
    pub fn flow(&self, making: &Making) -> Flow {
        Flow {
            next: 0,
            report: FlowReport {
                orders: making.takers.len(),
                makers_widened_ticks: self.widened_ticks,
                ..FlowReport::default()
            },
        }
    }
}

/// Where a session's flow is: the next order to send, and what was sent.
pub struct Flow {
    next: usize,
    report: FlowReport,
}

impl Flow {
    /// Send, in order, every order due by `at` slots into the session, priced
    /// off the scenario's level there.
    pub async fn step(
        &mut self,
        takers: &Takers,
        scenario: &Scenario,
        making: &Making,
        start_mark_usd: f64,
        at: u64,
    ) {
        while let Some(order) = making.takers.get(self.next).filter(|o| o.slot <= at) {
            self.next += 1;
            let level = start_mark_usd * (1.0 + level_at(&scenario.path, at) / 100.0);
            // The order's own limit: it fills only against quotes at least
            // this close to the level, or not at all.
            let reach = order.limit_bps / 10_000.0;
            let (side, limit) = match order.side {
                TakerSide::Buy => (Side::Bid, level * (1.0 + reach)),
                TakerSide::Sell => (Side::Ask, level * (1.0 - reach)),
            };
            let trader = &takers.traders[order.account as usize];
            self.report.sent += 1;
            self.report.volume += order.size;
            if let Err(error) = trader
                .place_market(side, order.size, (limit * 100.0).round() / 100.0)
                .await
            {
                self.report.failed += 1;
                warn!("a taker order failed: {error}");
            }
        }
    }

    pub fn report(self) -> FlowReport {
        self.report
    }
}
