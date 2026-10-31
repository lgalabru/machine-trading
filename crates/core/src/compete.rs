//! The grader's own maker in a market-making session (see
//! [`crate::making`]).
//!
//! The snapshot's makers are splines the grader can only reshape before the
//! cheatcodes lock, so on their own they stand still: a bot one tick inside
//! them collects the spread on every order, unopposed. This maker is the
//! opposition. Onboarded at setup like the takers, it trades through the
//! grader's own RPC with ordinary orders, between the bot's turns.
//!
//! Every `react_slots` it reads the best bid and offer. A side where it
//! isn't best means someone quotes inside it: it steps one tick inside them,
//! never closer to the level than its floor. A side where it is best, it
//! eases one tick back out toward the competitor's half-spread, the way a
//! maker probes for a wider price once nobody is undercutting. And it skews
//! both quotes against its inventory, so it doesn't warehouse the flow.

use std::path::PathBuf;

use mth_botkit::{BotEnv, Side, Trader};
use serde::Serialize;
use solana_keypair::Keypair;
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
    /// Onboard the maker. Setup: cheatcodes must still be unlocked. It
    /// trades through `rpc_url`.
    pub async fn prepare(
        world: &OfflineWorld,
        phoenix: &Phoenix,
        scenario: &Scenario,
        rpc_url: &str,
    ) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("mth-competitor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).map_err(io_error(&dir))?;
        let keypair = Keypair::new();
        phoenix::onboard_trader(&world.surfnet, phoenix, &keypair, DEPOSIT_USDC).await?;
        let path = dir.join("competitor.json");
        let bytes = serde_json::to_string(&keypair.to_bytes().to_vec())?;
        std::fs::write(&path, bytes).map_err(io_error(&path))?;
        let trader = Trader::connect(&BotEnv {
            rpc_url: rpc_url.to_string(),
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

    /// Its state through a session, before its first quote.
    pub fn quoting(&self, making: &Making) -> Quoting {
        Quoting {
            report: CompeteReport {
                tightest_bps: making.competitor_bps,
                ..CompeteReport::default()
            },
            bid_bps: making.competitor_bps,
            ask_bps: making.competitor_bps,
            quoted: None,
            next_look: 0,
        }
    }
}

/// Where the grader's maker is in a session.
pub struct Quoting {
    report: CompeteReport,
    /// Its half-spread on each side, in basis points: where it quotes before
    /// skew.
    bid_bps: f64,
    ask_bps: f64,
    /// Its resting prices, to tell its own quotes from a better one.
    quoted: Option<(f64, f64)>,
    /// The next slot into the session at which it looks at the book.
    next_look: u64,
}

impl Quoting {
    /// Look at the book and requote, if it's due `at` slots into the session.
    pub async fn step(
        &mut self,
        competitor: &Competitor,
        scenario: &Scenario,
        making: &Making,
        start_mark_usd: f64,
        at: u64,
    ) {
        let c: Competition = making.competition;
        if at < self.next_look {
            return;
        }
        self.next_look = at + c.react_slots;
        let trader = &competitor.trader;
        let level = start_mark_usd * (1.0 + level_at(&scenario.path, at) / 100.0);
        let tick_bps = TICK_USD / level * 10_000.0;

        if let (Some((bid, ask)), Ok(bbo)) = (self.quoted, trader.luce.bbo().await) {
            let market = trader.luce.market();
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
                self.bid_bps = (theirs - tick_bps).max(c.floor_bps);
                self.report.undercuts += 1;
            } else {
                self.bid_bps = (self.bid_bps + tick_bps).min(making.competitor_bps);
            }
            if best_ask > 0.0 && best_ask < ask.min(spline_ask) - TICK_USD / 2.0 {
                let theirs = (best_ask - level) / level * 10_000.0;
                self.ask_bps = (theirs - tick_bps).max(c.floor_bps);
                self.report.undercuts += 1;
            } else {
                self.ask_bps = (self.ask_bps + tick_bps).min(making.competitor_bps);
            }
        }
        self.report.tightest_bps = self.report.tightest_bps.min(self.bid_bps).min(self.ask_bps);

        let held = match trader.position().await {
            Ok(position) => position.size,
            Err(error) => {
                warn!("competitor reading its position: {error}");
                return;
            }
        };
        let skew = SKEW_BPS_PER_UNIT * held;
        let bid = round_down(level * (1.0 - (self.bid_bps + skew) / 10_000.0));
        let ask = round_up(level * (1.0 + (self.ask_bps - skew) / 10_000.0));
        if self.quoted == Some((bid, ask)) {
            return;
        }
        if let Err(error) = trader.cancel_all() {
            warn!("competitor cancelling: {error}");
            return;
        }
        if held < MAX_INVENTORY
            && let Err(error) = trader.place_limit(Side::Bid, bid, c.size).await
        {
            warn!("competitor bidding: {error}");
        }
        if held > -MAX_INVENTORY
            && let Err(error) = trader.place_limit(Side::Ask, ask, c.size).await
        {
            warn!("competitor offering: {error}");
        }
        self.quoted = Some((bid, ask));
        self.report.requotes += 1;
    }

    pub async fn report(mut self, competitor: &Competitor) -> CompeteReport {
        self.report.end_position = competitor.trader.position().await.map_or(0.0, |p| p.size);
        info!(report = ?self.report, "competitor done");
        self.report
    }
}

fn round_down(usd: f64) -> f64 {
    (usd / TICK_USD).floor() * TICK_USD
}

fn round_up(usd: f64) -> f64 {
    (usd / TICK_USD).ceil() * TICK_USD
}
