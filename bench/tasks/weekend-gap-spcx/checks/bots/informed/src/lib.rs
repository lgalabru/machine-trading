//! The weekend-gap-spcx informed bot, scoring v2's 1: told the session's
//! latent state as `MTX_REGIME`, which names the wallets of the traders who
//! position into the reopen on information (`wallets.informed`, their
//! signing keys) and the process every session is drawn from (how much of
//! the weekend's move the reopen keeps and give or take what, how far ahead
//! the informed see and how often they are right). Never the session's own
//! draws: the weekend's move, the open, whether a wallet is right this time.
//!
//! What it does with it: the reference's prior (the reopen keeps about four
//! fifths of the weekend's move), and the informed wallets' trades, read on
//! chain every step from their positions, as anyone could who knew which
//! wallets to watch. Each wallet's net flow over the last stretch is a vote
//! on which way the reopen goes, right with the told hit rate; the votes
//! move the prior's odds, and it takes the gap it then expects, with the
//! reference's mechanics (`reopen.rs`, the same file).

#[path = "../../../../reference/src/reopen.rs"]
pub mod reopen;

use mtx_kit::{
    Pubkey, log,
    prelude::*,
    serde_json,
    venue::{PhoenixVenue, perp_position, perp_position_live},
};
use reopen::{Account, Clock, Phase, position_for, trade_toward};

/// The most informed wallets it watches: the family seats two.
const WALLETS_MAX: usize = 8;
/// The most accounts a trader index or active trader buffer spans, as the
/// runtime bounds them.
const ARENA_ACCOUNTS_MAX: usize = 16;
/// A position change smaller than this, in SPCX, is no trade: under
/// Phoenix's lot (0.001 SPCX).
const FLOW_MIN: f64 = 0.0005;

/// One informed wallet: its trader account, its position at the last read
/// and its net flow since it started.
#[derive(Debug, Clone)]
struct Watched {
    trader: Pubkey,
    position: Option<f64>,
    net_flow: f64,
}

impl Watched {
    /// The way its flow points: +1 buying, -1 selling, 0 none.
    fn vote(&self) -> f64 {
        if self.net_flow.abs() < FLOW_MIN {
            0.0
        } else {
            self.net_flow.signum()
        }
    }
}

#[derive(Default)]
pub struct Informed {
    clock: Option<Clock>,
    cap: f64,
    symbol: String,
    venue: Option<PhoenixVenue>,
    told: Option<Told>,
    watched: Vec<Watched>,
    trader_index: Vec<String>,
    trader_buffer: Vec<String>,
    start_mid: Option<f64>,
    target: f64,
    account: Account,
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        assert!(session.cap > 0.0);
        let regime = session
            .var("MTX_REGIME")
            .ok_or("the informed bot runs told the session's latent state (MTX_REGIME)")?;
        let told = Told::from_regime(regime)?;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        self.watched = told
            .wallets
            .iter()
            .map(|authority| Watched {
                trader: venue.trader_account(authority),
                position: None,
                net_flow: 0.0,
            })
            .collect();
        let keys = &venue.exchange().keys;
        self.trader_index = keys.global_trader_index.clone();
        self.trader_buffer = keys.active_trader_buffer.clone();
        if self.trader_index.len() > ARENA_ACCOUNTS_MAX
            || self.trader_buffer.len() > ARENA_ACCOUNTS_MAX
        {
            return Err("the trader arenas span more accounts than the bot reads".into());
        }
        self.clock = Some(Clock::from_session(session)?);
        self.cap = session.cap;
        self.symbol = session.symbol.clone();
        self.venue = Some(venue);
        log(&format!(
            "watching {} informed wallets, right {:.0}% of the time",
            self.watched.len(),
            told.hit_rate * 100.0
        ));
        self.told = Some(told);
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let clock = self.clock.ok_or("step before start")?;
        let (Some(bid), Some(ask)) = (market.best_bid, market.best_ask) else {
            return Ok(());
        };
        assert!(bid > 0.0 && ask >= bid);
        let mid = 0.5 * (bid + ask);
        let start_mid = *self.start_mid.get_or_insert(mid);
        let held = market.position.size;
        if self.account.step(market.mark, held) {
            trade_toward(orders, held, 0.0, mid);
            return Ok(());
        }
        let phase = clock.phase(market.slot);
        if phase != Phase::Hold {
            self.read()?;
        }
        if phase == Phase::Entry {
            let told = self.told.as_ref().ok_or("step before start")?;
            let votes: f64 = self.watched.iter().map(Watched::vote).sum();
            let weekend_pct = (mid / start_mid - 1.0) * 100.0;
            let expected = told.expected_gap_pct(weekend_pct, votes);
            self.target = position_for(expected, self.cap);
        }
        assert!(self.target.abs() <= self.cap);
        trade_toward(orders, held, self.target, mid);
        Ok(())
    }
}

impl Informed {
    /// Read every watched wallet's position, and add what it traded since
    /// the last read to its net flow.
    fn read(&mut self) -> Result<(), String> {
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
                watched.net_flow += position - last;
            }
            watched.position = Some(position);
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

/// What the latent state tells: the informed wallets, as the session
/// resolved its roster, how often they are right, and the reopen's process.
#[derive(Debug, PartialEq)]
struct Told {
    wallets: Vec<Pubkey>,
    hit_rate: f64,
    reopen_keep: f64,
    open_residual_sd_pct: f64,
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
        let number = |field: &str| {
            latent[field]
                .as_f64()
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or(format!("MTX_REGIME has no `{field}`"))
        };
        let hit_rate = number("hit_rate")?;
        if !(0.5..1.0).contains(&hit_rate) {
            return Err(format!("a hit rate of {hit_rate}: 0.5 to under 1"));
        }
        Ok(Self {
            wallets,
            hit_rate,
            reopen_keep: number("reopen_keep")?,
            open_residual_sd_pct: number("open_residual_sd_pct")?,
        })
    }

    /// The gap it expects, in percent: the reopen's process for the prior
    /// (normal, around what the weekend's move says, give or take the
    /// residual), the votes' odds for whether it opens up or down, and the
    /// mean of the prior on that side.
    fn expected_gap_pct(&self, weekend_pct: f64, votes: f64) -> f64 {
        let mean = (self.reopen_keep - 1.0) * weekend_pct;
        let sd = self.open_residual_sd_pct;
        assert!(sd > 0.0 && votes.abs() <= WALLETS_MAX as f64);
        let z = mean / sd;
        let up = normal_cdf(z).clamp(1e-9, 1.0 - 1e-9);
        let density = normal_pdf(z);
        let mean_up = mean + sd * density / up;
        let mean_down = mean - sd * density / (1.0 - up);
        let odds = (self.hit_rate / (1.0 - self.hit_rate)).ln();
        let logit = (up / (1.0 - up)).ln() + votes * odds;
        let posterior_up = 1.0 / (1.0 + (-logit).exp());
        let expected = posterior_up * mean_up + (1.0 - posterior_up) * mean_down;
        assert!(expected.is_finite());
        expected
    }
}

fn normal_pdf(z: f64) -> f64 {
    (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// The standard normal's distribution function, through Abramowitz and
/// Stegun's 7.1.26 for the error function (error under 1.5e-7).
fn normal_cdf(z: f64) -> f64 {
    let x = z.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-x * x).exp();
    if z >= 0.0 {
        0.5 * (1.0 + erf)
    } else {
        0.5 * (1.0 - erf)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);

#[cfg(test)]
mod tests {
    use super::*;
    use reopen::expected_gap_pct;

    fn told() -> Told {
        Told {
            wallets: vec![Pubkey::new_unique()],
            hit_rate: 0.8,
            reopen_keep: 0.78,
            open_residual_sd_pct: 0.5,
        }
    }

    #[test]
    fn the_regime_names_the_informed_wallets_and_the_process() {
        let key = Pubkey::new_unique();
        let parsed = Told::from_regime(&format!(
            r#"{{"wallets": {{"informed": ["{key}"]}}, "hit_rate": 0.8, "reopen_keep": 0.78,
                "open_residual_sd_pct": 0.5, "horizon_slots": 40}}"#
        ))
        .unwrap();
        assert_eq!(parsed.wallets, vec![key]);
        assert_eq!(parsed.hit_rate, 0.8);
        assert!(Told::from_regime("{}").is_err());
        assert!(
            Told::from_regime(&format!(
                r#"{{"wallets": {{"informed": ["{key}"]}}, "hit_rate": 1.0, "reopen_keep": 0.78, "open_residual_sd_pct": 0.5}}"#
            ))
            .is_err()
        );
    }

    #[test]
    fn without_votes_it_expects_what_the_reference_does() {
        let told = told();
        for weekend in [-2.0, -0.5, 0.3, 1.0, 2.5] {
            let expected = told.expected_gap_pct(weekend, 0.0);
            assert!(
                (expected - expected_gap_pct(weekend)).abs() < 1e-6,
                "{weekend}"
            );
        }
    }

    #[test]
    fn votes_move_the_expected_gap_their_way() {
        let told = told();
        let prior = told.expected_gap_pct(1.0, 0.0);
        assert!(prior < 0.0);
        assert!(told.expected_gap_pct(1.0, 2.0) > 0.0);
        assert!(told.expected_gap_pct(1.0, -2.0) < prior);
        assert!(told.expected_gap_pct(1.0, 1.0) > prior);
    }

    #[test]
    fn the_normal_distribution_is_right_to_a_few_decimals() {
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((normal_cdf(1.96) - 0.975).abs() < 1e-4);
        assert!((normal_cdf(-1.0) - 0.158_655).abs() < 1e-5);
    }
}
