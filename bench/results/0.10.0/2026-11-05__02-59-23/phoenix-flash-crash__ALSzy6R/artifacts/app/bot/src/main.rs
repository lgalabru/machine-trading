//! Live bot: reads the market each turn, feeds the strategy, and trades
//! toward its target with market orders.

use anyhow::{Context, Result};
use bot::strategy::{Config, Strategy};
use mtx_botkit::{BotEnv, Side, Trader, Turns};
use std::time::Instant;

struct View {
    oracle: f64,
    mark: f64,
    bid: f64,
    ask: f64,
    position: f64,
    entry: f64,
    collateral: f64,
    unrealized: f64,
}

struct Bot {
    trader: Trader,
    tick: f64,
    lot: f64,
    wallet_usd: f64,
    strategy: Strategy,
}

impl Bot {
    async fn view(&self) -> Result<View> {
        let bbo = self.trader.luce.bbo().await.context("bbo")?;
        let margin = self
            .trader
            .phoenix
            .hawkeye(self.trader.authority())
            .view_margin(self.trader.authority(), 0, 0)
            .await
            .context("margin")?
            .value;
        let asset = self.trader.luce.margin().await.context("asset")?;
        let market = self.trader.phoenix.market(&self.trader.env.symbol)?;
        Ok(View {
            oracle: bbo.index_price_ticks as f64 * self.tick,
            mark: bbo.mark_price_ticks as f64 * self.tick,
            bid: bbo.best_bid_ticks as f64 * self.tick,
            ask: bbo.best_ask_ticks as f64 * self.tick,
            position: self.trader.phoenix.lots_to_base_units(market, asset.base_lots),
            entry: self
                .trader
                .phoenix
                .entry_price_to_usd(market, asset.entry_price_quote_lots_per_base_lot),
            collateral: margin.collateral_quote_lots as f64 / 1e6,
            unrealized: margin.unrealized_pnl_quote_lots as f64 / 1e6,
        })
    }

    async fn step(&mut self, k: u64, slot: u64) -> Result<()> {
        let t0 = Instant::now();
        let v = self.view().await?;
        let value = v.collateral + v.unrealized + self.wallet_usd;
        let price = if v.oracle > 0.0 { v.oracle } else { v.mark };
        let max_position = self.strategy.cfg.max_position;
        let target = self
            .strategy
            .on_step(k, price, v.position, value)
            .clamp(-max_position, max_position);
        let mut delta = target - v.position;
        // Round to whole lots and never let a fill take the position over
        // the cap, whatever the on-chain rounding does.
        delta = (delta / self.lot).round() * self.lot;
        let after = v.position + delta;
        if after.abs() > max_position - self.lot {
            delta = (max_position - self.lot) * after.signum() - v.position;
            delta = (delta / self.lot).trunc() * self.lot;
        }
        let mut sent = String::new();
        if delta.abs() >= 0.01 {
            let (side, limit) = if delta > 0.0 {
                (Side::Bid, v.ask.max(price) * 1.01)
            } else {
                (Side::Ask, v.bid.min(price) * 0.99)
            };
            // Split big orders so each stays well inside the compute budget.
            let mut remaining = delta.abs();
            while remaining >= self.lot {
                let size = remaining.min(40.0);
                match self.trader.place_market(side, size, limit).await {
                    Ok(sig) => sent.push_str(&format!(
                        " {}{:.3}@{:.2}:{}",
                        if delta > 0.0 { "+" } else { "-" },
                        size,
                        limit,
                        &sig.to_string()[..8]
                    )),
                    Err(e) => {
                        sent.push_str(&format!(" ERR({e})"));
                        break;
                    }
                }
                remaining -= size;
            }
        }
        println!(
            "k={k} slot={slot} oracle={:.2} mark={:.2} bid={:.2} ask={:.2} pos={:+.3} entry={:.2} value={:.2} peak={:.2} dd={:.0} anchor={:.2} d={:+.3}% mode={:?} legs={} target={:+.1}{} ms={}",
            v.oracle,
            v.mark,
            v.bid,
            v.ask,
            v.position,
            v.entry,
            value,
            self.strategy.peak_value,
            self.strategy.peak_value - value,
            self.strategy.anchor,
            (price / self.strategy.anchor.max(1e-9) - 1.0) * 100.0,
            self.strategy.mode,
            self.strategy.legs,
            target,
            sent,
            t0.elapsed().as_millis()
        );
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Answer the grader's hello first: the world waits for it.
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let market = trader.phoenix.market(&env.symbol)?;
    let tick = market.tick_size as f64 * 10f64.powi(market.base_lots_decimals as i32 - 6);
    let lot = 10f64.powi(-(market.base_lots_decimals as i32));
    let cfg = Config {
        max_position: env.max_position,
        max_drawdown: env.max_drawdown_usd,
        ..Config::default()
    };
    eprintln!(
        "connected authority={} end_slot={} hello={:?} cfg={:?}",
        trader.authority(),
        env.end_slot,
        turns.hello(),
        cfg
    );
    let mut bot = Bot {
        trader,
        tick,
        lot,
        wallet_usd: 100.0,
        strategy: Strategy::new(cfg),
    };
    while let Some(step) = turns.next()? {
        if let Err(e) = bot.step(step.k, step.slot).await {
            eprintln!("k={} step failed: {e:#}", step.k);
        }
    }
    eprintln!("session ended");
    Ok(())
}
