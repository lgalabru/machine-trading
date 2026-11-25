//! The flash-crash-spcx informed bot, scoring v2's 1: told the session's
//! latent state as `MTX_REGIME`, which names the informed takers' wallets
//! (`wallets.informed`, their signing keys) and the process every session
//! is drawn from (how far ahead they see, how often they are right).
//! Never the session's own draws: whether it is quiet, the shock's
//! direction, legs, timing or aftermath.
//!
//! What it does with it: it reads each informed taker's position on chain
//! every step, as anyone could who knew which accounts to watch, and takes
//! each one's recent flow as its view of where the price goes next. It
//! holds the full cap the way most of the views point, and keeps its
//! position while they are silent: the informed trade only while they see
//! a move ahead, so silence means the price stays about where it is. It
//! trades with the reference's mechanics (`rider.rs`, the same file): the
//! same slices, step limit and drawdown halt.
//!
//! The informed are right 85% of the time, one horizon at a time, each on
//! its own: a single wallet's view is followed only when no other
//! disagrees, and the bot can be wrong, as they are.

#[path = "../../../../reference/src/rider.rs"]
pub mod rider;

use std::collections::VecDeque;

use mtx_kit::{
    Pubkey, log,
    prelude::*,
    serde_json,
    venue::{PhoenixVenue, perp_position, perp_position_live},
};
use rider::{Account, toward};

/// Past this drawdown, in USD, it closes and stops: the reference's halt.
const HALT_DRAWDOWN_USD: f64 = 600.0;
/// The most informed wallets it watches: the family seats three.
const WALLETS_MAX: usize = 8;
/// The most accounts a trader index or active trader buffer spans, as the
/// runtime bounds them.
const ARENA_ACCOUNTS_MAX: usize = 16;
/// A position change smaller than this, in SPCX, is no trade: under
/// Phoenix's lot (0.001 SPCX).
const FLOW_MIN: f64 = 0.0005;

/// One informed taker: its trader account, its position at the last read,
/// and its trades in the window, by step.
#[derive(Debug, Clone)]
struct Watched {
    trader: Pubkey,
    position: Option<f64>,
    flow: VecDeque<(u64, f64)>,
}

impl Watched {
    /// The way its flow in the window points: +1 buying, -1 selling, 0 none.
    fn view(&self) -> f64 {
        let net: f64 = self.flow.iter().map(|(_, size)| size).sum();
        if net.abs() < FLOW_MIN {
            0.0
        } else {
            net.signum()
        }
    }
}

#[derive(Default)]
pub struct Informed {
    cap: f64,
    symbol: String,
    venue: Option<PhoenixVenue>,
    watched: Vec<Watched>,
    trader_index: Vec<String>,
    trader_buffer: Vec<String>,
    /// Steps a view lasts: the informed takers' horizon, in steps.
    window_steps: u64,
    direction: f64,
    account: Account,
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        assert!(session.cap > 0.0 && session.slots_per_step > 0);
        let regime = session
            .var("MTX_REGIME")
            .ok_or("the informed bot runs told the session's latent state (MTX_REGIME)")?;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let told = Told::from_regime(regime)?;
        self.watched = told
            .wallets
            .iter()
            .map(|authority| Watched {
                trader: venue.trader_account(authority),
                position: None,
                flow: VecDeque::new(),
            })
            .collect();
        self.window_steps = told.horizon_slots.div_ceil(session.slots_per_step).max(1);
        let keys = &venue.exchange().keys;
        self.trader_index = keys.global_trader_index.clone();
        self.trader_buffer = keys.active_trader_buffer.clone();
        if self.trader_index.len() > ARENA_ACCOUNTS_MAX
            || self.trader_buffer.len() > ARENA_ACCOUNTS_MAX
        {
            return Err("the trader arenas span more accounts than the bot reads".into());
        }
        self.cap = session.cap;
        self.symbol = session.symbol.clone();
        self.venue = Some(venue);
        log(&format!(
            "watching {} informed takers, views for {} steps",
            self.watched.len(),
            self.window_steps
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (mark, held) = (market.mark, market.position.size);
        let halted = self.account.step(mark, held, HALT_DRAWDOWN_USD);
        self.read(market.k)?;
        let vote: f64 = self.watched.iter().map(Watched::view).sum();
        if vote != 0.0 {
            self.direction = vote.signum();
        }
        let target = if halted {
            0.0
        } else {
            self.direction * self.cap
        };
        assert!(target.abs() <= self.cap);
        toward(orders, held, target, mark);
        Ok(())
    }
}

impl Informed {
    /// Read every watched taker's position, and keep what each traded since
    /// the last read in its window.
    fn read(&mut self, step: u64) -> Result<(), String> {
        let venue = self.venue.as_ref().ok_or("step before start")?;
        let market = venue.market(&self.symbol)?;
        let traders: Vec<String> = self.watched.iter().map(|w| w.trader.to_string()).collect();
        let addresses: Vec<&str> = traders
            .iter()
            .chain(&self.trader_index)
            .chain(&self.trader_buffer)
            .map(String::as_str)
            .collect();
        let accounts = mtx_kit::rpc_accounts(&addresses)?;
        assert_eq!(accounts.len(), addresses.len());
        let (own, arenas) = accounts.split_at(traders.len());
        let (index, buffer) = arenas.split_at(self.trader_index.len());
        let index: Vec<&[u8]> = arenas_of(index)?;
        let buffer: Vec<&[u8]> = arenas_of(buffer)?;
        for (watched, account) in self.watched.iter_mut().zip(own) {
            let live = perp_position_live(market, &watched.trader, &index, &buffer)?;
            let position = match (live, account) {
                (Some(position), _) => position.size,
                (None, Some(data)) => perp_position(market, data)?.size,
                // Not onboarded yet: it holds nothing.
                (None, None) => 0.0,
            };
            if let Some(last) = watched.position {
                let flow = position - last;
                if flow.abs() >= FLOW_MIN {
                    watched.flow.push_back((step, flow));
                }
            }
            watched.position = Some(position);
            while watched
                .flow
                .front()
                .is_some_and(|(at, _)| at + self.window_steps < step)
            {
                watched.flow.pop_front();
            }
            assert!(watched.flow.len() as u64 <= self.window_steps + 1);
        }
        Ok(())
    }
}

/// Each arena account's data: every one must exist.
fn arenas_of(accounts: &[Option<Vec<u8>>]) -> Result<Vec<&[u8]>, String> {
    accounts
        .iter()
        .map(|a| {
            a.as_deref()
                .ok_or_else(|| "a trader arena account is missing".to_string())
        })
        .collect()
}

/// What the latent state tells: the informed takers' wallets, as the
/// session resolved its roster, and their horizon.
#[derive(Debug, PartialEq)]
struct Told {
    wallets: Vec<Pubkey>,
    horizon_slots: u64,
}

impl Told {
    fn from_regime(regime: &str) -> Result<Self, String> {
        let latent: serde_json::Value = serde_json::from_str(regime).map_err(|e| e.to_string())?;
        let wallets = latent["wallets"]["informed"]
            .as_array()
            .ok_or("MTX_REGIME names no informed wallets (`wallets.informed`)")?
            .iter()
            .map(|w| {
                w.as_str()
                    .ok_or("an informed wallet isn't text")?
                    .parse::<Pubkey>()
                    .map_err(|e| format!("an informed wallet: {e}"))
            })
            .collect::<Result<Vec<_>, String>>()?;
        if wallets.is_empty() || wallets.len() > WALLETS_MAX {
            return Err(format!(
                "MTX_REGIME names {} informed wallets: 1 to {WALLETS_MAX}",
                wallets.len()
            ));
        }
        let horizon_slots = latent["horizon_slots"]
            .as_u64()
            .filter(|h| *h > 0)
            .ok_or("MTX_REGIME has no `horizon_slots`")?;
        Ok(Self {
            wallets,
            horizon_slots,
        })
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_regime_names_the_informed_wallets_and_their_horizon() {
        let key = Pubkey::new_unique();
        let told = Told::from_regime(&format!(
            r#"{{"wallets": {{"informed": ["{key}"]}}, "horizon_slots": 40, "hit_rate": 0.85}}"#
        ))
        .unwrap();
        assert_eq!(
            told,
            Told {
                wallets: vec![key],
                horizon_slots: 40
            }
        );
        assert!(Told::from_regime("{}").is_err());
        assert!(
            Told::from_regime(r#"{"wallets": {"informed": []}, "horizon_slots": 40}"#).is_err()
        );
        assert!(
            Told::from_regime(&format!(r#"{{"wallets": {{"informed": ["{key}"]}}}}"#)).is_err()
        );
    }

    #[test]
    fn a_view_is_the_way_the_window_s_flow_points() {
        let mut watched = Watched {
            trader: Pubkey::new_unique(),
            position: None,
            flow: VecDeque::new(),
        };
        assert_eq!(watched.view(), 0.0);
        watched.flow.extend([(1, 3.0), (2, -1.0)]);
        assert_eq!(watched.view(), 1.0);
        watched.flow.push_back((3, -4.0));
        assert_eq!(watched.view(), -1.0);
    }
}
