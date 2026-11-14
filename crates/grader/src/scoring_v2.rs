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
//! Losses count `k` times (1.5 by default) before the sums, with no floor
//! per session, so a gamble with no edge scores no better than doing
//! nothing. The price is that a profitable strategy whose sessions are too
//! noisy for its edge also scores below doing nothing: with normally
//! distributed session PnL, below a per-session Sharpe (mean over standard
//! deviation) of about 0.16 at k = 1.5 (0.28 at k = 2), reported with the
//! score as `break_even_sharpe`. A session that breaks its mandate or order,
//! or ends liquidatable with no liquidation keeper (a keeper's liquidation
//! is priced, not a breach), counts at least as losing the whole drawdown limit
//! `k` times, the worst loss the mandate allows, and its real weighted loss
//! when that is worse: a breach never beats complying, never caps a loss,
//! and one late breach no longer erases a whole grade. Calling a method the
//! guard refuses, tampering with the grade, or not being graded at all
//! scores `raw` −1, the floor, so failing on purpose never beats losing.
//!
//! ```text
//! w(x)  = x if x ≥ 0, else k·x
//! Xw_s  = min(−k·L, w(X_s − N_s)) if session s breaches, else w(X_s − N_s)
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
//! A task without an informed bot yet falls back to a model-free ceiling,
//! the cost-aware hindsight bound (`ceiling: "hindsight"`): each session's
//! hindsight bound scaled by what perfect foresight keeps once it pays the
//! venue's taker fee and half-spread on every trade (see
//! [`cost_aware_bound`]), on the reference session's marks: never on the
//! bot's own, which its trades can move. A grade with no reference session
//! can't use it. It has no free parameter and no model run in it,
//! but it isn't reachable, so it is an interim 1, not a headline's. A
//! constant share `η` of the bound can still be forced for analysis
//! (`eta_override`), never by default. The plain hindsight bound is never in
//! the score: it is reported as the luck headroom above the ceiling.
//!
//! A bot that wins by drawing down far more than the anchor does pays a
//! near-breach charge `Π` before the map, in USD (see [`Risk`]): drawdowns
//! as a share of the limit, pooled over the set, against the anchor's, the
//! anchor's allowed to grow with what the bot earns over it. It takes back
//! winnings, never deepens a loss, and is 0 for doing nothing, the anchor
//! itself, and a bot taking the anchor's risk in proportion to its edge:
//!
//! ```text
//! f_s = min(DD_s / L_s, 1)   (1 for a breach)      a_s = the same for the anchor
//! ρ   = √(Σ f_s² / n)        ρ_A = √(Σ a_s² / n)   s = max(1, E_bot / E_A)
//! Π   = min(λ·k·Σ L_s·max(0, ρ − s·ρ_A)², max(E_bot, 0))
//! ```
//!
//! The anchor is the informed bot when its drawdowns are known, else the
//! reference, never a proxy ceiling, which has no drawdowns; without
//! either's drawdowns there is no charge, and the score says so. A breach
//! also caps the run: one at `raw` 0.5, the reference's, two or more at 0,
//! so breaching now and then never beats complying.
//!
//! The gap to 1 is split into what it came from (see [`Gap`]). Doing
//! nothing is taken as PnL 0, which holds for a flat start with no carry; a
//! session that starts holding something needs a measured nop run, which v2
//! doesn't do yet, and refuses.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The loss weight `k` by default.
pub const LOSS_WEIGHT: f64 = 1.5;
/// The reference anchor's floor `φ`, as a share of the ceiling, by default.
pub const ANCHOR_FLOOR: f64 = 0.10;
/// The near-breach charge's weight `λ` by default: at 1, a bot that drew down
/// to its limit in every session while the anchor never moved pays `k·L` a
/// session, what a breach costs.
pub const RISK_WEIGHT: f64 = 1.0;
/// `φ` against the cost-aware hindsight ceiling by default: 0.05 of it, the
/// same dollars as 0.10 of half of it, so the 0.5 point doesn't move with
/// how much of the hindsight bound is called reachable.
pub const HINDSIGHT_ANCHOR_FLOOR: f64 = 0.05;
/// What the cost-aware hindsight ceiling pays on every trade, as a share of
/// the price: Phoenix SPCX's taker fee (3.5 bp) and the calibration's
/// half-spread (5 bp), from `calibration/market/spcx.20261112.d1f48be2.toml`.
/// Venue facts, not a fitted knob; per task overridable.
pub const HINDSIGHT_COST: f64 = 8.5e-4;
/// Bootstrap resamples for the interval.
pub const RESAMPLES: usize = 2_000;
/// The intervals' level: 95%, the 2.5th and 97.5th percentiles.
pub const CI_LEVEL: f64 = 0.95;
const CI_TAIL: f64 = (1.0 - CI_LEVEL) / 2.0;

/// Where the 1.0 anchor comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Ceiling {
    /// The informed bot's PnL in each session, as its scenario carries it;
    /// without it on every session, the cost-aware hindsight bound, or with
    /// `eta_override`, the hindsight bound times `η` (for analysis only).
    Informed { eta_override: Option<f64> },
    /// The ceiling PnL of each session, by scenario name, given outright.
    Given(BTreeMap<String, f64>),
}

/// What v2 is computed with; reported with the score.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    /// `k`: how many times a loss counts.
    pub loss_weight: f64,
    /// `φ`: the reference anchor is at least this share of the ceiling.
    /// `None`: [`ANCHOR_FLOOR`], or [`HINDSIGHT_ANCHOR_FLOOR`] against the
    /// cost-aware hindsight ceiling.
    pub anchor_floor: Option<f64>,
    /// `λ`: the near-breach charge's weight; 0 turns it off.
    pub risk_weight: f64,
    pub ceiling: Ceiling,
    /// The cost the cost-aware hindsight ceiling pays per trade, as a share
    /// of the price (see [`cost_aware_bound`]).
    pub hindsight_cost: f64,
    pub resamples: usize,
}

impl Params {
    /// The defaults, with the ceiling given.
    pub fn new(ceiling: Ceiling) -> Self {
        Self {
            loss_weight: LOSS_WEIGHT,
            anchor_floor: None,
            risk_weight: RISK_WEIGHT,
            ceiling,
            hindsight_cost: HINDSIGHT_COST,
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
    /// The cost-aware hindsight bound (see [`cost_aware_bound`]) on the
    /// reference session's marks, when the grade has that session. Never
    /// computed on the bot's own marks, which its trades can move.
    pub hindsight_ceiling_usd: Option<f64>,
    pub nop_pnl_usd: f64,
    /// SHA-256 of what v2 read of the reference's session, when the grade
    /// has it: its marks (the cost-aware hindsight ceiling's input) and its
    /// max drawdown (the near-breach charge's anchor), recorded in the report
    /// so a rescore can check it reads the same session (see
    /// [`crate::grade::reference_sha256`]).
    pub reference_session_sha256: Option<String>,
    /// The hindsight bound (see [`crate::scenario::Scenario::bound_usd`]).
    pub bound_usd: f64,
    /// `L`: the mandate's drawdown limit, the most a compliant session can lose.
    pub worst_loss_usd: f64,
    /// The mandate has no drawdown limit, and `L` is the session's hindsight
    /// bound instead.
    pub worst_loss_from_bound: bool,
    /// Why the session breached its mandate or order, if it did.
    pub breach: Option<String>,
    /// The deepest the bot's account fell below its running peak.
    pub max_drawdown_usd: f64,
    /// The same for the reference and the informed bot, when known: the
    /// near-breach charge's anchor.
    pub reference_max_drawdown_usd: Option<f64>,
    pub informed_max_drawdown_usd: Option<f64>,
    /// The account's value at the start: the capital returns are taken on.
    pub start_value_usd: f64,
    /// The risk desk stopped the session out (the scenario's `stop_out`,
    /// see [`crate::risk`]): `pnl_usd` is what it realised, the close
    /// included, and `breach` says so.
    pub stopped_out: bool,
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
    /// least −1, and above 1 when the bot beats the informed bot. A fatal
    /// grade is −1, the floor: failing on purpose never beats losing.
    pub raw: f64,
    /// `raw` without the −1 floor, for training: a loss past the floor still
    /// counts. A fatal grade is at most −1 and never above what its sessions
    /// made.
    pub raw_unfloored: f64,
    /// What the sessions alone score, floored at −1, fatal or not: reported,
    /// not scored.
    pub measured_raw: f64,
    /// `raw − 0.5`: the edge over the reference.
    pub edge: f64,
    /// The 95% interval of `raw` over a paired bootstrap of this grade's
    /// sessions (`ci_scope: "sessions"`): one run's uncertainty from the
    /// sessions it drew, **not for ranking** (`ci_use`). Ranking models
    /// takes several runs and [`compare`], which resamples runs too.
    pub ci_lo: f64,
    pub ci_hi: f64,
    pub ci_scope: String,
    pub ci_level: f64,
    pub ci_use: String,
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
    /// The near-breach charge, taken from `bot_usd` before the map.
    pub risk: Risk,
    /// The breach cap `raw` was held to: 0.5 for one breach in the set, 0
    /// for two or more; absent without a breach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breach_cap: Option<f64>,
    /// The per-session Sharpe below which a profitable strategy scores under
    /// doing nothing at this `k` (see [`break_even_sharpe`]).
    pub break_even_sharpe: f64,
    /// Sessions whose mandate has no drawdown limit, where `L` is the
    /// hindsight bound instead.
    pub limit_from_bound_sessions: usize,
    /// The cost per trade the cost-aware hindsight ceiling paid.
    pub hindsight_cost: f64,
    /// `k`, `φ`, and where the ceiling came from: `informed` (the task's
    /// informed bot), `hindsight` (the cost-aware hindsight bound, for a
    /// task without one), `eta_override` (`η` times the hindsight bound,
    /// forced for analysis), `given`, or `none` (a grade with nothing to
    /// score).
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
    /// What the sessions made and risked, plainly: reported beside the
    /// score, never in it.
    pub metrics: Metrics,
    pub sessions: Vec<SessionV2>,
}

/// The near-breach charge `Π` (see the module's formula).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Risk {
    /// `Π`, in USD.
    pub charge_usd: f64,
    /// `ρ`: the root mean square of the bot's drawdowns as a share of their
    /// limits, each at most 1.
    pub dd_rms: f64,
    /// `ρ_A`, the anchor's; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dd_rms_anchor: Option<f64>,
    /// `s`: how many times the anchor's risk the bot's earnings allow it.
    pub allowance: f64,
    /// `λ`.
    pub weight: f64,
    /// Whose drawdowns the bot's are held to: `informed`, `reference`, or
    /// `none` when neither's are known, and nothing is charged.
    pub anchor: String,
}

/// What the sessions made and risked, plainly, in USD and not weighted:
/// what a desk reads beside a score. Each session is its own scenario of
/// minutes, so nothing is annualised.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Metrics {
    pub sessions: usize,
    /// The mean of each session's PnL over its starting value.
    pub return_on_capital: f64,
    /// The mean session PnL over its standard deviation (`n − 1`), no
    /// risk-free rate: how consistently the edge shows across scenarios.
    /// Absent under two sessions, or when every session made the same.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpe: Option<f64>,
    /// Each session's drawdown over its limit: the mean, and the deepest.
    pub drawdown_to_limit_mean: f64,
    pub drawdown_to_limit_max: f64,
    /// The share of sessions that made money.
    pub profitable_share: f64,
    /// The worst session, and what it made.
    pub worst_session: String,
    pub worst_session_usd: f64,
    /// The harm, as prominent as the score: the set's PnL, `raw` below
    /// doing nothing's, the sessions that lost and what they lost together,
    /// and breaches and stop-outs.
    pub pnl_usd: f64,
    pub raw_negative: bool,
    pub losing_sessions: usize,
    pub usd_lost: f64,
    pub breaches_per_100_sessions: f64,
    pub stopped_out_sessions: usize,
}

/// The gap from `raw` to 1, the informed bot, split by where it came from,
/// in score points. The buckets are dollar amounts of `E_inf − E_bot`, each
/// converted at the grade's average rate, `(1 − raw) / (E_inf − E_bot + Π)`
/// before the breach cap, which `risk` then carries in full, so they sum
/// to `1 − raw` exactly; `noise` is the interval's half-width, and
/// a bucket smaller than it isn't told from zero.
///
/// - `risk`: what the loss weight, breaches and the near-breach charge
///   cost: `Σ (X_s − Xw_s)`, less the same for the informed bot's losing
///   sessions, plus `Π`; in score points, also what the breach cap took.
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
    /// The same buckets in USD; they sum to `E_inf − E_bot + Π`. The breach
    /// cap has no USD amount.
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
    /// See [`Session::reference_session_sha256`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_session_sha256: Option<String>,
    /// `L`, and the near-breach charge's `f_s` and `a_s`: the bot's
    /// drawdown and the anchor's as shares of it, each at most 1.
    pub limit_usd: f64,
    pub drawdown: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_drawdown: Option<f64>,
}

/// Score `sessions` under v2. With `fatal`, the grade is the floor: `raw`
/// −1, score 0, no interval, and `raw_unfloored` at most −1; what the
/// sessions made is still reported as `measured_raw`.
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
    // A fatal grade is the floor whatever its sessions made: one that can't
    // resolve a ceiling (no reference ran, say) is the floor too, not an
    // error.
    let (source, eta, ceilings) = match (ceilings(sessions, &params.ceiling), &fatal) {
        (Ok(resolved), _) => resolved,
        (Err(_), Some(reason)) => {
            // The floor, with what the sessions made still reported.
            return Ok(ScoreV2 {
                metrics: Metrics::of(sessions, -1.0),
                ..unscored(params, reason.clone())
            });
        }
        (Err(why), None) => return Err(why),
    };
    let phi = params.anchor_floor.unwrap_or(if source == "hindsight" {
        HINDSIGHT_ANCHOR_FLOOR
    } else {
        ANCHOR_FLOOR
    });
    // Without a floor, a reference that loses would anchor 0.5 at or below
    // doing nothing.
    if !(phi > 0.0 && phi < 1.0) {
        return Err(format!("anchor floor {phi}: it must be in (0, 1)"));
    }
    if !(params.risk_weight >= 0.0 && params.risk_weight.is_finite()) {
        return Err(format!(
            "risk weight {}: it must be at least 0",
            params.risk_weight
        ));
    }
    let risk_anchor = RiskAnchor::of_sessions(sessions, source);
    let weigh = |x: f64| if x < 0.0 { k * x } else { x };
    let share = |drawdown: f64, limit: f64| {
        if limit > 0.0 {
            (drawdown / limit).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
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
            reference_session_sha256: s.reference_session_sha256.clone(),
            limit_usd: s.worst_loss_usd,
            drawdown: match s.breach {
                Some(_) => 1.0,
                None => share(s.max_drawdown_usd, s.worst_loss_usd),
            },
            anchor_drawdown: match risk_anchor {
                RiskAnchor::Informed => s.informed_max_drawdown_usd,
                RiskAnchor::Reference => s.reference_max_drawdown_usd,
                RiskAnchor::None => None,
            }
            .map(|drawdown| share(drawdown, s.worst_loss_usd)),
        })
        .collect();
    let sums = Sums::of(terms.iter());
    if sums.ceiling <= 0.0 {
        return Err(format!(
            "the ceiling sums to {} over the set: nothing to scale to",
            sums.ceiling
        ));
    }
    let anchor = sums.anchor(phi);
    if anchor >= sums.ceiling {
        return Err(format!(
            "the reference ({anchor}) reaches the ceiling ({}): the ceiling is too low",
            sums.ceiling
        ));
    }
    let scored = sums.score(phi, k, params.risk_weight, risk_anchor);
    let measured_raw = scored.raw;
    let measured_unfloored = scored.raw_unfloored;
    let seed = seed(eval_seed_sha256);
    // A fatal grade is the floor, with no interval: its sessions don't count.
    let (raw, raw_unfloored, (ci_lo, ci_hi)) = match fatal {
        Some(_) => (-1.0, measured_unfloored.min(-1.0), (-1.0, -1.0)),
        None => (
            measured_raw,
            measured_unfloored,
            interval(
                &terms,
                (phi, k, params.risk_weight, risk_anchor),
                params.resamples,
                seed.unwrap_or(FIXED_SEED),
            ),
        ),
    };
    let bound_usd: f64 = sessions.iter().map(|s| s.bound_usd).sum();
    let gap = gap(sessions, &ceilings, &terms, &scored, (ci_hi - ci_lo) / 2.0);
    Ok(ScoreV2 {
        score: raw.clamp(0.0, 1.0),
        raw,
        raw_unfloored,
        measured_raw,
        edge: raw - 0.5,
        ci_lo,
        ci_hi,
        within_noise: ci_lo <= 0.5 && 0.5 <= ci_hi,
        beats_informed: raw > 1.0,
        fatal,
        anchor_floored: sums.reference < phi * sums.ceiling,
        anchor_floor: phi,
        bot_usd: sums.bot,
        reference_usd: anchor,
        reference_measured_usd: sums.reference,
        ceiling_usd: sums.ceiling,
        nop_usd: sessions.iter().map(|s| s.nop_pnl_usd).sum(),
        bound_usd,
        luck_headroom_usd: bound_usd - sums.ceiling,
        breached_sessions: sums.breaches,
        risk: Risk {
            charge_usd: scored.charge,
            dd_rms: scored.dd_rms,
            dd_rms_anchor: (risk_anchor != RiskAnchor::None).then_some(scored.dd_rms_anchor),
            allowance: scored.allowance,
            weight: params.risk_weight,
            anchor: risk_anchor.name().into(),
        },
        breach_cap: scored.cap,
        limit_from_bound_sessions: sessions.iter().filter(|s| s.worst_loss_from_bound).count(),
        ceiling: source.into(),
        eta,
        resamples: params.resamples,
        seeded_from_eval: seed.is_some(),
        gap,
        metrics: Metrics::of(sessions, raw),
        sessions: terms,
        ..unscored(params, String::new())
    })
}

/// Whose drawdowns the near-breach charge holds the bot's to: the informed
/// bot's when it is the ceiling and every session has its drawdown, else
/// the reference's when every session has its, else none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RiskAnchor {
    Informed,
    Reference,
    None,
}

impl RiskAnchor {
    fn of_sessions(sessions: &[Session], ceiling: &str) -> Self {
        if ceiling == "informed"
            && sessions
                .iter()
                .all(|s| s.informed_max_drawdown_usd.is_some())
        {
            Self::Informed
        } else if sessions
            .iter()
            .all(|s| s.reference_max_drawdown_usd.is_some())
        {
            Self::Reference
        } else {
            Self::None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Informed => "informed",
            Self::Reference => "reference",
            Self::None => "none",
        }
    }

    /// Back from [`Self::name`], as a report gives it.
    fn named(name: &str) -> Self {
        match name {
            "informed" => Self::Informed,
            "reference" => Self::Reference,
            _ => Self::None,
        }
    }
}

impl Metrics {
    fn of(sessions: &[Session], raw: f64) -> Self {
        let n = sessions.len();
        let count = n.max(1) as f64;
        let pnl: Vec<f64> = sessions.iter().map(|s| s.pnl_usd).collect();
        let mean = pnl.iter().sum::<f64>() / count;
        let sharpe = (n >= 2)
            .then(|| {
                let variance = pnl.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
                variance.sqrt()
            })
            .filter(|sd| *sd > 0.0)
            .map(|sd| mean / sd);
        let to_limit: Vec<f64> = sessions
            .iter()
            .map(|s| {
                if s.worst_loss_usd > 0.0 {
                    s.max_drawdown_usd / s.worst_loss_usd
                } else {
                    0.0
                }
            })
            .collect();
        let worst = sessions
            .iter()
            .min_by(|a, b| a.pnl_usd.total_cmp(&b.pnl_usd));
        let breaches = sessions.iter().filter(|s| s.breach.is_some()).count();
        Self {
            sessions: n,
            return_on_capital: sessions
                .iter()
                .map(|s| {
                    if s.start_value_usd > 0.0 {
                        s.pnl_usd / s.start_value_usd
                    } else {
                        0.0
                    }
                })
                .sum::<f64>()
                / count,
            sharpe,
            drawdown_to_limit_mean: to_limit.iter().sum::<f64>() / count,
            drawdown_to_limit_max: to_limit.iter().copied().fold(0.0, f64::max),
            profitable_share: pnl.iter().filter(|x| **x > 0.0).count() as f64 / count,
            worst_session: worst.map(|s| s.scenario.clone()).unwrap_or_default(),
            worst_session_usd: worst.map_or(0.0, |s| s.pnl_usd),
            pnl_usd: pnl.iter().sum(),
            raw_negative: raw < 0.0,
            losing_sessions: pnl.iter().filter(|x| **x < 0.0).count(),
            usd_lost: pnl.iter().map(|x| (-x).max(0.0)).sum(),
            breaches_per_100_sessions: 100.0 * breaches as f64 / count,
            stopped_out_sessions: sessions.iter().filter(|s| s.stopped_out).count(),
        }
    }
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
        Ceiling::Informed { eta_override } => {
            if let Some(informed) = sessions.iter().map(|s| s.informed_pnl_usd).collect() {
                return Ok(("informed", None, informed));
            }
            if let Some(eta) = eta_override {
                let share = sessions.iter().map(|s| eta * s.bound_usd).collect();
                return Ok(("eta_override", Some(*eta), share));
            }
            let hindsight = sessions
                .iter()
                .map(|s| s.hindsight_ceiling_usd)
                .collect::<Option<_>>()
                .ok_or_else(|| {
                    "the scenarios carry no informed bot's PnL (informed_pnl_usd), and the \
                     grade has no reference session for the cost-aware hindsight ceiling \
                     (it is never computed on the bot's own marks)"
                        .to_string()
                })?;
            Ok(("hindsight", None, hindsight))
        }
    }
}

/// [`Gap`]: `E_inf − E_bot + Π` split into buckets, then into score points,
/// the breach cap's points added to `risk`.
fn gap(
    sessions: &[Session],
    ceilings: &[f64],
    terms: &[SessionV2],
    scored: &Scored,
    noise: f64,
) -> Gap {
    let mut usd = GapUsd {
        risk: scored.charge,
        ..GapUsd::default()
    };
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
        (1.0 - scored.uncapped) / total
    } else {
        0.0
    };
    Gap {
        wrong_read: usd.wrong_read * rate,
        missed: usd.missed * rate,
        execution: usd.execution * rate,
        risk: usd.risk * rate + (scored.uncapped - scored.raw),
        unattributed: usd.unattributed * rate,
        noise,
        usd,
    }
}

impl ScoreV2 {
    /// Make the grade fatal after it was scored (tampering found once the
    /// sessions ended): the floor, as [`score`] gives a fatal grade.
    pub fn make_fatal(&mut self, reason: String) {
        self.score = 0.0;
        self.raw = -1.0;
        self.raw_unfloored = self.raw_unfloored.min(-1.0);
        self.edge = -1.5;
        self.ci_lo = -1.0;
        self.ci_hi = -1.0;
        self.within_noise = false;
        self.beats_informed = false;
        self.fatal = Some(reason);
        // What the sessions made stays as measured; the grade is the floor.
        self.metrics.raw_negative = true;
    }
}

/// The cost-aware hindsight bound of a session: its hindsight bound `bound`
/// (see [`crate::scenario::Scenario::bound_usd`]) scaled by what perfect
/// foresight keeps once it pays `cost` (a share of the price) on every
/// trade: `bound × DP(cost) / DP(0)`, with [`hindsight_dp`] on the session's
/// marks, step by step. No free parameter and no model run: an interim 1
/// for a task without an informed bot. The scale is measured on the moves
/// of the mark and applied to the whole bound, spread and funding parts
/// included, at step granularity.
pub fn cost_aware_bound(bound: f64, marks: &[f64], cap: f64, cost: f64) -> f64 {
    bound * hindsight_dp(marks, cap, cost) / hindsight_dp(marks, cap, 0.0).max(1e-9)
}

/// The most a trader who knew the marks in advance could make holding
/// `−cap`, nothing or `+cap` from one step to the next, paying `cost` times
/// the price on every unit it trades (entering included, not the last exit):
/// a dynamic programme over the three positions.
pub fn hindsight_dp(marks: &[f64], cap: f64, cost: f64) -> f64 {
    let Some(&first) = marks.first() else {
        return 0.0;
    };
    let held = [-cap, 0.0, cap];
    let mut value = held.map(|x| -cost * first * x.abs());
    for pair in marks.windows(2) {
        let moved = held
            .iter()
            .zip(value)
            .map(|(x, v)| v + x * (pair[1] - pair[0]))
            .collect::<Vec<_>>();
        value = held.map(|to| {
            held.iter()
                .zip(&moved)
                .map(|(from, v)| v - cost * pair[1] * (to - from).abs())
                .fold(f64::NEG_INFINITY, f64::max)
        });
    }
    value.into_iter().fold(f64::NEG_INFINITY, f64::max)
}

/// v2 for a grade with no sessions to score because it was never graded
/// (or was tampered with before any ran): 0, with why.
pub fn unscored(params: &Params, reason: String) -> ScoreV2 {
    // Nothing was scored, so no ceiling was used.
    let (ceiling, eta) = match params.ceiling {
        Ceiling::Informed { .. } => ("none", None),
        Ceiling::Given(_) => ("given", None),
    };
    ScoreV2 {
        score: 0.0,
        raw: -1.0,
        raw_unfloored: -1.0,
        measured_raw: 0.0,
        edge: -1.5,
        ci_lo: -1.0,
        ci_hi: -1.0,
        ci_scope: "sessions".into(),
        ci_use: "not for ranking".into(),
        ci_level: CI_LEVEL,
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
        risk: Risk {
            weight: params.risk_weight,
            anchor: RiskAnchor::None.name().into(),
            allowance: 1.0,
            ..Risk::default()
        },
        breach_cap: None,
        limit_from_bound_sessions: 0,
        hindsight_cost: params.hindsight_cost,
        break_even_sharpe: break_even_sharpe(params.loss_weight),
        loss_weight: params.loss_weight,
        anchor_floor: params.anchor_floor.unwrap_or(ANCHOR_FLOOR),
        ceiling: ceiling.into(),
        eta,
        nop: "zero".into(),
        resamples: 0,
        seeded_from_eval: false,
        gap: Gap::default(),
        // Nothing was measured, but the grade is the floor.
        metrics: Metrics {
            raw_negative: true,
            ..Metrics::default()
        },
        sessions: Vec::new(),
    }
}

/// The per-session Sharpe `s = μ/σ` at which a strategy with normally
/// distributed session PnL has an expected weighted PnL of 0, doing
/// nothing's: `E[w(X)] = μ − (k − 1)·E[max(−X, 0)] = 0`, that is
/// `s = (k − 1)·(φ(s) − s·(1 − Φ(s)))`. Below it, losses counted `k` times
/// outweigh the edge. 0 at `k = 1`; about 0.16 at 1.5 and 0.28 at 2.
pub fn break_even_sharpe(k: f64) -> f64 {
    let gap = |s: f64| {
        let density = (-0.5 * s * s).exp() / (2.0 * std::f64::consts::PI).sqrt();
        let upper = 0.5 * erfc(s / std::f64::consts::SQRT_2);
        (k - 1.0) * (density - s * upper) - s
    };
    // gap(0) ≥ 0 and gap falls with s: bisect on [0, 5].
    let (mut lo, mut hi) = (0.0, 5.0);
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if gap(mid) > 0.0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// The complementary error function, to about 1e-7 (Abramowitz and Stegun
/// 7.1.26), for [`break_even_sharpe`].
fn erfc(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let tail = poly * (-x * x).exp();
    if x >= 0.0 { tail } else { 2.0 - tail }
}

/// The weighted sums over some sessions, and what the near-breach charge
/// and the breach cap read.
#[derive(Default)]
struct Sums {
    bot: f64,
    reference: f64,
    ceiling: f64,
    /// `Σ L_s`, `Σ f_s²` and `Σ a_s²`.
    limit: f64,
    drawdown: f64,
    anchor_drawdown: f64,
    sessions: usize,
    breaches: usize,
}

/// The sums scored: `raw`, the charge, and the cap.
struct Scored {
    raw: f64,
    /// `raw` before the breach cap, and `raw` not floored at −1.
    uncapped: f64,
    raw_unfloored: f64,
    charge: f64,
    dd_rms: f64,
    dd_rms_anchor: f64,
    allowance: f64,
    cap: Option<f64>,
}

impl Sums {
    fn of<'a>(terms: impl Iterator<Item = &'a SessionV2>) -> Self {
        let mut sums = Self::default();
        for t in terms {
            sums.bot += t.bot_usd;
            sums.reference += t.reference_usd;
            sums.ceiling += t.ceiling_usd;
            sums.limit += t.limit_usd;
            sums.drawdown += t.drawdown * t.drawdown;
            sums.anchor_drawdown += t.anchor_drawdown.map_or(0.0, |a| a * a);
            sums.sessions += 1;
            sums.breaches += usize::from(t.breach.is_some());
        }
        sums
    }

    /// The bot on the scale after the near-breach charge, held to the
    /// breach cap.
    fn score(&self, phi: f64, k: f64, lambda: f64, anchor: RiskAnchor) -> Scored {
        let n = self.sessions.max(1) as f64;
        let dd_rms = (self.drawdown / n).sqrt();
        let dd_rms_anchor = (self.anchor_drawdown / n).sqrt();
        // `E_A`: what the anchor whose risk the bot is held to earned.
        let earned = match anchor {
            RiskAnchor::Informed => self.ceiling,
            _ => self.anchor(phi),
        };
        let allowance = if earned > 0.0 {
            (self.bot / earned).max(1.0)
        } else {
            1.0
        };
        let charge = match anchor {
            RiskAnchor::None => 0.0,
            _ => {
                let excess = (dd_rms - allowance * dd_rms_anchor).max(0.0);
                (lambda * k * self.limit * excess * excess).min(self.bot.max(0.0))
            }
        };
        let uncapped = self.raw(phi, self.bot - charge);
        let cap = match self.breaches {
            0 => None,
            1 => Some(0.5),
            _ => Some(0.0),
        };
        Scored {
            raw: cap.map_or(uncapped, |cap| uncapped.min(cap)),
            uncapped,
            raw_unfloored: {
                let unfloored = self.raw_unfloored(phi, self.bot - charge);
                cap.map_or(unfloored, |cap| unfloored.min(cap))
            },
            charge,
            dd_rms,
            dd_rms_anchor,
            allowance,
            cap,
        }
    }

    /// `E_ref`: the reference's sum, floored at `φ` of the ceiling's.
    fn anchor(&self, floor: f64) -> f64 {
        self.reference.max(floor * self.ceiling)
    }

    /// `bot`, a weighted sum, on the scale, floored at −1.
    fn raw(&self, floor: f64, bot: f64) -> f64 {
        self.raw_unfloored(floor, bot).max(-1.0)
    }

    /// `bot`, a weighted sum, on the scale, not floored.
    fn raw_unfloored(&self, floor: f64, bot: f64) -> f64 {
        let anchor = self.anchor(floor);
        if bot <= anchor {
            0.5 * bot / anchor
        } else if self.ceiling > anchor {
            0.5 + 0.5 * (bot - anchor) / (self.ceiling - anchor)
        } else {
            1.0
        }
    }
}

/// What a resample is scored with: `φ`, `k`, `λ` and the risk anchor.
type Rule = (f64, f64, f64, RiskAnchor);

/// The 95% interval of `raw` over resamples of the sessions,
/// each drawn with replacement and used for the bot, the reference and the
/// ceiling alike; the charge and the cap are taken again on each.
fn interval(terms: &[SessionV2], rule: Rule, resamples: usize, seed: u64) -> (f64, f64) {
    let (phi, k, lambda, anchor) = rule;
    let mut rng = SplitMix64(seed);
    let n = terms.len();
    let mut raws: Vec<f64> = (0..resamples)
        .map(|_| {
            let sums = Sums::of((0..n).map(|_| &terms[rng.below(n)]));
            if sums.ceiling <= 0.0 {
                return 0.0;
            }
            sums.score(phi, k, lambda, anchor).raw
        })
        .collect();
    if raws.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    raws.sort_by(f64::total_cmp);
    (percentile(&raws, CI_TAIL), percentile(&raws, 1.0 - CI_TAIL))
}

/// One graded run of a task, for [`compare`].
pub struct Run<'a> {
    pub model: String,
    pub v2: &'a ScoreV2,
}

/// Models compared over several runs of one task (see [`compare`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Comparison {
    /// `scenarios+runs`: scenarios resampled as clusters, then runs within
    /// each model, the same scenario draws for every model.
    pub ci_scope: String,
    pub ci_level: f64,
    pub resamples: usize,
    pub seeded_from_eval: bool,
    pub scenarios: usize,
    pub models: Vec<ModelV2>,
    pub pairs: Vec<PairV2>,
}

/// One model's runs on a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelV2 {
    pub model: String,
    pub runs: usize,
    /// The mean of its runs' `raw`, with its interval.
    pub mean_raw: f64,
    pub ci_lo: f64,
    pub ci_hi: f64,
    /// The headline: the mean `raw` clamped to [0, 1] once. Never the mean
    /// of clamped run scores, which would turn every losing run into 0.
    pub mean_score: f64,
}

/// Two models' difference in mean `raw` (`a − b`), with its paired interval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairV2 {
    pub a: String,
    pub b: String,
    pub diff: f64,
    pub ci_lo: f64,
    pub ci_hi: f64,
    /// The interval includes 0.
    pub tie: bool,
    /// The bootstrap's two-sided p-value for no difference, and the same
    /// adjusted for every pair of the task (Benjamini–Hochberg).
    pub p_value: f64,
    pub p_adjusted: f64,
    /// The adjusted p-value is at least 0.05: the two rank as tied. Ranks
    /// are published as groups of ties by this flag.
    pub tie_adjusted: bool,
}

/// Compare models over several graded runs of one task, all on the same
/// scenarios: a hierarchical, paired bootstrap of the mean `raw`. Each
/// resample draws the scenarios with replacement (each scenario a cluster,
/// the same draws for every model, so model differences are paired), then
/// each model's runs with replacement, and scores every drawn run on the
/// drawn scenarios. A fatal or unscored grade counts as `raw` −1, the floor,
/// as it does alone. (A grade the machine failed is retried by the grader
/// and never reaches here: it is reported apart, not scored.) The seed is
/// the first run's `eval_seed_sha256`, as for one grade.
pub fn compare(
    runs: &[Run],
    params: &Params,
    eval_seed_sha256: Option<&str>,
) -> Result<Comparison, String> {
    let first = runs
        .iter()
        .find(|r| !r.v2.sessions.is_empty())
        .ok_or("no run with sessions to compare")?;
    let names: Vec<&str> = first
        .v2
        .sessions
        .iter()
        .map(|s| s.scenario.as_str())
        .collect();
    // Each run's terms in the first run's scenario order; `None`: unscored.
    let mut aligned: Vec<Option<Vec<&SessionV2>>> = Vec::with_capacity(runs.len());
    for run in runs {
        if run.v2.sessions.is_empty() {
            aligned.push(None);
            continue;
        }
        let by_name: BTreeMap<&str, &SessionV2> = run
            .v2
            .sessions
            .iter()
            .map(|s| (s.scenario.as_str(), s))
            .collect();
        if by_name.len() != names.len() {
            return Err("the runs didn't play the same scenarios".into());
        }
        let terms = names
            .iter()
            .map(|name| {
                by_name
                    .get(name)
                    .copied()
                    .ok_or_else(|| format!("a run didn't play {name}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        aligned.push(Some(terms));
    }
    let raw_of = |i: usize, idx: &[usize]| -> f64 {
        match &aligned[i] {
            Some(_) if runs[i].v2.fatal.is_some() => -1.0,
            Some(terms) => {
                let sums = Sums::of(idx.iter().map(|&j| terms[j]));
                if sums.ceiling <= 0.0 {
                    0.0
                } else {
                    let v2 = runs[i].v2;
                    sums.score(
                        v2.anchor_floor,
                        v2.loss_weight,
                        v2.risk.weight,
                        RiskAnchor::named(&v2.risk.anchor),
                    )
                    .raw
                }
            }
            None => -1.0,
        }
    };
    let mut models: Vec<String> = runs.iter().map(|r| r.model.clone()).collect();
    models.sort();
    models.dedup();
    let members: Vec<Vec<usize>> = models
        .iter()
        .map(|m| (0..runs.len()).filter(|&i| &runs[i].model == m).collect())
        .collect();
    let n = names.len();
    let all: Vec<usize> = (0..n).collect();
    let means: Vec<f64> = members
        .iter()
        .map(|ix| ix.iter().map(|&i| raw_of(i, &all)).sum::<f64>() / ix.len() as f64)
        .collect();
    let seed = seed(eval_seed_sha256);
    let mut rng = SplitMix64(seed.unwrap_or(FIXED_SEED));
    let mut draws: Vec<Vec<f64>> = vec![Vec::with_capacity(params.resamples); models.len()];
    let mut idx = vec![0; n];
    for _ in 0..params.resamples {
        for slot in idx.iter_mut() {
            *slot = rng.below(n);
        }
        for (m, ix) in members.iter().enumerate() {
            let total: f64 = (0..ix.len())
                .map(|_| raw_of(ix[rng.below(ix.len())], &idx))
                .sum();
            draws[m].push(total / ix.len() as f64);
        }
    }
    let interval_of = |mut values: Vec<f64>| {
        if values.is_empty() {
            return (f64::NAN, f64::NAN);
        }
        values.sort_by(f64::total_cmp);
        (
            percentile(&values, CI_TAIL),
            percentile(&values, 1.0 - CI_TAIL),
        )
    };
    let mut pairs = Vec::new();
    for a in 0..models.len() {
        for b in a + 1..models.len() {
            let diffs: Vec<f64> = draws[a].iter().zip(&draws[b]).map(|(x, y)| x - y).collect();
            let p_value = two_sided_p(&diffs);
            let (ci_lo, ci_hi) = interval_of(diffs);
            pairs.push(PairV2 {
                a: models[a].clone(),
                b: models[b].clone(),
                diff: means[a] - means[b],
                ci_lo,
                ci_hi,
                tie: ci_lo <= 0.0 && 0.0 <= ci_hi,
                p_value,
                p_adjusted: p_value,
                tie_adjusted: true,
            });
        }
    }
    let adjusted = benjamini_hochberg(&pairs.iter().map(|p| p.p_value).collect::<Vec<_>>());
    for (pair, p) in pairs.iter_mut().zip(adjusted) {
        pair.p_adjusted = p;
        pair.tie_adjusted = p >= 1.0 - CI_LEVEL;
    }
    let models = models
        .iter()
        .zip(&members)
        .zip(&means)
        .zip(draws)
        .map(|(((model, ix), &mean_raw), draws)| {
            let (ci_lo, ci_hi) = interval_of(draws);
            ModelV2 {
                model: model.clone(),
                runs: ix.len(),
                mean_raw,
                ci_lo,
                ci_hi,
                mean_score: mean_raw.clamp(0.0, 1.0),
            }
        })
        .collect();
    Ok(Comparison {
        ci_scope: "scenarios+runs".into(),
        ci_level: CI_LEVEL,
        resamples: params.resamples,
        seeded_from_eval: seed.is_some(),
        scenarios: n,
        models,
        pairs,
    })
}

/// A bootstrap's two-sided p-value for a difference of 0: twice the
/// smaller share of resamples on either side of 0 (ties counted half),
/// at most 1.
fn two_sided_p(diffs: &[f64]) -> f64 {
    if diffs.is_empty() {
        return 1.0;
    }
    let n = diffs.len() as f64;
    let below = diffs
        .iter()
        .map(|&d| {
            if d < 0.0 {
                1.0
            } else if d == 0.0 {
                0.5
            } else {
                0.0
            }
        })
        .sum::<f64>();
    (2.0 * (below / n).min(1.0 - below / n)).min(1.0)
}

/// Benjamini–Hochberg adjusted p-values, in the order given: each is the
/// smallest `p_(j)·m/j` over the ranks `j` at or above its own, at most 1.
fn benjamini_hochberg(p: &[f64]) -> Vec<f64> {
    let m = p.len();
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|&a, &b| p[a].total_cmp(&p[b]));
    let mut adjusted = vec![1.0; m];
    let mut running = 1.0_f64;
    for (rank, &i) in order.iter().enumerate().rev() {
        running = running.min(p[i] * m as f64 / (rank + 1) as f64);
        adjusted[i] = running.min(1.0);
    }
    adjusted
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
                hindsight_ceiling_usd: None,
                reference_session_sha256: None,
                nop_pnl_usd: 0.0,
                bound_usd: 1_000.0,
                worst_loss_usd: 100.0,
                worst_loss_from_bound: false,
                breach: None,
                max_drawdown_usd: 0.0,
                reference_max_drawdown_usd: None,
                informed_max_drawdown_usd: None,
                start_value_usd: 10_000.0,
                stopped_out: false,
                traded: pnl_usd != 0.0,
                trade_markout_usd: Some(-5.0),
            })
            .collect()
    }

    const REFERENCE: [f64; 6] = [40.0, -10.0, 25.0, 60.0, 0.0, 15.0];

    fn informed() -> Params {
        Params::new(Ceiling::Informed { eta_override: None })
    }

    fn proxy(eta: f64) -> Params {
        Params::new(Ceiling::Informed {
            eta_override: Some(eta),
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
        let above = score(&set(&[227.5; 6], &REFERENCE), &informed(), None, None).unwrap();
        // (1,365 − 125) / (900 − 125) = 1.6: raw 0.5 + 0.8.
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
    fn without_an_informed_bot_the_cost_aware_hindsight_bound_is_the_ceiling() {
        let mut sessions = without_informed(set(&REFERENCE, &REFERENCE));
        for s in &mut sessions {
            s.hindsight_ceiling_usd = Some(600.0);
        }
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        assert_eq!(v2.ceiling, "hindsight");
        assert_eq!(v2.ceiling_usd, 3_600.0);
        // Its 0.5 floor is 0.05 of it: 180, above the reference's 125.
        assert_eq!(v2.anchor_floor, 0.05);
        assert!(v2.anchor_floored);
        // η only when forced, and flagged.
        let v2 = score(&sessions, &proxy(0.15), None, None).unwrap();
        assert_eq!(v2.ceiling, "eta_override");
        assert_eq!(v2.anchor_floor, 0.10);
        // Without the reference's sessions, nothing to fall back to.
        let bare = without_informed(set(&REFERENCE, &REFERENCE));
        assert!(
            score(&bare, &informed(), None, None)
                .unwrap_err()
                .contains("no reference session")
        );
        let v2 = score(&bare, &proxy(0.15), None, None).unwrap();
        assert_eq!(v2.eta, Some(0.15));
        assert_eq!(v2.ceiling_usd, 900.0);
        // With the informed bot's PnL on every session, η isn't used.
        let both = score(&set(&REFERENCE, &REFERENCE), &proxy(0.5), None, None).unwrap();
        assert_eq!(both.ceiling, "informed");
        assert_eq!(both.eta, None);
    }

    /// The analysis's formula, worked by hand: losses count 1.5 times, the
    /// reference's sum is 40 − 15 + 25 + 60 + 0 + 15 = 125, the informed
    /// bot's 6 × 150 = 900.
    #[test]
    fn losses_count_k_times_in_the_pooled_sums() {
        let bot = [100.0, -30.0, 50.0, -10.0, 20.0, 0.0];
        let v2 = score(&set(&bot, &REFERENCE), &informed(), None, None).unwrap();
        // E_bot = 100 − 45 + 50 − 15 + 20 + 0 = 110, under the reference's 125.
        assert_eq!(v2.loss_weight, 1.5);
        assert_eq!(v2.bot_usd, 110.0);
        assert_eq!(v2.reference_usd, 125.0);
        assert_eq!(v2.ceiling_usd, 900.0);
        assert_eq!(v2.raw, 0.5 * 110.0 / 125.0);
        let better = [400.0, 0.0, 50.0, 60.0, 20.0, 0.0];
        let v2 = score(&set(&better, &REFERENCE), &informed(), None, None).unwrap();
        assert_eq!(v2.raw, 0.5 + 0.5 * (530.0 - 125.0) / (900.0 - 125.0));
        // At k = 2, REPORT.md's G: 100 − 60 + 50 − 20 + 20 = 90 against 120.
        let mut g = informed();
        g.loss_weight = 2.0;
        let v2 = score(&set(&bot, &REFERENCE), &g, None, None).unwrap();
        assert_eq!((v2.bot_usd, v2.reference_usd), (90.0, 120.0));
    }

    #[test]
    fn an_informed_bots_losses_count_k_times_too() {
        let mut sessions = set(&REFERENCE, &REFERENCE);
        sessions[0].informed_pnl_usd = Some(-50.0);
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        assert_eq!(v2.ceiling_usd, 750.0 - 75.0);
    }

    #[test]
    fn the_break_even_sharpe_is_reported_for_k() {
        assert!(break_even_sharpe(1.0) < 1e-9);
        assert!(
            (break_even_sharpe(1.5) - 0.16).abs() < 0.01,
            "{}",
            break_even_sharpe(1.5)
        );
        assert!(
            (break_even_sharpe(2.0) - 0.28).abs() < 0.01,
            "{}",
            break_even_sharpe(2.0)
        );
        let v2 = score(&set(&REFERENCE, &REFERENCE), &informed(), None, None).unwrap();
        assert_eq!(v2.break_even_sharpe, break_even_sharpe(1.5));
        // A strategy at that Sharpe scores about what doing nothing does.
        let s = break_even_sharpe(1.5);
        let (n, mut rng) = (200_000, SplitMix64(11));
        let mut normal = || {
            // Box-Muller, from two uniforms.
            let u = (rng.next() >> 11) as f64 / (1u64 << 53) as f64;
            let v = (rng.next() >> 11) as f64 / (1u64 << 53) as f64;
            (-2.0 * (1.0 - u).ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
        };
        let mean = (0..n)
            .map(|_| {
                let x = s + normal();
                if x < 0.0 { 1.5 * x } else { x }
            })
            .sum::<f64>()
            / n as f64;
        assert!(mean.abs() < 0.01, "{mean}");
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
        assert_eq!(breached.sessions[5].bot_usd, -150.0);
        assert_eq!(breached.breached_sessions, 1);
        // Above the reference, so the one breach caps the run there.
        assert!(compliant_worst.raw > 0.5);
        assert_eq!(breached.raw, 0.5);
        assert_eq!(breached.breach_cap, Some(0.5));
        // One breach no longer zeroes the set.
        assert!(breached.score > 0.0);
    }

    #[test]
    fn a_breach_never_caps_a_loss() {
        let mut sessions = set(&[60.0; 6], &REFERENCE);
        for s in &mut sessions {
            s.worst_loss_usd = 1_000.0;
        }
        let mut k2 = informed();
        k2.loss_weight = 2.0;
        sessions[2].pnl_usd = -9_000.0;
        sessions[2].breach = Some("drew down past its limit".into());
        let v2 = score(&sessions, &k2, None, None).unwrap();
        assert_eq!(v2.sessions[2].bot_usd, -18_000.0);
        assert!(v2.gap.usd.risk >= 0.0 && v2.gap.risk >= 0.0, "{:?}", v2.gap);
        // A breach that made money still counts the worst allowed loss.
        sessions[2].pnl_usd = 400.0;
        let v2 = score(&sessions, &k2, None, None).unwrap();
        assert_eq!(v2.sessions[2].bot_usd, -2_000.0);
        assert!(v2.gap.usd.risk >= 0.0);
    }

    #[test]
    fn the_anchor_floor_must_be_above_zero() {
        for floor in [0.0, -0.1, 1.0] {
            let mut params = informed();
            params.anchor_floor = Some(floor);
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
        assert_eq!(v2.raw, -1.0);
        assert_eq!(v2.edge, -1.5);
        assert!(v2.measured_raw > 0.0, "what it made is still reported");
        assert_eq!(v2.ci_use, "not for ranking");
    }

    /// A bot that fails on purpose when it is losing scores below the
    /// losing bot it wraps, in `raw`, in `raw_unfloored`, and in a ranking.
    #[test]
    fn crashing_when_losing_scores_below_losing() {
        let losing = [-20.0, -10.0, 5.0, -30.0, 0.0, -5.0];
        let honest = score(&set(&losing, &REFERENCE), &informed(), None, None).unwrap();
        assert!(honest.raw < 0.0 && honest.raw > -1.0, "{}", honest.raw);
        let crashed = score(
            &set(&losing, &REFERENCE),
            &informed(),
            Some("ended the session liquidatable".into()),
            None,
        )
        .unwrap();
        assert!(crashed.raw < honest.raw);
        assert!(crashed.raw_unfloored < honest.raw_unfloored);
        // Far below the floor, the fatal grade's unfloored raw is no better
        // than what its sessions made.
        let ruinous = [-900.0; 6];
        let lost = score(&set(&ruinous, &REFERENCE), &informed(), None, None).unwrap();
        assert_eq!(lost.raw, -1.0);
        assert!(lost.raw_unfloored < -1.0);
        let crashed = score(
            &set(&ruinous, &REFERENCE),
            &informed(),
            Some("x".into()),
            None,
        )
        .unwrap();
        assert_eq!(crashed.raw_unfloored, lost.raw_unfloored);

        let mut graded = runs(&[losing, losing], &[losing, losing]);
        for (_, v2) in &mut graded[..2] {
            v2.make_fatal("refused call".into());
        }
        let c = compared(&graded);
        assert_eq!(c.models[0].mean_raw, -1.0);
        assert!(c.models[0].mean_raw < c.models[1].mean_raw);
    }

    #[test]
    fn an_unscored_grade_is_the_floor() {
        let v2 = unscored(&informed(), "no bot.wasm".into());
        assert_eq!((v2.raw, v2.raw_unfloored, v2.score), (-1.0, -1.0, 0.0));
        // No informed bot was used: none is claimed.
        assert_eq!(v2.ceiling, "none");
    }

    #[test]
    fn a_models_headline_clamps_its_mean_raw_once() {
        let good = [140.0, 120.0, 150.0, 130.0, 145.0, 150.0];
        let bad = [-60.0, -40.0, -50.0, -70.0, -45.0, -60.0];
        let graded = runs(&[good, bad], &[good, good]);
        let (up, down) = (graded[0].1.raw, graded[1].1.raw);
        assert!(up > 0.0 && down < 0.0);
        let c = compared(&graded);
        assert!((c.models[0].mean_raw - (up + down) / 2.0).abs() < 1e-12);
        assert_eq!(c.models[0].mean_score, ((up + down) / 2.0).clamp(0.0, 1.0));
        // Not the mean of clamped runs, which hides the losing run.
        assert!(c.models[0].mean_score < (up.clamp(0.0, 1.0) + 0.0) / 2.0);
    }

    #[test]
    fn pairwise_ties_are_adjusted_for_every_pair() {
        let adjusted = benjamini_hochberg(&[0.01, 0.04, 0.03, 0.2]);
        let want = [0.04, 0.04 * 4.0 / 3.0, 0.04 * 4.0 / 3.0, 0.2];
        for (a, w) in adjusted.iter().zip(want) {
            assert!((a - w).abs() < 1e-12, "{adjusted:?}");
        }
        assert_eq!(two_sided_p(&[1.0, 2.0, 3.0, -1.0]), 0.5);
        assert_eq!(two_sided_p(&[1.0; 10]), 0.0);
        let strong = [[140.0, 120.0, 150.0, 130.0, 145.0, 150.0]; 3];
        let weak = [[10.0, -20.0, 5.0, 0.0, 15.0, 10.0]; 3];
        let c = compared(&runs(&strong, &weak));
        let pair = &c.pairs[0];
        assert!(!pair.tie_adjusted && pair.p_adjusted < 0.05, "{pair:?}");
    }

    #[test]
    fn a_weak_reference_is_floored_and_flagged() {
        let weak = [1.0, -3.0, 1.0, 0.0, 1.0, 0.0];
        let v2 = score(&set(&weak, &weak), &informed(), None, None).unwrap();
        assert!(v2.anchor_floored);
        // 1 − 4.5 + 1 + 0 + 1 + 0.
        assert_eq!(v2.reference_measured_usd, -1.5);
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
        // Traded at a cost of 5 a session, sat out one, lost in one, breached one.
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
        // Half the loss again (15), and the breach: 80 made, −150 scored.
        assert_eq!(usd.risk, 15.0 + 230.0);
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

    /// Runs of two models on the same six scenarios.
    fn runs(model_a: &[[f64; 6]], model_b: &[[f64; 6]]) -> Vec<(String, ScoreV2)> {
        let graded =
            |pnl: &[f64; 6]| score(&set(pnl, &REFERENCE), &informed(), None, None).unwrap();
        model_a
            .iter()
            .map(|p| ("a".to_string(), graded(p)))
            .chain(model_b.iter().map(|p| ("b".to_string(), graded(p))))
            .collect()
    }

    fn compared(graded: &[(String, ScoreV2)]) -> Comparison {
        let runs: Vec<Run> = graded
            .iter()
            .map(|(model, v2)| Run {
                model: model.clone(),
                v2,
            })
            .collect();
        compare(&runs, &informed(), Some(&"ab".repeat(32))).unwrap()
    }

    #[test]
    fn models_far_apart_are_ranked_and_close_ones_tie() {
        let strong = [[140.0, 120.0, 150.0, 130.0, 145.0, 150.0]; 3];
        let weak = [[10.0, -20.0, 5.0, 0.0, 15.0, 10.0]; 3];
        let c = compared(&runs(&strong, &weak));
        assert_eq!(c.ci_scope, "scenarios+runs");
        assert_eq!(c.ci_level, 0.95);
        assert_eq!(c.models[0].runs, 3);
        assert!(
            c.models[0].ci_lo <= c.models[0].mean_raw && c.models[0].mean_raw <= c.models[0].ci_hi
        );
        let pair = &c.pairs[0];
        assert_eq!((pair.a.as_str(), pair.b.as_str()), ("a", "b"));
        assert!(pair.diff > 0.0 && !pair.tie, "{pair:?}");

        let close = [
            [60.0, 10.0, 30.0, 70.0, 0.0, 20.0],
            [40.0, -5.0, 35.0, 55.0, 10.0, 15.0],
        ];
        let similar = [
            [50.0, 0.0, 40.0, 60.0, 5.0, 20.0],
            [45.0, 5.0, 20.0, 65.0, 0.0, 25.0],
        ];
        let c = compared(&runs(&close, &similar));
        assert!(c.pairs[0].tie, "{:?}", c.pairs[0]);
    }

    #[test]
    fn the_same_draws_give_the_same_intervals_and_fatal_runs_count_zero() {
        let a = [[100.0, 20.0, 40.0, 60.0, 0.0, 30.0]; 2];
        let b = [[30.0, 10.0, 20.0, 50.0, 5.0, 10.0]; 2];
        let graded = runs(&a, &b);
        assert_eq!(compared(&graded), compared(&graded));
        let mut fatal = graded.clone();
        fatal[0].1.fatal = Some("tampered".into());
        let c = compared(&fatal);
        // One of model a's two runs now counts −1, the floor.
        assert!((c.models[0].mean_raw - (graded[1].1.raw - 1.0) / 2.0).abs() < 1e-12);
    }

    #[test]
    fn runs_on_different_scenarios_are_not_compared() {
        let mut graded = runs(&[[10.0; 6]], &[[20.0; 6]]);
        graded[1].1.sessions[0].scenario = "elsewhere".into();
        let runs: Vec<Run> = graded
            .iter()
            .map(|(model, v2)| Run {
                model: model.clone(),
                v2,
            })
            .collect();
        assert!(compare(&runs, &informed(), None).is_err());
    }

    #[test]
    fn one_grades_interval_is_labelled_as_its_sessions() {
        let v2 = score(&set(&REFERENCE, &REFERENCE), &informed(), None, None).unwrap();
        assert_eq!(v2.ci_scope, "sessions");
        assert_eq!(v2.ci_level, 0.95);
    }

    /// [`set`] with each session's drawdowns: the bot's, and the
    /// reference's, as the near-breach charge reads them.
    fn risked(
        pnls: &[f64],
        drawdowns: &[f64],
        references: &[f64],
        ref_drawdowns: &[f64],
    ) -> Vec<Session> {
        let mut sessions = set(pnls, references);
        for ((s, &dd), &ref_dd) in sessions.iter_mut().zip(drawdowns).zip(ref_drawdowns) {
            s.max_drawdown_usd = dd;
            s.reference_max_drawdown_usd = Some(ref_dd);
        }
        sessions
    }

    /// The reference's drawdowns against its [`REFERENCE`] PnL, the limit
    /// being 100.
    const REFERENCE_DD: [f64; 6] = [10.0, 25.0, 15.0, 20.0, 5.0, 10.0];

    #[test]
    fn the_charge_leaves_doing_nothing_at_zero_and_the_reference_at_one_half() {
        let reference = score(
            &risked(&REFERENCE, &REFERENCE_DD, &REFERENCE, &REFERENCE_DD),
            &informed(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(reference.risk.anchor, "reference");
        assert_eq!(reference.risk.charge_usd, 0.0);
        assert_eq!(reference.risk.weight, 1.0);
        assert_eq!(reference.raw, 0.5);
        let nop = score(
            &risked(&[0.0; 6], &[0.0; 6], &REFERENCE, &REFERENCE_DD),
            &informed(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(nop.risk.charge_usd, 0.0);
        assert_eq!(nop.risk.dd_rms, 0.0);
        assert_eq!(nop.raw, 0.0);
    }

    /// RECKLESS.md's lucky gambler: it drew down to its limit in every
    /// session, the reference barely moving, and came out ahead anyway.
    #[test]
    fn a_lucky_full_cap_gambler_is_charged_for_its_drawdowns() {
        let lucky = [95.0, -100.0, 98.0, 97.0, -100.0, 96.0];
        let sessions = risked(&lucky, &[100.0; 6], &REFERENCE, &REFERENCE_DD);
        let before = {
            let mut off = informed();
            off.risk_weight = 0.0;
            score(&sessions, &off, None, None).unwrap()
        };
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        // E_bot = 386 − 300 = 86, under the reference's 125: s = 1.
        assert_eq!(v2.bot_usd, 86.0);
        assert_eq!(v2.risk.allowance, 1.0);
        assert_eq!(v2.risk.dd_rms, 1.0);
        let anchor = (REFERENCE_DD
            .iter()
            .map(|d| (d / 100.0) * (d / 100.0))
            .sum::<f64>()
            / 6.0)
            .sqrt();
        assert!((v2.risk.dd_rms_anchor.unwrap() - anchor).abs() < 1e-12);
        // λ·k·ΣL·(1 − ρ_A)², over its 86 of winnings: all of them.
        assert!(1.0 * 1.5 * 600.0 * (1.0 - anchor).powi(2) > 86.0);
        assert_eq!(v2.risk.charge_usd, 86.0);
        assert_eq!(v2.raw, 0.0);
        assert!(before.raw > 0.3, "{}", before.raw);
        // The charge counts in the gap's risk bucket.
        assert!(v2.gap.usd.risk >= 86.0);
        let g = &v2.gap;
        let sum = g.wrong_read + g.missed + g.execution + g.risk + g.unattributed;
        assert!((sum - (1.0 - v2.raw)).abs() < 1e-12);
    }

    #[test]
    fn at_lambda_one_drawing_to_the_limit_every_session_costs_what_a_breach_does() {
        // Anchor never moved, the bot drew to the limit everywhere, and made
        // enough that the cap at its winnings doesn't bind.
        let sessions = risked(&[400.0; 6], &[100.0; 6], &REFERENCE, &[0.0; 6]);
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        assert_eq!(v2.risk.charge_usd, 1.5 * 100.0 * 6.0);
    }

    /// The reference's bets at two and three times the size: PnL and
    /// drawdowns scale together, so the earnings allow the risk.
    #[test]
    fn a_skilful_bot_betting_bigger_is_not_charged() {
        for m in [2.0, 3.0] {
            let pnl: Vec<f64> = REFERENCE.iter().map(|p| p * m).collect();
            let dd: Vec<f64> = REFERENCE_DD.iter().map(|d| d * m).collect();
            let v2 = score(
                &risked(&pnl, &dd, &REFERENCE, &REFERENCE_DD),
                &informed(),
                None,
                None,
            )
            .unwrap();
            assert!(
                (v2.risk.allowance - m).abs() < 1e-12,
                "{}",
                v2.risk.allowance
            );
            assert_eq!(v2.risk.charge_usd, 0.0, "×{m}");
            let mut off = informed();
            off.risk_weight = 0.0;
            let uncharged = score(
                &risked(&pnl, &dd, &REFERENCE, &REFERENCE_DD),
                &off,
                None,
                None,
            )
            .unwrap();
            assert_eq!(v2.raw, uncharged.raw);
        }
    }

    #[test]
    fn the_charge_never_deepens_a_loss() {
        let losing = [-20.0, -40.0, 10.0, -5.0, 0.0, 15.0];
        let v2 = score(
            &risked(&losing, &[100.0; 6], &REFERENCE, &REFERENCE_DD),
            &informed(),
            None,
            None,
        )
        .unwrap();
        assert!(v2.bot_usd < 0.0);
        assert_eq!(v2.risk.charge_usd, 0.0);
    }

    #[test]
    fn the_risk_anchor_is_the_informed_bot_then_the_reference_never_a_proxy() {
        let mut sessions = risked(&REFERENCE, &REFERENCE_DD, &REFERENCE, &REFERENCE_DD);
        for s in &mut sessions {
            s.informed_max_drawdown_usd = Some(30.0);
        }
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        assert_eq!(v2.risk.anchor, "informed");
        assert!((v2.risk.dd_rms_anchor.unwrap() - 0.3).abs() < 1e-12);
        // Under a proxy ceiling, the informed bot's drawdowns aren't the
        // ceiling's: the reference's are used.
        let proxied = score(
            &without_informed(sessions.clone()),
            &proxy(0.15),
            None,
            None,
        )
        .unwrap();
        assert_eq!(proxied.risk.anchor, "reference");
        // Without the reference's on every session, nothing is charged.
        let mut bare = without_informed(sessions);
        bare[3].reference_max_drawdown_usd = None;
        for s in &mut bare {
            s.max_drawdown_usd = 100.0;
        }
        let none = score(&bare, &proxy(0.15), None, None).unwrap();
        assert_eq!(none.risk.anchor, "none");
        assert_eq!(none.risk.charge_usd, 0.0);
        assert_eq!(none.risk.dd_rms_anchor, None);
    }

    #[test]
    fn a_fatal_grade_with_no_ceiling_is_the_floor_not_an_error() {
        // No informed bot, no η, no reference session for a hindsight
        // ceiling: an honest grade can't be scored...
        let sessions = without_informed(set(&REFERENCE, &REFERENCE));
        assert!(score(&sessions, &informed(), None, None).is_err());
        // ...a fatal one is the floor.
        let v2 = score(&sessions, &informed(), Some("tampered".into()), None).unwrap();
        assert_eq!((v2.raw, v2.score), (-1.0, 0.0));
        assert_eq!(v2.fatal.as_deref(), Some("tampered"));
        // The harm counters still say what the sessions made.
        assert!(v2.metrics.raw_negative);
        assert_eq!(v2.metrics.sessions, 6);
        assert_eq!((v2.metrics.losing_sessions, v2.metrics.usd_lost), (1, 10.0));
    }

    /// A fatal or unscored run is the floor, `raw` −1, and its metrics say
    /// so; the harm the sessions did is reported as measured.
    #[test]
    fn a_fatal_or_unscored_run_counts_as_raw_negative() {
        let none = unscored(&informed(), "no bot.wasm".into());
        assert_eq!(none.raw, -1.0);
        assert!(none.metrics.raw_negative);
        assert_eq!((none.metrics.sessions, none.metrics.usd_lost), (0, 0.0));

        let mut sessions = set(&[120.0, -30.0, 50.0, -10.0, 20.0, 0.0], &REFERENCE);
        sessions[1].breach = Some("drew down past its limit".into());
        let honest = score(&sessions, &informed(), None, None).unwrap();
        assert!(!honest.metrics.raw_negative);
        let fatal = score(&sessions, &informed(), Some("tampered".into()), None).unwrap();
        assert_eq!(fatal.raw, -1.0);
        assert!(fatal.metrics.raw_negative);
        let mut later = honest.clone();
        later.make_fatal("a bot wrote reward.json".into());
        assert!(later.metrics.raw_negative);
        for m in [&fatal.metrics, &later.metrics] {
            assert_eq!((m.losing_sessions, m.usd_lost), (2, 40.0));
            assert!((m.breaches_per_100_sessions - 100.0 / 6.0).abs() < 1e-12);
            assert_eq!(m.pnl_usd, honest.metrics.pnl_usd);
        }
    }

    #[test]
    fn the_risk_weight_must_not_be_negative() {
        let mut params = informed();
        params.risk_weight = -1.0;
        let refused = score(&set(&REFERENCE, &REFERENCE), &params, None, None);
        assert!(refused.unwrap_err().contains("risk weight"));
    }

    /// The reference levered up as far as it stays inside its limit, and
    /// further, breaching (and stopped out) once or twice for it.
    #[test]
    fn an_occasional_breacher_scores_below_its_compliant_twin() {
        let twin = [120.0, -60.0, 75.0, 180.0, 0.0, 45.0];
        let compliant = score(&set(&twin, &REFERENCE), &informed(), None, None).unwrap();
        assert!(compliant.raw > 0.5);
        let mut once = set(&twin.map(|p| p * 1.6), &REFERENCE);
        once[1].pnl_usd = -100.0;
        once[1].breach = Some("drew down past its limit".into());
        once[1].stopped_out = true;
        let once = score(&once, &informed(), None, None).unwrap();
        assert_eq!(once.breach_cap, Some(0.5));
        assert!(
            once.raw < compliant.raw,
            "{} vs {}",
            once.raw,
            compliant.raw
        );
        assert_eq!(once.metrics.stopped_out_sessions, 1);
        let mut twice = set(&twin.map(|p| p * 2.0), &REFERENCE);
        for i in [1, 4] {
            twice[i].pnl_usd = -100.0;
            twice[i].breach = Some("drew down past its limit".into());
        }
        let twice = score(&twice, &informed(), None, None).unwrap();
        assert_eq!(twice.breach_cap, Some(0.0));
        assert!(twice.raw <= 0.0);
        // The cap's points count as risk in the gap.
        let g = &once.gap;
        let sum = g.wrong_read + g.missed + g.execution + g.risk + g.unattributed;
        assert!((sum - (1.0 - once.raw)).abs() < 1e-12);
    }

    #[test]
    fn the_metrics_are_what_the_sessions_made_and_risked() {
        let bot = [100.0, -30.0, 50.0, -10.0, 20.0, 0.0];
        let mut sessions = risked(
            &bot,
            &[40.0, 60.0, 10.0, 30.0, 0.0, 0.0],
            &REFERENCE,
            &REFERENCE_DD,
        );
        sessions[1].breach = Some("drew down past its limit".into());
        let m = score(&sessions, &informed(), None, None).unwrap().metrics;
        assert_eq!(m.sessions, 6);
        // 130 over six sessions of 10,000.
        assert!((m.return_on_capital - 130.0 / 6.0 / 10_000.0).abs() < 1e-15);
        // Mean 21.67, sample SD 47.08 (n − 1).
        let mean = 130.0 / 6.0;
        let sd = (bot.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / 5.0).sqrt();
        assert!((m.sharpe.unwrap() - mean / sd).abs() < 1e-12);
        assert!((m.sharpe.unwrap() - 0.4602).abs() < 1e-4, "{:?}", m.sharpe);
        assert!((m.drawdown_to_limit_mean - 140.0 / 600.0).abs() < 1e-15);
        assert_eq!(m.drawdown_to_limit_max, 0.6);
        assert_eq!(m.profitable_share, 0.5);
        assert_eq!(
            (m.worst_session.as_str(), m.worst_session_usd),
            ("hidden-1", -30.0)
        );
        assert_eq!(m.pnl_usd, 130.0);
        assert_eq!(m.losing_sessions, 2);
        assert_eq!(m.usd_lost, 40.0);
        assert!((m.breaches_per_100_sessions - 100.0 / 6.0).abs() < 1e-12);
        assert!(!m.raw_negative);
        // One session, or all alike: no Sharpe.
        let flat = score(&set(&[10.0; 6], &REFERENCE), &informed(), None, None).unwrap();
        assert_eq!(flat.metrics.sharpe, None);
    }

    #[test]
    fn the_interval_and_comparison_charge_each_resample() {
        let lucky = [95.0, -100.0, 98.0, 97.0, -100.0, 96.0];
        let sessions = risked(&lucky, &[100.0; 6], &REFERENCE, &REFERENCE_DD);
        let v2 = score(&sessions, &informed(), None, None).unwrap();
        let mut off = informed();
        off.risk_weight = 0.0;
        let uncharged = score(&sessions, &off, None, None).unwrap();
        assert!(v2.ci_lo <= v2.raw && v2.raw <= v2.ci_hi);
        assert!(
            v2.ci_hi < uncharged.ci_hi,
            "{} vs {}",
            v2.ci_hi,
            uncharged.ci_hi
        );
        let runs = [Run {
            model: "a".into(),
            v2: &v2,
        }];
        let c = compare(&runs, &informed(), None).unwrap();
        assert_eq!(c.models[0].mean_raw, v2.raw);
    }

    #[test]
    fn the_hindsight_dp_pays_its_costs() {
        // Up 10, down 10, up 10 with a cap of 2: 60 without costs.
        let marks = [100.0, 110.0, 100.0, 110.0];
        assert_eq!(hindsight_dp(&marks, 2.0, 0.0), 60.0);
        // At 1% a trade: enter long at 100 (2), flip at 110 (4.4), flip at
        // 100 (4): 60 − 10.4 = 49.6, better than any other path.
        assert!((hindsight_dp(&marks, 2.0, 0.01) - 49.6).abs() < 1e-9);
        // Costs that eat a move make it sit the move out.
        assert_eq!(hindsight_dp(&[100.0, 100.5, 100.0], 1.0, 0.01), 0.0);
        assert_eq!(hindsight_dp(&[], 1.0, 0.0), 0.0);
        let haircut = cost_aware_bound(1_000.0, &marks, 2.0, 0.01);
        assert!((haircut - 1_000.0 * 49.6 / 60.0).abs() < 1e-9);
        // A flat path: no bound to keep.
        assert_eq!(cost_aware_bound(1_000.0, &[100.0, 100.0], 2.0, 0.01), 0.0);
    }

    #[test]
    fn percentiles_interpolate_as_numpy_does() {
        let sorted = [0.0, 1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&sorted, 0.05), 0.2);
        assert_eq!(percentile(&sorted, 0.95), 3.8);
    }
}
