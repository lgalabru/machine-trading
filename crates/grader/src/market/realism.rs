//! Realism checks: stylized facts of generated sessions against the real
//! asset's, as its calibration file records them.
//!
//! Each statistic is computed the same way, on windows of the same length,
//! for the real data (by the fitter, into the file's `[realism]`) and for the
//! generated sessions (here). Short-window estimators are biased, kurtosis
//! and autocorrelations especially; comparing like with like cancels the
//! bias. A single session is never judged alone.
//!
//! The checks:
//! - volatility level, and how much it varies between sessions;
//! - fat tails: kurtosis within sessions, and the tail ratio (q99 / q90 of
//!   |r|) pooled; pooled kurtosis and Hill's tail index are reported, not
//!   gated, being too noisy on a few hundred windows;
//! - volatility clustering within a session (ACF of |r|);
//! - no linear predictability (lag-1 ACF);
//! - gaps where the calendar opens a session, against the fitted gap sizes;
//! - probe round trips through the book lose spread, impact and fees.
//!
//! Each band is the file's, widened to the statistic's sampling spread at
//! the number of windows judged, so a fair generator passes whatever its
//! seed. Windows that hold a gap are left out of the return checks; the
//! gaps are checked on their own. A report whose return checks didn't run
//! is [`Verdict::NotJudged`], never a pass. [`window_stats`] is the port of the fitter's
//! `window_stats` (infra-calib `calib/export_v1.py`): keep the two in step.

use serde::{Deserialize, Serialize};

use super::{
    calibration::{
        Band, Calibration, DiffSpread, LogSpread, LongClustering, Realism, Spread, WindowStats,
    },
    model::{EventKind, MarketModel, ModelPath, ModelSpec, Start},
};
use crate::{
    making::{book_at, level_at},
    scenario::{SLOT_SECONDS, Scenario, SplitMix64},
};

/// Fewer windows than this and the return checks are skipped: the
/// statistics are too noisy to judge.
pub const MIN_WINDOWS: usize = 30;
/// What a pass needs judged, with power: the volatility level, its spread
/// across sessions, and fat tails (either tail check).
pub const REQUIRED: [&[&str]; 3] = [
    &["vol_level"],
    &["vol_of_vol"],
    &["tail_ratio", "kurtosis_within"],
];
/// Fewer gaps than this and the gap check is skipped.
pub const MIN_GAPS: usize = 20;

/// One session's path, as the checks read it.
#[derive(Debug, Clone)]
pub struct SessionPath {
    /// Log price relative to the start at each slot.
    pub log_levels: Vec<f64>,
    /// Slots of events left out of the return checks (gaps).
    pub masked_slots: Vec<u64>,
    /// Gap sizes, in percent.
    pub gaps_pct: Vec<f64>,
}

impl SessionPath {
    /// A scenario's oracle level at every slot. A scenario drawn from a
    /// market model has its gaps masked.
    pub fn from_scenario(scenario: &Scenario) -> Self {
        let log_levels = (0..=scenario.duration_slots)
            .map(|slot| (1.0 + level_at(&scenario.path, slot) / 100.0).ln())
            .collect();
        let gaps: Vec<_> = scenario
            .market_model
            .iter()
            .flat_map(|m| &m.events)
            .filter(|e| e.kind == EventKind::Gap)
            .collect();
        Self {
            log_levels,
            masked_slots: gaps.iter().map(|e| e.slot).collect(),
            gaps_pct: gaps.iter().map(|e| e.pct).collect(),
        }
    }

    pub fn from_model(path: &ModelPath) -> Self {
        let gaps: Vec<_> = path
            .record
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Gap)
            .collect();
        Self {
            log_levels: path.log_levels.clone(),
            masked_slots: gaps.iter().map(|e| e.slot).collect(),
            gaps_pct: gaps.iter().map(|e| e.pct).collect(),
        }
    }
}

/// One probe round trip: open, then close the same size one slot later, in
/// units of the starting price.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RoundTrip {
    pub size: f64,
    pub open_px: f64,
    pub close_px: f64,
    /// `true` for buy-then-sell.
    pub buy: bool,
    pub fees: f64,
}

impl RoundTrip {
    pub fn pnl(&self) -> f64 {
        let sign = if self.buy { 1.0 } else { -1.0 };
        sign * (self.close_px - self.open_px) * self.size - self.fees
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    /// Not enough data to judge, or nothing to judge it on.
    Skipped,
    /// Reported for the record; never fails.
    Info,
    /// Within its band, but so is the obviously wrong market's value: the
    /// check can't tell them apart here, so it judges nothing.
    NoPower,
}

/// A report's outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every check that ran passed, and the return checks ran.
    Pass,
    /// A check failed.
    Fail,
    /// Nothing failed, but the market wasn't judged: the return checks
    /// didn't run (too few windows, or no `[realism]`), or a required one
    /// (vol level, vol of vol, a tail check) had no power. Not a pass.
    NotJudged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Check {
    pub name: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub band: Option<Band>,
    pub detail: String,
}

impl Check {
    fn skipped(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Skipped,
            real: None,
            generated: None,
            band: None,
            detail: detail.into(),
        }
    }
}

/// What `mtx market-cal check` prints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub calibration: String,
    pub calibration_sha256: String,
    pub target: String,
    pub sessions: usize,
    /// Windows the return checks used, and those left out for holding a gap.
    pub windows: usize,
    pub masked_windows: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated: Option<WindowStats>,
    pub checks: Vec<Check>,
    pub verdict: Verdict,
    /// The verdict is [`Verdict::Pass`].
    pub passed: bool,
}

impl Report {
    /// Set the verdict from the checks: a failure fails; otherwise the
    /// report passes only if the return checks ran and each of
    /// [`REQUIRED`] has a check that passed with power.
    fn decide(&mut self) {
        let judged = |names: &[&str]| {
            self.checks
                .iter()
                .any(|c| c.status == Status::Pass && names.contains(&c.name.as_str()))
        };
        self.verdict = if self.checks.iter().any(|c| c.status == Status::Fail) {
            Verdict::Fail
        } else if self.generated.is_none() || !REQUIRED.iter().all(|group| judged(group)) {
            Verdict::NotJudged
        } else {
            Verdict::Pass
        };
        self.passed = self.verdict == Verdict::Pass;
    }
}

/// The per-bar returns of each window of `realism.window_bars` bars, from
/// slot-level paths, rounded to the reference's tick. Windows holding a
/// masked slot are dropped; returns the windows and how many were dropped.
pub fn windows(
    sessions: &[SessionPath],
    realism: &Realism,
    slot_seconds: f64,
) -> (Vec<Vec<f64>>, usize) {
    cut(
        sessions,
        realism.bar_seconds,
        realism.window_bars,
        realism.price_tick_rel,
        slot_seconds,
    )
}

/// Windows of `window_bars` returns over `bar_seconds` bars, prices
/// rounded to `tick` (relative), dropping windows that hold a masked slot.
fn cut(
    sessions: &[SessionPath],
    bar_seconds: f64,
    window_bars: usize,
    tick_rel: Option<f64>,
    slot_seconds: f64,
) -> (Vec<Vec<f64>>, usize) {
    let bar = ((bar_seconds / slot_seconds).round() as usize).max(1);
    let span = window_bars * bar;
    let mut out = Vec::new();
    let mut masked = 0;
    for session in sessions {
        let slots = session.log_levels.len().saturating_sub(1);
        let mut from = 0;
        while from + span <= slots {
            let to = from + span;
            if session
                .masked_slots
                .iter()
                .any(|s| (from as u64..=to as u64).contains(s))
            {
                masked += 1;
            } else {
                let prices: Vec<f64> = (0..=window_bars)
                    .map(|i| tick(session.log_levels[from + i * bar].exp(), tick_rel))
                    .collect();
                out.push(prices.windows(2).map(|p| (p[1] / p[0]).ln()).collect());
            }
            from = to;
        }
    }
    (out, masked)
}

fn tick(price: f64, tick: Option<f64>) -> f64 {
    match tick {
        Some(t) if t > 0.0 => (price / t).round() * t,
        _ => price,
    }
}

/// The stylized facts of a set of windows of per-bar returns.
pub fn window_stats(windows: &[Vec<f64>], realism: &Realism) -> WindowStats {
    let (agg, cluster) = (realism.agg_bars, realism.cluster_bars);
    let mut sd = Vec::new();
    let mut pooled = Vec::new();
    let mut normalized = Vec::new();
    let mut lag1 = Vec::new();
    for r in windows {
        sd.push(moments(&aggregate(r, realism.vol_bars)).sd);
        let ra = aggregate(r, agg);
        let rms = (mean(&ra.iter().map(|v| v * v).collect::<Vec<_>>())).sqrt();
        if rms > 0.0 {
            normalized.extend(ra.iter().map(|v| v / rms));
        }
        lag1.push(acf(&ra, 1));
        pooled.extend(ra);
    }
    let clustering = abs_acf(windows, cluster, &realism.cluster_lags);
    WindowStats {
        vol_level: median(&sd),
        vol_of_vol: {
            // The interquartile range of the log sds over 1.349 (an sd for a
            // normal): a few wild or ticked-flat windows can't move it.
            let logs: Vec<f64> = sd.iter().map(|v| v.max(1e-300).ln()).collect();
            (quantile(&logs, 0.75) - quantile(&logs, 0.25)) / 1.349
        },
        kurtosis_within: moments(&normalized).excess_kurtosis,
        kurtosis_pooled: moments(&pooled).excess_kurtosis,
        tail_index: hill(&pooled, 0.025),
        clustering,
        predictability: mean(&lag1),
        tail_ratio: {
            let abs: Vec<f64> = normalized.iter().map(|v| v.abs()).collect();
            quantile(&abs, 0.99) / quantile(&abs, 0.9)
        },
    }
}

/// The ACF of |r| over `bars`-bar returns at `lags`, each window scaled to
/// unit RMS (so volatility differing between windows doesn't count) and
/// lag pairs taken within a window.
fn abs_acf(windows: &[Vec<f64>], bars: usize, lags: &[usize]) -> Vec<f64> {
    let scaled: Vec<Vec<f64>> = windows
        .iter()
        .filter_map(|r| {
            let rb = aggregate(r, bars);
            let rms = (mean(&rb.iter().map(|v| v * v).collect::<Vec<_>>())).sqrt();
            (rms > 0.0).then(|| rb.iter().map(|v| v.abs() / rms).collect())
        })
        .collect();
    let flat: Vec<f64> = scaled.iter().flatten().copied().collect();
    let m = mean(&flat);
    let c0 = mean(&flat.iter().map(|v| (v - m).powi(2)).collect::<Vec<_>>());
    lags.iter()
        .map(|&lag| {
            let num: Vec<f64> = scaled
                .iter()
                .flat_map(|x| {
                    (0..x.len().saturating_sub(lag)).map(move |i| (x[i] - m) * (x[i + lag] - m))
                })
                .collect();
            if num.is_empty() || c0 == 0.0 {
                0.0
            } else {
                mean(&num) / c0
            }
        })
        .collect()
}

/// Volatility clustering within longer windows (see
/// [`LongClustering`]): each lag gated like the other clustering checks,
/// its band from the real and the generated windows' sampling spread.
fn long_checks(long: &LongClustering, windows: &[Vec<f64>], sigmas: f64) -> Vec<Check> {
    let n = windows.len();
    let own: Vec<f64> = {
        let mut rng = SplitMix64(0x6c6f_6e67_0000_0000 ^ n as u64);
        let draws: Vec<Vec<f64>> = (0..SPREAD_DRAWS)
            .map(|_| {
                let sample: Vec<Vec<f64>> = (0..n)
                    .map(|_| windows[(rng.next_u64() % n as u64) as usize].clone())
                    .collect();
                abs_acf(&sample, 1, &long.cluster_lags)
            })
            .collect();
        (0..long.cluster_lags.len())
            .map(|i| moments(&draws.iter().map(|d| d[i]).collect::<Vec<_>>()).sd)
            .collect()
    };
    let generated = abs_acf(windows, 1, &long.cluster_lags);
    let real_scale = (long.spread_windows as f64 / long.windows.max(1) as f64).sqrt();
    let at_n = (long.spread_windows as f64 / n.max(1) as f64).sqrt();
    long.cluster_lags
        .iter()
        .enumerate()
        .map(|(i, lag)| {
            let name = format!("long_clustering_lag_{lag}");
            if long.info_lags.contains(lag) {
                return info(&name, long.reference[i], generated[i]);
            }
            let sd = long.spread_sd[i];
            let d = sigmas * (sd * real_scale).hypot(own[i].max(sd * at_n));
            let held = ((long.null[i] - long.reference[i]).abs() <= d).then_some(long.null[i]);
            let band = Band {
                ratio: None,
                diff: Some(d),
            };
            gate(&name, long.reference[i], generated[i], band, held)
        })
        .collect()
}

/// Bootstrap draws of the generated windows behind each band.
const SPREAD_DRAWS: usize = 200;

/// Compare generated windows' statistics with the reference.
///
/// Each gated check's band is `spread.sigmas` standard deviations of the
/// difference's sampling error: the real statistic's (from the file, scaled
/// to the real window count) and the generated one's (bootstrapped from the
/// generated windows, and at least the real statistic's spread at that many
/// windows). A fair generator therefore passes whatever its seed.
///
/// A check judges only if its band leaves out the obviously wrong value
/// (the file's nulls: thin tails, no vol of vol, no clustering; half and
/// twice the volatility; a lag-1 autocorrelation 0.3 off). One that passes
/// without that power reports [`Status::NoPower`]. Pooled kurtosis and
/// Hill's index are reported, not gated.
pub fn compare(realism: &Realism, windows: &[Vec<f64>]) -> (WindowStats, Vec<Check>) {
    let (r, s, null) = (&realism.reference, &realism.spread, &realism.null);
    let g = window_stats(windows, realism);
    let own = bootstrap(windows, realism);
    // Spread sds are at `s.windows` windows: scale them to the real count
    // and to the generated one. The generated statistic's own spread is its
    // bootstrap's, or the real one's at its size when larger: a bootstrap of
    // a small sample with no outlier in it understates a heavy-tailed
    // statistic's spread.
    let real = (s.windows as f64 / realism.windows.max(1) as f64).sqrt();
    let at_n = (s.windows as f64 / windows.len().max(1) as f64).sqrt();
    let width =
        |real_sd: f64, own_sd: f64| s.sigmas * (real_sd * real).hypot(own_sd.max(real_sd * at_n));
    let ratio = |name: &str, real_v: f64, gen_v: f64, sds: (f64, f64), nulls: &[f64]| {
        let w = width(sds.0, sds.1).exp();
        let held = nulls
            .iter()
            .find(|n| (1.0 / w..=w).contains(&(**n / real_v)));
        let band = Band {
            ratio: Some([1.0 / w, w]),
            diff: None,
        };
        gate(name, real_v, gen_v, band, held.copied())
    };
    let diff = |name: &str, real_v: f64, gen_v: f64, sds: (f64, f64), nulls: &[f64]| {
        let d = width(sds.0, sds.1);
        let held = nulls.iter().find(|n| (**n - real_v).abs() <= d);
        let band = Band {
            ratio: None,
            diff: Some(d),
        };
        gate(name, real_v, gen_v, band, held.copied())
    };
    let mut checks = vec![
        ratio(
            "vol_level",
            r.vol_level,
            g.vol_level,
            (s.log_sd.vol_level, own.log_sd.vol_level),
            &[0.5 * r.vol_level, 2.0 * r.vol_level],
        ),
        ratio(
            "vol_of_vol",
            r.vol_of_vol,
            g.vol_of_vol,
            (s.log_sd.vol_of_vol, own.log_sd.vol_of_vol),
            &[null.vol_of_vol],
        ),
        diff(
            "kurtosis_within",
            r.kurtosis_within,
            g.kurtosis_within,
            (s.sd.kurtosis_within, own.sd.kurtosis_within),
            &[null.kurtosis_within],
        ),
        ratio(
            "tail_ratio",
            r.tail_ratio,
            g.tail_ratio,
            (s.log_sd.tail_ratio, own.log_sd.tail_ratio),
            &[null.tail_ratio],
        ),
        info("kurtosis_pooled", r.kurtosis_pooled, g.kurtosis_pooled),
        info("tail_index", r.tail_index, g.tail_index),
    ];
    for (i, lag) in realism.cluster_lags.iter().enumerate() {
        checks.push(diff(
            &format!("clustering_lag_{lag}"),
            r.clustering[i],
            g.clustering[i],
            (s.sd.clustering[i], own.sd.clustering[i]),
            &[null.clustering[i]],
        ));
    }
    checks.push(diff(
        "predictability",
        r.predictability,
        g.predictability,
        (s.sd.predictability, own.sd.predictability),
        &[r.predictability - 0.3, r.predictability + 0.3],
    ));
    (g, checks)
}

/// Judge a gated check; a pass whose band holds a null value (`held`) has
/// no power.
fn gate(name: &str, real: f64, generated: f64, band: Band, held: Option<f64>) -> Check {
    let mut check = judge(name, real, generated, band);
    if let (Status::Pass, Some(null)) = (check.status, held) {
        check.status = Status::NoPower;
        check.detail = format!(
            "{}; no power: the band also holds {null:.4}, the obviously wrong market's value",
            check.detail
        );
    }
    check
}

/// The sampling spread of each gated statistic over `windows`: drawn with
/// replacement, as many as there are, [`SPREAD_DRAWS`] times, from a fixed
/// seed. In the file's terms (see [`Spread`]).
fn bootstrap(windows: &[Vec<f64>], realism: &Realism) -> Spread {
    let mut rng = SplitMix64(0x7370_7265_6164_0000 ^ windows.len() as u64);
    let n = windows.len();
    let draws: Vec<WindowStats> = (0..SPREAD_DRAWS)
        .map(|_| {
            let sample: Vec<Vec<f64>> = (0..n)
                .map(|_| windows[(rng.next_u64() % n as u64) as usize].clone())
                .collect();
            window_stats(&sample, realism)
        })
        .collect();
    let sd_of = |f: &dyn Fn(&WindowStats) -> f64| {
        let xs: Vec<f64> = draws.iter().map(f).filter(|x| x.is_finite()).collect();
        if xs.len() < 2 {
            f64::INFINITY
        } else {
            moments(&xs).sd
        }
    };
    Spread {
        windows: n,
        draws: SPREAD_DRAWS,
        sigmas: realism.spread.sigmas,
        sigmas_source: None,
        log_sd: LogSpread {
            vol_level: sd_of(&|d| d.vol_level.ln()),
            vol_of_vol: sd_of(&|d| d.vol_of_vol.ln()),
            tail_ratio: sd_of(&|d| d.tail_ratio.ln()),
        },
        sd: DiffSpread {
            kurtosis_within: sd_of(&|d| d.kurtosis_within),
            clustering: (0..realism.cluster_lags.len())
                .map(|i| sd_of(&|d| d.clustering[i]))
                .collect(),
            predictability: sd_of(&|d| d.predictability),
        },
    }
}

/// A statistic reported beside the real one, never failing.
fn info(name: &str, real: f64, generated: f64) -> Check {
    Check {
        name: name.into(),
        status: Status::Info,
        real: Some(real),
        generated: Some(generated),
        band: None,
        detail: "reported, not gated: its sampling spread on a few hundred windows is wider than any useful band".into(),
    }
}

fn judge(name: &str, real: f64, generated: f64, band: Band) -> Check {
    let (status, detail) = match (band.ratio, band.diff) {
        _ if !generated.is_finite() => (Status::Fail, "the generated value is not finite".into()),
        (Some([lo, hi]), _) => {
            if real <= 0.0 {
                (
                    Status::Fail,
                    format!("real {real:.4} is not positive: a ratio band can't judge it"),
                )
            } else {
                let ratio = generated / real;
                let ok = (lo..=hi).contains(&ratio);
                (
                    pass(ok),
                    format!("generated / real = {ratio:.3}, band [{lo:.3}, {hi:.3}]"),
                )
            }
        }
        (None, Some(d)) => {
            let gap = (generated - real).abs();
            (
                pass(gap <= d),
                format!("|generated - real| = {gap:.4}, band {d:.4}"),
            )
        }
        (None, None) => (Status::Fail, "no band".into()),
    };
    Check {
        name: name.into(),
        status,
        real: Some(real),
        generated: Some(generated),
        band: Some(band),
        detail,
    }
}

fn pass(ok: bool) -> Status {
    if ok { Status::Pass } else { Status::Fail }
}

/// Gap sizes at session opens against the calibration's: the median and
/// the 90th percentile of |gap|, each within the band.
pub fn check_gaps(calibration: &Calibration, gaps_pct: &[f64], band: Band) -> Vec<Check> {
    let Some(spec) = calibration.calendar.gaps.first() else {
        return vec![Check::skipped("gap_size", "the calendar has no gaps")];
    };
    if gaps_pct.len() < MIN_GAPS {
        return vec![Check::skipped(
            "gap_size",
            format!("{} gaps, fewer than {MIN_GAPS}", gaps_pct.len()),
        )];
    }
    let abs: Vec<f64> = gaps_pct.iter().map(|g| g.abs()).collect();
    vec![
        judge(
            "gap_size_median",
            spec.abs_median_pct,
            quantile(&abs, 0.5),
            band,
        ),
        judge("gap_size_p90", spec.abs_p90_pct, quantile(&abs, 0.9), band),
    ]
}

/// Round trips must lose: the mean trip loses, and at least
/// `min_losing_share` of them do.
pub fn check_round_trips(trips: &[RoundTrip], min_losing_share: f64, book: &str) -> Check {
    if trips.is_empty() {
        return Check::skipped("round_trips", "no round trips");
    }
    let pnl: Vec<f64> = trips.iter().map(RoundTrip::pnl).collect();
    let losing = pnl.iter().filter(|p| **p < 0.0).count() as f64 / pnl.len() as f64;
    let mean_pnl = mean(&pnl);
    Check {
        name: "round_trips".into(),
        status: pass(mean_pnl < 0.0 && losing >= min_losing_share),
        real: None,
        generated: Some(losing),
        band: None,
        detail: format!(
            "{} trips through {book}: {:.1}% lose (at least {:.0}% must), mean PnL {mean_pnl:.3e} a trip (start price 1)",
            trips.len(),
            losing * 100.0,
            min_losing_share * 100.0
        ),
    }
}

/// Probe round trips on a session: at random slots, take `size` through a
/// book `half_spread_bps` wide around the level, plus impact, and unwind it
/// the next slot the same way, paying the taker fee on both legs.
pub fn probe_round_trips(
    calibration: &Calibration,
    levels: &[f64],
    half_spread_bps: f64,
    sizes: &[f64],
    probes: usize,
    seed: u64,
) -> Vec<RoundTrip> {
    let mut rng = SplitMix64(seed ^ 0x7072_6f62_6573_0000);
    let fee = calibration
        .venues
        .values()
        .next()
        .map_or(0.0, |v| v.taker_fee);
    let impact = &calibration.book.impact;
    let mut trips = Vec::new();
    if levels.len() < 3 {
        return trips;
    }
    for i in 0..probes {
        let slot = (rng.next_u64() % (levels.len() as u64 - 1)) as usize;
        let size = sizes[i % sizes.len()];
        let buy = rng.next_f64() < 0.5;
        let sign = if buy { 1.0 } else { -1.0 };
        let cost_bps = half_spread_bps + impact.k_bps * size.powf(impact.gamma);
        let open_px = levels[slot] * (1.0 + sign * cost_bps / 10_000.0);
        let close_px = levels[slot + 1] * (1.0 - sign * cost_bps / 10_000.0);
        let fees = fee * size * (open_px + close_px);
        trips.push(RoundTrip {
            size,
            open_px,
            close_px,
            buy,
            fees,
        });
    }
    trips
}

/// Check scenario files against a calibration. Sessions shorter than the
/// reference window can't be judged on returns; round trips go through
/// each market-making scenario's competing makers, or the calibration's
/// default book.
pub fn check_scenarios(calibration: &Calibration, target: &str, scenarios: &[Scenario]) -> Report {
    let sessions: Vec<SessionPath> = scenarios.iter().map(SessionPath::from_scenario).collect();
    let mut trips = Vec::new();
    let mut books = Vec::new();
    for (i, scenario) in scenarios.iter().enumerate() {
        let levels: Vec<f64> = (0..=scenario.duration_slots)
            .map(|slot| 1.0 + book_at(&scenario.path, slot) / 100.0)
            .collect();
        let (half_spread, sizes) = match &scenario.making {
            Some(making) => {
                let mut sizes: Vec<f64> = making.takers.iter().map(|t| t.size).collect();
                sizes.sort_by(f64::total_cmp);
                let q = |p: f64| quantile(&sizes, p);
                let sizes = if sizes.is_empty() {
                    vec![1.0]
                } else {
                    vec![q(0.5), q(0.9), q(0.99)]
                };
                (making.competitor_bps, sizes)
            }
            None => (calibration.book.half_spread_bps, default_sizes(calibration)),
        };
        books.push(half_spread);
        trips.extend(probe_round_trips(
            calibration,
            &levels,
            half_spread,
            &sizes,
            30,
            i as u64,
        ));
    }
    let book = if scenarios.iter().all(|s| s.making.is_some()) {
        "the scenarios' competing makers".to_string()
    } else {
        format!(
            "half-spreads of {:.1}-{:.1} bp (the calibration's default book where a scenario has no makers)",
            books.iter().copied().fold(f64::INFINITY, f64::min),
            books.iter().copied().fold(0.0, f64::max)
        )
    };
    let mut report = report(
        calibration,
        target,
        &sessions,
        SLOT_SECONDS,
        Some((trips, book)),
    );
    if let Some(realism) = &calibration.realism {
        add_long(&mut report, realism, &sessions);
    }
    report
}

/// Draw `count` sessions from the calibration's own model and check them:
/// the market model's realism, independent of any task. Sessions are one
/// reference window long and start where the reference windows come from;
/// gaps are drawn separately, each session just before the open.
pub fn check_model(
    calibration: &Calibration,
    spec: &ModelSpec,
    count: usize,
    seed: u64,
) -> Result<Report, String> {
    let realism = calibration
        .realism
        .as_ref()
        .ok_or("the calibration has no [realism] section")?;
    let mut spec = spec.clone();
    if let Some(kind) = &realism.session_kind {
        spec.start = Start::Session { kind: kind.clone() };
    }
    let span_seconds = realism.bar_seconds * realism.window_bars as f64;
    let duration = (span_seconds / SLOT_SECONDS).round() as u64;
    let model = MarketModel::new(calibration.clone(), spec.clone())?;
    let mut sessions = Vec::with_capacity(count);
    let mut trips = Vec::new();
    let sizes = default_sizes(calibration);
    for i in 0..count {
        let path = model.draw(
            seed.wrapping_mul(1_000_003).wrapping_add(i as u64),
            duration,
        );
        let levels: Vec<f64> = path.log_levels.iter().map(|l| l.exp()).collect();
        trips.extend(probe_round_trips(
            calibration,
            &levels,
            calibration.book.half_spread_bps,
            &sizes,
            10,
            seed.wrapping_add(i as u64),
        ));
        sessions.push(SessionPath::from_model(&path));
    }
    let book = format!(
        "the calibration's default book ({} bp half-spread)",
        calibration.book.half_spread_bps
    );
    let mut report = report(
        calibration,
        "model",
        &sessions,
        SLOT_SECONDS,
        Some((trips, book)),
    );
    if let Some(long) = &realism.long {
        // Sessions as long as the long windows, drawn the same way.
        let duration = (long.bar_seconds * long.window_bars as f64 / SLOT_SECONDS).round() as u64;
        let long_sessions: Vec<SessionPath> = (0..count)
            .map(|i| {
                let draw = seed
                    .wrapping_add(0x6c6f_6e67)
                    .wrapping_mul(1_000_003)
                    .wrapping_add(i as u64);
                SessionPath::from_model(&model.draw(draw, duration))
            })
            .collect();
        add_long(&mut report, realism, &long_sessions);
    }
    if let Some(gap) = calibration.calendar.gaps.first() {
        let opens = MarketModel::new(
            calibration.clone(),
            ModelSpec {
                start: Start::Before {
                    kind: gap.into.clone(),
                    lead_seconds: 60.0,
                },
                ..spec
            },
        )?;
        let gaps: Vec<f64> = (0..count.max(MIN_GAPS))
            .flat_map(|i| {
                opens
                    .draw(
                        seed.wrapping_add(0x6761_7073)
                            .wrapping_mul(31)
                            .wrapping_add(i as u64),
                        480,
                    )
                    .record
                    .events
                    .into_iter()
                    .filter(|e| e.kind == EventKind::Gap)
                    .map(|e| e.pct)
            })
            .collect();
        report.checks.retain(|c| !c.name.starts_with("gap_size"));
        report
            .checks
            .extend(check_gaps(calibration, &gaps, realism.bands.gap_size));
        report.decide();
    }
    Ok(report)
}

/// Add the long-window clustering checks (if the calibration has them) on
/// `sessions`, skipped when they hold too few long windows.
fn add_long(report: &mut Report, realism: &Realism, sessions: &[SessionPath]) {
    let Some(long) = &realism.long else {
        return;
    };
    let (windows, _) = cut(
        sessions,
        long.bar_seconds,
        long.window_bars,
        realism.price_tick_rel,
        SLOT_SECONDS,
    );
    if windows.len() < MIN_WINDOWS {
        report.checks.push(Check::skipped(
            "long_clustering",
            format!(
                "{} windows of {} s, fewer than {MIN_WINDOWS}: sessions shorter than the long reference window, or too few",
                windows.len(),
                long.bar_seconds * long.window_bars as f64
            ),
        ));
    } else {
        report
            .checks
            .extend(long_checks(long, &windows, realism.spread.sigmas));
    }
    report.decide();
}

/// Sizes the probe trades without a scenario's flow: the median and the
/// 90th and 99th percentiles of the first venue's trade sizes.
fn default_sizes(calibration: &Calibration) -> Vec<f64> {
    match calibration.venues.values().next() {
        Some(v) => [0.0, 1.281_551_565_545, 2.326_347_874_041]
            .iter()
            .map(|z| v.trade_size.median * (v.trade_size.sigma * z).exp())
            .collect(),
        None => vec![1.0],
    }
}

/// Run every check on a set of session paths.
pub fn report(
    calibration: &Calibration,
    target: &str,
    sessions: &[SessionPath],
    slot_seconds: f64,
    round_trips: Option<(Vec<RoundTrip>, String)>,
) -> Report {
    let mut checks = Vec::new();
    let mut generated = None;
    let (mut used, mut masked) = (0, 0);
    match &calibration.realism {
        None => checks.push(Check::skipped(
            "returns",
            "the calibration has no [realism] section",
        )),
        Some(realism) => {
            let (windows, dropped) = windows(sessions, realism, slot_seconds);
            used = windows.len();
            masked = dropped;
            if windows.len() < MIN_WINDOWS {
                checks.push(Check::skipped(
                    "returns",
                    format!(
                        "{} windows of {} s, fewer than {MIN_WINDOWS}: sessions shorter than the reference window, or too few",
                        windows.len(),
                        realism.bar_seconds * realism.window_bars as f64
                    ),
                ));
            } else {
                let (stats, compared) = compare(realism, &windows);
                checks.extend(compared);
                generated = Some(stats);
            }
            let gaps: Vec<f64> = sessions
                .iter()
                .flat_map(|s| s.gaps_pct.iter().copied())
                .collect();
            if !calibration.calendar.gaps.is_empty() {
                checks.extend(check_gaps(calibration, &gaps, realism.bands.gap_size));
            }
            if let Some((trips, book)) = &round_trips {
                checks.push(check_round_trips(
                    trips,
                    realism.round_trip_min_losing_share,
                    book,
                ));
            }
        }
    }
    let mut report = Report {
        calibration: calibration.meta.calibration_id.clone(),
        calibration_sha256: calibration.sha256.clone(),
        target: target.into(),
        sessions: sessions.len(),
        windows: used,
        masked_windows: masked,
        generated,
        checks,
        verdict: Verdict::NotJudged,
        passed: false,
    };
    report.decide();
    report
}

struct Moments {
    sd: f64,
    excess_kurtosis: f64,
}

fn mean(x: &[f64]) -> f64 {
    if x.is_empty() {
        return f64::NAN;
    }
    x.iter().sum::<f64>() / x.len() as f64
}

fn moments(x: &[f64]) -> Moments {
    let n = x.len() as f64;
    let m = mean(x);
    let v2 = x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / n;
    let v4 = x.iter().map(|v| (v - m).powi(4)).sum::<f64>() / n;
    Moments {
        sd: v2.sqrt(),
        excess_kurtosis: if v2 > 0.0 { v4 / (v2 * v2) - 3.0 } else { 0.0 },
    }
}

fn aggregate(r: &[f64], k: usize) -> Vec<f64> {
    if r.len() < k {
        return Vec::new();
    }
    (0..=r.len() - k)
        .step_by(k)
        .map(|i| r[i..i + k].iter().sum())
        .collect()
}

fn acf(x: &[f64], lag: usize) -> f64 {
    let n = x.len();
    if lag >= n {
        return 0.0;
    }
    let m = mean(x);
    let d: Vec<f64> = x.iter().map(|v| v - m).collect();
    let c0 = d.iter().map(|e| e * e).sum::<f64>() / n as f64;
    if c0 == 0.0 {
        return 0.0;
    }
    (0..n - lag).map(|i| d[i] * d[i + lag]).sum::<f64>() / n as f64 / c0
}

/// Hill's estimator of the tail index of |x| over the top `share`.
fn hill(x: &[f64], share: f64) -> f64 {
    let mut a: Vec<f64> = x.iter().map(|v| v.abs()).filter(|v| *v != 0.0).collect();
    a.sort_by(|p, q| q.total_cmp(p));
    let k = 10.max((share * a.len() as f64) as usize);
    if a.len() <= k + 1 {
        return f64::NAN;
    }
    let xk = a[k];
    1.0 / mean(&a[..k].iter().map(|v| (v / xk).ln()).collect::<Vec<_>>())
}

fn median(x: &[f64]) -> f64 {
    let mut s = x.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// Linear interpolation between order statistics, as the fitter's.
fn quantile(x: &[f64], q: f64) -> f64 {
    let mut s = x.to_vec();
    s.sort_by(f64::total_cmp);
    if s.is_empty() {
        return f64::NAN;
    }
    let i = q * (s.len() - 1) as f64;
    let lo = i.floor() as usize;
    let hi = (lo + 1).min(s.len() - 1);
    s[lo] + (s[hi] - s[lo]) * (i - lo as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::tests::{sol, spcx, template};

    /// The fitter's statistics on a fixed series of fat-tailed windows
    /// with SOL's reference settings, computed by `window_stats` in
    /// infra-calib's `calib/export_v1.py` (`testdata/window_stats.json`).
    #[test]
    fn matches_the_fitters_statistics() {
        let realism = sol().realism.unwrap();
        let fixture = include_str!("testdata/window_stats.json");
        let expected: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let windows: Vec<Vec<f64>> = serde_json::from_value(expected["windows"].clone()).unwrap();
        let s = window_stats(&windows, &realism);
        let e = &expected["stats"];
        let close = |a: f64, b: &serde_json::Value| {
            (a - b.as_f64().unwrap()).abs() <= 1e-9 * (1.0 + a.abs())
        };
        assert!(close(s.vol_level, &e["vol_level"]));
        assert!(close(s.vol_of_vol, &e["vol_of_vol"]));
        assert!(close(s.kurtosis_within, &e["kurtosis_within"]));
        assert!(close(s.kurtosis_pooled, &e["kurtosis_pooled"]));
        assert!(close(s.tail_index, &e["tail_index"]));
        assert!(close(s.predictability, &e["predictability"]));
        assert!(close(s.tail_ratio, &e["tail_ratio"]));
        for (i, c) in s.clustering.iter().enumerate() {
            assert!(close(*c, &e["clustering"][i]));
        }
    }

    #[test]
    fn the_calibrated_model_passes_its_own_checks_whatever_the_seed() {
        // Each check's band is its sampling spread, so a fair generator
        // passes on every seed; with no stated floors, vol_level judges the
        // level itself.
        for cal in [sol(), spcx()] {
            for seed in 1..=4 {
                let report = check_model(&cal, &ModelSpec::default(), 300, seed).unwrap();
                let failed: Vec<_> = report
                    .checks
                    .iter()
                    .filter(|c| c.status != Status::Pass && c.status != Status::Info)
                    .collect();
                assert_eq!(
                    report.verdict,
                    Verdict::Pass,
                    "{} seed {seed}: {failed:#?}",
                    cal.meta.calibration_id
                );
                assert!(report.passed);
                assert_eq!(status(&report, "vol_level"), Status::Pass);
            }
        }
        let spcx = check_model(&spcx(), &ModelSpec::default(), 100, 2).unwrap();
        assert_eq!(status(&spcx, "gap_size_median"), Status::Pass);
        assert_eq!(status(&spcx, "round_trips"), Status::Pass);
    }

    fn status(report: &Report, name: &str) -> Status {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}"))
            .status
    }

    #[test]
    fn half_and_twice_the_volatility_fail_on_both_files() {
        for cal in [sol(), spcx()] {
            for vol_mult in [0.5, 2.0] {
                let spec = ModelSpec {
                    vol_mult,
                    ..ModelSpec::default()
                };
                let report = check_model(&cal, &spec, 300, 1).unwrap();
                assert_eq!(status(&report, "vol_level"), Status::Fail, "{vol_mult}");
                assert_eq!(report.verdict, Verdict::Fail);
            }
        }
    }

    #[test]
    fn thin_tails_do_not_pass() {
        // Gaussian innovations, everything else the calibration's.
        for cal in [sol(), spcx()] {
            let mut gaussian = cal.clone();
            gaussian.fair_value.tails.df = 1e6;
            for seed in 1..=2 {
                let report = check_model(&gaussian, &ModelSpec::default(), 300, seed).unwrap();
                assert_eq!(report.verdict, Verdict::Fail, "{}", cal.meta.calibration_id);
                assert!(
                    [
                        status(&report, "tail_ratio"),
                        status(&report, "kurtosis_within")
                    ]
                    .contains(&Status::Fail)
                );
            }
        }
    }

    /// The calibrated model's false-fail rate, and the no-regimes and
    /// no-GARCH markets' catch rates, on fresh seeds at 300 and at 1000
    /// sessions, for each file. The bar is at most 2% false fails and at
    /// least 95% catches, except SPCX's no GARCH, required only at 1000
    /// sessions. In release: `FFR_SEEDS=60 FFR_OFFSET=0 cargo test
    /// --release -p mtx-grader --lib false_fail_rates -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore]
    fn false_fail_rates() {
        type Wrong = fn(&mut Calibration);
        let wrongs: [(&str, Wrong); 2] = [
            ("no regimes", |c| {
                let r = &mut c.fair_value.regimes;
                r.log_sd_within.iter_mut().for_each(|s| *s = 0.0);
                r.vol_mult.iter_mut().for_each(|m| *m = 1.0);
            }),
            ("no GARCH", |c| {
                c.fair_value.clustering.alpha = 0.0;
                c.fair_value.clustering.beta = 0.0;
            }),
        ];
        // Seeds per row, and where they start: FFR_SEEDS, FFR_OFFSET.
        let env = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let (seeds, offset) = (env("FFR_SEEDS", 60), env("FFR_OFFSET", 0));
        let run = |cal: &Calibration, count: usize, seeds: std::ops::Range<u64>| {
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
            let seeds: Vec<u64> = seeds.collect();
            let reports: Vec<Report> = std::thread::scope(|scope| {
                let handles: Vec<_> = seeds
                    .chunks(seeds.len().div_ceil(threads))
                    .map(|chunk| {
                        scope.spawn(move || {
                            chunk
                                .iter()
                                .map(|&seed| {
                                    check_model(cal, &ModelSpec::default(), count, seed).unwrap()
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap())
                    .collect()
            });
            let count_of = |v: Verdict| reports.iter().filter(|r| r.verdict == v).count();
            // Failed checks, and the required ones without power in
            // reports that weren't judged.
            let mut failed: Vec<String> = reports
                .iter()
                .flat_map(|r| {
                    r.checks.iter().filter_map(move |c| match c.status {
                        Status::Fail => Some(c.name.clone()),
                        Status::NoPower
                            if r.verdict == Verdict::NotJudged
                                && REQUIRED.iter().any(|g| g.contains(&c.name.as_str())) =>
                        {
                            Some(format!("{} (no power)", c.name))
                        }
                        _ => None,
                    })
                })
                .collect();
            failed.sort();
            failed.dedup();
            // Where the generated statistics centre: the median over seeds
            // of generated / real.
            let centre = |name: &str| {
                let ratios: Vec<f64> = reports
                    .iter()
                    .filter_map(|r| r.checks.iter().find(|c| c.name == name))
                    .filter_map(|c| Some(c.generated? / c.real?))
                    .collect();
                format!("{name} {:.2}", median(&ratios))
            };
            // And for the ACFs, the median of generated - real.
            let offset = |name: &str| {
                let diffs: Vec<f64> = reports
                    .iter()
                    .filter_map(|r| r.checks.iter().find(|c| c.name == name))
                    .filter_map(|c| Some(c.generated? - c.real?))
                    .collect();
                (!diffs.is_empty()).then(|| format!("{name} {:+.4}", median(&diffs)))
            };
            let mut centres = ["vol_level", "vol_of_vol", "tail_ratio", "predictability"]
                .map(centre)
                .join(", ");
            for name in [
                "kurtosis_within",
                "clustering_lag_1",
                "clustering_lag_2",
                "clustering_lag_3",
                "clustering_lag_5",
                "clustering_lag_10",
                "long_clustering_lag_1",
                "long_clustering_lag_5",
                "long_clustering_lag_15",
            ] {
                if let Some(o) = offset(name) {
                    centres = format!("{centres}, {o}");
                }
            }
            (
                count_of(Verdict::Pass),
                count_of(Verdict::Fail),
                count_of(Verdict::NotJudged),
                format!("{}; centres: {centres}", failed.join(", ")),
            )
        };
        println!(
            "| market | file | sessions | seeds | pass | fail | not judged | checks that failed |"
        );
        println!("|---|---|---|---|---|---|---|---|");
        let mut false_fails = Vec::new();
        for cal in [sol(), spcx()] {
            for (count, base) in [(300, 1000), (1000, 2000)] {
                let range = base * 1000 + offset..base * 1000 + offset + seeds;
                let (pass, fail, unjudged, names) = run(&cal, count, range.clone());
                println!(
                    "| calibrated | {} | {count} | {seeds} | {pass} | {fail} | {unjudged} | {names} |",
                    cal.meta.asset
                );
                false_fails.push((cal.meta.asset.clone(), count, fail));
                for (name, wrong) in wrongs {
                    let mut c = cal.clone();
                    wrong(&mut c);
                    let (pass, fail, unjudged, names) = run(&c, count, range.clone());
                    println!(
                        "| {name} | {} | {count} | {seeds} | {pass} | {fail} | {unjudged} | {names} |",
                        cal.meta.asset
                    );
                    // SPCX's 30-minute windows can't reliably tell no GARCH
                    // from its short-lived clustering at 300 sessions (about
                    // 90-98% caught); 1000 sessions catch it every time.
                    let required =
                        !(name == "no GARCH" && cal.meta.asset == "spcx" && count < 1000);
                    assert!(
                        !required || fail as f64 >= 0.95 * seeds as f64,
                        "{name} caught {fail}/{seeds}"
                    );
                }
            }
        }
        for (asset, count, fail) in false_fails {
            assert!(
                fail as f64 <= 0.02 * seeds as f64,
                "{asset} at {count}: {fail}/{seeds} false fails"
            );
        }
    }

    /// The adversarial table in PR_NOTES: each wrong market against each
    /// file, and which checks catch it. `cargo test -p mtx-grader --lib
    /// adversarial_table -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn adversarial_table() {
        type Wrong = fn(&mut Calibration, &mut ModelSpec);
        let cases: [(&str, Wrong); 11] = [
            ("calibrated", |_, _| {}),
            ("gaussian, constant vol", |c, _| {
                let fv = &mut c.fair_value;
                fv.tails.df = 1e6;
                fv.clustering.alpha = 0.0;
                fv.clustering.beta = 0.0;
                fv.regimes.log_sd_within.iter_mut().for_each(|s| *s = 0.0);
                fv.regimes.vol_mult.iter_mut().for_each(|m| *m = 1.0);
                fv.intraday_vol_mult_utc = None;
            }),
            ("vol 0.5x", |_, s| s.vol_mult = 0.5),
            ("vol 0.7x", |_, s| s.vol_mult = 0.7),
            ("vol 1.5x", |_, s| s.vol_mult = 1.5),
            ("vol 2x", |_, s| s.vol_mult = 2.0),
            ("no regimes", |c, _| {
                let r = &mut c.fair_value.regimes;
                r.log_sd_within.iter_mut().for_each(|s| *s = 0.0);
                r.vol_mult.iter_mut().for_each(|m| *m = 1.0);
            }),
            ("no GARCH", |c, _| {
                c.fair_value.clustering.alpha = 0.0;
                c.fair_value.clustering.beta = 0.0;
            }),
            ("thin tails (df 1e6)", |c, _| c.fair_value.tails.df = 1e6),
            ("tails too heavy (df 2.5)", |c, _| {
                c.fair_value.tails.df = 2.5
            }),
            ("tails too heavy (df 3)", |c, _| c.fair_value.tails.df = 3.0),
        ];
        println!("| case | file | seed | verdict | fail | no power |");
        println!("|---|---|---|---|---|---|");
        for (name, wrong) in cases {
            for cal in [sol(), spcx()] {
                for seed in 1..=3 {
                    let (mut c, mut spec) = (cal.clone(), ModelSpec::default());
                    wrong(&mut c, &mut spec);
                    let r = check_model(&c, &spec, 300, seed).unwrap();
                    let named = |s: Status| {
                        r.checks
                            .iter()
                            .filter(|c| c.status == s)
                            .map(|c| match (s, c.real, c.generated) {
                                (Status::Fail, Some(real), Some(generated)) => {
                                    format!("{} ({:.3}x)", c.name, generated / real)
                                }
                                _ => c.name.clone(),
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    println!(
                        "| {name} | {} | {seed} | {:?} | {} | {} |",
                        cal.meta.asset,
                        r.verdict,
                        named(Status::Fail),
                        named(Status::NoPower)
                    );
                }
            }
        }
    }

    #[test]
    fn a_gaussian_walk_fails() {
        // Constant volatility, Gaussian steps, at the real level.
        let cal = sol();
        let realism = cal.realism.as_ref().unwrap();
        let mut rng = SplitMix64(9);
        let per_slot = cal.fair_value.per_second_sd * SLOT_SECONDS.sqrt();
        let sessions: Vec<SessionPath> = (0..1000)
            .map(|_| {
                let mut level = 0.0;
                let mut log_levels = vec![0.0];
                for _ in 0..480 {
                    level += per_slot * rng.normal();
                    log_levels.push(level);
                }
                SessionPath {
                    log_levels,
                    masked_slots: vec![],
                    gaps_pct: vec![],
                }
            })
            .collect();
        assert_eq!(realism.window_bars as f64 * realism.bar_seconds, 120.0);
        let report = report(&cal, "gaussian", &sessions, SLOT_SECONDS, None);
        assert_eq!(status(&report, "vol_of_vol"), Status::Fail);
        assert_eq!(status(&report, "tail_ratio"), Status::Fail);
        assert_eq!(status(&report, "kurtosis_pooled"), Status::Info);
        assert_eq!(report.verdict, Verdict::Fail);
        assert!(!report.passed);
    }

    #[test]
    fn todays_making_family_is_too_hot_for_spcx() {
        let scenarios = crate::making::MakingFamily::spcx().sample_set("mm", 60, 3);
        let report = check_scenarios(&spcx(), "market-making", &scenarios);
        // Sessions of minutes can't fill SPCX's hour-long windows: the
        // round trips pass, but the market isn't judged, and that isn't a
        // pass.
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "returns" && c.status == Status::Skipped)
        );
        assert_eq!(report.windows, 0);
        assert_eq!(report.verdict, Verdict::NotJudged);
        assert!(!report.passed);
        let report = check_scenarios(&sol(), "market-making", &scenarios);
        let vol = report
            .checks
            .iter()
            .find(|c| c.name == "vol_level")
            .unwrap();
        assert_eq!(vol.status, Status::Fail, "{vol:?}");
        // The competing makers' spread still makes round trips lose.
        let trips = report
            .checks
            .iter()
            .find(|c| c.name == "round_trips")
            .unwrap();
        assert_eq!(trips.status, Status::Pass, "{trips:?}");
    }

    #[test]
    fn scenarios_from_the_model_mask_their_gaps() {
        let model = MarketModel::new(
            spcx(),
            ModelSpec {
                start: Start::Before {
                    kind: "regular".into(),
                    lead_seconds: 30.0,
                },
                ..ModelSpec::default()
            },
        )
        .unwrap();
        let set = model.sample_set(&template(), "open", 25, 4).unwrap();
        let report = check_scenarios(&spcx(), "open", &set);
        let gap = report
            .checks
            .iter()
            .find(|c| c.name == "gap_size_median")
            .unwrap();
        assert_ne!(gap.status, Status::Skipped, "{gap:?}");
        let paths: Vec<SessionPath> = set.iter().map(SessionPath::from_scenario).collect();
        assert!(paths.iter().all(|p| p.masked_slots == [120]));
    }

    #[test]
    fn round_trips_that_win_fail() {
        let win = RoundTrip {
            size: 1.0,
            open_px: 1.0,
            close_px: 1.01,
            buy: true,
            fees: 0.0,
        };
        let lose = RoundTrip {
            close_px: 0.999,
            ..win
        };
        assert_eq!(
            check_round_trips(&[lose; 20], 0.95, "a book").status,
            Status::Pass
        );
        let mostly = [vec![lose; 18], vec![win; 2]].concat();
        assert_eq!(
            check_round_trips(&mostly, 0.95, "a book").status,
            Status::Fail
        );
        // A book with no spread, no impact and no fees: half the trips win.
        let mut cal = sol();
        cal.book.impact.k_bps = 1e-9;
        cal.venues.clear();
        let levels: Vec<f64> = (0..500)
            .map(|i| 1.0 + 1e-4 * ((i * 7919 % 13) as f64 - 6.0))
            .collect();
        let trips = probe_round_trips(&cal, &levels, 1e-9, &[1.0], 200, 1);
        assert_eq!(
            check_round_trips(&trips, 0.95, "no book").status,
            Status::Fail
        );
    }

    #[test]
    fn bands_judge_ratios_and_differences() {
        let ratio = Band {
            ratio: Some([0.5, 2.0]),
            diff: None,
        };
        let diff = Band {
            ratio: None,
            diff: Some(0.1),
        };
        assert_eq!(judge("x", 1.0, 1.9, ratio).status, Status::Pass);
        assert_eq!(judge("x", 1.0, 2.1, ratio).status, Status::Fail);
        assert_eq!(judge("x", -1.0, 1.0, ratio).status, Status::Fail);
        assert_eq!(judge("x", 0.0, f64::NAN, diff).status, Status::Fail);
        assert_eq!(judge("x", -0.05, 0.04, diff).status, Status::Pass);
    }
}
