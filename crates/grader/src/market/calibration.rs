//! Market-calibration files: one per asset, fitted to real market data, the
//! numbers the market model draws sessions from.
//!
//! A file is TOML, `calibration/market/<calibration_id>.toml`, with its
//! inputs (every downloaded file's URL and SHA-256) beside it in
//! `<calibration_id>.inputs.json`. The id is `<asset>.<date>.<hash>`, the
//! hash over the inputs and the fitter's code, so a refit is a new file: a
//! file is never edited in place. A scenario drawn from one records the id
//! and the file's SHA-256 (see [`super::model::MarketModelRecord`]).
//!
//! Every section names its `model`, and unknown models and fields fail to
//! load. Each fitted section says where its numbers came from; defaults that
//! no data backs yet say so in their `source`. See `docs/market-calibration.md`.

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, error::io_error};

/// The schema's major version. A file with another major version fails to
/// load; minor versions add optional fields only.
pub const SCHEMA_MAJOR: &str = "1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    pub meta: Meta,
    pub fair_value: FairValue,
    pub calendar: Calendar,
    /// Per venue: fees, trade flow, premium and funding. Keyed by venue,
    /// such as `phoenix_perp`.
    #[serde(default)]
    pub venues: BTreeMap<String, Venue>,
    pub book: Book,
    /// What real session-length windows of this asset look like, for
    /// [`super::realism`]. Without it, only the calibration-level checks run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realism: Option<Realism>,
    /// SHA-256 of the file's bytes, set by [`Calibration::load`].
    #[serde(skip)]
    pub sha256: String,
}

/// Where the numbers came from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub schema_version: String,
    pub calibration_id: String,
    /// Such as `sol-usd` or `spcx`.
    pub asset: String,
    /// The market symbol scenarios use, such as `SOL` or `SPCX`.
    pub symbol: String,
    pub fitted_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported_at: Option<String>,
    pub fitter: String,
    pub fitter_code_sha256_12: String,
    /// The earlier fit this file was converted from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
    /// The inputs manifest beside the file.
    pub inputs_file: String,
    /// SHA-256 over the inputs' SHA-256s, truncated.
    pub inputs_digest: String,
    pub inputs_count: u64,
    /// The data sources, in words.
    pub sources: Vec<String>,
    /// What the fit can't vouch for.
    pub limits: Vec<String>,
    /// The date windows each source covers.
    #[serde(default)]
    pub windows: BTreeMap<String, Vec<String>>,
}

/// The fair-value process: volatility level, tails, clustering, regimes and
/// background jumps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FairValue {
    pub model: FairValueModel,
    /// What the fitted price is, such as a CEX mid or a venue's mark.
    pub reference: String,
    /// Standard deviation of one second's log return, in the reference
    /// state: regime multiplier 1, and the calendar's base session.
    pub per_second_sd: f64,
    pub per_second_sd_source: String,
    /// The block the tails and the clustering are fitted at, in seconds.
    pub block_seconds: f64,
    /// For a continuous calendar: volatility by UTC hour, over the day's mean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intraday_vol_mult_utc: Option<Vec<f64>>,
    pub tails: Tails,
    pub clustering: Clustering,
    pub regimes: Regimes,
    pub jumps: Jumps,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FairValueModel {
    /// Regime-switching GARCH(1,1) on blocks, Student-t innovations, with a
    /// Gaussian bridge from blocks to slots (see [`super::model`]).
    RegimeGarchT,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tails {
    pub model: TailModel,
    /// Degrees of freedom of the block innovations, above 2.
    pub df: f64,
    pub df_source: String,
    /// Hill's tail index of 1 m returns, for the record.
    pub hill_alpha_1m: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hill_alpha_1m_ci90: Option<[f64; 2]>,
    /// Excess kurtosis of returns by horizon (`"10s"`, `"60s"`, ...).
    #[serde(default)]
    pub excess_kurtosis_by_horizon: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TailModel {
    StudentT,
}

/// GARCH(1,1) at the block: `h' = (1 - alpha - beta) + alpha z^2 h + beta h`
/// on variance relative to its target.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Clustering {
    pub model: ClusteringModel,
    pub alpha: f64,
    pub beta: f64,
    pub half_life_seconds: f64,
    pub source: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClusteringModel {
    Garch11,
}

/// Volatility states. A session draws its starting state from `share`, and
/// moves between states at `transition_per_hour`. Within a state, each
/// session's level is the state's multiplier times a log-normal draw of
/// `log_sd_within`: sessions differ in volatility as real windows do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Regimes {
    pub model: RegimeModel,
    pub names: Vec<String>,
    pub vol_mult: Vec<f64>,
    pub log_sd_within: Vec<f64>,
    pub share: Vec<f64>,
    /// Row `i`: the chance of being in each state an hour after being in `i`.
    pub transition_per_hour: Vec<Vec<f64>>,
    pub source: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegimeModel {
    Markov,
}

/// Background jumps: Poisson arrivals, log-normal absolute sizes (fitted to
/// the median and the 90th percentile), either sign.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Jumps {
    pub model: JumpModel,
    pub per_hour: f64,
    pub abs_size_median_pct: f64,
    pub abs_size_p90_pct: f64,
    pub rule: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JumpModel {
    PoissonLognormal,
}

/// When the asset trades, and how its volatility changes with the clock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Calendar {
    pub model: CalendarModel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// For `sessions`: the sessions' clock. `America/New_York` (with US
    /// daylight saving) or `UTC`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// For `sessions`: each session kind's hours and volatility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionSpec>,
    /// For `sessions`: the volatility multiplier outside every session
    /// (weekends, holidays).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_vol_mult: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_source: Option<String>,
    /// Dates (`YYYY-MM-DD`, local) with no session starting on them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holidays: Vec<String>,
    /// Dates (`YYYY-MM-DD`) a drawn session start falls between.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draw_range: Option<[String; 2]>,
    /// Price gaps when a session of a kind begins (the open).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<GapSpec>,
    /// The whole move across closed hours, for the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_hours_move: Option<MoveStats>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CalendarModel {
    /// Always open, with an intraday volatility shape.
    Continuous,
    /// Sessions by local time and weekday, closed otherwise.
    Sessions,
}

/// One kind of session, such as `regular` 09:30-16:00 on weekdays. A session
/// whose end is before its start runs past midnight into the next day.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionSpec {
    pub kind: String,
    /// The weekdays it starts on: `mon` ... `sun`.
    pub days: Vec<String>,
    /// `HH:MM`, local.
    pub start: String,
    pub end: String,
    pub vol_mult: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GapSpec {
    /// The session kind whose start gaps.
    pub into: String,
    pub n: u64,
    pub abs_median_pct: f64,
    pub abs_p90_pct: f64,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MoveStats {
    pub n: u64,
    pub abs_median_pct: f64,
    pub abs_p90_pct: f64,
    pub source: String,
}

/// One venue's market for the asset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Venue {
    pub market: String,
    pub taker_fee: f64,
    pub maker_fee: f64,
    pub tick_size: u64,
    pub base_lots_decimals: u32,
    /// Mean trades a minute, and their dispersion index (variance over mean
    /// of per-minute counts: above 1 is bursty).
    pub trades_per_minute: f64,
    pub trades_dispersion_index: f64,
    pub trade_size: SizeDistribution,
    /// Share of the day's trades in each local hour, if fitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_share_by_hour: Option<Vec<f64>>,
    /// Trade price over the mark, in basis points.
    pub premium_bps: Quantiles,
    pub funding: VenueFunding,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SizeDistribution {
    pub model: SizeModel,
    pub median: f64,
    pub sigma: f64,
    pub unit: String,
    pub source: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SizeModel {
    Lognormal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Quantiles {
    pub p10: f64,
    pub median: f64,
    pub p90: f64,
    pub source: String,
}

/// The venue's funding parameters, as its API publishes them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VenueFunding {
    pub interval_seconds: u64,
    pub period_seconds: u64,
    /// In the venue's own unit.
    pub max_rate_per_interval: i64,
    pub max_rate_per_interval_pct: f64,
    pub source: String,
}

/// Spread, depth and impact. Until a book recorder runs, these are mostly
/// defaults, and each `*_source` says which.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Book {
    pub model: BookModel,
    pub half_spread_bps: f64,
    pub half_spread_source: String,
    /// Distances from mid, in basis points, and the depth within each, in
    /// base units.
    pub depth_bps: Vec<f64>,
    pub depth_base: Vec<f64>,
    pub depth_source: String,
    pub impact: Impact,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BookModel {
    /// Stated defaults, partly derived from quotes; not a recorded book.
    Defaults,
    /// Fitted to recorded books.
    Fitted,
}

/// Average cost of a market order of `q` base units: `k_bps * q^gamma`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Impact {
    pub model: ImpactModel,
    pub k_bps: f64,
    pub gamma: f64,
    pub size_unit: String,
    /// The share of impact that stays, and how fast the rest decays.
    pub permanent_share: f64,
    pub decay_half_life_seconds: f64,
    pub source: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImpactModel {
    PowerLaw,
}

/// Real windows of the asset, cut to a session's length, and the bands
/// generated sessions must fall in (see [`super::realism`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Realism {
    pub source: String,
    pub windows: u64,
    /// One bar of a window's returns, in seconds.
    pub bar_seconds: f64,
    pub window_bars: usize,
    /// Bars per aggregated return (tails, predictability).
    pub agg_bars: usize,
    /// Bars per return in the clustering ACF, and its lags in those returns.
    pub cluster_bars: usize,
    pub cluster_lags: Vec<usize>,
    /// The real prices' tick over their level: generated paths are rounded
    /// to it before comparing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_tick_rel: Option<f64>,
    /// The session kind the real windows come from (a `sessions` calendar).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<String>,
    /// Probe round trips: at least this share must lose.
    pub round_trip_min_losing_share: f64,
    pub reference: WindowStats,
    pub bands: Bands,
}

/// Stylized facts of a set of windows, computed the same way for real and
/// generated ones (see [`super::realism::window_stats`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WindowStats {
    /// Median over windows of the per-bar return sd.
    pub vol_level: f64,
    /// Coefficient of variation of the windows' sd: volatility clustering
    /// across sessions.
    pub vol_of_vol: f64,
    /// Excess kurtosis of aggregated returns, each window scaled to unit RMS.
    pub kurtosis_within: f64,
    /// Excess kurtosis of all aggregated returns pooled.
    pub kurtosis_pooled: f64,
    /// Hill's tail index of the pooled aggregated returns (top 2.5%).
    pub tail_index: f64,
    /// ACF of |r| at `cluster_lags`, within windows, each window scaled.
    pub clustering: Vec<f64>,
    /// Mean over windows of the lag-1 ACF of aggregated returns.
    pub predictability: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Bands {
    pub vol_level: Band,
    pub vol_of_vol: Band,
    pub kurtosis_within: Band,
    pub kurtosis_pooled: Band,
    pub tail_index: Band,
    pub clustering: Band,
    pub predictability: Band,
    pub gap_size: Band,
}

/// A pass rule: generated over real within `ratio`, or the two within
/// `diff` of each other. Exactly one is set.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Band {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<[f64; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<f64>,
}

impl Calibration {
    /// Read, parse and validate a calibration file. Its stem must be its
    /// `calibration_id`.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(io_error(path))?;
        let invalid = |reason: String| Error::InvalidCalibration {
            path: path.to_path_buf(),
            reason,
        };
        let text = std::str::from_utf8(&bytes).map_err(|e| invalid(e.to_string()))?;
        let mut calibration = Self::parse(text).map_err(invalid)?;
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".toml"))
            .unwrap_or_default();
        if stem != calibration.meta.calibration_id {
            return Err(invalid(format!(
                "the file is named {stem}, its id is {}: a calibration file is named by its id",
                calibration.meta.calibration_id
            )));
        }
        calibration.sha256 = sha256_hex(&bytes);
        Ok(calibration)
    }

    /// Parse and validate a calibration from TOML. [`Calibration::sha256`]
    /// stays empty.
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let calibration: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        calibration.validate()?;
        Ok(calibration)
    }

    /// Standard deviation of one slot's log return in the reference state.
    pub fn per_slot_sd(&self, slot_seconds: f64) -> f64 {
        self.fair_value.per_second_sd * slot_seconds.sqrt()
    }

    /// A venue by name, such as `phoenix_perp`.
    pub fn venue(&self, name: &str) -> Option<&Venue> {
        self.venues.get(name)
    }

    /// The gap at the start of a session kind, if it gaps.
    pub fn gap(&self, kind: &str) -> Option<&GapSpec> {
        self.calendar.gaps.iter().find(|g| g.into == kind)
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        let meta = &self.meta;
        if meta.schema_version.split('.').next() != Some(SCHEMA_MAJOR) {
            return Err(format!(
                "schema_version {} is not {SCHEMA_MAJOR}.x",
                meta.schema_version
            ));
        }
        if !meta
            .calibration_id
            .strip_prefix(&meta.asset)
            .is_some_and(|rest| rest.starts_with('.') && rest.matches('.').count() == 2)
        {
            return Err(format!(
                "calibration_id {} is not <asset>.<date>.<hash> for asset {}",
                meta.calibration_id, meta.asset
            ));
        }
        if meta.symbol.is_empty() || meta.sources.is_empty() {
            return Err("meta needs a symbol and its sources".into());
        }
        let fv = &self.fair_value;
        positive("fair_value.per_second_sd", fv.per_second_sd)?;
        positive("fair_value.block_seconds", fv.block_seconds)?;
        if let Some(shape) = &fv.intraday_vol_mult_utc {
            if shape.len() != 24 {
                return Err("fair_value.intraday_vol_mult_utc needs 24 hours".into());
            }
            for (i, v) in shape.iter().enumerate() {
                positive(&format!("fair_value.intraday_vol_mult_utc[{i}]"), *v)?;
            }
        }
        if !(fv.tails.df.is_finite() && fv.tails.df > 2.0) {
            return Err(format!(
                "fair_value.tails.df {} must be above 2 (finite variance)",
                fv.tails.df
            ));
        }
        let c = &fv.clustering;
        if !(c.alpha >= 0.0 && c.beta >= 0.0 && c.alpha + c.beta < 1.0) {
            return Err(format!(
                "fair_value.clustering: alpha {} and beta {} must be non-negative with alpha + beta < 1",
                c.alpha, c.beta
            ));
        }
        let r = &fv.regimes;
        let n = r.names.len();
        if n == 0
            || r.vol_mult.len() != n
            || r.log_sd_within.len() != n
            || r.share.len() != n
            || r.transition_per_hour.len() != n
            || r.transition_per_hour.iter().any(|row| row.len() != n)
        {
            return Err("fair_value.regimes: every field needs one entry per state".into());
        }
        for i in 0..n {
            positive("fair_value.regimes.vol_mult", r.vol_mult[i])?;
            non_negative("fair_value.regimes.log_sd_within", r.log_sd_within[i])?;
            non_negative("fair_value.regimes.share", r.share[i])?;
            let row: f64 = r.transition_per_hour[i].iter().sum();
            if (row - 1.0).abs() > 1e-3 || r.transition_per_hour[i].iter().any(|p| *p < 0.0) {
                return Err(format!(
                    "fair_value.regimes.transition_per_hour row {i} must be probabilities summing to 1"
                ));
            }
        }
        if (r.share.iter().sum::<f64>() - 1.0).abs() > 1e-3 {
            return Err("fair_value.regimes.share must sum to 1".into());
        }
        let j = &fv.jumps;
        non_negative("fair_value.jumps.per_hour", j.per_hour)?;
        if j.per_hour > 0.0 {
            positive(
                "fair_value.jumps.abs_size_median_pct",
                j.abs_size_median_pct,
            )?;
            if j.abs_size_p90_pct < j.abs_size_median_pct {
                return Err("fair_value.jumps: the p90 size is below the median".into());
            }
        }
        self.validate_calendar()?;
        for (name, venue) in &self.venues {
            positive(
                &format!("venues.{name}.trades_per_minute"),
                venue.trades_per_minute,
            )?;
            positive(
                &format!("venues.{name}.trade_size.median"),
                venue.trade_size.median,
            )?;
            non_negative(
                &format!("venues.{name}.trade_size.sigma"),
                venue.trade_size.sigma,
            )?;
            non_negative(&format!("venues.{name}.taker_fee"), venue.taker_fee)?;
            if venue.funding.interval_seconds == 0
                || venue.funding.period_seconds < venue.funding.interval_seconds
            {
                return Err(format!(
                    "venues.{name}.funding needs an interval and a period at least as long"
                ));
            }
        }
        let book = &self.book;
        positive("book.half_spread_bps", book.half_spread_bps)?;
        if book.depth_bps.len() != book.depth_base.len() {
            return Err("book: depth_bps and depth_base need one entry each".into());
        }
        positive("book.impact.k_bps", book.impact.k_bps)?;
        positive("book.impact.gamma", book.impact.gamma)?;
        if !(0.0..=1.0).contains(&book.impact.permanent_share) {
            return Err("book.impact.permanent_share is a share, in [0, 1]".into());
        }
        if let Some(realism) = &self.realism {
            realism.validate(&self.calendar)?;
        }
        Ok(())
    }

    fn validate_calendar(&self) -> std::result::Result<(), String> {
        let cal = &self.calendar;
        match cal.model {
            CalendarModel::Continuous => {
                if !cal.sessions.is_empty() || !cal.gaps.is_empty() {
                    return Err("calendar: a continuous calendar has no sessions or gaps".into());
                }
            }
            CalendarModel::Sessions => {
                super::calendar::SessionCalendar::new(cal)?;
                if self.fair_value.intraday_vol_mult_utc.is_some() {
                    return Err(
                        "fair_value.intraday_vol_mult_utc is for a continuous calendar".into(),
                    );
                }
                for gap in &cal.gaps {
                    if !cal.sessions.iter().any(|s| s.kind == gap.into) {
                        return Err(format!("calendar.gaps: no session kind {}", gap.into));
                    }
                    positive("calendar.gaps.abs_median_pct", gap.abs_median_pct)?;
                    if gap.abs_p90_pct < gap.abs_median_pct {
                        return Err("calendar.gaps: the p90 size is below the median".into());
                    }
                }
            }
        }
        Ok(())
    }
}

impl Realism {
    fn validate(&self, calendar: &Calendar) -> std::result::Result<(), String> {
        positive("realism.bar_seconds", self.bar_seconds)?;
        if self.agg_bars == 0 || self.cluster_bars == 0 || self.cluster_lags.is_empty() {
            return Err("realism: agg_bars, cluster_bars and cluster_lags must be set".into());
        }
        if self.window_bars < 2 * self.agg_bars.max(self.cluster_bars) {
            return Err("realism: a window must hold at least two aggregated returns".into());
        }
        if self.reference.clustering.len() != self.cluster_lags.len() {
            return Err("realism.reference.clustering needs one value per cluster lag".into());
        }
        if let Some(kind) = &self.session_kind
            && !calendar.sessions.iter().any(|s| &s.kind == kind)
        {
            return Err(format!(
                "realism.session_kind {kind} is not a calendar session"
            ));
        }
        if !(0.0..=1.0).contains(&self.round_trip_min_losing_share) {
            return Err("realism.round_trip_min_losing_share is a share, in [0, 1]".into());
        }
        let b = &self.bands;
        for (name, band) in [
            ("vol_level", b.vol_level),
            ("vol_of_vol", b.vol_of_vol),
            ("kurtosis_within", b.kurtosis_within),
            ("kurtosis_pooled", b.kurtosis_pooled),
            ("tail_index", b.tail_index),
            ("clustering", b.clustering),
            ("predictability", b.predictability),
            ("gap_size", b.gap_size),
        ] {
            match (band.ratio, band.diff) {
                (Some([lo, hi]), None) if lo > 0.0 && lo <= hi => {}
                (None, Some(d)) if d >= 0.0 => {}
                _ => {
                    return Err(format!(
                        "realism.bands.{name} needs exactly one of ratio = [lo, hi] (0 < lo <= hi) or diff >= 0"
                    ));
                }
            }
        }
        Ok(())
    }
}

fn positive(name: &str, value: f64) -> std::result::Result<(), String> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(format!("{name} must be positive, not {value}"))
    }
}

fn non_negative(name: &str, value: f64) -> std::result::Result<(), String> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(format!("{name} must be non-negative, not {value}"))
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
