//! The jump regime the data supports in an instrument's opening window: the
//! most a stated session spec may state (see
//! `realism::stated_regime_bound`). Read from the hand-maintained
//! `calibration/market/opening-windows.toml`, by the calibration's asset.

use std::{collections::BTreeMap, sync::OnceLock};

use serde::Deserialize;

const FILE: &str = include_str!("../../../../calibration/market/opening-windows.toml");

/// One asset's opening-window measurement.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Opening {
    pub per_hour: f64,
    pub abs_size_median_pct: f64,
    pub abs_size_p90_pct: f64,
    /// The rate an earnings-reaction day (`earnings_day`) may state, where
    /// the data showed one.
    #[serde(default)]
    pub earnings_per_hour: Option<f64>,
    pub measured_from: String,
    pub measured_through: String,
    pub source: String,
}

/// Parse and validate the file's text: every number finite and positive,
/// the p90 at least the median, the earnings rate at least the opening rate.
pub fn parse(text: &str) -> Result<BTreeMap<String, Opening>, String> {
    let all: BTreeMap<String, Opening> = toml::from_str(text).map_err(|e| e.to_string())?;
    for (asset, o) in &all {
        let numbers = [
            ("per_hour", Some(o.per_hour)),
            ("abs_size_median_pct", Some(o.abs_size_median_pct)),
            ("abs_size_p90_pct", Some(o.abs_size_p90_pct)),
            ("earnings_per_hour", o.earnings_per_hour),
        ];
        for (name, value) in numbers {
            if let Some(v) = value
                && !(v.is_finite() && v > 0.0)
            {
                return Err(format!("{asset}.{name} {v} must be positive and finite"));
            }
        }
        if o.abs_size_p90_pct < o.abs_size_median_pct {
            return Err(format!("{asset}: the p90 size is below the median"));
        }
        if o.earnings_per_hour.is_some_and(|e| e < o.per_hour) {
            return Err(format!("{asset}: earnings_per_hour is below per_hour"));
        }
        if o.source.trim().is_empty()
            || o.measured_from.trim().is_empty()
            || o.measured_through.trim().is_empty()
        {
            return Err(format!("{asset}: names no source or measurement window"));
        }
    }
    Ok(all)
}

/// The shipped file's measurement for `asset`, if it has one.
pub fn for_asset(asset: &str) -> Option<&'static Opening> {
    static ALL: OnceLock<BTreeMap<String, Opening>> = OnceLock::new();
    ALL.get_or_init(|| parse(FILE).expect("calibration/market/opening-windows.toml is valid"))
        .get(&asset.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_file_parses_and_names_the_calibrated_assets() {
        let all = parse(FILE).unwrap();
        for asset in ["nvda", "tsla", "spy", "qqq", "spcx"] {
            assert!(all.contains_key(asset), "{asset}");
            assert!(for_asset(asset).is_some());
        }
        assert_eq!(for_asset("TSLA").unwrap().earnings_per_hour, Some(6.7));
        assert!(for_asset("sol-usd").is_none());
    }

    #[test]
    fn non_finite_or_inverted_values_are_refused() {
        let entry = |per_hour: &str, p90: &str, earnings: &str| {
            format!(
                "[x]\nper_hour = {per_hour}\nabs_size_median_pct = 0.3\nabs_size_p90_pct = {p90}\n{earnings}measured_from = \"a\"\nmeasured_through = \"b\"\nsource = \"s\"\n"
            )
        };
        assert!(parse(&entry("2.0", "0.5", "")).is_ok());
        assert!(parse(&entry("nan", "0.5", "")).is_err());
        assert!(parse(&entry("2.0", "nan", "")).is_err());
        assert!(parse(&entry("2.0", "inf", "")).is_err());
        assert!(parse(&entry("2.0", "0.5", "earnings_per_hour = inf\n")).is_err());
        assert!(parse(&entry("2.0", "0.5", "earnings_per_hour = 1.0\n")).is_err());
        assert!(parse(&entry("2.0", "0.2", "")).is_err());
        assert!(parse(&entry("-1.0", "0.5", "")).is_err());
    }

    /// Calibration files cite one another by file SHA-256 (a source line
    /// like `spcx.20261112.d1f48be2 (file SHA-256 ff93578f4af6)`): each cited
    /// SHA must be the one the file it names has now.
    #[test]
    fn every_file_sha_a_calibration_cites_is_the_files_own() {
        use std::{fs, path::Path};
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../calibration/market");
        let mut files: BTreeMap<String, String> = BTreeMap::new();
        let mut texts = Vec::new();
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if let Some(id) = name
                .strip_suffix(".toml")
                .filter(|id| id.matches('.').count() == 2)
            {
                let bytes = fs::read(&path).unwrap();
                files.insert(id.to_string(), super::super::calibration::sha256_hex(&bytes));
                texts.push((name, String::from_utf8(bytes).unwrap()));
            }
        }
        assert!(files.len() >= 5);
        let mut cited = 0;
        for (name, text) in &texts {
            for (at, _) in text.match_indices("(file SHA-256 ") {
                let head = text[..at].trim_end();
                let id = head
                    .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
                    .next()
                    .unwrap();
                let sha: String = text[at + "(file SHA-256 ".len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_hexdigit())
                    .collect();
                let Some(own) = files.get(id) else { continue };
                cited += 1;
                assert!(
                    own.starts_with(&sha) && sha.len() >= 12,
                    "{name} cites {id} as {sha}, but the file's SHA-256 is {own}"
                );
            }
        }
        assert!(cited >= 1, "no cross-references found");
    }
}
