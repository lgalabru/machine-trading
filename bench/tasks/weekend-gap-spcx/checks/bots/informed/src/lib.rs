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
//! on which way the reopen goes, right with the told hit rate; a wallet
//! still silent is evidence too, since it trades only while the gap it sees
//! is past its edge (told, with its view's noise and its pace). It weighs a
//! grid of gaps by the prior and every wallet's record, takes the
//! posterior's mean, and trades it with the reference's mechanics
//! (`reopen.rs`, the same file).

#[path = "../../../../reference/src/reopen.rs"]
pub mod reopen;

use mtx_kit::{Pubkey, log, prelude::*, serde_json, venue::PhoenixVenue};
use reopen::{
    ACCOUNTS_MAX, Account, Clock, Phase, Positions, normal_cdf, position_for, trade_toward,
};

/// The most informed wallets it watches: the family seats two.
const WALLETS_MAX: usize = 8;
const _: () = assert!(WALLETS_MAX <= ACCOUNTS_MAX);
/// A position change smaller than this, in SPCX, is no trade: under
/// Phoenix's lot (0.001 SPCX).
const FLOW_MIN: f64 = 0.0005;

/// One informed wallet: its trader account, its position at the last read
/// and its net flow since it started.
#[derive(Debug, Clone)]
struct Watched {
    authority: Pubkey,
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
    positions: Positions,
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
                authority: *authority,
                position: None,
                net_flow: 0.0,
            })
            .collect();
        self.positions = Positions::new(&venue)?;
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
            let votes: Vec<f64> = self.watched.iter().map(Watched::vote).collect();
            let weekend_pct = (mid / start_mid - 1.0) * 100.0;
            let stretch_from = clock.reopen_slot as f64 - told.horizon_slots;
            let elapsed = (market.slot as f64 - stretch_from).max(0.0);
            let expected = told.expected_gap_pct(weekend_pct, &votes, elapsed);
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
        let authorities: Vec<Pubkey> = self.watched.iter().map(|w| w.authority).collect();
        let positions = self.positions.read(venue, &self.symbol, &authorities)?;
        for (watched, position) in self.watched.iter_mut().zip(positions) {
            if let Some(last) = watched.position {
                watched.net_flow += position - last;
            }
            watched.position = Some(position);
        }
        Ok(())
    }
}

/// What the latent state tells: the informed wallets, as the session
/// resolved its roster, how often they are right, and the reopen's process.
#[derive(Debug, PartialEq)]
struct Told {
    wallets: Vec<Pubkey>,
    hit_rate: f64,
    reopen_keep: f64,
    open_residual_sd_pct: f64,
    /// When an informed wallet trades at all: from `horizon_slots` before
    /// the reopen, while the gap it sees (off by a normal `view_noise_bps`)
    /// is past `edge_bps`, about `orders_per_wallet` orders over the stretch.
    horizon_slots: f64,
    edge_bps: f64,
    view_noise_bps: f64,
    orders_per_wallet: f64,
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
            horizon_slots: number("horizon_slots")?,
            edge_bps: number("edge_bps")?,
            view_noise_bps: number("view_noise_bps")?,
            orders_per_wallet: number("orders_per_wallet")?,
        })
    }

    /// The gap it expects, in percent: the posterior mean over a grid of
    /// gaps, from the reopen's prior (normal, around `(keep - 1) × weekend`,
    /// give or take the residual) and each informed wallet's record so far
    /// (`votes`: +1 bought, -1 sold, 0 silent), `elapsed_slots` into its
    /// stretch. A wallet trades only while the gap it sees is past its edge,
    /// so silence is evidence too: of a gap too small to trade, or of no
    /// order drawn yet.
    fn expected_gap_pct(&self, weekend_pct: f64, votes: &[f64], elapsed_slots: f64) -> f64 {
        assert!(votes.len() <= WALLETS_MAX && elapsed_slots >= 0.0);
        let mean = (self.reopen_keep - 1.0) * weekend_pct;
        let sd = self.open_residual_sd_pct;
        let elapsed = elapsed_slots.min(self.horizon_slots);
        let none_drawn = (-self.orders_per_wallet * elapsed / self.horizon_slots).exp();
        let (mut weight_sum, mut gap_sum) = (0.0_f64, 0.0_f64);
        for k in 0..=GRID_POINTS {
            let z = -GRID_SDS + 2.0 * GRID_SDS * k as f64 / GRID_POINTS as f64;
            let gap_pct = mean + sd * z;
            let mut weight = (-0.5 * z * z).exp();
            let gap_bps = gap_pct * 100.0;
            let up = normal_cdf((gap_bps - self.edge_bps) / self.view_noise_bps);
            let down = normal_cdf((-gap_bps - self.edge_bps) / self.view_noise_bps);
            for vote in votes {
                weight *= if *vote > 0.0 {
                    (1.0 - none_drawn) * (up * self.hit_rate + down * (1.0 - self.hit_rate))
                } else if *vote < 0.0 {
                    (1.0 - none_drawn) * (down * self.hit_rate + up * (1.0 - self.hit_rate))
                } else {
                    (1.0 - up - down) + (up + down) * none_drawn
                };
            }
            weight_sum += weight;
            gap_sum += weight * gap_pct;
        }
        assert!(weight_sum > 0.0);
        let expected = gap_sum / weight_sum;
        assert!(expected.is_finite());
        expected
    }
}

/// The posterior's grid: this many intervals over this many standard
/// deviations of the prior either side.
const GRID_POINTS: u32 = 240;
const GRID_SDS: f64 = 6.0;

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
            horizon_slots: 40.0,
            edge_bps: 10.0,
            view_noise_bps: 10.0,
            orders_per_wallet: 3.0,
        }
    }

    #[test]
    fn the_regime_names_the_informed_wallets_and_the_process() {
        let key = Pubkey::new_unique();
        let parsed = Told::from_regime(&format!(
            r#"{{"wallets": {{"informed": ["{key}"]}}, "hit_rate": 0.8, "reopen_keep": 0.78,
                "open_residual_sd_pct": 0.5, "horizon_slots": 40,
                "edge_bps": 10.0, "view_noise_bps": 10.0, "orders_per_wallet": 3.0}}"#
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
    fn with_no_record_yet_it_expects_what_the_reference_does() {
        let told = told();
        for weekend in [-2.0, -0.5, 0.3, 1.0, 2.5] {
            // Before the stretch, no wallet has had a chance to trade.
            let expected = told.expected_gap_pct(weekend, &[0.0, 0.0], 0.0);
            assert!(
                (expected - expected_gap_pct(weekend)).abs() < 1e-3,
                "{weekend}"
            );
        }
    }

    #[test]
    fn votes_move_the_expected_gap_their_way() {
        let told = told();
        let prior = told.expected_gap_pct(1.0, &[0.0, 0.0], 0.0);
        assert!(prior < 0.0);
        assert!(told.expected_gap_pct(1.0, &[1.0, 1.0], 32.0) > 0.0);
        assert!(told.expected_gap_pct(1.0, &[-1.0, -1.0], 32.0) < prior);
        assert!(told.expected_gap_pct(1.0, &[1.0, 0.0], 32.0) > prior);
    }

    /// Both informed wallets silent most of the way through their stretch:
    /// the gap is most likely under their edge, so it expects little.
    #[test]
    fn silence_late_in_the_stretch_says_the_gap_is_small() {
        let told = told();
        let prior = told.expected_gap_pct(2.0, &[0.0, 0.0], 0.0);
        let silent = told.expected_gap_pct(2.0, &[0.0, 0.0], 32.0);
        assert!(prior < -0.4, "{prior}");
        assert!(silent.abs() < 0.15, "{silent}");
    }
}
