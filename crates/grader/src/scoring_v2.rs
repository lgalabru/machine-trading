//! Scoring v2: a second score, reported beside `reward` and `skill`, never
//! in their place (see `specs/` WAVE2_11, Part B as amended).
//!
//! The scale has three anchors, pooled over the set as `reward` is: doing
//! nothing scores 0, the reference 0.5, and a realistic ceiling 1. Losses
//! count `k` times (2 by default) before the sums, with no floor per session,
//! so a gamble with no edge scores no better than doing nothing. A session
//! that breaks its mandate or order, or ends liquidatable, counts as losing
//! the whole drawdown limit `k` times, the worst loss the mandate allows: a
//! breach never beats complying, and one late breach no longer erases a
//! whole grade. Calling a method the guard refuses, tampering with the
//! grade, or not being graded at all still scores 0.
//!
//! ```text
//! w(x)  = x if x ≥ 0, else k·x
//! Xw_s  = −k·L if session s breaches, else w(X_s − N_s)
//! Rw_s  = w(R_s − N_s)        Cw_s = C_s − N_s
//! E_bot = Σ Xw_s   E_ceil = Σ Cw_s   E_ref = max(Σ Rw_s, φ·E_ceil)
//! raw   = 0.5·E_bot/E_ref                              if E_bot ≤ E_ref
//!       = 0.5 + 0.5·(E_bot − E_ref)/(E_ceil − E_ref)   otherwise
//! ```
//!
//! `raw` is clamped to [−1, 1] and reported (below 0: worse than doing
//! nothing); the score is `raw` clamped to [0, 1], and `edge`, `raw − 0.5`,
//! is the bot's edge over the reference in score points. A reference too
//! weak to anchor anything (its weighted sum under `φ` of the ceiling's) is
//! floored there, and the score says so.
//!
//! The ceiling is an input: the session's hindsight bound times a task's
//! efficiency `η`, until a cost-aware hindsight ceiling exists, or explicit
//! per-session values. Doing nothing is taken as PnL 0, which holds for a
//! flat start with no carry; a session that starts holding something needs a
//! measured nop run, which v2 doesn't do yet, and refuses.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The loss weight `k` by default.
pub const LOSS_WEIGHT: f64 = 2.0;
/// The reference anchor's floor `φ`, as a share of the ceiling, by default.
pub const ANCHOR_FLOOR: f64 = 0.10;
/// Bootstrap resamples for the interval.
pub const RESAMPLES: usize = 2_000;
/// The interval's tails: the 5th and 95th percentiles.
const CI_TAIL: f64 = 0.05;

/// Where the 1.0 anchor comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Ceiling {
    /// Each session's hindsight bound times the task's efficiency `η`.
    BoundShare(f64),
    /// Each session's ceiling PnL, by scenario name.
    PerSession(BTreeMap<String, f64>),
}

/// What v2 is computed with; reported with the score.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    /// `k`: how many times a loss counts.
    pub loss_weight: f64,
    /// `φ`: the reference anchor is at least this share of the ceiling.
    pub anchor_floor: f64,
    pub ceiling: Ceiling,
    pub resamples: usize,
}

impl Params {
    /// The defaults, with the ceiling given.
    pub fn new(ceiling: Ceiling) -> Self {
        Self {
            loss_weight: LOSS_WEIGHT,
            anchor_floor: ANCHOR_FLOOR,
            ceiling,
            resamples: RESAMPLES,
        }
    }
}

/// One session, as v2 reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub scenario: String,
    pub pnl_usd: f64,
    pub reference_pnl_usd: f64,
    pub nop_pnl_usd: f64,
    /// The hindsight bound (see [`crate::scenario::Scenario::bound_usd`]).
    pub bound_usd: f64,
    /// `L`: the mandate's drawdown limit, the most a compliant session can lose.
    pub worst_loss_usd: f64,
    /// Why the session breached its mandate or order, if it did.
    pub breach: Option<String>,
}

/// Scoring v2's result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoreV2 {
    /// `raw` clamped to [0, 1]; 0 when [`Self::fatal`].
    pub score: f64,
    /// 0 = doing nothing, 0.5 = the reference, 1 = the ceiling; in [−1, 1].
    pub raw: f64,
    /// `raw − 0.5`: the edge over the reference.
    pub edge: f64,
    /// The 5th and 95th percentiles of `raw` over a paired bootstrap of the
    /// sessions.
    pub ci_lo: f64,
    pub ci_hi: f64,
    /// The interval of `edge` spans 0: the bot isn't told apart from the
    /// reference on this set.
    pub within_noise: bool,
    /// Why the grade scores 0 whatever its sessions made: a refused call,
    /// tampering, or no grade at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fatal: Option<String>,
    /// The reference's weighted PnL was under `φ` of the ceiling's, and the
    /// 0.5 anchor is set there instead.
    pub anchor_floored: bool,
    /// `E_bot`, `E_ref` (after the floor), `Σ Rw_s` (before it), `E_ceil`.
    pub bot_usd: f64,
    pub reference_usd: f64,
    pub reference_measured_usd: f64,
    pub ceiling_usd: f64,
    pub nop_usd: f64,
    pub breached_sessions: usize,
    /// `k`, `φ`, and where the ceiling came from (`η`, for a bound share).
    pub loss_weight: f64,
    pub anchor_floor: f64,
    pub ceiling: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta: Option<f64>,
    /// How doing nothing was valued: `zero` (flat start, no carry).
    pub nop: String,
    pub resamples: usize,
    /// The bootstrap's seed came from the grade's `eval_seed_sha256`; without
    /// one, from a fixed seed.
    pub seeded_from_eval: bool,
    pub sessions: Vec<SessionV2>,
}

/// One session's weighted terms in the sums.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionV2 {
    pub scenario: String,
    pub bot_usd: f64,
    pub reference_usd: f64,
    pub ceiling_usd: f64,
    pub nop_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breach: Option<String>,
}

/// Score `sessions` under v2. `fatal` zeroes the score (but not `raw`);
/// `eval_seed_sha256`, when the grade has one, seeds the bootstrap.
pub fn score(
    sessions: &[Session],
    params: &Params,
    fatal: Option<String>,
    eval_seed_sha256: Option<&str>,
) -> Result<ScoreV2, String> {
    if sessions.is_empty() {
        return Err("no sessions to score".into());
    }
    let k = params.loss_weight;
    if !(k >= 1.0 && k.is_finite()) {
        return Err(format!("loss weight {k}: it must be at least 1"));
    }
    if !(0.0..1.0).contains(&params.anchor_floor) {
        return Err(format!(
            "anchor floor {}: it must be in [0, 1)",
            params.anchor_floor
        ));
    }
    let weigh = |x: f64| if x < 0.0 { k * x } else { x };
    let mut terms = Vec::with_capacity(sessions.len());
    for s in sessions {
        let ceiling = match &params.ceiling {
            Ceiling::BoundShare(eta) => eta * s.bound_usd,
            Ceiling::PerSession(by_name) => *by_name
                .get(&s.scenario)
                .ok_or_else(|| format!("no ceiling for {}", s.scenario))?,
        };
        terms.push(SessionV2 {
            scenario: s.scenario.clone(),
            bot_usd: match s.breach {
                Some(_) => -k * s.worst_loss_usd,
                None => weigh(s.pnl_usd - s.nop_pnl_usd),
            },
            reference_usd: weigh(s.reference_pnl_usd - s.nop_pnl_usd),
            ceiling_usd: ceiling - s.nop_pnl_usd,
            nop_usd: s.nop_pnl_usd,
            breach: s.breach.clone(),
        });
    }
    let sums = Sums::of(terms.iter());
    if sums.ceiling <= 0.0 {
        return Err(format!(
            "the ceiling sums to {} over the set: nothing to scale to",
            sums.ceiling
        ));
    }
    let anchor = sums.anchor(params.anchor_floor);
    if anchor >= sums.ceiling {
        return Err(format!(
            "the reference ({anchor}) reaches the ceiling ({}): the ceiling is too low",
            sums.ceiling
        ));
    }
    let raw = sums.raw(params.anchor_floor);
    let (ci_lo, ci_hi) = interval(&terms, params, seed(eval_seed_sha256));
    Ok(ScoreV2 {
        score: if fatal.is_some() {
            0.0
        } else {
            raw.clamp(0.0, 1.0)
        },
        raw,
        edge: raw - 0.5,
        ci_lo,
        ci_hi,
        within_noise: ci_lo <= 0.5 && 0.5 <= ci_hi,
        fatal,
        anchor_floored: sums.reference < params.anchor_floor * sums.ceiling,
        bot_usd: sums.bot,
        reference_usd: anchor,
        reference_measured_usd: sums.reference,
        ceiling_usd: sums.ceiling,
        nop_usd: sessions.iter().map(|s| s.nop_pnl_usd).sum(),
        breached_sessions: sessions.iter().filter(|s| s.breach.is_some()).count(),
        resamples: params.resamples,
        seeded_from_eval: eval_seed_sha256.is_some(),
        sessions: terms,
        ..unscored(params, String::new())
    })
}

/// v2 for a grade with no sessions to score because it was never graded
/// (or was tampered with before any ran): 0, with why.
pub fn unscored(params: &Params, reason: String) -> ScoreV2 {
    let (ceiling, eta) = match params.ceiling {
        Ceiling::BoundShare(eta) => ("bound_share", Some(eta)),
        Ceiling::PerSession(_) => ("per_session", None),
    };
    ScoreV2 {
        score: 0.0,
        raw: 0.0,
        edge: -0.5,
        ci_lo: 0.0,
        ci_hi: 0.0,
        within_noise: false,
        fatal: Some(reason),
        anchor_floored: false,
        bot_usd: 0.0,
        reference_usd: 0.0,
        reference_measured_usd: 0.0,
        ceiling_usd: 0.0,
        nop_usd: 0.0,
        breached_sessions: 0,
        loss_weight: params.loss_weight,
        anchor_floor: params.anchor_floor,
        ceiling: ceiling.into(),
        eta,
        nop: "zero".into(),
        resamples: 0,
        seeded_from_eval: false,
        sessions: Vec::new(),
    }
}

/// The weighted sums over some sessions.
struct Sums {
    bot: f64,
    reference: f64,
    ceiling: f64,
}

impl Sums {
    fn of<'a>(terms: impl Iterator<Item = &'a SessionV2>) -> Self {
        let mut sums = Self {
            bot: 0.0,
            reference: 0.0,
            ceiling: 0.0,
        };
        for t in terms {
            sums.bot += t.bot_usd;
            sums.reference += t.reference_usd;
            sums.ceiling += t.ceiling_usd;
        }
        sums
    }

    /// `E_ref`: the reference's sum, floored at `φ` of the ceiling's.
    fn anchor(&self, floor: f64) -> f64 {
        self.reference.max(floor * self.ceiling)
    }

    /// The bot on the scale, clamped to [−1, 1].
    fn raw(&self, floor: f64) -> f64 {
        let anchor = self.anchor(floor);
        let raw = if self.bot <= anchor {
            0.5 * self.bot / anchor
        } else if self.ceiling > anchor {
            0.5 + 0.5 * (self.bot - anchor) / (self.ceiling - anchor)
        } else {
            1.0
        };
        raw.clamp(-1.0, 1.0)
    }
}

/// The 5th and 95th percentiles of `raw` over resamples of the sessions,
/// each drawn with replacement and used for the bot, the reference and the
/// ceiling alike.
fn interval(terms: &[SessionV2], params: &Params, seed: u64) -> (f64, f64) {
    let mut rng = SplitMix64(seed);
    let n = terms.len();
    let mut raws: Vec<f64> = (0..params.resamples)
        .map(|_| {
            let sums = Sums::of((0..n).map(|_| &terms[rng.below(n)]));
            if sums.ceiling <= 0.0 {
                return 0.0;
            }
            sums.raw(params.anchor_floor)
        })
        .collect();
    if raws.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    raws.sort_by(f64::total_cmp);
    (percentile(&raws, CI_TAIL), percentile(&raws, 1.0 - CI_TAIL))
}

/// The `q` quantile of sorted values, interpolated between neighbours.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    let at = q * (sorted.len() - 1) as f64;
    let (lo, hi) = (at.floor() as usize, at.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (at - lo as f64)
}

/// The bootstrap's seed: the first 16 hex digits of the grade's
/// `eval_seed_sha256`, so a rescore draws the same resamples; a fixed one
/// without it.
fn seed(eval_seed_sha256: Option<&str>) -> u64 {
    eval_seed_sha256
        .and_then(|hash| hash.get(..16))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .unwrap_or(0x6d74_785f_7632)
}

/// A small, portable generator: the same draws on every platform and
/// version, which a library's generator doesn't promise.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next()) * n as u128) >> 64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(pnls: &[f64], references: &[f64]) -> Vec<Session> {
        pnls.iter()
            .zip(references)
            .enumerate()
            .map(|(i, (&pnl_usd, &reference_pnl_usd))| Session {
                scenario: format!("hidden-{i}"),
                pnl_usd,
                reference_pnl_usd,
                nop_pnl_usd: 0.0,
                bound_usd: 1_000.0,
                worst_loss_usd: 100.0,
                breach: None,
            })
            .collect()
    }

    const REFERENCE: [f64; 6] = [40.0, -10.0, 25.0, 60.0, 0.0, 15.0];

    fn bound_share() -> Params {
        Params::new(Ceiling::BoundShare(0.3))
    }

    #[test]
    fn the_reference_scores_one_half_and_doing_nothing_zero() {
        let reference = score(&set(&REFERENCE, &REFERENCE), &bound_share(), None, None).unwrap();
        assert_eq!(reference.raw, 0.5);
        assert_eq!(reference.edge, 0.0);
        assert!(!reference.anchor_floored);
        let nop = score(&set(&[0.0; 6], &REFERENCE), &bound_share(), None, None).unwrap();
        assert_eq!(nop.raw, 0.0);
        assert_eq!(nop.score, 0.0);
    }

    #[test]
    fn a_bot_at_the_ceiling_on_every_session_scores_one() {
        let at = score(&set(&[300.0; 6], &REFERENCE), &bound_share(), None, None).unwrap();
        assert!((at.raw - 1.0).abs() < 1e-12, "{}", at.raw);
        let above = score(&set(&[900.0; 6], &REFERENCE), &bound_share(), None, None).unwrap();
        assert_eq!(above.raw, 1.0);
    }

    /// The analysis's formula, worked by hand: losses count twice, the
    /// reference's sum is 40 − 20 + 25 + 60 + 0 + 15 = 120, the ceiling's
    /// 6 × 300 = 1,800.
    #[test]
    fn losses_count_twice_in_the_pooled_sums() {
        let bot = [100.0, -30.0, 50.0, -10.0, 20.0, 0.0];
        let v2 = score(&set(&bot, &REFERENCE), &bound_share(), None, None).unwrap();
        // E_bot = 100 − 60 + 50 − 20 + 20 + 0 = 90, under the reference's 120.
        assert_eq!(v2.bot_usd, 90.0);
        assert_eq!(v2.reference_usd, 120.0);
        assert_eq!(v2.ceiling_usd, 1_800.0);
        assert_eq!(v2.raw, 0.5 * 90.0 / 120.0);
        let better = [400.0, 0.0, 50.0, 60.0, 20.0, 0.0];
        let v2 = score(&set(&better, &REFERENCE), &bound_share(), None, None).unwrap();
        assert_eq!(v2.raw, 0.5 + 0.5 * (530.0 - 120.0) / (1_800.0 - 120.0));
    }

    #[test]
    fn a_breach_counts_as_the_worst_allowed_loss() {
        let mut sessions = set(&[60.0; 6], &REFERENCE);
        let compliant_worst = {
            let mut s = sessions.clone();
            s[5].pnl_usd = -100.0;
            score(&s, &bound_share(), None, None).unwrap()
        };
        sessions[5].pnl_usd = 500.0;
        sessions[5].breach = Some("drew down past its limit".into());
        let breached = score(&sessions, &bound_share(), None, None).unwrap();
        assert_eq!(breached.sessions[5].bot_usd, -200.0);
        assert_eq!(breached.breached_sessions, 1);
        assert_eq!(breached.raw, compliant_worst.raw);
        // One breach no longer zeroes the set.
        assert!(breached.score > 0.0);
    }

    #[test]
    fn a_fatal_grade_scores_zero_and_reports_what_it_made() {
        let v2 = score(
            &set(&[60.0; 6], &REFERENCE),
            &bound_share(),
            Some("called methods bots may not use: getProgramAccounts".into()),
            None,
        )
        .unwrap();
        assert_eq!(v2.score, 0.0);
        assert!(v2.raw > 0.5);
    }

    #[test]
    fn a_weak_reference_is_floored_and_flagged() {
        let weak = [1.0, -2.0, 1.0, 0.0, 1.0, 0.0];
        let v2 = score(&set(&weak, &weak), &bound_share(), None, None).unwrap();
        assert!(v2.anchor_floored);
        assert_eq!(v2.reference_measured_usd, -1.0);
        assert!((v2.reference_usd - 0.10 * 1_800.0).abs() < 1e-9);
        // The reference itself now scores below 0.5, and nop still 0.
        assert!(v2.raw < 0.0);
        let nop = score(&set(&[0.0; 6], &weak), &bound_share(), None, None).unwrap();
        assert_eq!(nop.raw, 0.0);
    }

    /// With no edge, a coin-flip gamble of any size scores no better than
    /// doing nothing in expectation: what a per-session floor would leak.
    #[test]
    fn a_gamble_with_no_edge_does_not_beat_doing_nothing() {
        let n = 72;
        let reference: Vec<f64> = (0..n).map(|i| 20.0 + (i % 5) as f64).collect();
        let mut rng = SplitMix64(7);
        for size in [10.0, 50.0, 90.0] {
            let runs = 400;
            let mean = (0..runs)
                .map(|_| {
                    let pnl: Vec<f64> = (0..n)
                        .map(|_| if rng.next() & 1 == 0 { size } else { -size })
                        .collect();
                    let mut v2 = Params::new(Ceiling::BoundShare(0.3));
                    v2.resamples = 0;
                    score(&set(&pnl, &reference), &v2, None, None)
                        .unwrap()
                        .score
                })
                .sum::<f64>()
                / runs as f64;
            assert!(mean < 0.02, "a ±{size} gamble scores {mean}");
        }
    }

    #[test]
    fn the_score_rises_with_any_session_pnl() {
        let mut pnl = [10.0, -40.0, 5.0, 0.0, 30.0, -5.0];
        let mut last = f64::NEG_INFINITY;
        for step in 0..200 {
            pnl[step % 6] += 7.5;
            let raw = score(&set(&pnl, &REFERENCE), &bound_share(), None, None)
                .unwrap()
                .raw;
            assert!(raw >= last, "step {step}: {raw} < {last}");
            last = raw;
        }
    }

    #[test]
    fn the_interval_is_the_same_for_the_same_seed_and_flags_noise() {
        let bot = [55.0, -5.0, 20.0, 70.0, 0.0, 10.0];
        let hash = Some("eb93508e26b157978d5b1af6ed1a259e18140ff097f623a364547f84a2862560");
        let a = score(&set(&bot, &REFERENCE), &bound_share(), None, hash).unwrap();
        let b = score(&set(&bot, &REFERENCE), &bound_share(), None, hash).unwrap();
        assert_eq!((a.ci_lo, a.ci_hi), (b.ci_lo, b.ci_hi));
        assert!(a.seeded_from_eval);
        assert!(a.ci_lo <= a.raw && a.raw <= a.ci_hi);
        // Close to the reference on six sessions: within noise.
        assert!(a.within_noise);
        let far = score(&set(&[300.0; 6], &REFERENCE), &bound_share(), None, hash).unwrap();
        assert!(!far.within_noise);
    }

    #[test]
    fn explicit_ceilings_replace_the_bound_share() {
        let ceilings: BTreeMap<String, f64> = (0..6)
            .map(|i| (format!("hidden-{i}"), 200.0 + i as f64))
            .collect();
        let v2 = score(
            &set(&REFERENCE, &REFERENCE),
            &Params::new(Ceiling::PerSession(ceilings)),
            None,
            None,
        )
        .unwrap();
        assert_eq!(v2.ceiling_usd, 1_215.0);
        assert_eq!(v2.ceiling, "per_session");
        assert_eq!(v2.eta, None);
        let missing = score(
            &set(&REFERENCE, &REFERENCE),
            &Params::new(Ceiling::PerSession(BTreeMap::new())),
            None,
            None,
        );
        assert!(missing.unwrap_err().contains("no ceiling for hidden-0"));
    }

    #[test]
    fn a_ceiling_below_the_reference_is_refused() {
        let low = score(
            &set(&REFERENCE, &REFERENCE),
            &Params::new(Ceiling::BoundShare(0.01)),
            None,
            None,
        );
        assert!(low.unwrap_err().contains("too low"));
    }

    #[test]
    fn percentiles_interpolate_as_numpy_does() {
        let sorted = [0.0, 1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&sorted, 0.05), 0.2);
        assert_eq!(percentile(&sorted, 0.95), 3.8);
    }
}
