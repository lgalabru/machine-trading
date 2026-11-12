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
//! - fat tails: kurtosis within sessions and pooled, and the tail index;
//! - volatility clustering within a session (ACF of |r|);
//! - no linear predictability (lag-1 ACF);
//! - gaps where the calendar opens a session, against the fitted gap sizes;
//! - probe round trips through the book lose spread, impact and fees.
//!
//! Windows that hold a gap are left out of the return checks; the gaps are
//! checked on their own. [`window_stats`] is the port of the fitter's
//! `window_stats` (infra-calib `calib/export_v1.py`): keep the two in step.

use serde::{Deserialize, Serialize};

use super::{
    calibration::{Band, Calibration, Realism, WindowStats},
    model::{EventKind, MarketModel, ModelPath, ModelSpec, Start},
};
use crate::{
    making::{book_at, level_at},
    scenario::{SLOT_SECONDS, Scenario, SplitMix64},
};

/// Fewer windows than this and the return checks are skipped: the
/// statistics are too noisy to judge.
pub const MIN_WINDOWS: usize = 30;
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
    /// No check failed (skipped checks don't fail).
    pub passed: bool,
}

/// The per-bar returns of each window of `realism.window_bars` bars, from
/// slot-level paths, rounded to the reference's tick. Windows holding a
/// masked slot are dropped; returns the windows and how many were dropped.
pub fn windows(
    sessions: &[SessionPath],
    realism: &Realism,
    slot_seconds: f64,
) -> (Vec<Vec<f64>>, usize) {
    let bar = ((realism.bar_seconds / slot_seconds).round() as usize).max(1);
    let span = realism.window_bars * bar;
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
                let prices: Vec<f64> = (0..=realism.window_bars)
                    .map(|i| {
                        tick(
                            session.log_levels[from + i * bar].exp(),
                            realism.price_tick_rel,
                        )
                    })
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
        sd.push(moments(r).sd);
        let ra = aggregate(r, agg);
        let rms = (mean(&ra.iter().map(|v| v * v).collect::<Vec<_>>())).sqrt();
        if rms > 0.0 {
            normalized.extend(ra.iter().map(|v| v / rms));
        }
        lag1.push(acf(&ra, 1));
        pooled.extend(ra);
    }
    let scaled: Vec<Vec<f64>> = windows
        .iter()
        .filter_map(|r| {
            let rb = aggregate(r, cluster);
            let rms = (mean(&rb.iter().map(|v| v * v).collect::<Vec<_>>())).sqrt();
            (rms > 0.0).then(|| rb.iter().map(|v| v.abs() / rms).collect())
        })
        .collect();
    let flat: Vec<f64> = scaled.iter().flatten().copied().collect();
    let m = mean(&flat);
    let c0 = mean(&flat.iter().map(|v| (v - m).powi(2)).collect::<Vec<_>>());
    let clustering = realism
        .cluster_lags
        .iter()
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
        .collect();
    WindowStats {
        vol_level: median(&sd),
        vol_of_vol: moments(&sd).sd / mean(&sd),
        kurtosis_within: moments(&normalized).excess_kurtosis,
        kurtosis_pooled: moments(&pooled).excess_kurtosis,
        tail_index: hill(&pooled, 0.025),
        clustering,
        predictability: mean(&lag1),
    }
}

/// Compare generated statistics with the reference, band by band.
pub fn compare(realism: &Realism, generated: &WindowStats) -> Vec<Check> {
    let (r, g, b) = (&realism.reference, generated, &realism.bands);
    let mut checks = vec![
        judge("vol_level", r.vol_level, g.vol_level, b.vol_level),
        judge("vol_of_vol", r.vol_of_vol, g.vol_of_vol, b.vol_of_vol),
        judge(
            "kurtosis_within",
            r.kurtosis_within,
            g.kurtosis_within,
            b.kurtosis_within,
        ),
        judge(
            "kurtosis_pooled",
            r.kurtosis_pooled,
            g.kurtosis_pooled,
            b.kurtosis_pooled,
        ),
        judge("tail_index", r.tail_index, g.tail_index, b.tail_index),
    ];
    for (i, lag) in realism.cluster_lags.iter().enumerate() {
        checks.push(judge(
            &format!("clustering_lag_{lag}"),
            r.clustering[i],
            g.clustering[i],
            b.clustering,
        ));
    }
    checks.push(judge(
        "predictability",
        r.predictability,
        g.predictability,
        b.predictability,
    ));
    checks
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
                    format!("generated / real = {ratio:.3}, band [{lo}, {hi}]"),
                )
            }
        }
        (None, Some(d)) => {
            let gap = (generated - real).abs();
            (
                pass(gap <= d),
                format!("|generated - real| = {gap:.4}, band {d}"),
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
    report(
        calibration,
        target,
        &sessions,
        SLOT_SECONDS,
        Some((trips, book)),
    )
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
        report.passed = report.checks.iter().all(|c| c.status != Status::Fail);
    }
    Ok(report)
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
                let stats = window_stats(&windows, realism);
                checks.extend(compare(realism, &stats));
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
    Report {
        calibration: calibration.meta.calibration_id.clone(),
        calibration_sha256: calibration.sha256.clone(),
        target: target.into(),
        sessions: sessions.len(),
        windows: used,
        masked_windows: masked,
        generated,
        passed: checks.iter().all(|c| c.status != Status::Fail),
        checks,
    }
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
        for (i, c) in s.clustering.iter().enumerate() {
            assert!(close(*c, &e["clustering"][i]));
        }
    }

    #[test]
    fn the_calibrated_model_passes_its_own_checks() {
        for cal in [sol(), spcx()] {
            let report = check_model(&cal, &ModelSpec::default(), 300, 1).unwrap();
            let failed: Vec<_> = report
                .checks
                .iter()
                .filter(|c| c.status == Status::Fail)
                .collect();
            assert!(report.passed, "{}: {failed:#?}", cal.meta.calibration_id);
            assert!(report.windows >= 250);
            assert!(
                report
                    .checks
                    .iter()
                    .any(|c| c.name == "round_trips" && c.status == Status::Pass)
            );
        }
        let spcx = check_model(&spcx(), &ModelSpec::default(), 100, 2).unwrap();
        assert!(
            spcx.checks
                .iter()
                .any(|c| c.name == "gap_size_median" && c.status == Status::Pass)
        );
    }

    #[test]
    fn a_gaussian_walk_fails_the_tail_checks() {
        // Constant volatility, Gaussian steps, at the real level.
        let cal = sol();
        let realism = cal.realism.as_ref().unwrap();
        let mut rng = SplitMix64(9);
        let per_slot = realism.reference.vol_level / (realism.bar_seconds / SLOT_SECONDS).sqrt();
        let sessions: Vec<SessionPath> = (0..300)
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
        let report = report(&cal, "gaussian", &sessions, SLOT_SECONDS, None);
        let status = |name: &str| {
            report
                .checks
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .status
        };
        assert_eq!(status("vol_level"), Status::Pass);
        assert_eq!(status("vol_of_vol"), Status::Fail);
        assert_eq!(status("kurtosis_pooled"), Status::Fail);
        assert!(!report.passed);
    }

    #[test]
    fn todays_making_family_is_too_hot_for_spcx() {
        let scenarios = crate::making::MakingFamily::spcx().sample_set("mm", 60, 3);
        let report = check_scenarios(&spcx(), "market-making", &scenarios);
        // Sessions of minutes can't fill SPCX's hour-long windows.
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "returns" && c.status == Status::Skipped)
        );
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
