//! The joint realism check: generated sessions of several assets, drawn
//! together from a correlation file, against the real assets' correlation.
//!
//! The statistic is each pair's median, over 30-minute windows, of the
//! correlation of the windows' 1 m returns, computed the same way for the real
//! windows (by the fitter, into the file's `[realism.pairs]`) and for the
//! generated sessions. Like the single-asset checks, a band is `sigmas`
//! standard deviations of the difference of two sampling errors: the real
//! median's (its spread over resamples of the real windows) and the generated
//! one's (bootstrapped from the generated windows, and at least the real
//! spread at that many windows). A band that also holds the value of windows
//! with the assets' sessions shuffled against each other (no correlation) has
//! no power, and judges nothing.
//!
//! Sessions are as long as the real windows and start inside a regular
//! session, as the real windows do, so a session is one window.

use serde::{Deserialize, Serialize};

use super::{
    Calibration, Correlation,
    calibration::Band,
    model::{MarketModel, ModelSpec, Start},
    realism::{Check, SessionPath, Status, Verdict, gate, windows},
};
use crate::scenario::{SLOT_SECONDS, SplitMix64};

/// Fewer generated sessions than this and the pairs are not judged.
pub const MIN_SESSIONS: usize = 30;
const SPREAD_DRAWS: usize = 200;

/// The assets drawn and each one's generated windows, session by session.
pub type JointWindows = (Vec<String>, Vec<Vec<Vec<f64>>>);

/// What `mtx market-cal check-joint` prints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JointReport {
    pub correlation: String,
    pub correlation_sha256: String,
    /// The assets drawn together, in the draw's order.
    pub assets: Vec<String>,
    pub sessions: usize,
    pub checks: Vec<Check>,
    pub verdict: Verdict,
    /// The verdict is [`Verdict::Pass`].
    pub passed: bool,
}

impl JointReport {
    /// A failure fails; otherwise the report passes when at least one pair
    /// was judged with power and none was left unjudged.
    fn decide(&mut self) {
        let any = |status| self.checks.iter().any(|c| c.status == status);
        self.verdict = if any(Status::Fail) {
            Verdict::Fail
        } else if !any(Status::Pass) || any(Status::NoPower) || any(Status::Skipped) {
            Verdict::NotJudged
        } else {
            Verdict::Pass
        };
        self.passed = self.verdict == Verdict::Pass;
    }
}

/// Draw `count` joint sessions of `calibrations` from the file's matrix and
/// check each pair's median window correlation. The calibrations must be the
/// file's pinned ones, in any order and any subset of its assets.
pub fn check_joint(
    correlation: &Correlation,
    calibrations: &[Calibration],
    spec: &ModelSpec,
    count: usize,
    seed: u64,
) -> Result<JointReport, String> {
    let (assets, windows) = joint_windows(correlation, calibrations, spec, count, seed)?;
    let checks = judge(correlation, &assets, &windows)?;
    let mut report = JointReport {
        correlation: correlation.meta.correlation_id.clone(),
        correlation_sha256: correlation.sha256.clone(),
        assets,
        sessions: count,
        checks,
        verdict: Verdict::NotJudged,
        passed: false,
    };
    report.decide();
    Ok(report)
}

/// The generated windows of each asset: `count` joint draws, one window of 1 m
/// returns per session, read as the real windows are (prices at the tick).
/// Asset `i`'s windows are `result.1[i]`, session by session.
pub fn joint_windows(
    correlation: &Correlation,
    calibrations: &[Calibration],
    spec: &ModelSpec,
    count: usize,
    seed: u64,
) -> Result<JointWindows, String> {
    let refs: Vec<&Calibration> = calibrations.iter().collect();
    let matrix = correlation.joint_matrix(&refs)?;
    draw_windows(correlation, calibrations, spec, &matrix, count, seed)
}

/// [`joint_windows`] with a matrix of the caller's: for sessions drawn from a
/// wrong one, to see that the check rejects them.
pub fn draw_windows(
    correlation: &Correlation,
    calibrations: &[Calibration],
    spec: &ModelSpec,
    matrix: &[Vec<f64>],
    count: usize,
    seed: u64,
) -> Result<JointWindows, String> {
    let minutes = correlation
        .realism
        .pairs
        .first()
        .map(|p| p.window_minutes)
        .ok_or("the correlation has no realism pairs")?;
    let mut spec = spec.clone();
    if let Some(kind) = calibrations
        .first()
        .and_then(|c| c.realism.as_ref())
        .and_then(|r| r.session_kind.clone())
    {
        spec.start = Start::Session { kind };
    }
    let duration = (minutes as f64 * 60.0 / SLOT_SECONDS).round() as u64;
    let models: Vec<MarketModel> = calibrations
        .iter()
        .map(|c| MarketModel::new(c.clone(), spec.clone()))
        .collect::<Result<_, _>>()?;
    let model_refs: Vec<&MarketModel> = models.iter().collect();
    let mut paths: Vec<Vec<SessionPath>> = vec![Vec::with_capacity(count); models.len()];
    for i in 0..count {
        let draw = seed.wrapping_mul(1_000_003).wrapping_add(i as u64);
        let drawn = super::model::draw_joint(&model_refs, matrix, draw, duration)?;
        for (slot, path) in paths.iter_mut().zip(&drawn) {
            slot.push(SessionPath::from_model(path));
        }
    }
    let mut out = Vec::with_capacity(models.len());
    for (cal, sessions) in calibrations.iter().zip(&paths) {
        let realism = cal
            .realism
            .as_ref()
            .ok_or_else(|| format!("{} has no [realism] section", cal.meta.calibration_id))?;
        if (realism.bar_seconds * realism.window_bars as f64 - minutes as f64 * 60.0).abs() > 1e-9 {
            return Err(format!(
                "{}'s realism windows are not {minutes} minutes long",
                cal.meta.calibration_id
            ));
        }
        let (windows, masked) = windows(sessions, realism, SLOT_SECONDS);
        if masked > 0 || windows.len() != count {
            return Err(format!(
                "{}: {} windows from {count} sessions",
                cal.meta.calibration_id,
                windows.len()
            ));
        }
        out.push(windows);
    }
    Ok((
        calibrations.iter().map(|c| c.meta.asset.clone()).collect(),
        out,
    ))
}

/// Judge generated windows against the file's pairs: `windows[i]` are asset
/// `assets[i]`'s windows, one per session. Each pair of the given assets is
/// one check, `joint_corr_<a>_<b>`.
pub fn judge(
    correlation: &Correlation,
    assets: &[String],
    windows: &[Vec<Vec<f64>>],
) -> Result<Vec<Check>, String> {
    if windows.len() != assets.len() || windows.iter().any(|w| w.len() != windows[0].len()) {
        return Err("every asset needs the same number of windows".into());
    }
    let n = windows[0].len();
    let r = &correlation.realism;
    let mut checks = Vec::new();
    for i in 0..assets.len() {
        for j in 0..assets.len() {
            let Some(pair) = r
                .pairs
                .iter()
                .find(|p| p.a == assets[i] && p.b == assets[j])
            else {
                continue;
            };
            let name = format!("joint_corr_{}_{}", pair.a, pair.b);
            if n < MIN_SESSIONS {
                checks.push(Check {
                    name,
                    status: Status::Skipped,
                    real: Some(pair.corr_median),
                    generated: None,
                    band: None,
                    detail: format!("{n} sessions, at least {MIN_SESSIONS} are needed"),
                });
                continue;
            }
            let per_window = window_correlations(&windows[i], &windows[j]);
            let generated = median(&per_window);
            // Real spread is at the real window count; the generated one's is
            // its bootstrap, or the real one at that many windows when larger.
            let own = bootstrap_median_sd(&per_window, r.draws.min(SPREAD_DRAWS));
            let at_n = (pair.n_windows as f64 / per_window.len().max(1) as f64).sqrt();
            let width = r.sigmas
                * pair
                    .corr_spread_sd
                    .hypot(own.max(pair.corr_spread_sd * at_n));
            // The obviously wrong value: the same sessions of one asset against
            // the next session of the other (no correlation).
            let rotated: Vec<Vec<f64>> = (0..n).map(|k| windows[j][(k + 1) % n].clone()).collect();
            let null = median(&window_correlations(&windows[i], &rotated));
            let held = ((null - pair.corr_median).abs() <= width).then_some(null);
            let mut check = gate(
                &name,
                pair.corr_median,
                generated,
                Band {
                    ratio: None,
                    diff: Some(width),
                },
                held,
            );
            if !generated.is_finite() {
                check.status = Status::Fail;
            }
            checks.push(check);
        }
    }
    if checks.is_empty() {
        return Err("none of the assets' pairs is in the correlation file".into());
    }
    Ok(checks)
}

/// The Pearson correlation of each session's two windows, leaving out the
/// windows where either is constant.
fn window_correlations(a: &[Vec<f64>], b: &[Vec<f64>]) -> Vec<f64> {
    a.iter()
        .zip(b)
        .map(|(x, y)| pearson(x, y))
        .filter(|c| c.is_finite())
        .collect()
}

fn pearson(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len() as f64;
    let (mx, my) = (x.iter().sum::<f64>() / n, y.iter().sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (a, b) in x.iter().zip(y) {
        sxy += (a - mx) * (b - my);
        sxx += (a - mx) * (a - mx);
        syy += (b - my) * (b - my);
    }
    if sxx > 0.0 && syy > 0.0 {
        sxy / (sxx * syy).sqrt()
    } else {
        f64::NAN
    }
}

fn median(x: &[f64]) -> f64 {
    let mut s = x.to_vec();
    s.sort_by(f64::total_cmp);
    match s.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => s[n / 2],
        n => (s[n / 2 - 1] + s[n / 2]) / 2.0,
    }
}

/// The sd of the median over `draws` resamples (with replacement, as many as
/// there are), from a fixed seed.
fn bootstrap_median_sd(values: &[f64], draws: usize) -> f64 {
    let n = values.len();
    if n < 2 {
        return f64::INFINITY;
    }
    let mut rng = SplitMix64(0x6a6f_696e_7473_7072 ^ n as u64);
    let medians: Vec<f64> = (0..draws)
        .map(|_| {
            let sample: Vec<f64> = (0..n)
                .map(|_| values[(rng.next_u64() % n as u64) as usize])
                .collect();
            median(&sample)
        })
        .collect();
    let mean = medians.iter().sum::<f64>() / draws as f64;
    (medians.iter().map(|m| (m - mean) * (m - mean)).sum::<f64>() / (draws - 1) as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::correlation::tests::shipped;

    fn pick(cals: &[Calibration], assets: &[&str]) -> Vec<Calibration> {
        assets
            .iter()
            .map(|a| cals.iter().find(|c| c.meta.asset == *a).unwrap().clone())
            .collect()
    }

    #[test]
    fn spy_and_qqq_pass() {
        let (c, cals) = shipped();
        let r = check_joint(
            &c,
            &pick(&cals, &["spy", "qqq"]),
            &ModelSpec::default(),
            300,
            500,
        )
        .unwrap();
        assert_eq!(r.checks.len(), 1);
        assert_eq!(r.verdict, Verdict::Pass, "{r:#?}");
    }

    #[test]
    fn the_four_asset_block_passes() {
        let (c, cals) = shipped();
        let r = check_joint(
            &c,
            &pick(&cals, &["nvda", "tsla", "spy", "spcx"]),
            &ModelSpec::default(),
            300,
            500,
        )
        .unwrap();
        assert_eq!(r.checks.len(), 6);
        assert_eq!(r.verdict, Verdict::Pass, "{r:#?}");
    }

    #[test]
    fn shuffling_one_assets_sessions_fails() {
        let (c, cals) = shipped();
        let chosen = pick(&cals, &["spy", "qqq"]);
        let (assets, mut windows) =
            joint_windows(&c, &chosen, &ModelSpec::default(), 300, 500).unwrap();
        let honest = judge(&c, &assets, &windows).unwrap();
        assert!(honest.iter().all(|k| k.status == Status::Pass));
        windows[1].rotate_left(1);
        let shuffled = judge(&c, &assets, &windows).unwrap();
        assert!(
            shuffled.iter().any(|k| k.status == Status::Fail),
            "{shuffled:#?}"
        );
    }

    #[test]
    fn independent_draws_fail() {
        let (c, cals) = shipped();
        let chosen = pick(&cals, &["spy", "qqq"]);
        let identity = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        let (assets, windows) =
            draw_windows(&c, &chosen, &ModelSpec::default(), &identity, 300, 500).unwrap();
        let checks = judge(&c, &assets, &windows).unwrap();
        assert!(
            checks.iter().any(|k| k.status == Status::Fail),
            "{checks:#?}"
        );
    }

    #[test]
    fn too_few_sessions_are_not_judged() {
        let (c, cals) = shipped();
        let r = check_joint(
            &c,
            &pick(&cals, &["spy", "qqq"]),
            &ModelSpec::default(),
            10,
            1,
        )
        .unwrap();
        assert_eq!(r.verdict, Verdict::NotJudged);
    }

    #[test]
    fn a_pair_order_either_way_is_judged() {
        let (c, cals) = shipped();
        let r = check_joint(
            &c,
            &pick(&cals, &["qqq", "spy"]),
            &ModelSpec::default(),
            100,
            3,
        )
        .unwrap();
        assert_eq!(r.checks.len(), 1);
        assert_eq!(r.checks[0].name, "joint_corr_qqq_spy");
    }

    /// The calibrated draw's false-fail rate and the wrong draws' catch rates,
    /// on fresh seeds, for the SPY-QQQ pair, the four-asset block of cards 2
    /// and 5, and the five assets. The bars: at most 2% false fails, at least
    /// 95% catches, for sessions shuffled between assets and for independent
    /// draws. In release: `FFR_SEEDS=300 FFR_OFFSET=500 cargo test --release
    /// -p mtx-grader --lib joint_false_fail_rates -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn joint_false_fail_rates() {
        let env = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let (seeds, offset) = (env("FFR_SEEDS", 60), env("FFR_OFFSET", 0));
        let (c, cals) = shipped();
        let sets: [(&str, &[&str]); 3] = [
            ("spy-qqq", &["spy", "qqq"]),
            ("nvda-tsla-spy-spcx", &["nvda", "tsla", "spy", "spcx"]),
            ("all five", &["nvda", "qqq", "spcx", "spy", "tsla"]),
        ];
        let mut rows = Vec::new();
        for (label, assets) in sets {
            let chosen = pick(&cals, assets);
            let k = chosen.len();
            let identity: Vec<Vec<f64>> = (0..k)
                .map(|i| (0..k).map(|j| if i == j { 1.0 } else { 0.0 }).collect())
                .collect();
            let all: Vec<u64> = (offset..offset + seeds).collect();
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
            // (false fail, shuffled caught, independent caught) per seed.
            let outcomes: Vec<(bool, bool, bool)> = std::thread::scope(|scope| {
                let handles: Vec<_> = all
                    .chunks(all.len().div_ceil(threads))
                    .map(|chunk| {
                        let (c, chosen, identity) = (&c, &chosen, &identity);
                        scope.spawn(move || {
                            chunk
                                .iter()
                                .map(|&seed| {
                                    let spec = ModelSpec::default();
                                    let (names, mut w) =
                                        joint_windows(c, chosen, &spec, 300, seed).unwrap();
                                    let honest = judge(c, &names, &w).unwrap();
                                    let failed = honest.iter().any(|x| x.status == Status::Fail);
                                    w[1].rotate_left(1);
                                    let shuffled = judge(c, &names, &w).unwrap();
                                    let (names, w) =
                                        draw_windows(c, chosen, &spec, identity, 300, seed)
                                            .unwrap();
                                    let indep = judge(c, &names, &w).unwrap();
                                    (
                                        failed,
                                        shuffled.iter().any(|x| x.status == Status::Fail),
                                        indep.iter().any(|x| x.status == Status::Fail),
                                    )
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
            let n = outcomes.len();
            let ff = outcomes.iter().filter(|o| o.0).count();
            let sh = outcomes.iter().filter(|o| o.1).count();
            let ind = outcomes.iter().filter(|o| o.2).count();
            println!(
                "{label:<20} sessions 300: false fails {ff}/{n}, shuffled caught {sh}/{n}, independent caught {ind}/{n}"
            );
            rows.push((label, n, ff, sh, ind));
        }
        for (label, n, ff, sh, ind) in rows {
            assert!(
                ff as f64 <= 0.02 * n as f64,
                "{label}: {ff}/{n} false fails"
            );
            assert!(
                sh as f64 >= 0.95 * n as f64,
                "{label}: shuffled caught {sh}/{n}"
            );
            assert!(
                ind as f64 >= 0.95 * n as f64,
                "{label}: independent caught {ind}/{n}"
            );
        }
    }
}
