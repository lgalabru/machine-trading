//! Market calibration, market model v2 and realism checks.
//!
//! - [`calibration`]: the per-asset files fitted to real market data
//!   (`calibration/market/<asset>.<date>.<hash>.toml`).
//! - [`correlation`]: the cross-asset correlation file `draw_joint` takes its
//!   matrix from, and [`joint`], its realism check.
//! - [`calendar`]: trading calendars (regular hours, extended hours,
//!   overnight, weekends, holidays).
//! - [`model`]: the fair-value process a calibration drives, compiled to the
//!   waypoints sessions replay. Opt-in: a scenario uses it only when drawn
//!   from it, and records which file it came from.
//! - [`realism`]: stylized-fact checks of generated sessions against the
//!   real asset's.
//!
//! "Market calibration" is not `mtx calibrate`, which writes a scenario's
//! reference PnL. See `docs/market-calibration.md`.

pub mod calendar;
pub mod calibration;
pub mod correlation;
pub mod joint;
pub mod model;
pub mod realism;

pub use calibration::Calibration;
pub use correlation::Correlation;
pub use model::{MarketModel, MarketModelRecord, ModelSpec};

#[cfg(test)]
pub(crate) mod tests {
    use mtx_core::mandate::Mandate;

    use super::Calibration;
    use crate::scenario::{Scenario, VENUE};

    const SOL: &str = include_str!("../../../../calibration/market/sol-usd.20261112.0dbc6cdf.toml");
    const SPCX: &str = include_str!("../../../../calibration/market/spcx.20261112.d1f48be2.toml");

    pub(crate) fn sol() -> Calibration {
        with_sha(Calibration::parse(SOL).unwrap(), SOL)
    }

    pub(crate) fn spcx() -> Calibration {
        with_sha(Calibration::parse(SPCX).unwrap(), SPCX)
    }

    fn with_sha(mut calibration: Calibration, text: &str) -> Calibration {
        calibration.sha256 = super::calibration::sha256_hex(text.as_bytes());
        calibration
    }

    /// A perp scenario with no path, for a model to fill.
    pub(crate) fn template() -> Scenario {
        Scenario {
            name: "template".into(),
            description: "drawn from a market calibration".into(),
            symbol: "SPCX".into(),
            duration_slots: 480,
            mandate: Mandate::perp(VENUE, "SPCX", 70.0, 1_000.0),
            path: Vec::new(),
            making: None,
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            actors: None,
            takers_pricing: None,
            turn_data: None,
            order: None,
            participation: None,
            net: None,
            reference_pnl_usd: None,
            informed_pnl_usd: None,
            reference_max_drawdown_usd: None,
            informed_max_drawdown_usd: None,
            fair_path: None,
            market_model: None,
            raw_sends: None,
            markets: Vec::new(),
            joint: None,
            start_positions: Default::default(),
            close_out: Default::default(),
            tx_layer: None,
            stop_out: None,
            oracles: None,
            lending: None,
            warps: Vec::new(),
        }
    }

    #[test]
    fn the_shipped_files_load() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../calibration/market");
        let mut assets = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "toml") {
                let cal = Calibration::load(&path).unwrap();
                assert_eq!(cal.sha256.len(), 64);
                let inputs = dir.join(&cal.meta.inputs_file);
                let inputs: Vec<serde_json::Value> =
                    serde_json::from_str(&std::fs::read_to_string(inputs).unwrap()).unwrap();
                assert_eq!(inputs.len() as u64, cal.meta.inputs_count);
                assert!(
                    inputs
                        .iter()
                        .all(|i| i["sha256"].as_str().is_some_and(|s| s.len() == 64))
                );
                assets.push(cal.meta.asset.clone());
            }
        }
        for asset in ["nvda", "qqq", "sol-usd", "spcx", "spy", "tsla"] {
            assert!(assets.iter().any(|a| a == asset), "no file for {asset}");
        }
    }

    #[test]
    fn the_us_equity_files_draw_jointly() {
        use super::model::draw_joint;

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../calibration/market");
        let models: Vec<super::MarketModel> = ["nvda", "qqq", "spcx", "spy", "tsla"]
            .iter()
            .map(|asset| {
                let prefix = format!("{asset}.");
                let path = std::fs::read_dir(&dir)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.extension().is_some_and(|e| e == "toml")
                            && p.file_name()
                                .unwrap()
                                .to_string_lossy()
                                .starts_with(&prefix)
                    })
                    .unwrap();
                super::MarketModel::new(Calibration::load(&path).unwrap(), Default::default())
                    .unwrap()
            })
            .collect();
        // The matrix is a design parameter here; the point is the shared block.
        let corr: Vec<Vec<f64>> = (0..5)
            .map(|i| (0..5).map(|j| if i == j { 1.0 } else { 0.5 }).collect())
            .collect();
        let refs: Vec<&super::MarketModel> = models.iter().collect();
        let paths = draw_joint(&refs, &corr, 7, 2000).unwrap();
        assert_eq!(paths.len(), 5);
        assert!(
            paths
                .iter()
                .all(|p| p.log_levels.len() == paths[0].log_levels.len())
        );
    }

    #[test]
    fn unknown_fields_and_models_fail() {
        let extra = SOL.replace("[book]\n", "[book]\nmystery = 1\n");
        assert!(Calibration::parse(&extra).unwrap_err().contains("mystery"));
        let model = SOL.replace("model = \"garch11\"", "model = \"egarch\"");
        assert!(Calibration::parse(&model).unwrap_err().contains("egarch"));
        let major = SOL.replace("schema_version = \"1.0\"", "schema_version = \"2.0\"");
        assert!(
            Calibration::parse(&major)
                .unwrap_err()
                .contains("schema_version")
        );
        let df = SOL.replace("df = 4.194", "df = 1.5");
        assert!(Calibration::parse(&df).unwrap_err().contains("df"));
        let gap = SPCX.replace("into = \"regular\"", "into = \"lunch\"");
        assert!(Calibration::parse(&gap).unwrap_err().contains("lunch"));
    }

    #[test]
    fn a_renamed_file_fails() {
        let dir = std::env::temp_dir().join(format!("mtx-market-cal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sol-usd.20261111.00000000.toml");
        std::fs::write(&path, SOL).unwrap();
        let err = Calibration::load(&path).unwrap_err().to_string();
        assert!(err.contains("named by its id"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn accessors() {
        let sol = sol();
        let slot = sol.per_slot_sd(0.25);
        assert!((slot / sol.fair_value.per_second_sd - 0.5).abs() < 1e-12);
        let venue = sol.venue("phoenix_perp").unwrap();
        assert_eq!(venue.funding.interval_seconds, 3600);
        assert!(sol.gap("regular").is_none());
        assert!(spcx().gap("regular").is_some());
    }
}
