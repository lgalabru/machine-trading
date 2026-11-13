//! The transaction layer: how a session's transactions land, when a
//! scenario turns it on (`tx_layer`, see `docs/tx-layer.md`).
//!
//! Without it, the bot's guard forwards a send at once and the surfnet runs
//! it on receipt. With it, nothing the bot sends runs on receipt. **Collect,
//! then build:**
//!
//! 1. The bot's sends go into one [`pool::Pool`]: a process bot's through
//!    its guard (`sendTransaction`, and the mock Jito endpoint under
//!    `/api/v1/`), a component's through the grader, its sheet's lines and
//!    raw sends with their transaction options applied ([`client`]).
//! 2. At each slot, a seeded [`builder`] decides what lands and in what
//!    order: the leader of the slot ([`leader`]), whether it skipped, each
//!    entry's route latency and forwarding loss, its blockhash's expiry, the
//!    background demand's clearing price on the block and on each hot
//!    account ([`ghost`]), what the block and each account still hold, and
//!    for a Jito leader the bundles' tips.
//! 3. The session commits the block to the surfnet in that order, one
//!    transaction (or bundle) at a time, each awaited, then produces the
//!    slot. What landed is recorded ([`report`]): the slot's ordered block,
//!    each entry's fee and tip, and what the bot paid ([`fees`]).
//!
//! Everything the builder decides is a pure function of the scenario, its
//! seed, and what the bot sent and when, counted in steps and sequence
//! numbers, never wall time; ties are broken by a seeded hash of the
//! sender and its sequence number, never by a transaction's bytes.

pub mod builder;
pub mod calibration;
pub mod client;
pub mod engine;
pub mod fees;
pub mod ghost;
pub mod jito;
pub mod leader;
#[cfg(test)]
mod live;
pub mod pool;
pub mod report;

pub use engine::TxEngine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A scenario's transaction layer. Everything but the seed and the
/// calibration has a default.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TxLayer {
    /// What the layer's draws come from: the background demand, the
    /// leaders, latencies, losses and ties.
    pub seed: u64,
    /// The fee calibration's id ([`calibration::Calibration::builtin`]).
    pub calibration: String,
    /// The accounts background demand contends for, each with the class
    /// whose calibration prices it: the market, the pool, the oracle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hot_accounts: Vec<HotAccount>,
    /// Windows of steps pinned to a regime. Elsewhere the regime is drawn
    /// from the calibration's switching, if `drawn_regimes`, or calm.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regimes: Vec<RegimeWindow>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub drawn_regimes: bool,
    /// The routes a send can take, and what each costs in latency and loss.
    #[serde(default)]
    pub routes: Routes,
    /// The synthetic validators the leader schedule is drawn over.
    #[serde(default = "default_validators")]
    pub validators: u32,
    /// The most entries a bot may have pending at once; past it a send is
    /// refused as a rate-limited node would (`-32429`).
    #[serde(default = "default_max_pending")]
    pub max_pending: usize,
    /// Value the wallet's native SOL in the account, so fees and tips count
    /// in PnL.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fees_in_pnl: bool,
    /// The SOL the bot's wallet starts with, in place of the session's
    /// usual grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_sol: Option<f64>,
    /// What a SOL is worth in the account's currency, for `fees_in_pnl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sol_usd: Option<f64>,
}

fn default_validators() -> u32 {
    24
}

fn default_max_pending() -> usize {
    64
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HotAccount {
    /// The account, base58.
    pub address: String,
    /// `amm_pool`, `book` or `oracle`.
    pub class: String,
}

/// A window of steps, `from_step` up to but not including `to_step`, in one
/// regime: `calm`, `busy` or `spike`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RegimeWindow {
    pub from_step: u64,
    pub to_step: u64,
    pub regime: String,
}

/// The routes a send can take. `rpc` is the public node every send takes by
/// default; `staked` is a staked connection, open to a bot only where the
/// scenario lists it; `jito` is the block engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Routes {
    #[serde(default = "Route::rpc")]
    pub rpc: Route,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staked: Option<Route>,
    #[serde(default = "Route::jito")]
    pub jito: Route,
}

impl Default for Routes {
    fn default() -> Self {
        Self {
            rpc: Route::rpc(),
            staked: None,
            jito: Route::jito(),
        }
    }
}

/// One route: its latency in slots, uniform in the range, and the chance a
/// delivery is lost, which rises with how full the block is when the send
/// is priced below what clears it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub latency_slots: (u64, u64),
    pub loss: f64,
    pub loss_congested: f64,
    /// What a send on it costs, in lamports, beyond its fees: a staked
    /// connection's price. Counted in the fee ledger, not on chain.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fee_lamports: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Route {
    pub fn rpc() -> Self {
        Self {
            latency_slots: (0, 2),
            loss: 0.01,
            loss_congested: 0.3,
            fee_lamports: 0,
        }
    }

    pub fn staked() -> Self {
        Self {
            latency_slots: (0, 1),
            loss: 0.002,
            loss_congested: 0.05,
            fee_lamports: 0,
        }
    }

    pub fn jito() -> Self {
        Self {
            latency_slots: (0, 1),
            loss: 0.0,
            loss_congested: 0.0,
            fee_lamports: 0,
        }
    }
}

/// Which route an entry took.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    Rpc,
    Staked,
    Jito,
}

impl TxLayer {
    /// A layer must make sense for a session of `steps` steps.
    pub fn validate(&self, steps: u64) -> Result<(), String> {
        let calibration = calibration::Calibration::builtin(&self.calibration)?;
        let _ = calibration;
        for hot in &self.hot_accounts {
            hot.address
                .parse::<solana_pubkey::Pubkey>()
                .map_err(|_| format!("hot account {} is not a base58 address", hot.address))?;
            if !["amm_pool", "book", "oracle"].contains(&hot.class.as_str()) {
                return Err(format!(
                    "hot account {}: class {} is not amm_pool, book or oracle",
                    hot.address, hot.class
                ));
            }
        }
        for window in &self.regimes {
            regime_index(&window.regime)?;
            if window.from_step >= window.to_step || window.to_step > steps {
                return Err(format!(
                    "regime window {}..{} is empty or past the session's {steps} steps",
                    window.from_step, window.to_step
                ));
            }
        }
        let routes = [
            Some(&self.routes.rpc),
            self.routes.staked.as_ref(),
            Some(&self.routes.jito),
        ];
        for route in routes.into_iter().flatten() {
            let (lo, hi) = route.latency_slots;
            if lo > hi || hi > 150 {
                return Err(format!("route latency {lo}..{hi} slots"));
            }
            if !(0.0..=1.0).contains(&route.loss) || !(0.0..=1.0).contains(&route.loss_congested) {
                return Err("route losses are in [0, 1]".into());
            }
        }
        if self.validators == 0 || self.validators > 4096 {
            return Err(format!("{} validators", self.validators));
        }
        if self.max_pending == 0 {
            return Err("max_pending is 0".into());
        }
        if self.fees_in_pnl && !self.sol_usd.is_some_and(|p| p.is_finite() && p > 0.0) {
            return Err("fees_in_pnl needs sol_usd, a positive price".into());
        }
        if let Some(sol) = self.native_sol
            && !(sol.is_finite() && sol > 0.0 && sol < 1e6)
        {
            return Err(format!("native_sol {sol}"));
        }
        Ok(())
    }
}

/// Lamports a tip account holds: rent-exempt for its 8 bytes.
const TIP_ACCOUNT_LAMPORTS: u64 = 946_560;

/// Ready the world for the layer, while setup's cheatcodes still work: the
/// bot's wallet holds `native_sol` if the scenario sets it, and Jito's tip
/// accounts exist as they do on mainnet, so a tip lands as a transfer.
pub fn setup(
    surfnet: &surfpool_sdk::Surfnet,
    layer: &TxLayer,
    bot: &solana_pubkey::Pubkey,
) -> crate::Result<()> {
    let cheats = surfnet.cheatcodes();
    if let Some(sol) = layer.native_sol {
        cheats.fund_sol(bot, (sol * 1e9).round() as u64)?;
    }
    for account in jito::TIP_ACCOUNTS {
        cheats.set_account(
            &account.parse().expect("a tip account"),
            TIP_ACCOUNT_LAMPORTS,
            &jito::TIP_ACCOUNT_DATA,
            &jito::TIP_PAYMENT_PROGRAM,
        )?;
    }
    Ok(())
}

/// A regime's index in [`calibration::REGIMES`].
pub fn regime_index(name: &str) -> Result<usize, String> {
    calibration::REGIMES
        .iter()
        .position(|r| *r == name)
        .ok_or_else(|| format!("regime {name} is not calm, busy or spike"))
}

/// A uniform draw in `[0, 1)`: a pure function of the seed, a tag for what
/// it decides, and the numbers it decides it for.
pub fn draw(seed: u64, tag: &str, keys: &[u64]) -> f64 {
    (hash(seed, tag, keys, &[]) >> 11) as f64 / (1u64 << 53) as f64
}

/// A standard normal draw, by Box-Muller on two uniform ones.
pub fn normal(seed: u64, tag: &str, keys: &[u64]) -> f64 {
    let mut a = keys.to_vec();
    a.push(0);
    let u1 = draw(seed, tag, &a).max(1e-300);
    *a.last_mut().expect("pushed") = 1;
    let u2 = draw(seed, tag, &a);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// 64 bits of SHA-256 over the seed, a tag, numbers and bytes: what ties
/// are broken by. Callers pass a sender's id as bytes, never a
/// transaction's.
pub fn hash(seed: u64, tag: &str, keys: &[u64], bytes: &[u8]) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(seed.to_le_bytes());
    hasher.update((tag.len() as u64).to_le_bytes());
    hasher.update(tag.as_bytes());
    for key in keys {
        hasher.update(key.to_le_bytes());
    }
    hasher.update(bytes);
    let digest = hasher.finalize();
    u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn layer() -> TxLayer {
        TxLayer {
            seed: 7,
            calibration: "fixture-fees.20261113.00000000".into(),
            hot_accounts: vec![HotAccount {
                address: "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE".into(),
                class: "amm_pool".into(),
            }],
            regimes: vec![RegimeWindow {
                from_step: 10,
                to_step: 20,
                regime: "spike".into(),
            }],
            drawn_regimes: false,
            routes: Routes::default(),
            validators: 24,
            max_pending: 64,
            fees_in_pnl: true,
            native_sol: Some(1.0),
            sol_usd: Some(150.0),
        }
    }

    #[test]
    fn a_layer_that_makes_no_sense_is_refused() {
        assert_eq!(layer().validate(40), Ok(()));
        assert!(layer().validate(15).is_err(), "window past the session");
        let mut bad = layer();
        bad.hot_accounts[0].class = "vault".into();
        assert!(bad.validate(40).is_err());
        let mut bad = layer();
        bad.sol_usd = None;
        assert!(bad.validate(40).is_err());
        let mut bad = layer();
        bad.calibration = "none".into();
        assert!(bad.validate(40).is_err());
        let json = serde_json::to_string(&layer()).unwrap();
        assert_eq!(serde_json::from_str::<TxLayer>(&json).unwrap(), layer());
        assert!(
            serde_json::from_str::<TxLayer>(r#"{"seed":1,"calibration":"x","nope":2}"#).is_err()
        );
    }

    #[test]
    fn draws_are_pure_and_spread() {
        assert_eq!(draw(1, "a", &[2, 3]), draw(1, "a", &[2, 3]));
        assert_ne!(draw(1, "a", &[2, 3]), draw(1, "b", &[2, 3]));
        assert_ne!(draw(1, "a", &[2, 3]), draw(2, "a", &[2, 3]));
        let mean: f64 = (0..10_000).map(|i| draw(9, "u", &[i])).sum::<f64>() / 10_000.0;
        assert!((mean - 0.5).abs() < 0.02, "{mean}");
        let z: Vec<f64> = (0..10_000).map(|i| normal(9, "z", &[i])).collect();
        let m = z.iter().sum::<f64>() / z.len() as f64;
        let v = z.iter().map(|x| (x - m).powi(2)).sum::<f64>() / z.len() as f64;
        assert!(m.abs() < 0.05 && (v - 1.0).abs() < 0.05, "{m} {v}");
    }
}
