//! Correlation files: the cross-asset correlation of several calibrated
//! assets, fitted to real market data, which [`super::model::draw_joint`]
//! draws their paths from.
//!
//! A file is TOML, `calibration/correlation/<correlation_id>.toml`, with its
//! inputs beside it in `<correlation_id>.inputs.json`. Like a calibration it
//! is named by its id (`<set>.<date>.<hash>`), immutable, and strict: unknown
//! fields fail to load. It pins the calibration files it was fitted through:
//! the matrix means nothing for another fit of an asset, so a joint draw whose
//! calibrations are not the pinned ones fails.
//!
//! The matrix is what `draw_joint` takes (the correlation of the block
//! innovations). It is not the correlation of the generated returns, which is
//! lower because each asset has its own Student-t factor, jumps and volatility
//! level; `realized_target` is what the draw reproduces, as the median
//! 30-minute-window correlation of 1 m returns. See `docs/market-calibration.md`.

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};

use super::{
    calibration::{SCHEMA_MAJOR, sha256_hex},
    model::{MarketModel, ModelPath, draw_joint},
};
use crate::{Error, Result, error::io_error};

/// Fewer real windows than this and a pair's band means little.
const MIN_WINDOWS: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Correlation {
    pub meta: Meta,
    pub correlation: Matrices,
    /// What real windows of the assets look like, for [`super::joint`].
    pub realism: JointRealism,
    /// How often the assets' large moves coincide in the data. Reported only:
    /// the draw's jumps are independent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_cooccurrence: Option<JumpCooccurrence>,
    /// SHA-256 of the file's bytes, set by [`Correlation::load`].
    #[serde(skip)]
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub schema_version: String,
    pub correlation_id: String,
    /// The calibration `asset` ids, in the matrices' order.
    pub assets: Vec<String>,
    /// The calibration files the matrix was fitted through, one per asset.
    pub calibrations: Vec<String>,
    pub fitted_at: String,
    pub fitter: String,
    pub fitter_code_sha256_12: String,
    /// The inputs manifest beside the file.
    pub inputs_file: String,
    pub inputs_digest: String,
    pub inputs_count: u64,
    pub sources: Vec<String>,
    /// What the fit can't vouch for.
    pub limits: Vec<String>,
    #[serde(default)]
    pub windows: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Matrices {
    /// Every asset's `block_seconds`, which a joint draw needs to share.
    pub block_seconds: f64,
    /// What `draw_joint` takes.
    pub matrix: Vec<Vec<f64>>,
    /// The median 30-minute-window 1 m correlation the draw reproduces: the
    /// real one, shrunk toward its average.
    pub realized_target: Vec<Vec<f64>>,
    /// The real one, before shrinking.
    pub realized_sample: Vec<Vec<f64>>,
    /// What the draw gave at `matrix`, in the fitter's own simulation.
    pub simulated_realized: Vec<Vec<f64>>,
    /// The real pooled 1 m and 5 m correlations, and the draw's pooled 1 m
    /// one: reported, because the draw can't reproduce the pooled value.
    pub real_pooled_1m: Vec<Vec<f64>>,
    pub real_pooled_5m: Vec<Vec<f64>>,
    pub simulated_pooled_1m: Vec<Vec<f64>>,
    /// How far `realized_target` was pulled toward the average off-diagonal.
    pub shrinkage: f64,
    pub source: String,
}

/// What the joint check judges for one pair of assets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JointRealism {
    pub source: String,
    pub statistic: String,
    /// How many standard deviations of the difference a band spans.
    pub sigmas: f64,
    pub sigmas_source: String,
    pub draws: usize,
    pub pairs: Vec<PairRealism>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PairRealism {
    pub a: String,
    pub b: String,
    /// The median over windows of the correlation of their 1 m returns.
    pub corr_median: f64,
    /// Its sd over resamples of the real windows (as many as there are).
    pub corr_spread_sd: f64,
    pub window_minutes: usize,
    pub n_windows: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JumpCooccurrence {
    pub rule: String,
    pub pairs: Vec<JumpPair>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JumpPair {
    pub a: String,
    pub b: String,
    pub minutes: u64,
    pub jumps_a: u64,
    pub jumps_b: u64,
    pub both: u64,
    pub expected_if_independent: f64,
    pub same_sign: u64,
}

impl Correlation {
    /// Read, parse and validate a correlation file. Its stem must be its
    /// `correlation_id`.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(io_error(path))?;
        let invalid = |reason: String| Error::InvalidCalibration {
            path: path.to_path_buf(),
            reason,
        };
        let text = std::str::from_utf8(&bytes).map_err(|e| invalid(e.to_string()))?;
        let mut correlation = Self::parse(text).map_err(invalid)?;
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".toml"))
            .unwrap_or_default();
        if stem != correlation.meta.correlation_id {
            return Err(invalid(format!(
                "the file is named {stem}, its id is {}: a correlation file is named by its id",
                correlation.meta.correlation_id
            )));
        }
        correlation.sha256 = sha256_hex(&bytes);
        Ok(correlation)
    }

    /// Parse and validate a correlation from TOML. [`Correlation::sha256`]
    /// stays empty.
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let correlation: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        correlation.validate()?;
        Ok(correlation)
    }

    /// The correlation matrix of `assets` (calibration asset ids, in the
    /// order given): the file's matrix restricted to them. Any subset of a
    /// positive-definite matrix is positive definite.
    pub fn submatrix(&self, assets: &[&str]) -> std::result::Result<Vec<Vec<f64>>, String> {
        let at = self.indices(assets)?;
        Ok(at
            .iter()
            .map(|&i| at.iter().map(|&j| self.correlation.matrix[i][j]).collect())
            .collect())
    }

    /// The matrix for a joint draw of these calibrations, in their order. Each
    /// must be the file's pinned calibration of its asset: a different fit
    /// of the same asset fails.
    pub fn joint_matrix(
        &self,
        calibrations: &[&super::Calibration],
    ) -> std::result::Result<Vec<Vec<f64>>, String> {
        let assets: Vec<&str> = calibrations.iter().map(|c| c.meta.asset.as_str()).collect();
        let at = self.indices(&assets)?;
        for (cal, &i) in calibrations.iter().zip(&at) {
            let pinned = &self.meta.calibrations[i];
            if &cal.meta.calibration_id != pinned {
                return Err(format!(
                    "{} is fitted for {pinned}, not {}",
                    self.meta.correlation_id, cal.meta.calibration_id
                ));
            }
        }
        self.submatrix(&assets)
    }

    /// Draw the models' paths together from this file's matrix
    /// ([`draw_joint`]), after checking that their calibrations are the pinned
    /// ones.
    pub fn draw(
        &self,
        models: &[&MarketModel],
        seed: u64,
        duration_slots: u64,
    ) -> std::result::Result<Vec<ModelPath>, String> {
        let calibrations: Vec<&super::Calibration> =
            models.iter().map(|m| &m.calibration).collect();
        let matrix = self.joint_matrix(&calibrations)?;
        draw_joint(models, &matrix, seed, duration_slots)
    }

    fn indices(&self, assets: &[&str]) -> std::result::Result<Vec<usize>, String> {
        let mut at = Vec::with_capacity(assets.len());
        for asset in assets {
            let i = self
                .meta
                .assets
                .iter()
                .position(|a| a == asset)
                .ok_or_else(|| {
                    format!(
                        "{} has no asset {asset} (it has {:?})",
                        self.meta.correlation_id, self.meta.assets
                    )
                })?;
            if at.contains(&i) {
                return Err(format!("asset {asset} is listed twice"));
            }
            at.push(i);
        }
        if at.is_empty() {
            return Err("a joint draw needs at least one asset".into());
        }
        Ok(at)
    }

    fn validate(&self) -> std::result::Result<(), String> {
        let meta = &self.meta;
        let major = meta.schema_version.split('.').next().unwrap_or_default();
        if major != SCHEMA_MAJOR {
            return Err(format!(
                "schema_version {} is not a {SCHEMA_MAJOR}.x schema",
                meta.schema_version
            ));
        }
        let id: Vec<&str> = meta.correlation_id.split('.').collect();
        if id.len() != 3 || id.iter().any(|p| p.is_empty()) {
            return Err(format!(
                "correlation_id {} is not <set>.<date>.<hash>",
                meta.correlation_id
            ));
        }
        let k = meta.assets.len();
        if k < 2 {
            return Err("a correlation needs at least two assets".into());
        }
        let mut sorted = meta.assets.clone();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != k {
            return Err("assets must be distinct".into());
        }
        if meta.calibrations.len() != k {
            return Err("calibrations needs one id per asset".into());
        }
        for (asset, cal) in meta.assets.iter().zip(&meta.calibrations) {
            if !cal.starts_with(&format!("{asset}.")) {
                return Err(format!(
                    "calibration {cal} is not a calibration of asset {asset}"
                ));
            }
        }
        let c = &self.correlation;
        if !(c.block_seconds.is_finite() && c.block_seconds > 0.0) {
            return Err("correlation.block_seconds must be positive".into());
        }
        if !(0.0..1.0).contains(&c.shrinkage) {
            return Err("correlation.shrinkage is a share, in [0, 1)".into());
        }
        let tables = [
            ("matrix", &c.matrix),
            ("realized_target", &c.realized_target),
            ("realized_sample", &c.realized_sample),
            ("simulated_realized", &c.simulated_realized),
            ("real_pooled_1m", &c.real_pooled_1m),
            ("real_pooled_5m", &c.real_pooled_5m),
            ("simulated_pooled_1m", &c.simulated_pooled_1m),
        ];
        for (name, table) in tables {
            correlation_matrix(name, table, k)?;
        }
        positive_definite(&c.matrix)?;
        let r = &self.realism;
        if !(r.sigmas.is_finite() && r.sigmas > 0.0) || r.draws == 0 {
            return Err("realism needs positive sigmas and draws".into());
        }
        let mut seen = Vec::new();
        for p in &r.pairs {
            let (Some(i), Some(j)) = (
                meta.assets.iter().position(|a| *a == p.a),
                meta.assets.iter().position(|a| *a == p.b),
            ) else {
                return Err(format!(
                    "realism pair {}-{} names an unknown asset",
                    p.a, p.b
                ));
            };
            if i >= j || seen.contains(&(i, j)) {
                return Err(format!(
                    "realism pair {}-{}: each pair once, the assets in file order",
                    p.a, p.b
                ));
            }
            seen.push((i, j));
            if !(p.corr_median.is_finite() && p.corr_median.abs() <= 1.0)
                || !(p.corr_spread_sd.is_finite() && p.corr_spread_sd > 0.0)
                || p.window_minutes == 0
                || p.n_windows < MIN_WINDOWS
            {
                return Err(format!(
                    "realism pair {}-{} needs a correlation in [-1, 1], a positive spread, windows and at least {MIN_WINDOWS} of them",
                    p.a, p.b
                ));
            }
        }
        if seen.len() != k * (k - 1) / 2 {
            return Err("realism.pairs needs every pair of assets".into());
        }
        if let Some(j) = &self.jump_cooccurrence {
            for p in &j.pairs {
                if !meta.assets.contains(&p.a)
                    || !meta.assets.contains(&p.b)
                    || p.both > p.jumps_a.min(p.jumps_b)
                {
                    return Err(format!(
                        "jump_cooccurrence pair {}-{} is inconsistent",
                        p.a, p.b
                    ));
                }
            }
        }
        Ok(())
    }
}

fn correlation_matrix(name: &str, m: &[Vec<f64>], k: usize) -> std::result::Result<(), String> {
    if m.len() != k || m.iter().any(|row| row.len() != k) {
        return Err(format!("correlation.{name} must be {k} by {k}"));
    }
    for (i, row) in m.iter().enumerate() {
        if row[i] != 1.0 {
            return Err(format!("correlation.{name} needs a unit diagonal"));
        }
        for (j, v) in row.iter().enumerate().take(i) {
            if *v != m[j][i] {
                return Err(format!("correlation.{name} must be symmetric"));
            }
            if !(v.is_finite() && v.abs() < 1.0) {
                return Err(format!("correlation.{name} entries must be in (-1, 1)"));
            }
        }
    }
    Ok(())
}

/// Cholesky succeeds with every pivot clear of zero: the matrix `draw_joint`
/// takes must not be barely singular.
fn positive_definite(m: &[Vec<f64>]) -> std::result::Result<(), String> {
    let n = m.len();
    let mut l = vec![vec![0.0f64; n]; n];
    for i in 0..n {
        for j in 0..=i {
            let sum: f64 = (0..j).map(|k| l[i][k] * l[j][k]).sum();
            if i == j {
                let d = m[i][i] - sum;
                if d <= 1e-6 {
                    return Err("correlation.matrix must be positive definite".into());
                }
                l[i][j] = d.sqrt();
            } else {
                l[i][j] = (m[i][j] - sum) / l[j][j];
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::market::Calibration;

    pub(crate) fn dir(kind: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../calibration")
            .join(kind)
    }

    /// The shipped correlation file and its pinned calibrations, in the
    /// file's asset order.
    pub(crate) fn shipped() -> (Correlation, Vec<Calibration>) {
        let path = std::fs::read_dir(dir("correlation"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "toml"))
            .unwrap();
        let correlation = Correlation::load(&path).unwrap();
        let calibrations = correlation
            .meta
            .calibrations
            .iter()
            .map(|id| Calibration::load(&dir("market").join(format!("{id}.toml"))).unwrap())
            .collect();
        (correlation, calibrations)
    }

    fn text() -> String {
        let path = std::fs::read_dir(dir("correlation"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "toml"))
            .unwrap();
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn every_shipped_file_loads_and_its_inputs_match() {
        let mut loaded = 0;
        for entry in std::fs::read_dir(dir("correlation")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            let correlation = Correlation::load(&path).unwrap();
            assert_eq!(correlation.sha256.len(), 64);
            let inputs: Vec<serde_json::Value> = serde_json::from_str(
                &std::fs::read_to_string(dir("correlation").join(&correlation.meta.inputs_file))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(inputs.len() as u64, correlation.meta.inputs_count);
            assert!(
                inputs
                    .iter()
                    .all(|i| i["sha256"].as_str().is_some_and(|s| s.len() == 64))
            );
            // Every pinned calibration is a shipped file with that id, and its asset.
            for (asset, id) in correlation
                .meta
                .assets
                .iter()
                .zip(&correlation.meta.calibrations)
            {
                let cal = Calibration::load(&dir("market").join(format!("{id}.toml"))).unwrap();
                assert_eq!(&cal.meta.asset, asset);
                assert_eq!(
                    cal.fair_value.block_seconds,
                    correlation.correlation.block_seconds
                );
            }
            loaded += 1;
        }
        assert!(loaded >= 1);
    }

    #[test]
    fn unknown_fields_and_bad_matrices_fail() {
        let text = text();
        let extra = text.replace("[correlation]\n", "[correlation]\nmystery = 1\n");
        assert!(Correlation::parse(&extra).unwrap_err().contains("mystery"));
        let major = text.replace("schema_version = \"1.0\"", "schema_version = \"2.0\"");
        assert!(
            Correlation::parse(&major)
                .unwrap_err()
                .contains("schema_version")
        );
        // An entry that makes the matrix asymmetric, then one that makes it
        // indefinite (two assets near-perfectly correlated with a third that
        // disagrees).
        let (c, _) = shipped();
        let mut bad = c.clone();
        bad.correlation.matrix[0][1] += 0.01;
        let asym = toml::to_string(&bad).unwrap();
        assert!(Correlation::parse(&asym).unwrap_err().contains("symmetric"));
        let mut bad = c.clone();
        let k = bad.meta.assets.len();
        for i in 0..k {
            for j in 0..k {
                if i != j {
                    bad.correlation.matrix[i][j] = 0.0;
                }
            }
        }
        for (i, j, v) in [(0, 1, 0.95), (0, 2, 0.95), (1, 2, -0.95)] {
            bad.correlation.matrix[i][j] = v;
            bad.correlation.matrix[j][i] = v;
        }
        let indefinite = toml::to_string(&bad).unwrap();
        assert!(
            Correlation::parse(&indefinite)
                .unwrap_err()
                .contains("positive definite")
        );
        let mut bad = c;
        bad.correlation.matrix[2][2] = 0.9;
        let diagonal = toml::to_string(&bad).unwrap();
        assert!(
            Correlation::parse(&diagonal)
                .unwrap_err()
                .contains("unit diagonal")
        );
    }

    #[test]
    fn a_renamed_file_fails() {
        let dir = std::env::temp_dir().join(format!("mtx-correlation-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("us-eq.20261111.00000000.toml");
        std::fs::write(&path, text()).unwrap();
        let err = Correlation::load(&path).unwrap_err().to_string();
        assert!(err.contains("named by its id"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn submatrices_follow_the_order_asked() {
        let (c, _) = shipped();
        let m = c.submatrix(&["spy", "qqq"]).unwrap();
        let spy = c.meta.assets.iter().position(|a| a == "spy").unwrap();
        let qqq = c.meta.assets.iter().position(|a| a == "qqq").unwrap();
        assert_eq!(
            m,
            vec![
                vec![1.0, c.correlation.matrix[spy][qqq]],
                vec![c.correlation.matrix[qqq][spy], 1.0]
            ]
        );
        assert!(c.submatrix(&["spy", "spy"]).unwrap_err().contains("twice"));
        assert!(
            c.submatrix(&["spy", "sol-usd"])
                .unwrap_err()
                .contains("no asset")
        );
        assert!(c.submatrix(&[]).is_err());
    }

    #[test]
    fn calibrations_other_than_the_pinned_ones_fail() {
        let (c, cals) = shipped();
        let pick = |asset: &str| cals.iter().find(|c| c.meta.asset == asset).unwrap();
        c.joint_matrix(&[pick("spy"), pick("qqq")]).unwrap();
        let mut refit = pick("spy").clone();
        refit.meta.calibration_id = "spy.20261201.deadbeef".into();
        let err = c.joint_matrix(&[&refit, pick("qqq")]).unwrap_err();
        assert!(err.contains("is fitted for"), "{err}");
    }

    #[test]
    fn the_matrix_draws_through_the_pinned_models() {
        use crate::market::{MarketModel, ModelSpec};
        let (c, cals) = shipped();
        let models: Vec<MarketModel> = cals
            .into_iter()
            .map(|cal| MarketModel::new(cal, ModelSpec::default()).unwrap())
            .collect();
        let refs: Vec<&MarketModel> = models.iter().collect();
        let paths = c.draw(&refs, 7, 2000).unwrap();
        assert_eq!(paths.len(), models.len());
        // The same draw twice is the same paths.
        let again = c.draw(&refs, 7, 2000).unwrap();
        assert_eq!(paths[0].log_levels, again[0].log_levels);
    }
}
