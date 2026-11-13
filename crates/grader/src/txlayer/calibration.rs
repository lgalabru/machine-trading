//! The fee market's calibration: one file of numbers fitted to sampled
//! public mainnet blocks, in the style of the market calibrations
//! (`docs/market-calibration.md`).
//!
//! ```text
//! calibration/tx_layer/<id>.toml          the calibration
//! calibration/tx_layer/<id>.inputs.json   every input: url, sha256, bytes, fetch time
//! ```
//!
//! A file is named by its id and never edited: a refit is a new file. The
//! grader carries the shipped files in its binary, so a session needs no
//! path to find them: a scenario names one by id ([`super::TxLayer::calibration`]).
//! The schema is strict: an unknown field fails to load.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The calibration a scenario's layer uses unless it names another.
pub const DEFAULT: &str = "mainnet-fees.20261113.43786520";

/// The calibrations the grader ships, by id.
const BUILTIN: &[(&str, &str)] = &[(
    "mainnet-fees.20261113.43786520",
    include_str!("../../../../calibration/tx_layer/mainnet-fees.20261113.43786520.toml"),
)];
/// A fixture for the tests: round numbers, with a sharp local spike.
#[cfg(test)]
const FIXTURES: &[(&str, &str)] = &[(
    "fixture-fees.20261113.00000000",
    include_str!("testdata/fixture-fees.20261113.00000000.toml"),
)];

/// The regimes, in the order the calibration's arrays index them.
pub const REGIMES: [&str; 3] = ["calm", "busy", "spike"];

/// The account classes the ghost load prices: the block as a whole, and
/// the hot accounts a scenario names.
pub const CLASSES: [&str; 4] = ["block", "amm_pool", "book", "oracle"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    pub meta: Meta,
    pub block: Block,
    pub regimes: Regimes,
    /// Per class, its regimes' parameters.
    pub ghost: BTreeMap<String, GhostClass>,
    pub tips: Tips,
    /// Per class and regime, what was observed: what the synthesised fees
    /// are checked against.
    pub observed: BTreeMap<String, BTreeMap<String, Observed>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub schema_version: String,
    pub calibration_id: String,
    pub network: String,
    pub fitted_at: String,
    pub fitter: String,
    pub fitter_code_sha256_12: String,
    pub inputs_file: String,
    pub inputs_digest: String,
    pub inputs_count: u64,
    pub sources: Vec<String>,
    pub limits: Vec<String>,
    pub windows: Windows,
}

/// The slot ranges each regime's blocks were sampled from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Windows {
    pub calm: Vec<(u64, u64)>,
    pub busy: Vec<(u64, u64)>,
    pub spike: Vec<(u64, u64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Block {
    /// Compute units a block holds, and each writable account in it.
    pub cu_cap: u64,
    pub account_cu_cap: u64,
    pub lamports_per_signature: u64,
    /// Consecutive slots each leader gets.
    pub slots_per_leader: u64,
    pub skip_rate: f64,
    pub skip_rate_source: String,
    /// The stake share running Jito's client: the leaders that take bundles.
    pub jito_stake_share: f64,
    pub jito_stake_share_source: String,
    /// How often two adjacent conflicting transactions land the other way
    /// round: the scheduler isn't a strict sort.
    pub inversion_p: f64,
    pub inversion_p_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Regimes {
    pub names: Vec<String>,
    /// Per slot: row `i` is the chance of moving from regime `i` to each.
    pub transitions: Vec<Vec<f64>>,
    pub share: Vec<f64>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GhostClass {
    pub source: String,
    pub calm: GhostRegime,
    pub busy: GhostRegime,
    pub spike: GhostRegime,
}

impl GhostClass {
    pub fn regime(&self, regime: usize) -> &GhostRegime {
        match regime {
            0 => &self.calm,
            1 => &self.busy,
            _ => &self.spike,
        }
    }
}

/// Background demand for one class in one regime.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GhostRegime {
    /// The chance a slot's demand fills the account: below it, the price
    /// to land is 0.
    pub p_contended: f64,
    /// The clearing price when contended, in micro-lamports per compute
    /// unit: its log's mean and spread, and its AR(1) coefficient slot to
    /// slot.
    pub log_price_mean: f64,
    pub log_price_sd: f64,
    pub phi: f64,
    /// The share of the cap the background takes when contended.
    pub utilization: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tips {
    pub p25: u64,
    pub p50: u64,
    pub p75: u64,
    pub p95: u64,
    pub p99: u64,
    /// A lognormal fit of the tips, in lamports.
    pub log_mean: f64,
    pub log_sd: f64,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Observed {
    /// `(percentile, price)`, zeros included.
    pub quantiles: Vec<(u8, u64)>,
    pub zero_share: f64,
    pub n: u64,
}

impl Calibration {
    /// A shipped calibration, by id.
    pub fn builtin(id: &str) -> Result<Self, String> {
        #[cfg(test)]
        let shipped = BUILTIN.iter().chain(FIXTURES);
        #[cfg(not(test))]
        let shipped = BUILTIN.iter();
        let (_, text) = shipped
            .into_iter()
            .find(|(known, _)| *known == id)
            .ok_or_else(|| {
                let known: Vec<&str> = BUILTIN.iter().map(|(id, _)| *id).collect();
                format!(
                    "no fee calibration {id}; the grader ships {}",
                    known.join(", ")
                )
            })?;
        let calibration = Self::parse(text)?;
        if calibration.meta.calibration_id != id {
            return Err(format!(
                "the file {id} says it is {}",
                calibration.meta.calibration_id
            ));
        }
        Ok(calibration)
    }

    /// Parse and check a calibration's text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let calibration: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        calibration.check()?;
        Ok(calibration)
    }

    fn check(&self) -> Result<(), String> {
        if !self.meta.schema_version.starts_with("1.") {
            return Err(format!("schema {} is not 1.x", self.meta.schema_version));
        }
        if self.regimes.names != REGIMES {
            return Err(format!("regimes {:?}, not {REGIMES:?}", self.regimes.names));
        }
        let rows = &self.regimes.transitions;
        if rows.len() != 3
            || rows
                .iter()
                .any(|row| row.len() != 3 || (row.iter().sum::<f64>() - 1.0).abs() > 1e-6)
        {
            return Err("regime transitions: three rows of three that each sum to 1".into());
        }
        for class in CLASSES {
            let ghost = self
                .ghost
                .get(class)
                .ok_or_else(|| format!("no ghost.{class}"))?;
            for (regime, name) in REGIMES.iter().enumerate() {
                let g = ghost.regime(regime);
                let unit = |x: f64| (0.0..=1.0).contains(&x);
                if !unit(g.p_contended) || !unit(g.utilization) || !(0.0..1.0).contains(&g.phi) {
                    return Err(format!("ghost.{class}.{name}: shares and phi in [0, 1)"));
                }
                if !(g.log_price_sd >= 0.0 && g.log_price_mean.is_finite()) {
                    return Err(format!("ghost.{class}.{name}: a bad price"));
                }
            }
        }
        if let Some(extra) = self.ghost.keys().find(|k| !CLASSES.contains(&k.as_str())) {
            return Err(format!("ghost.{extra}: no such class"));
        }
        let unit = |x: f64| (0.0..=1.0).contains(&x);
        if !unit(self.block.skip_rate)
            || !unit(self.block.jito_stake_share)
            || !unit(self.block.inversion_p)
        {
            return Err("block: skip rate, Jito share and inversions in [0, 1]".into());
        }
        if self.block.cu_cap == 0
            || self.block.account_cu_cap == 0
            || self.block.slots_per_leader == 0
        {
            return Err("block: caps and leader slots above 0".into());
        }
        Ok(())
    }

    /// The class's parameters in `regime`.
    pub fn ghost(&self, class: &str, regime: usize) -> &GhostRegime {
        self.ghost
            .get(class)
            .expect("checked: every class is there")
            .regime(regime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_calibrations_load_by_id() {
        for (id, _) in BUILTIN {
            let calibration = Calibration::builtin(id).unwrap();
            assert_eq!(calibration.meta.calibration_id, *id);
        }
        assert!(Calibration::builtin("nope").is_err());
        assert!(Calibration::builtin(DEFAULT).is_ok());
    }

    #[test]
    fn unknown_fields_and_bad_numbers_fail_to_load() {
        let (_, text) = FIXTURES[0];
        let extra = text.replacen("[block]\n", "[block]\nextra = 1\n", 1);
        assert!(Calibration::parse(&extra).is_err());
        let bad = text.replacen("phi = ", "phi = 1.5 # ", 1);
        assert!(Calibration::parse(&bad).is_err());
    }
}
