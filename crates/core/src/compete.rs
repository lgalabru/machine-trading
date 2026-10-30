//! The grader's own maker in a market-making session (see
//! [`crate::making`]).
//!
//! The snapshot's makers are splines the grader can only reshape before the
//! cheatcodes lock, so on their own they stand still: a bot one tick inside
//! them collects the spread on every order, unopposed. This maker is the
//! opposition. Onboarded at setup like the takers, it trades through the
//! surfnet's own RPC with ordinary orders while the bot runs.
//!
//! Every `react_slots` it reads the best bid and offer. A side where it
//! isn't best means someone quotes inside it: it steps one tick inside them,
//! never closer to the level than its floor. A side where it is best, it
//! eases one tick back out toward the competitor's half-spread, the way a
//! maker probes for a wider price once nobody is undercutting. And it skews
//! both quotes against its inventory, so it doesn't warehouse the flow.

use std::{path::PathBuf, time::Duration};

use mth_botkit::{BotEnv, Side, Trader};
use serde::Serialize;
use solana_keypair::Keypair;
use solana_rpc_client::rpc_client::RpcClient;
use tracing::{info, warn};

use crate::{
    Result,
    error::io_error,
    making::{Competition, Making, level_at},
    phoenix::{self, Phoenix},
    scenario::Scenario,
    snapshot::OfflineWorld,
};

/// Its collateral: far more than its quotes need.
const DEPOSIT_USDC: f64 = 100_000.0;
/// A tick, in USD: Phoenix quotes SPCX in cents.
const TICK_USD: f64 = 0.01;
/// How far its quotes lean against what it holds, in basis points per base
/// unit, and the inventory at which it stops quoting the side that adds.
const SKEW_BPS_PER_UNIT: f64 = 1.0;
const MAX_INVENTORY: f64 = 25.0;

/// What the grader's maker did in a session.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CompeteReport {
    /// Times it requoted, and how many of those it stepped inside a quote
    /// better than its own.
    pub requotes: usize,
    pub undercuts: usize,
    /// Its tightest half-spread, in basis points.
    pub tightest_bps: f64,
    /// Its position at the end, in base units.
    pub end_position: f64,
}

pub struct Competitor {
    trader: Trader,
    /// Grader-only: the bot can't read the maker's keypair.
    dir: PathBuf,
}

impl Drop for Competitor {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Competitor {
    /// Onboard the maker. Setup: cheatcodes must still be unlocked.
    pub async fn prepare(
        world: &OfflineWorld,
        phoenix: &Phoenix,
        scenario: &Scenario,
    ) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("mth-competitor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).map_err(io_error(&dir))?;
        let keypair = Keypair::new();
        phoenix::onboard_trader(&world.surfnet, phoenix, &keypair, DEPOSIT_USDC).await?;
        let path = dir.join("competitor.json");
        let bytes = serde_json::to_string(&keypair.to_bytes().to_vec())?;
        std::fs::write(&path, bytes).map_err(io_error(&path))?;
        let trader = Trader::connect(&BotEnv {
            rpc_url: world.surfnet.rpc_url().to_string(),
            keypair_path: path,
            exchange_path: world.exchange_path.clone(),
            symbol: scenario.symbol.clone(),
            end_slot: u64::MAX,
            max_position: f64::MAX,
            max_drawdown_usd: f64::MAX,
            mandate_path: None,
        })?;
        Ok(Self { trader, dir })
    }

    /// Quote until `end_slot`, slots counted from `base_slot`.
    pub async fn run(
        &self,
        rpc: &RpcClient,
        scenario: &Scenario,
        making: &Making,
        start_mark_usd: f64,
        base_slot: u64,
        end_slot: u64,
    ) -> CompeteReport {
        let c: Competition = making.competition;
        let mut report = CompeteReport {
            tightest_bps: making.competitor_bps,
            ..CompeteReport::default()
        };
        // Its half-spread on each side, in basis points: where it quotes
        // before skew.
        let (mut bid_bps, mut ask_bps) = (making.competitor_bps, making.competitor_bps);
        // Its resting prices, to tell its own quotes from a better one.
        let mut quoted: Option<(f64, f64)> = None;
        let mut next_look = 0u64;
        loop {
            let slot = match rpc.get_slot() {
                Ok(slot) => slot,
                Err(error) => {
                    warn!("competitor reading the slot: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if slot >= end_slot {
                break;
            }
            if slot < next_look {
                tokio::time::sleep(Duration::from_millis(80)).await;
                continue;
            }
            next_look = slot + c.react_slots;
            let level = start_mark_usd
                * (1.0 + level_at(&scenario.path, slot.saturating_sub(base_slot)) / 100.0);
            let tick_bps = TICK_USD / level * 10_000.0;

            if let (Some((bid, ask)), Ok(bbo)) = (quoted, self.trader.luce.bbo().await) {
                let market = self.trader.luce.market();
                let usd = |ticks: Option<u64>| {
                    ticks.map(|t| {
                        market
                            .ticks_to_price(t)
                            .to_string()
                            .parse::<f64>()
                            .unwrap_or(0.0)
                    })
                };
                let best_bid = usd(bbo.best_bid_ticks()).unwrap_or(0.0);
                let best_ask = usd(bbo.best_ask_ticks()).unwrap_or(0.0);
                // The splines' level: quotes there are the background, not a
                // rival. Only a quote inside both it and its own counts.
                let wide = making.competitor_bps / 10_000.0;
                let (spline_bid, spline_ask) = (level * (1.0 - wide), level * (1.0 + wide));
                // Someone bids above it: step inside them, down to the floor.
                if best_bid > bid.max(spline_bid) + TICK_USD / 2.0 {
                    let theirs = (level - best_bid) / level * 10_000.0;
                    bid_bps = (theirs - tick_bps).max(c.floor_bps);
                    report.undercuts += 1;
                } else {
                    bid_bps = (bid_bps + tick_bps).min(making.competitor_bps);
                }
                if best_ask > 0.0 && best_ask < ask.min(spline_ask) - TICK_USD / 2.0 {
                    let theirs = (best_ask - level) / level * 10_000.0;
                    ask_bps = (theirs - tick_bps).max(c.floor_bps);
                    report.undercuts += 1;
                } else {
                    ask_bps = (ask_bps + tick_bps).min(making.competitor_bps);
                }
            }
            report.tightest_bps = report.tightest_bps.min(bid_bps).min(ask_bps);

            let held = match self.trader.position().await {
                Ok(position) => position.size,
                Err(error) => {
                    warn!("competitor reading its position: {error}");
                    continue;
                }
            };
            let skew = SKEW_BPS_PER_UNIT * held;
            let bid = round_down(level * (1.0 - (bid_bps + skew) / 10_000.0));
            let ask = round_up(level * (1.0 + (ask_bps - skew) / 10_000.0));
            if quoted == Some((bid, ask)) {
                continue;
            }
            if let Err(error) = self.trader.cancel_all() {
                warn!("competitor cancelling: {error}");
                continue;
            }
            if held < MAX_INVENTORY
                && let Err(error) = self.trader.place_limit(Side::Bid, bid, c.size).await
            {
                warn!("competitor bidding: {error}");
            }
            if held > -MAX_INVENTORY
                && let Err(error) = self.trader.place_limit(Side::Ask, ask, c.size).await
            {
                warn!("competitor offering: {error}");
            }
            quoted = Some((bid, ask));
            report.requotes += 1;
        }
        report.end_position = self.trader.position().await.map_or(0.0, |p| p.size);
        info!(?report, "competitor done");
        report
    }
}

fn round_down(usd: f64) -> f64 {
    (usd / TICK_USD).floor() * TICK_USD
}

fn round_up(usd: f64) -> f64 {
    (usd / TICK_USD).ceil() * TICK_USD
}
