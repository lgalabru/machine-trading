//! Scoring v2: a second score, reported beside `reward` and `skill`, never
//! in their place (see `specs/` WAVE2_11, Part B as amended).
//!
//! The scale has three anchors, pooled over the set as `reward` is: doing
//! nothing scores 0, the reference 0.5, and the task's informed bot 1. The
//! informed bot knows every fact the task's data could tell in principle
//! (who the informed takers are, the regime, the fair-value model, the
//! funding process, the cost curves) but not the session's future
//! randomness, and trades the same markets under the same costs, limits and
//! latency: 1 is perfect reading and perfect execution, reachable by
//! definition. A bot that beats it scores above 1 in `raw`, flagged.
//!
//! Losses count `k` times (2 by default) before the sums, with no floor per
//! session, so a gamble with no edge scores no better than doing nothing. A
//! session that breaks its mandate or order, or ends liquidatable, counts as
//! losing the whole drawdown limit `k` times, the worst loss the mandate
//! allows: a breach never beats complying, and one late breach no longer
//! erases a whole grade. Calling a method the guard refuses, tampering with
//! the grade, or not being graded at all still scores 0.
//!
//! ```text
//! w(x)  = x if x ≥ 0, else k·x
//! Xw_s  = −k·L if session s breaches, else w(X_s − N_s)
//! Rw_s  = w(R_s − N_s)        Iw_s = w(I_s − N_s)
//! E_bot = Σ Xw_s   E_inf = Σ Iw_s   E_ref = max(Σ Rw_s, φ·E_inf)
//! raw   = 0.5·E_bot/E_ref                            if E_bot ≤ E_ref
//!       = 0.5 + 0.5·(E_bot − E_ref)/(E_inf − E_ref)  otherwise
//! ```
//!
//! `raw` is floored at −1 and not capped (below 0: worse than doing nothing;
//! above 1: better than the informed bot); the score is `raw` clamped to
//! [0, 1], and `edge`, `raw − 0.5`, is the bot's edge over the reference in
//! score points. A reference too weak to anchor anything (its weighted sum
//! under `φ` of the informed bot's) is floored there, and the score says so.
//!
//! A task without an informed bot yet falls back to a proxy, the session's
//! hindsight bound times a task constant `η`, flagged `ceiling: "proxy"`.
//! Otherwise the hindsight bound is never in the score: it is reported as
//! the luck headroom above the informed bot, what only knowing the
//! session's randomness would add.
//!
//! The gap to 1 is split into what it came from (see [`Gap`]). Doing
//! nothing is taken as PnL 0, which holds for a flat start with no carry; a
//! session that starts holding something needs a measured nop run, which v2
//! doesn't do yet, and refuses.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The loss weight `k` by default.
pub const LOSS_WEIGHT: f64 = 2.0;
/// The reference anchor's floor `φ`, as a share of the informed bot's, by
/// default.
pub const ANCHOR_FLOOR: f64 = 0.10;
/// Bootstrap resamples for the interval.
pub const RESAMPLES: usize = 2_000;
/// The interval's tails: the 5th and 95th percentiles.
const CI_TAIL: f64 = 0.05;

/// Where the 1.0 anchor comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Ceiling {
    /// The informed bot's PnL in each session, as its scenario carries it;
    /// without it on every session, the hindsight bound times `η`, when
    /// given, as a proxy.
    Informed { proxy_eta: Option<f64> },
    /// The ceiling PnL of each session, by scenario name, given outright.
    Given(BTreeMap<String, f64>),
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
    /// The informed bot's PnL, when the scenario carries it.
    pub informed_pnl_usd: Option<f64>,
    pub nop_pnl_usd: f64,
    /// The hindsight bound (see [`crate::scenario::Scenario::bound_usd`]).
    pub bound_usd: f64,
    /// `L`: the mandate's drawdown limit, the most a compliant session can lose.
    pub worst_loss_usd: f64,
    /// The mandate has no drawdown limit, and `L` is the session's hindsight
    /// bound instead.
    pub worst_loss_from_bound: bool,
    /// Why the session breached its mandate or order, if it did.
    pub breach: Option<String>,
    /// The bot held a position at some point.
    pub traded: bool,
    /// What the bot's trades made against the price at the time they were
    /// made: the session's markout against fair value when the scenario has
    /// one, else against the price the position is valued at (see
    /// [`mtx_kit::core::MarkoutMeter`]). Negative: spread, fees and impact
    /// paid. `None` without the session's steps.
    pub trade_markout_usd: Option<f64>,
}

/// Scoring v2's result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoreV2 {
    /// `raw` clamped to [0, 1]; 0 when [`Self::fatal`].
    pub score: f64,
    /// 0 = doing nothing, 0.5 = the reference, 1 = the informed bot; at
    /// least −1, and above 1 when the bot beats the informed bot.
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
    /// `raw` is above 1: the bot made more than the informed bot.
    pub beats_informed: bool,
    /// Why the grade scores 0 whatever its sessions made: a refused call,
    /// tampering, or no grade at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fatal: Option<String>,
    /// The reference's weighted PnL was under `φ` of the ceiling's, and the
    /// 0.5 anchor is set there instead.
    pub anchor_floored: bool,
    /// `E_bot`, `E_ref` (after the floor), `Σ Rw_s` (before it), `E_inf`.
    pub bot_usd: f64,
    pub reference_usd: f64,
    pub reference_measured_usd: f64,
    pub ceiling_usd: f64,
    pub nop_usd: f64,
    /// The sessions' hindsight bound, and what it adds above the ceiling:
    /// reported, never scored.
    pub bound_usd: f64,
    pub luck_headroom_usd: f64,
    pub breached_sessions: usize,
    /// Sessions whose mandate has no drawdown limit, where `L` is the
    /// hindsight bound instead.
    pub limit_from_bound_sessions: usize,
    /// `k`, `φ`, and where the ceiling came from: `informed` (the task's
    /// informed bot), `proxy` (`η` times the hindsight bound, for a task
    /// without one) or `given`.
    pub loss_weight: f64,
    pub anchor_floor: f64,
    pub ceiling: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta: Option<f64>,
    /// How doing nothing was valued: `zero` (flat start, no carry).
    pub nop: String,
    pub resamples: usize,
    /// The bootstrap's seed came from the grade's `eval_seed_sha256`; without
    /// one, or with one that isn't hex, from a fixed seed.
    pub seeded_from_eval: bool,
    /// Where the gap from `raw` to 1 came from.
    pub gap: Gap,
    pub sessions: Vec<SessionV2>,
}

/// The gap from `raw` to 1, the informed bot, split by where it came from,
/// in score points. The buckets are dollar amounts of `E_inf − E_bot`, each
/// converted at the grade's average rate, `(1 − raw) / (E_inf − E_bot)`, so
/// they sum to `1 − raw` exactly; `noise` is the interval's half-width, and
/// a bucket smaller than it isn't told from zero.
///
/// - `risk`: what the loss weight and breaches cost: `Σ (X_s − Xw_s)`, less
///   the same for the informed bot's losing sessions.
/// - `missed`: the whole gap in sessions the bot never held a position in.
/// - `execution`: in sessions it traded, what its trades paid against the
///   price at the time (`−` its trade markout): spread, fees and impact,
///   and funding, which moves cash the same way.
/// - `wrong_read`: the rest of the gap in sessions it traded: positions the
///   wrong way or the wrong size, adverse selection, trading against the
///   informed flow.
/// - `unattributed`: sessions whose steps weren't there to split.
///
/// The informed bot's own costs aren't known per session, so `execution`
/// is the bot's whole cost, not its excess over the informed bot's, and
/// `wrong_read` is net of the difference. A bucket can be negative: a maker
/// earns its spread, so its execution term reduces the gap.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Gap {
    pub wrong_read: f64,
    pub missed: f64,
    pub execution: f64,
    pub risk: f64,
    pub unattributed: f64,
    pub noise: f64,
    /// The same buckets in USD; they sum to `E_inf − E_bot`.
    pub usd: GapUsd,
}

/// [`Gap`]'s buckets in USD.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GapUsd {
    pub wrong_read: f64,
    pub missed: f64,
    pub execution: f64,
    pub risk: f64,
    pub unattributed: f64,
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
    // Without a floor, a reference that loses would anchor 0.5 at or below
    // doing nothing.
    if !(params.anchor_floor > 0.0 && params.anchor_floor < 1.0) {
        return Err(format!(
            "anchor floor {}: it must be in (0, 1)",
            params.anchor_floor
        ));
    }
    let (source, eta, ceilings) = ceilings(sessions, &params.ceiling)?;
    let weigh = |x: f64| if x < 0.0 { k * x } else { x };
    let terms: Vec<SessionV2> = sessions
        .iter()
        .zip(&ceilings)
        .map(|(s, ceiling)| SessionV2 {
            scenario: s.scenario.clone(),
            bot_usd: match s.breach {
                // At least the worst allowed loss, and never less than what
                // the session really lost: a breach doesn't cap a loss.
                Some(_) => (-k * s.worst_loss_usd).min(weigh(s.pnl_usd - s.nop_pnl_usd)),
                None => weigh(s.pnl_usd - s.nop_pnl_usd),
            },
            reference_usd: weigh(s.reference_pnl_usd - s.nop_pnl_usd),
            ceiling_usd: weigh(ceiling - s.nop_pnl_usd),
            nop_usd: s.nop_pnl_usd,
            breach: s.breach.clone(),
        })
        .collect();
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
    let seed = seed(eval_seed_sha256);
    let (ci_lo, ci_hi) = interval(&terms, params, seed.unwrap_or(FIXED_SEED));
    let bound_usd: f64 = sessions.iter().map(|s| s.bound_usd).sum();
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
        beats_informed: raw > 1.0,
        fatal,
        anchor_floored: sums.reference < params.anchor_floor * sums.ceiling,
        bot_usd: sums.bot,
        reference_usd: anchor,
        reference_measured_usd: sums.reference,
        ceiling_usd: sums.ceiling,
        nop_usd: sessions.iter().map(|s| s.nop_pnl_usd).sum(),
        bound_usd,
        luck_headroom_usd: bound_usd - sums.ceiling,
        breached_sessions: sessions.iter().filter(|s| s.breach.is_some()).count(),
        limit_from_bound_sessions: sessions.iter().filter(|s| s.worst_loss_from_bound).count(),
        ceiling: source.into(),
        eta,
        resamples: params.resamples,
        seeded_from_eval: seed.is_some(),
        gap: gap(sessions, &ceilings, &terms, raw, (ci_hi - ci_lo) / 2.0),
        sessions: terms,
        ..unscored(params, String::new())
    })
}

/// Each session's ceiling, and where they came from.
fn ceilings(
    sessions: &[Session],
    ceiling: &Ceiling,
) -> Result<(&'static str, Option<f64>, Vec<f64>), String> {
    match ceiling {
        Ceiling::Given(by_name) => {
            let given = sessions
                .iter()
                .map(|s| {
                    by_name
                        .get(&s.scenario)
                        .copied()
                        .ok_or_else(|| format!("no ceiling for {}", s.scenario))
                })
                .collect::<Result<_, _>>()?;
            Ok(("given", None, given))
        }
        Ceiling::Informed { proxy_eta } => {
            if let Some(informed) = sessions.iter().map(|s| s.informed_pnl_usd).collect() {
                return Ok(("informed", None, informed));
            }
            let eta = proxy_eta.ok_or_else(|| {
                "the scenarios carry no informed bot's PnL (informed_pnl_usd), and no η \
                 was given for a proxy ceiling"
                    .to_string()
            })?;
            let proxy = sessions.iter().map(|s| eta * s.bound_usd).collect();
            Ok(("proxy", Some(eta), proxy))
        }
    }
}

/// [`Gap`]: `E_inf − E_bot` split into buckets, then into score points.
fn gap(sessions: &[Session], ceilings: &[f64], terms: &[SessionV2], raw: f64, noise: f64) -> Gap {
    let mut usd = GapUsd::default();
    for ((s, ceiling), t) in sessions.iter().zip(ceilings).zip(terms) {
        let informed = ceiling - s.nop_pnl_usd;
        let bot = s.pnl_usd - s.nop_pnl_usd;
        // Xw_s and Iw_s against their unweighted values.
        usd.risk += (bot - t.bot_usd) - (informed - t.ceiling_usd);
        let behind = informed - bot;
        if !s.traded {
            usd.missed += behind;
        } else if let Some(markout) = s.trade_markout_usd {
            usd.execution -= markout;
            usd.wrong_read += behind + markout;
        } else {
            usd.unattributed += behind;
        }
    }
    let total = usd.wrong_read + usd.missed + usd.execution + usd.risk + usd.unattributed;
    let rate = if total.abs() > 1e-9 {
        (1.0 - raw) / total
    } else {
        0.0
    };
    Gap {
        wrong_read: usd.wrong_read * rate,
        missed: usd.missed * rate,
        execution: usd.execution * rate,
        risk: usd.risk * rate,
        unattributed: usd.unattributed * rate,
        noise,
        usd,
    }
}

/// v2 for a grade with no sessions to score because it was never graded
/// (or was tampered with before any ran): 0, with why.
pub fn unscored(params: &Params, reason: String) -> ScoreV2 {
    let (ceiling, eta) = match params.ceiling {
        Ceiling::Informed { proxy_eta } => ("informed", proxy_eta),
        Ceiling::Given(_) => ("given", None),
    };
    ScoreV2 {
        score: 0.0,
        raw: 0.0,
        edge: -0.5,
        ci_lo: 0.0,
        ci_hi: 0.0,
        within_noise: false,
        beats_informed: false,
        fatal: Some(reason),
        anchor_floored: false,
        bot_usd: 0.0,
        reference_usd: 0.0,
        reference_measured_usd: 0.0,
        ceiling_usd: 0.0,
        nop_usd: 0.0,
        bound_usd: 0.0,
        luck_headroom_usd: 0.0,
        breached_sessions: 0,
        limit_from_bound_sessions: 0,
        loss_weight: params.loss_weight,
        anchor_floor: params.anchor_floor,
        ceiling: ceiling.into(),
        eta,
        nop: "zero".into(),
        resamples: 0,
        seeded_from_eval: false,
        gap: Gap::default(),
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

    /// The bot on the scale, floored at −1.
    fn raw(&self, floor: f64) -> f64 {
        let anchor = self.anchor(floor);
        let raw = if self.bot <= anchor {
            0.5 * self.bot / anchor
        } else if self.ceiling > anchor {
            0.5 + 0.5 * (self.bot - anchor) / (self.ceiling - anchor)
        } else {
            1.0
        };
        raw.max(-1.0)
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
/// `eval_seed_sha256`, so a rescore draws the same resamples; `None`
/// without one, or when it isn't hex.
fn seed(eval_seed_sha256: Option<&str>) -> Option<u64> {
    eval_seed_sha256
        .and_then(|hash| hash.get(..16))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
}

/// The bootstrap's seed without one from the grade.
const FIXED_SEED: u64 = 0x6d74_785f_7632;

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

    /// Sessions with a hindsight bound of 1,000 and an informed bot making
    /// 150 in each: 900 over six.
    fn set(pnls: &[f64], references: &[f64]) -> Vec<Session> {
        pnls.iter()
            .zip(references)
            .enumerate()
            .map(|(i, (&pnl_usd, &reference_pnl_usd))| Session {
                scenario: format!("hidden-{i}"),
                pnl_usd,
                reference_pnl_usd,
                informed_pnl_usd: Some(150.0),
                nop_pnl_usd: 0.0,
                bound_usd: 1_000.0,
                worst_loss_usd: 100.0,
                worst_loss_from_bound: false,
                breach: None,
                traded: pnl_usd != 0.0,
                trade_markout_usd: Some(-5.0),
            })
            .collect()
    }

    const REFERENCE: [f64; 6] = [40.0, -10.0, 25.0, 60.0, 0.0, 15.0];

    fn informed() -> Params {
        Params::new(Ceiling::Informed { proxy_eta: None })
    }

    fn proxy(eta: f64) -> Params {
        Params::new(Ceiling::Informed {
            proxy_eta: Some(eta),
        })
    }

    fn without_informed(mut sessions: Vec<Session>) -> Vec<Session> {
        for s in &mut sessions {
            s.informed_pnl_usd = None;
        }
        sessions
    }

    #[test]
    fn the_reference_scores_one_half_and_doing_nothing_zero() {
        let reference = score(&set(&REFERENCE, &REFERENCE), &informed(), None, None).unwrap();
        assert_eq!(reference.raw, 0.5);
        assert_eq!(reference.edge, 0.0);
        assert!(!reference.anchor_floored);
        assert_eq!(reference.ceiling, "informed");
        let nop = score(&set(&[0.0; 6], &REFERENCE), &informed(), None, None).unwrap();
        assert_eq!(nop.raw, 0.0);
        assert_eq!(nop.score, 0.0);
    }

    #[test]
    fn the_informed_bot_scores_one_and_beating_it_more() {
        let at = score(&set(&[150.0; 6], &REFERENCE), &informed(), None, None).unwrap();
        assert!((at.raw - 1.0).abs() < 1e-12, "{}", at.raw);
        assert!(!at.beats_informed);
        let above = score(&set(&[228.0; 6], &REFERENCE), &informed(), None, None).unwrap();
        // (1,368 − 120) / (900 − 120) = 1.6: raw 0.5 + 0.8.
        assert!((above.raw - 1.3).abs() < 1e-12, "{}", above.raw);
        assert!(above.beats_informed);
        assert_eq!(above.score, 1.0);
    }

    #[test]
    fn the_hindsight_bound_is_reported_not_scored() {
        let v2 = score(&set(&REFERENCE, &REFERENCE), &informed(), None, None).unwrap();
        assert_eq!(v2.bound_usd, 6_000.0);
        assert_eq!(v2.luck_headroom_usd, 6_000.0 - 900.0);
        let mut wider = set(&REFERENCE, &REFERENCE);
        for s in &mut wider {
            s.bound_usd *= 3.0;
        }
        assert_eq!(score(&wider, &informed(), None, None).unwrap().raw, v2.raw);
    }

    #[test]
    fn without_an_informed_bot_the_proxy_is_used_and_flagged() {
        let sessions = without_informed(set(&REFERENCE, &REFERENCE));
        let v2 = score(&sessions, &proxy(0.15), None, None).unwrap();
        assert_eq!(v2.ceiling, "proxy");
        assert_eq!(v2.eta, Some(0.15));
        assert_eq!(v2.ceiling_usd, 900.0);
        assert!(
            score(&sessions, &informed(), None, None)
                .unwrap_err()
                .contains("informed")
        );
        // With the informed bot's PnL on every session, η isn't used.
        let both = score(&set(&REFERENCE, &REFERENCE), &proxy(0.5), None, None).unwrap();
        assert_eq!(both.ceiling, "informed");
        assert_eq!(both.eta, None);
    }

    /// The analysis's formula, worked by hand: losses count twice, the
    /// reference's sum is 40 − 20 + 25 + 60 + 0 + 15 = 120, the informed
    /// bot's 6 × 150 = 900.
    #[test]
    fn losses_count_twice_in_the_pooled_sums() {
        let bot = [100.0, -30.0, 50.0, -10.0, 20.0, 0.0];
        let v2 = score(&set(&bot, &REFERENCE), &informed(), None, None).unwrap();
        // E_bot = 100 − 60 + 50 − 20 + 20 + 0 = 90, under the reference's 120.
        assert_eq!(v2.bot_usd, 90.0);
        assert_eq!(v2.reference_usd, 120.0);
        assert_eq!(v2.ceiling_usd, 900.0);
        assert_eq!(v2.raw, 0.5 * 90.0 / 120.0);
        let better = [400.0, 0.0, 50.0, 60.0, 20.0, 0.0];
        let v2 = score(&set(&better, &REFERENCE), &informed(), None, None).unwrap();
        assert_eq!(v2.raw, 0.5 + 0.5 * (530.0 - 120.0) / (900.0 - 120.0));
    }

    #[test]
    fn an_informed_bots_losses_count_twice_too() {
        let mut sessions = set(&REFERENCE, &REFERENCE);
        sessions[0].informed_pnl_usd = Some(-50.0);
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        assert_eq!(v2.ceiling_usd, 750.0 - 100.0);
    }

    #[test]
    fn a_breach_counts_as_the_worst_allowed_loss() {
        let mut sessions = set(&[60.0; 6], &REFERENCE);
        let compliant_worst = {
            let mut s = sessions.clone();
            s[5].pnl_usd = -100.0;
            score(&s, &informed(), None, None).unwrap()
        };
        sessions[5].pnl_usd = 500.0;
        sessions[5].breach = Some("drew down past its limit".into());
        let breached = score(&sessions, &informed(), None, None).unwrap();
        assert_eq!(breached.sessions[5].bot_usd, -200.0);
        assert_eq!(breached.breached_sessions, 1);
        assert_eq!(breached.raw, compliant_worst.raw);
        // One breach no longer zeroes the set.
        assert!(breached.score > 0.0);
    }

    #[test]
    fn a_breach_never_caps_a_loss() {
        let mut sessions = set(&[60.0; 6], &REFERENCE);
        for s in &mut sessions {
            s.worst_loss_usd = 1_000.0;
        }
        sessions[2].pnl_usd = -9_000.0;
        sessions[2].breach = Some("drew down past its limit".into());
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        assert_eq!(v2.sessions[2].bot_usd, -18_000.0);
        assert!(v2.gap.usd.risk >= 0.0 && v2.gap.risk >= 0.0, "{:?}", v2.gap);
        // A breach that made money still counts the worst allowed loss.
        sessions[2].pnl_usd = 400.0;
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        assert_eq!(v2.sessions[2].bot_usd, -2_000.0);
        assert!(v2.gap.usd.risk >= 0.0);
    }

    #[test]
    fn the_anchor_floor_must_be_above_zero() {
        for floor in [0.0, -0.1, 1.0] {
            let mut params = informed();
            params.anchor_floor = floor;
            let refused = score(&set(&REFERENCE, &REFERENCE), &params, None, None);
            assert!(refused.unwrap_err().contains("anchor floor"), "{floor}");
        }
    }

    #[test]
    fn only_a_hex_eval_seed_seeds_the_bootstrap() {
        let sessions = set(&REFERENCE, &REFERENCE);
        let not_hex = score(&sessions, &informed(), None, Some("not a sha256 at all")).unwrap();
        assert!(!not_hex.seeded_from_eval);
        let none = score(&sessions, &informed(), None, None).unwrap();
        assert!(!none.seeded_from_eval);
        assert_eq!((not_hex.ci_lo, not_hex.ci_hi), (none.ci_lo, none.ci_hi));
    }

    #[test]
    fn a_limit_taken_from_the_bound_is_reported() {
        let mut sessions = set(&REFERENCE, &REFERENCE);
        sessions[0].worst_loss_from_bound = true;
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        assert_eq!(v2.limit_from_bound_sessions, 1);
    }

    #[test]
    fn a_fatal_grade_scores_zero_and_reports_what_it_made() {
        let v2 = score(
            &set(&[60.0; 6], &REFERENCE),
            &informed(),
            Some("called methods bots may not use: getProgramAccounts".into()),
            None,
        )
        .unwrap();
        assert_eq!(v2.score, 0.0);
        assert!(v2.raw > 0.0);
    }

    #[test]
    fn a_weak_reference_is_floored_and_flagged() {
        let weak = [1.0, -2.0, 1.0, 0.0, 1.0, 0.0];
        let v2 = score(&set(&weak, &weak), &informed(), None, None).unwrap();
        assert!(v2.anchor_floored);
        assert_eq!(v2.reference_measured_usd, -1.0);
        assert!((v2.reference_usd - 0.10 * 900.0).abs() < 1e-9);
        // The reference itself now scores below 0.5, and nop still 0.
        assert!(v2.raw < 0.0);
        let nop = score(&set(&[0.0; 6], &weak), &informed(), None, None).unwrap();
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
                    let mut v2 = informed();
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
            let raw = score(&set(&pnl, &REFERENCE), &informed(), None, None)
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
        let a = score(&set(&bot, &REFERENCE), &informed(), None, hash).unwrap();
        let b = score(&set(&bot, &REFERENCE), &informed(), None, hash).unwrap();
        assert_eq!((a.ci_lo, a.ci_hi), (b.ci_lo, b.ci_hi));
        assert!(a.seeded_from_eval);
        assert!(a.ci_lo <= a.raw && a.raw <= a.ci_hi);
        // Close to the reference on six sessions: within noise.
        assert!(a.within_noise);
        let far = score(&set(&[300.0; 6], &REFERENCE), &informed(), None, hash).unwrap();
        assert!(!far.within_noise);
    }

    #[test]
    fn the_gap_buckets_sum_to_what_is_left_to_one() {
        // Traded at a cost of 5 a session, sat out one, lost twice in one.
        let bot = [100.0, -30.0, 50.0, 0.0, 20.0, 80.0];
        let mut sessions = set(&bot, &REFERENCE);
        sessions[5].breach = Some("went over the cap".into());
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        let g = &v2.gap;
        let sum = g.wrong_read + g.missed + g.execution + g.risk + g.unattributed;
        assert!(
            (sum - (1.0 - v2.raw)).abs() < 1e-12,
            "{sum} vs {}",
            1.0 - v2.raw
        );
        let usd = &g.usd;
        let usd_sum = usd.wrong_read + usd.missed + usd.execution + usd.risk + usd.unattributed;
        assert!((usd_sum - (v2.ceiling_usd - v2.bot_usd)).abs() < 1e-9);
        // Session 3 never traded: its whole 150 was missed.
        assert_eq!(usd.missed, 150.0);
        // Five traded sessions paid 5 each.
        assert_eq!(usd.execution, 25.0);
        // The loss counted again (30), and the breach: 80 made, −200 scored.
        assert_eq!(usd.risk, 30.0 + 280.0);
        assert_eq!(g.noise, (v2.ci_hi - v2.ci_lo) / 2.0);
        assert!(g.noise > 0.0);
    }

    #[test]
    fn given_ceilings_replace_the_informed_bot() {
        let ceilings: BTreeMap<String, f64> = (0..6)
            .map(|i| (format!("hidden-{i}"), 200.0 + i as f64))
            .collect();
        let v2 = score(
            &set(&REFERENCE, &REFERENCE),
            &Params::new(Ceiling::Given(ceilings)),
            None,
            None,
        )
        .unwrap();
        assert_eq!(v2.ceiling_usd, 1_215.0);
        assert_eq!(v2.ceiling, "given");
        assert_eq!(v2.eta, None);
        let missing = score(
            &set(&REFERENCE, &REFERENCE),
            &Params::new(Ceiling::Given(BTreeMap::new())),
            None,
            None,
        );
        assert!(missing.unwrap_err().contains("no ceiling for hidden-0"));
    }

    #[test]
    fn a_ceiling_below_the_reference_is_refused() {
        let low = score(
            &without_informed(set(&REFERENCE, &REFERENCE)),
            &proxy(0.01),
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
