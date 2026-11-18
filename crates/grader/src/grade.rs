//! Scores a bot over a set of scenarios.
//!
//! The reward is the bot's total PnL over the set as a fraction of the
//! hindsight bound: what a bot holding the full position cap the right way
//! through every move would make at no cost (see
//! [`Scenario::hindsight_bound_usd`]). Clamped to [0, 1]: doing nothing scores
//! 0, and no bot reaches 1, so the score doesn't saturate as bots improve.
//! Scoring the set as a whole lets losing scenarios count.
//!
//! The reference bot's PnL is reported alongside as a baseline, not used for
//! the reward: `skill` is the bot's total PnL over the reference's, so the
//! reference scores 1 and doing nothing 0, unclamped. On hidden sets drawn
//! fresh for each grade, the grader runs the reference itself first, on the
//! same sessions (see [`GradeOptions::reference_dir`]); otherwise scenarios
//! carry the PnL [`calibrate`] recorded.
//!
//! Any session that calls a method the RPC guard refuses, breaks its
//! mandate (trades outside its market, goes over the position cap, or draws
//! down past its limit), or ends liquidatable disqualifies the bot: the
//! reward is 0. Risk is otherwise reported, not scored: see [`Risk`].
//!
//! A finished grade can be scored again from what it wrote, without
//! running a session: see [`rescore`].

use std::{collections::BTreeMap, path::Path};

use futures::StreamExt;
use mtx_core::MeterError;
use mtx_kit::core::{Decomposition, MarkoutMeter, MarkoutSample};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{
    Error, Result,
    error::io_error,
    keys::{RoleKeys, SessionKeys},
    scenario::Scenario,
    scoring_v2::{self, ScoreV2},
    session::{self, SessionOptions, SessionReport, StepRecord, Timeline},
};

#[derive(Debug, Serialize)]
pub struct ScenarioResult {
    /// Why this session disqualifies the bot, if it does.
    pub disqualified: Option<String>,
    /// The hindsight bound for this session, at its starting mark.
    pub bound_pnl_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_pnl_usd: Option<f64>,
    /// The informed bot's PnL in this session, when the scenario carries it
    /// (see [`Scenario::informed_pnl_usd`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub informed_pnl_usd: Option<f64>,
    /// What a bot that does nothing made in this session, measured, when
    /// doing nothing may pay (see [`Scenario::nop_may_pay`]) and the grade
    /// ran under v2's rules: scoring v2's 0. Without it, doing nothing is
    /// taken to make 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nop_pnl_usd: Option<f64>,
    /// What the session's trades made against the price at the time, for
    /// scoring v2's gap (see [`trade_markout`]). Not reported.
    #[serde(skip)]
    pub trade_markout_usd: Option<f64>,
    /// The reference's and the informed bot's drawdowns in this session,
    /// for scoring v2's near-breach charge: the scenario's, or the
    /// reference's as the grade measured it when it ran the reference. Not
    /// reported.
    #[serde(skip)]
    pub reference_max_drawdown_usd: Option<f64>,
    #[serde(skip)]
    pub informed_max_drawdown_usd: Option<f64>,
    /// The session's PnL split into markout, luck and a residual, for a
    /// scenario that reports a fair value (see [`Scenario::fair_path`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markout: Option<Decomposition>,
    #[serde(flatten)]
    pub session: SessionReport,
}

#[derive(Debug, Serialize)]
pub struct GradeReport {
    /// Total PnL over the total hindsight bound, clamped to [0, 1].
    pub reward: f64,
    pub pnl_usd: f64,
    pub bound_pnl_usd: f64,
    /// The reference's total PnL, when every scenario carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_pnl_usd: Option<f64>,
    /// Total PnL over the reference's, unclamped: above 1 beats the reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_ratio: Option<f64>,
    /// The headline for comparing bots: [`Self::reference_ratio`], or 0 when
    /// the bot is disqualified. A breach never scores better than complying.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<f64>,
    /// Why the grade itself was tampered with, if it was: the reward is 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tampered: Option<String>,
    /// Why the bot wasn't graded at all, if it wasn't (a grade of strategy
    /// components only, and no `bot.wasm`): the reward is 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_graded: Option<String>,
    /// The SHA-256 of the secret a drawn hidden set came from: enough to
    /// tell two grades on the same set, nothing more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_seed_sha256: Option<String>,
    pub risk: Risk,
    /// `mtx grade --replay`: whether every run came out the same.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay: Option<crate::replay::ReplayCheck>,
    /// The sessions' markout, luck and residual, summed, when every scenario
    /// reports a fair value. Reported, not scored: `reward` and `skill` are
    /// PnL's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markout: Option<Decomposition>,
    /// Scoring v2, when asked for (see [`crate::scoring_v2`] and
    /// [`GradeReport::score_v2`]): reported beside `reward` and `skill`,
    /// which it leaves as they are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v2: Option<ScoreV2>,
    /// `"v2"` when scoring v2 is the grade's one score
    /// ([`scoring_v2::Params::headline`]): `reward` is then v2's clamped
    /// score, and `skill` and `reference_ratio` are left out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    /// Set when the grade stopped at the bot's first breach
    /// ([`GradeOptions::stop_on_disqualify`]): `scenarios` is then only the
    /// sessions up to it, which scoring v2 refuses to score.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Truncated>,
    pub scenarios: Vec<ScenarioResult>,
    /// The reference's sessions, when the grade ran it: kept out of the
    /// report (its PnL is in [`ScenarioResult::reference_pnl_usd`]), for the
    /// replay check.
    #[serde(skip)]
    pub anchors: Vec<SessionReport>,
}

/// A grade that stopped at the bot's first breach: how many of its sessions
/// it graded, of how many it was given.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Truncated {
    pub graded: usize,
    pub of: usize,
}

/// How the bot made its PnL, across sessions. Reported, not scored: with a
/// dozen short sessions these are rough, and a reward that blended them in
/// would reward whatever weights it picked. The drawdown limit is the risk
/// rule that is enforced.
#[derive(Debug, Serialize, PartialEq)]
pub struct Risk {
    /// The worst session's PnL.
    pub worst_pnl_usd: f64,
    /// The mean PnL of the worst sixth of sessions (at least one): a tail loss.
    pub tail_pnl_usd: f64,
    /// The deepest any session's account value fell below its running peak.
    pub max_drawdown_usd: f64,
    /// The share of sessions that made money.
    pub profitable_share: f64,
    /// Mean session PnL over its standard deviation (0 when it doesn't vary).
    pub sharpe: f64,
}

impl Risk {
    fn of(sessions: &[ScenarioResult]) -> Self {
        let mut pnls: Vec<f64> = sessions.iter().map(|s| s.session.pnl_usd).collect();
        if pnls.is_empty() {
            return Self {
                worst_pnl_usd: 0.0,
                tail_pnl_usd: 0.0,
                max_drawdown_usd: 0.0,
                profitable_share: 0.0,
                sharpe: 0.0,
            };
        }
        pnls.sort_by(f64::total_cmp);
        let n = pnls.len() as f64;
        let tail = pnls.len().div_ceil(6);
        let mean = pnls.iter().sum::<f64>() / n;
        let variance = if pnls.len() > 1 {
            pnls.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / (n - 1.0)
        } else {
            0.0
        };
        Self {
            worst_pnl_usd: pnls[0],
            tail_pnl_usd: pnls[..tail].iter().sum::<f64>() / tail as f64,
            max_drawdown_usd: sessions
                .iter()
                .map(|s| s.session.max_drawdown_usd)
                .fold(0.0, f64::max),
            profitable_share: pnls.iter().filter(|p| **p > 0.0).count() as f64 / n,
            sharpe: if variance > 0.0 {
                mean / variance.sqrt()
            } else {
                0.0
            },
        }
    }
}

impl GradeReport {
    /// Disqualify the bot for interfering with grading itself, e.g. writing
    /// the reward file the grader is about to write.
    pub fn tamper(&mut self, reason: String) {
        self.reward = 0.0;
        if let Some(v2) = &mut self.v2 {
            v2.make_fatal(reason.clone());
        }
        self.tampered = Some(reason);
    }

    /// Add scoring v2 to the report, from its sessions as scored. Fails,
    /// leaving the report as it was, if v2 can't score them: a session
    /// without a reference PnL, a spot session whose starting wallet makes
    /// doing nothing something other than 0, or a ceiling that doesn't fit.
    pub fn score_v2(&mut self, params: &scoring_v2::Params) -> Result<()> {
        // The set is the sessions up to the first breach: a score over them
        // would depend on where the breach fell, not on the bot.
        if let Some(Truncated { graded, of }) = self.truncated {
            return Err(Error::ScoringV2(format!(
                "the grade stopped at its first breach, after {graded} of {of} sessions, and \
                 v2 scores every session: grade without `--stop-on-disqualify`"
            )));
        }
        let fatal = self
            .not_graded
            .clone()
            .or_else(|| self.tampered.clone())
            .or_else(|| {
                self.scenarios
                    .iter()
                    .find(|s| !s.session.refused_calls.is_empty())
                    .and_then(|s| s.disqualified.clone())
            });
        if let (true, Some(reason)) = (self.scenarios.is_empty(), &fatal) {
            return self.set_v2(scoring_v2::unscored(params, reason.clone()), params);
        }
        let v2 = self
            .scenarios
            .iter()
            .map(|result| {
                let session = &result.session;
                // The cost-aware hindsight ceiling, on the reference's marks
                // only: the bot's own marks move with its trades, so a bot
                // could lower its own ceiling. Without a reference session
                // there is none, and v2 refuses a grade that needs it. It
                // reads one market's marks and cap: on several, the ceiling
                // is the informed bot's, or a given one.
                let reference = self
                    .anchors
                    .iter()
                    .find(|a| a.scenario == session.scenario && !a.timeline.steps.is_empty());
                let one_market = session.markets.is_empty();
                let hindsight = reference
                    .zip(session.mandate.universe.first())
                    .filter(|_| one_market)
                    .map(|(reference, allowance)| {
                        let marks: Vec<f64> = reference
                            .timeline
                            .steps
                            .iter()
                            .map(|step| step.mark_usd)
                            .collect();
                        scoring_v2::cost_aware_bound(
                            result.bound_pnl_usd,
                            &marks,
                            allowance.max_position,
                            params.hindsight_cost,
                        )
                    });
                if !one_market
                    && result.informed_pnl_usd.is_none()
                    && matches!(
                        params.ceiling,
                        scoring_v2::Ceiling::Informed { eta_override: None }
                    )
                {
                    return Err(format!(
                        "{}: the cost-aware hindsight ceiling reads one market's marks; this \
                         session trades {}, and needs the informed bot's PnL",
                        session.scenario,
                        session
                            .markets
                            .iter()
                            .map(|m| m.symbol.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let reference_pnl_usd = result.reference_pnl_usd.ok_or_else(|| {
                    format!("{}: no reference PnL to anchor to", session.scenario)
                })?;
                // Where a bot that does nothing was run, its PnL is v2's 0,
                // whatever the session started with.
                let nop_measured = result.nop_pnl_usd.is_some();
                if session.spot.is_some() && session.net.is_none() {
                    return Err(format!(
                        "{}: a spot session's starting wallet can move with the price, and \
                         v2 doesn't run a nop anchor yet",
                        session.scenario
                    ));
                }
                // A perp start position moves with the price too, unless an
                // order or net scoring scores the session against doing
                // nothing (holding the start); and it earns or pays the
                // scenario's funding, which neither's drift takes out.
                let starts_held = session.start_position != 0.0
                    || session.markets.iter().any(|m| m.start_position != 0.0);
                if starts_held && !nop_measured && session.order.is_none() && session.net.is_none()
                {
                    return Err(format!(
                        "{}: a perp session's starting position can move with the price, \
                         and v2 doesn't run a nop anchor yet",
                        session.scenario
                    ));
                }
                if session.start_funded && !nop_measured {
                    return Err(format!(
                        "{}: a perp session's starting position accrues the scenario's \
                         funding, and v2 doesn't run a nop anchor yet",
                        session.scenario
                    ));
                }
                // The risk desk's stop-out (the scenario's `stop_out`): its
                // close is in the session's PnL, and a breach all the same,
                // though the mandate's audit no longer disqualifies it. A
                // keeper's liquidation is priced, not a breach.
                let stop_out = session
                    .forced_closes
                    .iter()
                    .find_map(|close| match &close.reason {
                        crate::risk::ForcedCloseReason::StopOut { rule } => Some(*rule),
                        crate::risk::ForcedCloseReason::Liquidation { .. } => None,
                    });
                Ok(scoring_v2::Session {
                    scenario: session.scenario.clone(),
                    pnl_usd: session.pnl_usd,
                    reference_pnl_usd,
                    informed_pnl_usd: result.informed_pnl_usd,
                    hindsight_ceiling_usd: hindsight,
                    reference_session_sha256: reference.map(reference_sha256),
                    nop_pnl_usd: result.nop_pnl_usd.unwrap_or(0.0),
                    bound_usd: result.bound_pnl_usd,
                    // Without a drawdown limit, the most a session could
                    // lose is taken as its hindsight bound.
                    worst_loss_usd: session
                        .mandate
                        .limits
                        .max_drawdown_usd
                        .unwrap_or(result.bound_pnl_usd),
                    worst_loss_from_bound: session.mandate.limits.max_drawdown_usd.is_none(),
                    breach: result
                        .disqualified
                        .clone()
                        .filter(|_| session.refused_calls.is_empty())
                        .or_else(|| {
                            stop_out.map(|rule| match rule {
                                crate::scenario::StopRule::Drawdown => {
                                    "stopped out: drew down past its limit".to_string()
                                }
                            })
                        }),
                    max_drawdown_usd: session.max_drawdown_usd,
                    reference_max_drawdown_usd: result.reference_max_drawdown_usd,
                    informed_max_drawdown_usd: result.informed_max_drawdown_usd,
                    start_value_usd: session.start_value_usd,
                    stopped_out: stop_out.is_some(),
                    traded: traded(session),
                    trade_markout_usd: result.trade_markout_usd,
                })
            })
            .collect::<std::result::Result<Vec<_>, String>>()
            .and_then(|sessions| {
                scoring_v2::score(
                    &sessions,
                    params,
                    fatal.clone(),
                    self.eval_seed_sha256.as_deref(),
                )
            });
        // A fatal grade is the floor even when its sessions can't be scored
        // (a session without a reference PnL, say).
        let v2 = match (v2, fatal) {
            (Ok(v2), _) => v2,
            (Err(_), Some(reason)) => scoring_v2::unscored(params, reason),
            (Err(why), None) => return Err(Error::ScoringV2(why)),
        };
        self.set_v2(v2, params)
    }

    /// Keep `v2` in the report; when it is the headline, it is the grade's
    /// one score, on real anchors: its 1 is the task's informed bot (no
    /// hindsight, proxy or given ceiling), and no v1 score is beside it.
    fn set_v2(&mut self, v2: ScoreV2, params: &scoring_v2::Params) -> Result<()> {
        if params.headline {
            if v2.fatal.is_none() && v2.ceiling != "informed" {
                return Err(Error::ScoringV2(format!(
                    "v2 is this grade's headline, but its 1 is `{}`: every session needs the \
                     task's informed bot's PnL",
                    v2.ceiling
                )));
            }
            self.reward = v2.score;
            self.skill = None;
            self.reference_ratio = None;
            self.headline = Some("v2".into());
        }
        self.v2 = Some(v2);
        Ok(())
    }

    /// Harbor's `reward.json`: flat numeric values. `reward` is the headline;
    /// the rest are diagnostics that aggregate across trials.
    pub fn rewards(&self) -> BTreeMap<String, f64> {
        let mut rewards = BTreeMap::from([
            ("reward".to_string(), self.reward),
            ("pnl_usd".to_string(), self.pnl_usd),
            ("bound_pnl_usd".to_string(), self.bound_pnl_usd),
            (
                "refused_calls".to_string(),
                self.scenarios
                    .iter()
                    .map(|s| s.session.refused_calls.len() as f64)
                    .sum(),
            ),
            (
                "disqualified".to_string(),
                self.scenarios
                    .iter()
                    .filter(|s| s.disqualified.is_some())
                    .count() as f64,
            ),
            (
                "tampered".to_string(),
                f64::from(u8::from(self.tampered.is_some())),
            ),
            (
                "not_graded".to_string(),
                f64::from(u8::from(self.not_graded.is_some())),
            ),
            ("worst_pnl_usd".to_string(), self.risk.worst_pnl_usd),
            ("tail_pnl_usd".to_string(), self.risk.tail_pnl_usd),
            ("max_drawdown_usd".to_string(), self.risk.max_drawdown_usd),
            ("profitable_share".to_string(), self.risk.profitable_share),
            ("sharpe".to_string(), self.risk.sharpe),
        ]);
        if let (Some(reference), Some(ratio)) = (self.reference_pnl_usd, self.reference_ratio) {
            rewards.insert("reference_pnl_usd".to_string(), reference);
            rewards.insert("reference_ratio".to_string(), ratio);
        }
        if let Some(skill) = self.skill {
            rewards.insert("skill".to_string(), skill);
        }
        if let Some(markout) = &self.markout {
            rewards.insert("markout_usd".to_string(), markout.markout_usd);
            rewards.insert("luck_usd".to_string(), markout.luck_usd);
            rewards.insert("edge_usd".to_string(), markout.edge_usd());
        }
        if let Some(v2) = &self.v2 {
            rewards.extend([
                ("score_v2".to_string(), v2.score),
                ("score_v2_raw".to_string(), v2.raw),
                ("score_v2_raw_unfloored".to_string(), v2.raw_unfloored),
                ("score_v2_edge".to_string(), v2.edge),
                (
                    "score_v2_raw_before_charge".to_string(),
                    v2.raw_before_charge.unwrap_or(v2.raw),
                ),
                ("score_v2_ci_lo".to_string(), v2.ci_lo),
                ("score_v2_ci_hi".to_string(), v2.ci_hi),
                (
                    "score_v2_within_noise".to_string(),
                    f64::from(u8::from(v2.within_noise)),
                ),
                (
                    "score_v2_anchor_floored".to_string(),
                    f64::from(u8::from(v2.anchor_floored)),
                ),
                (
                    "score_v2_fatal".to_string(),
                    f64::from(u8::from(v2.fatal.is_some())),
                ),
                ("nop_pnl_usd".to_string(), v2.nop_usd),
                ("ceiling_pnl_usd".to_string(), v2.ceiling_usd),
                (
                    "score_v2_beats_informed".to_string(),
                    f64::from(u8::from(v2.beats_informed)),
                ),
                (
                    "score_v2_ceiling_informed".to_string(),
                    f64::from(u8::from(v2.ceiling == "informed")),
                ),
                ("score_v2_gap_wrong_read".to_string(), v2.gap.wrong_read),
                ("score_v2_gap_missed".to_string(), v2.gap.missed),
                ("score_v2_gap_execution".to_string(), v2.gap.execution),
                ("score_v2_gap_risk".to_string(), v2.gap.risk),
                ("score_v2_gap_noise".to_string(), v2.gap.noise),
                ("breached_sessions".to_string(), v2.breached_sessions as f64),
                ("score_v2_risk_charge_usd".to_string(), v2.risk.charge_usd),
                ("score_v2_dd_rms".to_string(), v2.risk.dd_rms),
                ("score_v2_risk_allowance".to_string(), v2.risk.allowance),
                ("score_v2_risk_weight".to_string(), v2.risk.weight),
            ]);
            if v2.breached_sessions > 0 {
                rewards.insert("score_v2_breach_penalty".to_string(), v2.breach_penalty);
            }
            if let Some(anchor) = v2.risk.dd_rms_anchor {
                rewards.insert("score_v2_dd_rms_anchor".to_string(), anchor);
            }
            let m = &v2.metrics;
            rewards.extend([
                ("metric_return_on_capital".to_string(), m.return_on_capital),
                (
                    "metric_drawdown_to_limit_mean".to_string(),
                    m.drawdown_to_limit_mean,
                ),
                (
                    "metric_drawdown_to_limit_max".to_string(),
                    m.drawdown_to_limit_max,
                ),
                ("metric_profitable_share".to_string(), m.profitable_share),
                ("metric_worst_session_usd".to_string(), m.worst_session_usd),
                (
                    "metric_losing_sessions".to_string(),
                    m.losing_sessions as f64,
                ),
                ("metric_usd_lost".to_string(), m.usd_lost),
                (
                    "metric_breaches_per_100_sessions".to_string(),
                    m.breaches_per_100_sessions,
                ),
                (
                    "metric_stopped_out_sessions".to_string(),
                    m.stopped_out_sessions as f64,
                ),
                (
                    "metric_raw_negative".to_string(),
                    f64::from(u8::from(m.raw_negative)),
                ),
            ]);
            if let Some(sharpe) = m.sharpe {
                rewards.insert("metric_sharpe".to_string(), sharpe);
            }
        }
        for result in &self.scenarios {
            rewards.insert(
                format!("pnl_{}", result.session.scenario),
                result.session.pnl_usd,
            );
        }
        rewards
    }
}

pub struct GradeOptions<'a> {
    pub snapshot_dir: &'a Path,
    pub bot_dir: &'a Path,
    pub scenarios: &'a [Scenario],
    /// Bot logs go here as `<scenario>.log`.
    pub log_dir: &'a Path,
    /// How many sessions to run at once.
    pub jobs: usize,
    /// The first of [`session::BOT_USERS`] this grade's bots run as; it
    /// uses `jobs` of them from there. Graders running at once on one
    /// machine never share a user at the same time (see
    /// [`session::BotUserLock`]); ranges that don't overlap keep them from
    /// waiting for each other.
    pub first_bot_user: usize,
    /// A reference bot to run on the same scenarios first, as the baseline,
    /// with `anchor_jobs` sessions at once (it's trusted: it can share the
    /// machine with itself). Its PnL replaces whatever the scenarios carry.
    pub reference_dir: Option<&'a Path>,
    /// The task's informed bot, to run on the same scenarios after the
    /// reference, as scoring v2's 1 (see [`Informed`]). Its PnL replaces
    /// whatever the scenarios carry, as the reference's does.
    pub informed: Option<Informed<'a>>,
    pub anchor_jobs: usize,
    /// Each scenario's regime, as JSON, handed to the bot (not the
    /// reference): the ladder's regime oracle only.
    pub regimes: Option<&'a [String]>,
    /// Stop at the bot's first disqualifying session: the grade is 0
    /// whatever the rest do, and a bot that ignores turns runs each session
    /// in real time. The report then covers the sessions up to it.
    pub stop_on_disqualify: bool,
    /// Where to write each session step by step, for review: one file per
    /// session, as `bot/<scenario>.json.gz` and `reference/<scenario>.json.gz`.
    /// They hold the scenarios themselves, so they're written once every
    /// session has ended: a bot can read the verifier's logs while it runs.
    pub detail_dir: Option<&'a Path>,
    /// What each scenario was drawn as (a shock's shape, which taker orders
    /// were informed), recorded in its detail file. Never handed to a bot.
    pub annotations: Option<&'a [serde_json::Value]>,
    /// Grade the bot only if it is a strategy component (a `bot.wasm`): the
    /// verifier's rule. The reference can still be a process.
    pub components_only: bool,
    /// The bot is trusted, as the reference is (an anchor or a ladder rung
    /// graded as the bot): see [`SessionOptions::trusted`].
    pub trusted: bool,
    /// Watches a component's sessions step by step (`mtx bot run --debug`).
    pub observer: Option<std::sync::Arc<dyn crate::wasm::StepObserver>>,
    /// Where the sessions' keypairs come from (see [`crate::keys`]): the same
    /// keys every run of the grade.
    pub keys: &'a SessionKeys,
    /// Run the sessions under scoring v2's rules (see [`session::Rules`]):
    /// positions are sampled every slot, and a session where doing nothing
    /// may pay is also run by a bot that does nothing, whose PnL is v2's 0
    /// there. Off, the grade runs as it always has. (The bot's seeds are
    /// salted through `keys`: see [`SessionKeys::salt_strategy_seeds`].)
    pub v2_sessions: bool,
}

/// The task's informed bot as a grade runs it (see
/// [`GradeOptions::informed`]): trusted like the reference, with
/// `anchor_jobs` sessions at once, on sessions of its own, after the
/// reference's and before the bot's.
pub struct Informed<'a> {
    pub dir: &'a Path,
    /// Each scenario's latent state ([`crate::generators::Hidden::latent`]),
    /// handed to the informed bot as `MTX_REGIME`, and to no other bot: what
    /// the task's data could tell in principle, never the session's future.
    /// None for scenarios from files, which carry no draw.
    pub latent: Option<&'a [String]>,
}

/// A directory only the grader can enter, removed with everything in it
/// when dropped.
struct PrivateDir(std::path::PathBuf);

impl PrivateDir {
    fn create(prefix: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
            .create(&path)
            .map_err(io_error(&path))?;
        Ok(Self(path))
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A phase's log files, by path relative to its log directory.
type HeldLogs = Vec<(std::path::PathBuf, Vec<u8>)>;

/// What a trusted phase logged, taken out of its private directory. A phase
/// that failed has run before any bot did, and its logs are the only evidence
/// of why, so they are written to `to` at once.
fn keep_logs<T>(ran: Result<T>, logs: PrivateDir, to: &Path) -> Result<(T, HeldLogs)> {
    let held = read_files(&logs.0);
    match (ran, held) {
        (Ok(value), Ok(held)) => Ok((value, held)),
        (Ok(_), Err(e)) => Err(e),
        (Err(e), held) => {
            if let Ok(held) = held {
                let _ = write_files(to, &held);
            }
            Err(e)
        }
    }
}

/// Every file under `dir`, by its path relative to `dir`, read into
/// memory.
fn read_files(dir: &Path) -> Result<Vec<(std::path::PathBuf, Vec<u8>)>> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(at) = pending.pop() {
        for entry in std::fs::read_dir(&at).map_err(io_error(&at))? {
            let path = entry.map_err(io_error(&at))?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = std::fs::read(&path).map_err(io_error(&path))?;
                let relative = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
                files.push((relative, bytes));
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Write `files` (see [`read_files`]) under `to`, replacing what it held.
fn write_files(to: &Path, files: &[(std::path::PathBuf, Vec<u8>)]) -> Result<()> {
    if to.exists() {
        std::fs::remove_dir_all(to).map_err(io_error(to))?;
    }
    for (relative, bytes) in files {
        let path = to.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_error(parent))?;
        }
        std::fs::write(&path, bytes).map_err(io_error(&path))?;
    }
    Ok(())
}

/// One session's detail file (see [`GradeOptions::detail_dir`]).
#[derive(Serialize)]
struct SessionDetail<'a> {
    role: &'static str,
    scenario: &'a Scenario,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotation: Option<&'a serde_json::Value>,
    disqualified: Option<String>,
    bound_pnl_usd: f64,
    report: &'a SessionReport,
    timeline: &'a session::Timeline,
}

fn write_details(
    dir: &Path,
    role: &'static str,
    sessions: &[SessionReport],
    scenarios: &[Scenario],
    annotations: Option<&[serde_json::Value]>,
) -> Result<()> {
    let dir = dir.join(role);
    std::fs::create_dir_all(&dir).map_err(io_error(&dir))?;
    for (index, (report, scenario)) in sessions.iter().zip(scenarios).enumerate() {
        let detail = SessionDetail {
            role,
            scenario,
            annotation: annotations.and_then(|a| a.get(index)),
            disqualified: disqualification(report),
            bound_pnl_usd: bound_of(scenario, report),
            report,
            timeline: &report.timeline,
        };
        // Gzipped: a trial writes two of these per session.
        let path = dir.join(format!("{}.json.gz", scenario.name));
        let file = std::fs::File::create(&path).map_err(io_error(&path))?;
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        serde_json::to_writer(&mut gz, &detail)?;
        gz.finish().map_err(io_error(&path))?;
    }
    Ok(())
}

pub async fn grade(opts: GradeOptions<'_>) -> Result<GradeReport> {
    if opts.components_only && crate::wasm::component_in(opts.bot_dir).is_none() {
        let reason = Error::NotAComponent(opts.bot_dir.to_path_buf()).to_string();
        tracing::warn!("{reason}: the reward is 0");
        return Ok(not_graded(reason));
    }
    let mut scenarios = opts.scenarios.to_vec();
    let mut anchored = Vec::new();
    // The reference plays the bot's world, so its logs say when its flow
    // arrives, and the informed bot's may say what it was told: each phase
    // logs where only the grader can enter and is held in memory, with
    // nothing of it on disk, while the bot runs.
    let mut reference_logs = None;
    if let Some(reference) = opts.reference_dir {
        let logs = PrivateDir::create("mtx-reference-logs")?;
        let ran = async {
            let anchors = run_sessions(
                opts.snapshot_dir,
                reference,
                &scenarios,
                &logs.0,
                opts.first_bot_user..opts.first_bot_user + opts.anchor_jobs,
                None,
                false,
                true,
                None,
                opts.keys.role("reference"),
                // The anchor's drawdown at the bot's granularity.
                session::Rules {
                    every_slot: opts.v2_sessions,
                },
            )
            .await?;
            check_timing(&anchors)?;
            check_anchors("the reference", &anchors)?;
            Ok(anchors)
        }
        .await;
        let (anchors, held) = keep_logs(ran, logs, &opts.log_dir.join("reference"))?;
        for (scenario, anchor) in scenarios.iter_mut().zip(&anchors) {
            scenario.reference_pnl_usd = Some((anchor.pnl_usd * 100.0).round() / 100.0);
        }
        info!(
            reference_pnl_usd = anchors.iter().map(|a| a.pnl_usd).sum::<f64>(),
            "reference graded"
        );
        anchored = anchors;
        reference_logs = Some(held);
    }
    let mut informed_sessions = Vec::new();
    let mut informed_logs = None;
    if let Some(informed) = &opts.informed {
        let logs = PrivateDir::create("mtx-informed-logs")?;
        let ran = async {
            let sessions = run_sessions(
                opts.snapshot_dir,
                informed.dir,
                &scenarios,
                &logs.0,
                opts.first_bot_user..opts.first_bot_user + opts.anchor_jobs,
                informed.latent,
                false,
                true,
                None,
                opts.keys.role("informed"),
                session::Rules {
                    every_slot: opts.v2_sessions,
                },
            )
            .await?;
            check_timing(&sessions)?;
            check_anchors("the informed bot", &sessions)?;
            Ok(sessions)
        }
        .await;
        let (sessions, held) = keep_logs(ran, logs, &opts.log_dir.join("informed"))?;
        // As `calibrate --informed` saves them in a scenario file.
        for (scenario, session) in scenarios.iter_mut().zip(&sessions) {
            scenario.informed_pnl_usd = Some((session.pnl_usd * 100.0).round() / 100.0);
            scenario.informed_max_drawdown_usd =
                Some((session.max_drawdown_usd * 100.0).round() / 100.0);
        }
        info!(
            informed_pnl_usd = sessions.iter().map(|s| s.pnl_usd).sum::<f64>(),
            "informed bot graded"
        );
        informed_sessions = sessions;
        informed_logs = Some(held);
    }
    // No bot has run yet, or every bot has stopped: where a later phase
    // fails, the held logs are the evidence of why, so they are written
    // before the error goes on.
    let publish_held = || -> Result<()> {
        if let Some(logs) = &reference_logs {
            write_files(&opts.log_dir.join("reference"), logs)?;
        }
        if let Some(logs) = &informed_logs {
            write_files(&opts.log_dir.join("informed"), logs)?;
        }
        Ok(())
    };
    // What doing nothing makes, where it isn't 0: measured by a bot that
    // does nothing, so no rule has to say what holding the start is worth.
    let nops = if opts.v2_sessions {
        match run_nop(&opts, &scenarios).await {
            Ok(nops) => nops,
            Err(e) => {
                publish_held()?;
                return Err(e);
            }
        }
    } else {
        vec![None; scenarios.len()]
    };
    let ran = run_sessions(
        opts.snapshot_dir,
        opts.bot_dir,
        &scenarios,
        opts.log_dir,
        opts.first_bot_user..opts.first_bot_user + opts.jobs,
        opts.regimes,
        opts.stop_on_disqualify,
        opts.trusted,
        opts.observer.clone(),
        opts.keys.role("bot"),
        session::Rules {
            every_slot: opts.v2_sessions,
        },
    )
    .await;
    // Every bot has stopped: the held logs join the others, and the
    // scenarios can be written out.
    publish_held()?;
    let sessions = ran?;
    check_timing(&sessions)?;
    if let Some(dir) = opts.detail_dir {
        // Whatever a bot left there goes first, links included: the
        // grader writes as root.
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(dir).map_err(io_error(dir))?,
            Ok(_) => std::fs::remove_file(dir).map_err(io_error(dir))?,
            Err(_) => {}
        }
        write_details(dir, "reference", &anchored, &scenarios, opts.annotations)?;
        if opts.informed.is_some() {
            write_details(
                dir,
                "informed",
                &informed_sessions,
                &scenarios,
                opts.annotations,
            )?;
        }
        write_details(dir, "bot", &sessions, &scenarios, opts.annotations)?;
    }
    let graded = sessions.len();
    let mut report = score(sessions.into_iter().zip(&scenarios).collect());
    report.truncated = (graded < scenarios.len()).then_some(Truncated {
        graded,
        of: scenarios.len(),
    });
    for (result, nop) in report.scenarios.iter_mut().zip(nops) {
        result.nop_pnl_usd = nop;
    }
    // The reference's drawdowns as this grade measured them, for scoring
    // v2: kept off the scenarios, which the detail files hold.
    for (result, anchor) in report.scenarios.iter_mut().zip(&anchored) {
        if anchor.scenario == result.session.scenario {
            result.reference_max_drawdown_usd = Some(anchor.max_drawdown_usd);
        }
    }
    report.anchors = anchored;
    // The scores are the caller's to log: the grade may yet be scored by
    // v2 alone, and then v1's are never shown.
    info!(
        pnl_usd = report.pnl_usd,
        bound_pnl_usd = report.bound_pnl_usd,
        "graded"
    );
    Ok(report)
}

/// A process bot that answers every turn and does nothing.
const NOP_BOT: &str = r#"#!/bin/bash
while IFS= read -r line <&3; do
  case "$line" in
    *'"t":"hello"'*) printf '{"t":"ready"}\n' >&3 ;;
    *'"t":"step"'*) k=${line#*\"k\":}; k=${k%%[,\}]*}; printf '{"t":"done","k":%s}\n' "$k" >&3 ;;
    *'"t":"end"'*) exit 0 ;;
  esac
done
"#;

/// Run a bot that does nothing on each of `scenarios` where doing nothing
/// may pay (see [`Scenario::nop_may_pay`]), and return its PnL in each,
/// `None` where doing nothing is 0 without running it.
async fn run_nop(opts: &GradeOptions<'_>, scenarios: &[Scenario]) -> Result<Vec<Option<f64>>> {
    let needing: Vec<usize> = scenarios
        .iter()
        .enumerate()
        .filter(|(_, scenario)| scenario.nop_may_pay())
        .map(|(i, _)| i)
        .collect();
    let mut nops = vec![None; scenarios.len()];
    if needing.is_empty() {
        return Ok(nops);
    }
    let dir = std::env::temp_dir().join(format!("mtx-nop-bot-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).map_err(io_error(&dir))?;
    let script = dir.join("run.sh");
    std::fs::write(&script, NOP_BOT).map_err(io_error(&script))?;
    // A process bot runs as its own user, which has to read it.
    for path in [&dir, &script] {
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .map_err(io_error(path))?;
    }
    let subset: Vec<Scenario> = needing.iter().map(|&i| scenarios[i].clone()).collect();
    let users = session::BOT_USERS
        .len()
        .saturating_sub(opts.first_bot_user)
        .max(1);
    let jobs = opts.anchor_jobs.clamp(1, users);
    let sessions = run_sessions(
        opts.snapshot_dir,
        &dir,
        &subset,
        &dir.join("logs"),
        opts.first_bot_user..opts.first_bot_user + jobs,
        None,
        false,
        true,
        None,
        opts.keys.role("nop"),
        session::Rules {
            every_slot: opts.v2_sessions,
        },
    )
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    let sessions = sessions?;
    check_timing(&sessions)?;
    for (&i, session) in needing.iter().zip(&sessions) {
        nops[i] = Some((session.pnl_usd * 100.0).round() / 100.0);
    }
    info!(
        sessions = sessions.len(),
        nop_pnl_usd = sessions.iter().map(|s| s.pnl_usd).sum::<f64>(),
        "a bot that does nothing graded"
    );
    Ok(nops)
}

/// The report of a bot that wasn't graded at all: 0.
fn not_graded(reason: String) -> GradeReport {
    let mut report = score(Vec::new());
    report.not_graded = Some(reason);
    report.skill = Some(0.0);
    report
}

/// A session as its detail file holds it (see [`GradeOptions::detail_dir`]):
/// what [`rescore`] reads back.
#[derive(Debug, Deserialize)]
pub struct StoredSession {
    /// The scenario as the session played it, the reference's PnL in it.
    pub scenario: Scenario,
    pub report: SessionReport,
    pub timeline: StoredTimeline,
}

/// The part of a session's timeline scoring reads: the market and the
/// account at every step. The transactions and calls are left out.
#[derive(Debug, Deserialize)]
pub struct StoredTimeline {
    pub start_slot: u64,
    pub end_slot: u64,
    pub slots_per_step: u64,
    pub start_mark_usd: f64,
    pub steps: Vec<StepRecord>,
}

impl StoredSession {
    /// The session's report, its timeline's steps back in it, and its
    /// scenario.
    pub fn into_parts(self) -> (SessionReport, Scenario) {
        let StoredTimeline {
            start_slot,
            end_slot,
            slots_per_step,
            start_mark_usd,
            steps,
        } = self.timeline;
        let mut report = self.report;
        report.timeline = Timeline {
            start_slot,
            end_slot,
            slots_per_step,
            start_mark_usd,
            steps,
            ..Timeline::default()
        };
        (report, self.scenario)
    }
}

/// What a grade's report says beyond its sessions, read back from it for
/// [`rescore`].
#[derive(Debug, Default, Deserialize)]
pub struct Verdict {
    pub tampered: Option<String>,
    pub not_graded: Option<String>,
    pub eval_seed_sha256: Option<String>,
    pub truncated: Option<Truncated>,
    /// The report's sessions, for what only they hold (see
    /// [`ScenarioResult::nop_pnl_usd`]).
    #[serde(default)]
    pub scenarios: Vec<StoredNop>,
}

/// What a report's session says of the nop bot's run.
#[derive(Debug, Default, Deserialize)]
pub struct StoredNop {
    #[serde(default)]
    pub nop_pnl_usd: Option<f64>,
}

/// Score a finished grade again from what it wrote, without running a
/// session: its bot's sessions, in the report's order, each with the
/// scenario it played (see [`StoredSession`]), and what the report said
/// beyond them. With the scoring rules unchanged, the result is the grade's
/// report, but for a replay check's section and the reference's sessions;
/// with them changed, the grade the same sessions get under the new ones.
pub fn rescore(sessions: Vec<(SessionReport, Scenario)>, verdict: Verdict) -> GradeReport {
    let mut report = match verdict.not_graded {
        Some(reason) => not_graded(reason),
        None => {
            let (reports, scenarios): (Vec<_>, Vec<_>) = sessions.into_iter().unzip();
            score(reports.into_iter().zip(&scenarios).collect())
        }
    };
    report.eval_seed_sha256 = verdict.eval_seed_sha256;
    report.truncated = verdict.truncated;
    for (result, stored) in report.scenarios.iter_mut().zip(&verdict.scenarios) {
        result.nop_pnl_usd = stored.nop_pnl_usd;
    }
    if let Some(reason) = verdict.tampered {
        report.tamper(reason);
    }
    report
}

/// Lift this process's soft limit on open files to its hard limit. Every
/// session's surfnet, RPC guards and clients live in this one process: with
/// several sessions at once, a container's default soft limit of 1,024 can
/// run out, and a request that can't open its socket fails the grade.
fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit only writes the struct given.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    // macOS refuses a soft limit above OPEN_MAX, and its hard limit is
    // usually unlimited.
    #[cfg(target_os = "macos")]
    let target = limit.rlim_max.min(10_240);
    #[cfg(not(target_os = "macos"))]
    let target = limit.rlim_max;
    if limit.rlim_cur >= target {
        return;
    }
    let raised = libc::rlimit {
        rlim_cur: target,
        ..limit
    };
    // SAFETY: setrlimit only reads the struct given.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
        warn!(soft = limit.rlim_cur, "couldn't raise the open-file limit");
    }
}

/// SHA-256 of what scoring v2 reads of a reference session: its marks, step
/// by step (the hindsight ceiling's input), then its max drawdown (the
/// near-breach charge's anchor), each `f64`'s little-endian bytes. Recorded
/// per session, for a rescore to check it reads the same session.
pub fn reference_sha256(reference: &SessionReport) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for step in &reference.timeline.steps {
        hash.update(step.mark_usd.to_le_bytes());
    }
    hash.update(reference.max_drawdown_usd.to_le_bytes());
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Score sessions against their scenarios' hindsight bounds.
fn score(sessions: Vec<(SessionReport, &Scenario)>) -> GradeReport {
    let scenarios: Vec<ScenarioResult> = sessions
        .into_iter()
        .map(|(session, scenario)| ScenarioResult {
            disqualified: disqualification(&session),
            bound_pnl_usd: bound_of(scenario, &session),
            reference_pnl_usd: scenario.reference_pnl_usd,
            informed_pnl_usd: scenario.informed_pnl_usd,
            nop_pnl_usd: None,
            trade_markout_usd: trade_markout(&session, scenario),
            reference_max_drawdown_usd: scenario.reference_max_drawdown_usd,
            informed_max_drawdown_usd: scenario.informed_max_drawdown_usd,
            markout: markout(&session, scenario),
            session,
        })
        .collect();
    let pnl_usd: f64 = scenarios.iter().map(|s| s.session.pnl_usd).sum();
    let bound_pnl_usd: f64 = scenarios.iter().map(|s| s.bound_pnl_usd).sum();
    let reference_pnl_usd: Option<f64> = scenarios.iter().map(|s| s.reference_pnl_usd).sum();
    let reference_ratio = reference_pnl_usd
        .filter(|reference| *reference > 0.0)
        .map(|reference| pnl_usd / reference);
    let disqualified = scenarios.iter().any(|s| s.disqualified.is_some());
    let reward = if disqualified || bound_pnl_usd <= 0.0 {
        0.0
    } else {
        (pnl_usd / bound_pnl_usd).clamp(0.0, 1.0)
    };
    let skill = reference_ratio.map(|ratio| if disqualified { 0.0 } else { ratio });
    GradeReport {
        reward,
        pnl_usd,
        bound_pnl_usd,
        reference_pnl_usd,
        reference_ratio,
        skill,
        tampered: None,
        not_graded: None,
        eval_seed_sha256: None,
        risk: Risk::of(&scenarios),
        replay: None,
        markout: total_markout(&scenarios),
        v2: None,
        headline: None,
        truncated: None,
        scenarios,
        anchors: Vec::new(),
    }
}

/// What a session's PnL is divided by: its scenario's bound at the session's
/// start mark, or, on several markets, each market's at its own.
fn bound_of(scenario: &Scenario, session: &SessionReport) -> f64 {
    if scenario.markets.is_empty() {
        scenario.bound_usd(session.start_mark_usd)
    } else {
        scenario.markets_bound_usd(&session.start_marks_usd())
    }
}

/// A session's PnL split against its scenario's fair value, if the scenario
/// reports one at every step. The position is valued as the session valued
/// it: at the mark, the scenario's oracle level (market making) or its book
/// level. A session on several markets has none: its account's value is
/// one, and the split is a one-market split.
fn markout(session: &SessionReport, scenario: &Scenario) -> Option<Decomposition> {
    decompose(session, scenario, true)
}

/// What a session's trades made against the price at the time (see
/// [`scoring_v2::Session::trade_markout_usd`]): their markout against fair
/// value when the scenario reports one, else against the price the
/// position is valued at.
fn trade_markout(session: &SessionReport, scenario: &Scenario) -> Option<f64> {
    decompose(session, scenario, true)
        .or_else(|| decompose(session, scenario, false))
        .map(|d| d.markout_usd)
}

/// [`markout`], or with `against_fair` false, the same split with the
/// valuation price standing in for fair value: then the markout is what the
/// trades (fees and funding with them) cost against that price, and the
/// residual is 0.
fn decompose(
    session: &SessionReport,
    scenario: &Scenario,
    against_fair: bool,
) -> Option<Decomposition> {
    // The split is one market's: a session on several has none, and its
    // flat fields describe no market.
    if !scenario.markets.is_empty() {
        return None;
    }
    let fair_or = |fair: Option<f64>, price: f64| if against_fair { fair } else { Some(price) };
    let timeline = &session.timeline;
    let start = fair_or(
        scenario.fair_value_usd(session.start_mark_usd, 0),
        session.start_mark_usd,
    )?;
    let mut meter = MarkoutMeter::open(MarkoutSample {
        fair: start,
        price: session.start_mark_usd,
        position: 0.0,
        value: session.start_value_usd,
    });
    let valuation = scenario.valuation();
    for step in &timeline.steps {
        let fair = step.fair_usd;
        let price = match valuation {
            "oracle" => step.level_usd,
            "book" => step.book_usd?,
            _ => step.mark_usd,
        };
        meter.record(MarkoutSample {
            fair: fair_or(fair, price)?,
            price,
            position: step.position,
            value: step.value_usd,
        });
    }
    let at = timeline.end_slot.saturating_sub(timeline.start_slot);
    let start_mark = session.start_mark_usd;
    let price = match valuation {
        "oracle" => start_mark * (1.0 + crate::making::level_at(&scenario.path, at) / 100.0),
        "book" => start_mark * (1.0 + crate::making::book_at(&scenario.path, at) / 100.0),
        _ => session.end_mark_usd,
    };
    meter.record(MarkoutSample {
        fair: fair_or(scenario.fair_value_usd(start_mark, at), price)?,
        price,
        position: session.end_position,
        value: session.end_value_usd,
    });
    Some(meter.decomposition())
}

/// The sessions' decompositions added up, if every session has one.
fn total_markout(sessions: &[ScenarioResult]) -> Option<Decomposition> {
    if sessions.is_empty() {
        return None;
    }
    sessions.iter().map(|s| s.markout).try_fold(
        Decomposition {
            pnl_usd: 0.0,
            markout_usd: 0.0,
            luck_usd: 0.0,
            residual_usd: 0.0,
        },
        |total, next| {
            let next = next?;
            Some(Decomposition {
                pnl_usd: total.pnl_usd + next.pnl_usd,
                markout_usd: total.markout_usd + next.markout_usd,
                luck_usd: total.luck_usd + next.luck_usd,
                residual_usd: total.residual_usd + next.residual_usd,
            })
        },
    )
}

/// The share of a market-making session's taker orders that may fail before
/// the session isn't scored.
pub const MAX_FAILED_TAKER_SHARE: f64 = 0.1;

/// Fail the grade, as an infrastructure error, rather than score sessions
/// that didn't run as their scenarios describe. Harbor retries a failed
/// verifier; a skewed score would stand. A loaded machine alone no longer
/// shows up here: sessions run on no clock, so slots can't come late (see
/// [`crate::snapshot::start_offline_stepped`]).
fn check_timing(sessions: &[SessionReport]) -> Result<()> {
    let late: Vec<String> = sessions
        .iter()
        .filter_map(|s| {
            // A session whose library actors failed to act didn't see the
            // market its scenario describes either.
            if let Some(actors) = s.actors.as_ref().filter(|a| a.degraded()) {
                let actions = actors.independent();
                return Some(format!(
                    "{} ({} of {} actor actions failed)",
                    s.scenario,
                    actions.failed,
                    actions.total()
                ));
            }
            // A market-making session whose takers didn't trade as listed
            // didn't see the flow its scenario describes.
            let flow = s.flow.as_ref()?;
            (flow.failed as f64 > MAX_FAILED_TAKER_SHARE * flow.sent as f64).then(|| {
                format!(
                    "{} ({} of {} taker orders failed)",
                    s.scenario, flow.failed, flow.sent
                )
            })
        })
        .collect();
    if late.is_empty() {
        Ok(())
    } else {
        Err(Error::Degraded(late.join(", ")))
    }
}

/// Void the grade, rather than score the bot against a broken baseline, if
/// the reference didn't run cleanly: a disqualification, a crash, or turns
/// it didn't finish.
fn check_anchors(role: &str, anchors: &[SessionReport]) -> Result<()> {
    let broken: Vec<String> = anchors
        .iter()
        .filter_map(|a| {
            let why = if let Some(reason) = disqualification(a) {
                reason
            } else if a.turns.legacy {
                "never answered the hello".to_string()
            } else if a.turns.overran > 0 {
                format!("overran {} turns", a.turns.overran)
            } else {
                let exit = a
                    .bot_exit
                    .as_ref()
                    .filter(|e| !e.contains("exit status: 0"))?;
                format!("exited: {exit}")
            };
            Some(format!("{}: {why}", a.scenario))
        })
        .collect();
    if broken.is_empty() {
        Ok(())
    } else {
        Err(Error::Degraded(format!("{role}: {}", broken.join(", "))))
    }
}

/// Whether the bot traded in the session: its position in the scenario's
/// market was ever other than where it started.
fn traded(session: &SessionReport) -> bool {
    // On several markets, any of them moving is trading.
    if !session.markets.is_empty() {
        return session.positions.iter().any(|range| {
            let start = session
                .markets
                .iter()
                .find(|m| range.instrument.symbol == m.symbol)
                .map_or(0.0, |m| m.start_position);
            range.lowest != start || range.highest != start
        });
    }
    if session.start_position == 0.0 {
        return session.peak_position > 0.0;
    }
    session
        .positions
        .iter()
        .find(|range| range.instrument == session.instrument)
        .is_some_and(|range| {
            range.lowest != session.start_position || range.highest != session.start_position
        })
}

fn disqualification(session: &SessionReport) -> Option<String> {
    if !session.refused_calls.is_empty() {
        Some(format!(
            "called methods bots may not use: {}",
            session.refused_calls.join(", ")
        ))
    } else if let Some(breach) = session.mandate.audit(&session.observed()) {
        Some(breach.to_string())
    } else if let Some(breach) = session.order.as_ref().and_then(|order| order.breach()) {
        Some(breach)
    } else if session.liquidatable && session.liquidator.is_none() {
        // With a competing keeper (the scenario's `liquidator`) an account
        // that ends liquidatable was liquidated as far as Phoenix allowed,
        // and what is left is priced in its value.
        Some("ended the session liquidatable".to_string())
    } else {
        None
    }
}

/// Run the bot on each scenario and record its PnL as the scenario's
/// reference, losses included: the reward normalizes by the set's total.
/// Sessions the reference itself would be disqualified from are reported, not
/// saved. A scenario with a path is saved there; a drawn one, which has no
/// file, keeps its PnL in memory, for the caller to write out (`mtx calibrate
/// --draw` writes an anchors file).
///
/// With `informed`, the bot is the task's informed bot and its PnL is saved
/// as the scenario's [`Scenario::informed_pnl_usd`] instead: scoring v2's 1.
/// It is handed `latent`, as a grade hands it to its informed bot (see
/// [`Informed`]). `keys` are the sessions' keys: a drawn set's, from its
/// secret, sign as a grade of the same set does.
pub struct Calibration<'a> {
    pub snapshot_dir: &'a Path,
    pub bot_dir: &'a Path,
    pub log_dir: &'a Path,
    pub first_bot_user: usize,
    pub jobs: usize,
    pub informed: bool,
    pub latent: Option<&'a [String]>,
    pub keys: &'a SessionKeys,
    pub v2_sessions: bool,
}

pub async fn calibrate(
    opts: Calibration<'_>,
    scenarios: &mut [(Option<std::path::PathBuf>, Scenario)],
) -> Result<Vec<SessionReport>> {
    let list: Vec<Scenario> = scenarios.iter().map(|(_, s)| s.clone()).collect();
    let sessions = run_sessions(
        opts.snapshot_dir,
        opts.bot_dir,
        &list,
        opts.log_dir,
        opts.first_bot_user..opts.first_bot_user + opts.jobs,
        opts.latent,
        false,
        true,
        None,
        opts.keys.role(if opts.informed {
            "informed"
        } else {
            "reference"
        }),
        session::Rules {
            every_slot: opts.v2_sessions,
        },
    )
    .await?;
    check_timing(&sessions)?;
    for ((path, scenario), session) in scenarios.iter_mut().zip(&sessions) {
        // The same rules as grading: a reference the grader would
        // disqualify can't be a baseline.
        if disqualification(session).is_none() {
            let pnl_usd = Some((session.pnl_usd * 100.0).round() / 100.0);
            let drawdown_usd = Some((session.max_drawdown_usd * 100.0).round() / 100.0);
            if opts.informed {
                scenario.informed_pnl_usd = pnl_usd;
                scenario.informed_max_drawdown_usd = drawdown_usd;
            } else {
                scenario.reference_pnl_usd = pnl_usd;
                scenario.reference_max_drawdown_usd = drawdown_usd;
            }
            if let Some(path) = path {
                scenario.save_any(path)?;
            }
        } else {
            tracing::warn!(
                scenario = scenario.name,
                pnl_usd = session.pnl_usd,
                "the anchor was disqualified; not calibrated"
            );
        }
    }
    Ok(sessions)
}

/// The most sessions of a strategy component a grade runs at once.
pub const MAX_COMPONENT_JOBS: usize = 32;

/// Run every scenario, `bot_users.len()` at a time, reports in scenario
/// order.
///
/// Concurrent sessions share nothing: each has its own surfnet, ports, RPC
/// guard, directory, keys (derived from the session, not the order it ran
/// in) and, for a component, its own store and fuel. So a grade comes out
/// the same whatever `jobs` is, and a machine too loaded for it grades
/// slower, not differently. A process bot also runs as its own user (see
/// [`session::BOT_USERS`]), one of `bot_users` that no other session
/// running holds (and no other grade on the machine: see
/// [`session::BotUserLock`]), so `jobs` is at most the number of users. A
/// component needs no user: up to [`MAX_COMPONENT_JOBS`] of its sessions run
/// at once. A new session starts as soon as any one ends. `trusted` bots
/// get [`SessionOptions::trusted`].
#[allow(clippy::too_many_arguments)]
async fn run_sessions(
    snapshot_dir: &Path,
    bot_dir: &Path,
    scenarios: &[Scenario],
    log_dir: &Path,
    bot_users: std::ops::Range<usize>,
    regimes: Option<&[String]>,
    stop_on_disqualify: bool,
    trusted: bool,
    observer: Option<std::sync::Arc<dyn crate::wasm::StepObserver>>,
    keys: RoleKeys<'_>,
    rules: session::Rules,
) -> Result<Vec<SessionReport>> {
    let (first_bot_user, jobs) = (bot_users.start, bot_users.len());
    let components = crate::wasm::component_in(bot_dir).is_some();
    let most = if components {
        MAX_COMPONENT_JOBS
    } else {
        session::BOT_USERS.len()
    };
    if !(1..=most).contains(&jobs) {
        return Err(Error::Jobs(jobs, most));
    }
    if !components && bot_users.end > session::BOT_USERS.len() {
        return Err(Error::BotUsers {
            first: first_bot_user,
            jobs,
            users: session::BOT_USERS.len(),
        });
    }
    // A component may grow to its memory cap in every session: run no more
    // at once than the machine's memory holds, or the grader is killed.
    let jobs = if components {
        let limit = memory_limit();
        let fit = jobs_for_memory(jobs, limit);
        if fit < jobs {
            info!(
                jobs,
                fit,
                limit = ?limit,
                "fewer sessions at once: the memory holds no more"
            );
        }
        fit
    } else {
        jobs
    };
    std::fs::create_dir_all(log_dir).map_err(io_error(log_dir))?;
    raise_open_file_limit();
    // The bot users free for a process bot's sessions: each takes one while
    // it runs, so the ones running at once never share.
    let free = std::sync::Mutex::new(if components {
        Vec::new()
    } else {
        (first_bot_user..first_bot_user + jobs).rev().collect()
    });
    let sessions = futures::stream::iter(scenarios.iter().enumerate())
        .map(|(index, scenario)| {
            let observer = observer.clone();
            let free = &free;
            async move {
            let bot_log = log_dir.join(format!("{}.log", scenario.name));
            let user = UserSlot::take(free);
            let run = || {
                session::run(SessionOptions {
                    snapshot_dir,
                    scenario,
                    bot_dir,
                    bot_log: &bot_log,
                    slot: user.slot,
                    regime: regimes.and_then(|r| r.get(index)).map(String::as_str),
                    cranks: None,
                    trusted,
                    observer: observer.clone(),
                    keys: keys.session(index, &scenario.name),
                    rules,
                })
            };
            // Another grade on this machine may use the same bot user, and
            // a session's end kills everything its user runs: hold the user
            // while the session runs.
            let _held = match user.slot {
                Some(slot) => match session::BotUserLock::acquire(session::BOT_USERS[slot]).await {
                    Ok(held) => Some(held),
                    Err(error) => return (index, Err(error)),
                },
                None => None,
            };
            // A session whose setup fails on the machine (a request to its
            // own surfnet that can't get through on a loaded host) runs once
            // more, from a fresh surfnet. Its bot hadn't started, so it sees
            // the session once either way. A second failure fails the grade.
            let report = match run().await {
                Err(Error::Setup(error)) if is_transient(&error) => {
                    warn!(scenario = scenario.name, %error, "session setup failed on the machine; running it again");
                    run().await
                }
                done => done,
            };
            (index, report)
        }
        })
        // A new session starts whenever any one ends: a long session doesn't
        // hold up the ones behind it.
        .buffer_unordered(jobs);
    in_order(sessions, scenarios.len(), stop_on_disqualify).await
}

/// What a component's session may take in memory beyond the strategy's own
/// cap ([`crate::wasm::MEMORY_BYTES`]): its surfnet, guard, takers and the
/// grader's records. A market-making session, the largest, measures about
/// 0.8 GB; this leaves headroom.
pub const SESSION_WORLD_BYTES: u64 = 1 << 30;
/// What the grader holds apart from its sessions: the snapshot cache, the
/// scenarios, the reports so far.
pub const GRADER_BASE_BYTES: u64 = 3 << 29;

/// How many of a component's sessions fit in `limit` bytes of memory at
/// once, each at its strategy's cap (at least one, at most `jobs`).
fn jobs_for_memory(jobs: usize, limit: Option<u64>) -> usize {
    let Some(limit) = limit else {
        return jobs;
    };
    let per_session = crate::wasm::MEMORY_BYTES as u64 + SESSION_WORLD_BYTES;
    let fit = limit.saturating_sub(GRADER_BASE_BYTES) / per_session;
    (fit as usize).clamp(1, jobs)
}

/// The memory this process may use: its cgroup's limit (a container's, as
/// the verifier's), else the machine's.
fn memory_limit() -> Option<u64> {
    let cgroup = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .filter_map(|path| std::fs::read_to_string(path).ok())
    .find_map(|text| text.trim().parse::<u64>().ok());
    let total = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            let kb = text
                .lines()
                .find(|line| line.starts_with("MemTotal:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<u64>()
                .ok()?;
            Some(kb * 1024)
        });
    // cgroup v1 reports "no limit" as a huge number.
    match (cgroup, total) {
        (Some(cgroup), Some(total)) => Some(cgroup.min(total)),
        (cgroup, total) => cgroup.or(total),
    }
}

/// One of the free bot users, taken for a session and given back when it
/// ends (or is dropped). `None` for a component, which needs none.
struct UserSlot<'a> {
    free: &'a std::sync::Mutex<Vec<usize>>,
    slot: Option<usize>,
}

impl<'a> UserSlot<'a> {
    fn take(free: &'a std::sync::Mutex<Vec<usize>>) -> Self {
        let slot = free
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        Self { free, slot }
    }
}

impl Drop for UserSlot<'_> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            self.free
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(slot);
        }
    }
}

/// Sessions' reports as they end, `(index, report)`, put back in scenario
/// order. With `stop_on_disqualify`, the reports stop at the first
/// disqualifying session in that order, once every session before it has
/// ended: the same reports whatever order they ended in.
async fn in_order(
    sessions: impl futures::Stream<Item = (usize, Result<SessionReport>)>,
    count: usize,
    stop_on_disqualify: bool,
) -> Result<Vec<SessionReport>> {
    let mut sessions = std::pin::pin!(sessions);
    let mut done: Vec<Option<SessionReport>> = (0..count).map(|_| None).collect();
    let mut first_breach: Option<usize> = None;
    while let Some((index, report)) = sessions.next().await {
        let report = report?;
        if stop_on_disqualify && disqualification(&report).is_some() {
            first_breach = Some(first_breach.map_or(index, |first| first.min(index)));
        }
        done[index] = Some(report);
        if let Some(last) = first_breach
            && done[..=last].iter().all(Option::is_some)
        {
            warn!(
                after = last + 1,
                of = count,
                "disqualified: the grade is 0; stopping"
            );
            done.truncate(last + 1);
            break;
        }
    }
    Ok(done.into_iter().flatten().collect())
}

/// Errors from the grader's own machinery talking to its surfnet, as
/// opposed to a missing bot, an invalid scenario or a loaded machine
/// ([`Error::Degraded`]): worth one more try.
fn is_transient(error: &Error) -> bool {
    matches!(
        error,
        Error::Hawkeye(_)
            | Error::Rpc(_)
            | Error::Kit(_)
            | Error::Surfnet(_)
            | Error::Timeout(_)
            | Error::Transaction { .. }
            | Error::Meter(MeterError::Read(_))
    )
}

#[cfg(test)]
mod tests {
    use mtx_kit::{Mandate, mandate::PositionRange};

    use super::*;
    use crate::scenario::{VENUE, Waypoint};

    #[test]
    fn a_failed_trusted_phase_leaves_its_logs_and_a_good_one_holds_them() {
        let to = std::env::temp_dir().join(format!("mtx-keep-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&to);
        let logs = PrivateDir::create("mtx-keep-logs-test").unwrap();
        std::fs::write(logs.0.join("hidden-1.log"), "the reference crashed").unwrap();
        let held = keep_logs(Ok(7), logs, &to).unwrap();
        assert_eq!(held.0, 7);
        assert_eq!(held.1.len(), 1);
        assert!(!to.exists(), "a good phase's logs wait for the bot to end");

        let logs = PrivateDir::create("mtx-keep-logs-test").unwrap();
        std::fs::write(logs.0.join("hidden-1.log"), "the reference crashed").unwrap();
        let held = keep_logs::<()>(Err(Error::Jobs(0, 1)), logs, &to);
        assert!(held.is_err());
        assert_eq!(
            std::fs::read_to_string(to.join("hidden-1.log")).unwrap(),
            "the reference crashed"
        );
        let _ = std::fs::remove_dir_all(&to);
    }

    #[tokio::test]
    async fn a_components_only_grade_of_a_process_bot_scores_zero() {
        let dir = std::env::temp_dir().join(format!("mtx-not-a-component-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
        let report = grade(GradeOptions {
            snapshot_dir: &dir,
            bot_dir: &dir,
            scenarios: &[],
            log_dir: &dir,
            jobs: 1,
            first_bot_user: 0,
            reference_dir: None,
            informed: None,
            anchor_jobs: 1,
            regimes: None,
            stop_on_disqualify: false,
            detail_dir: None,
            annotations: None,
            components_only: true,
            trusted: false,
            observer: None,
            keys: &SessionKeys::random(),
            v2_sessions: false,
        })
        .await
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(report.reward, 0.0);
        assert_eq!(report.skill, Some(0.0));
        assert!(report.not_graded.is_some());
        assert_eq!(report.rewards()["not_graded"], 1.0);
    }

    fn session(pnl_usd: f64) -> SessionReport {
        SessionReport {
            scenario: "s".into(),
            start_mark_usd: 100.0,
            end_mark_usd: 100.0,
            start_value_usd: 10_100.0,
            end_value_usd: 10_100.0 + pnl_usd,
            pnl_usd,
            end_position: 0.0,
            liquidatable: false,
            refused_calls: vec![],
            bot_exit: None,
            late_slots: 0,
            mandate: Mandate::perp(VENUE, "SPCX", 100.0, 1_000.0),
            instrument: "phoenix:perp:SPCX".parse().unwrap(),
            peak_position: 0.0,
            positions: vec![],
            positions_outside_universe: 0,
            max_drawdown_usd: 0.0,
            samples: Default::default(),
            term_start: Default::default(),
            valuation: "mark",
            flow: None,
            competitor: None,
            actors: None,
            spot: None,
            faults: None,
            order: None,
            participation: None,
            net: None,
            start_position: 0.0,
            start_funded: false,
            close_outs: Vec::new(),
            perp_fills: Vec::new(),
            perp_open_orders: Vec::new(),
            making_fills: Vec::new(),
            obligations: None,
            tx_layer: None,
            lending: None,
            crank_refusals: Vec::new(),
            forced_closes: Vec::new(),
            liquidator: None,
            oracle_starts: Default::default(),
            markets: Vec::new(),
            turns: Default::default(),
            wall_ms: 0,
            timeline: Default::default(),
        }
    }

    #[test]
    fn a_perp_sessions_fills_and_resting_sides_are_audited() {
        use mtx_core::Side;
        let mut buy_only = session(5.0);
        let allowance = &mut buy_only.mandate.universe[0];
        allowance.sides = Some(vec![Side::Buy]);
        allowance.max_buy_price = Some(101.0);
        assert_eq!(disqualification(&buy_only), None);
        let fill = |side, price| crate::perp::PerpFillRecord {
            at: 4,
            instrument: "phoenix:perp:SPCX".parse().unwrap(),
            side,
            price,
            base: 1.0,
            worst_price: None,
            forced: false,
        };
        // A buy and a sell inside one step net to nothing; the sell is still
        // a fill.
        let mut sold = buy_only.clone();
        sold.perp_fills = vec![fill(Side::Buy, 100.0), fill(Side::Sell, 100.5)];
        assert_eq!(sold.observed().fills.len(), 2);
        assert!(
            disqualification(&sold)
                .unwrap()
                .contains("a side its mandate")
        );
        let mut over = buy_only.clone();
        over.perp_fills = vec![fill(Side::Buy, 102.0)];
        assert!(disqualification(&over).is_some());
        let mut resting = buy_only.clone();
        resting.perp_open_orders = vec![("phoenix:perp:SPCX".parse().unwrap(), Side::Sell)];
        assert!(disqualification(&resting).is_some());
        // They round-trip through the report, and a session without them
        // writes neither.
        let json = serde_json::to_string(&sold).unwrap();
        let back: SessionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.perp_fills, sold.perp_fills);
        let plain = serde_json::to_string(&buy_only).unwrap();
        assert!(!plain.contains("perp_fills") && !plain.contains("close_outs"));
    }

    #[test]
    fn details_are_written_gzipped_one_per_session() {
        use std::io::Read;
        let dir = std::env::temp_dir().join(format!("mtx-details-{}", uuid::Uuid::new_v4()));
        let annotations = [serde_json::json!({ "shape": "crash" })];
        write_details(
            &dir,
            "bot",
            &[session(5.0)],
            &[scenario(Some(2.0))],
            Some(&annotations),
        )
        .unwrap();
        let mut json = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(dir.join("bot/s.json.gz")).unwrap())
            .read_to_string(&mut json)
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(detail["role"], "bot");
        assert_eq!(detail["annotation"]["shape"], "crash");
        assert_eq!(detail["report"]["pnl_usd"], 5.0);
        assert_eq!(detail["scenario"]["reference_pnl_usd"], 2.0);
        assert!(detail["timeline"]["steps"].is_array());
        assert!(
            detail["report"].get("timeline").is_none(),
            "only once per file"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// One 10% move with a 100-unit cap at a $100 mark: a $1,000 bound.
    fn scenario(reference_pnl_usd: Option<f64>) -> Scenario {
        Scenario {
            name: "s".into(),
            description: String::new(),
            symbol: "SPCX".into(),
            duration_slots: 400,
            mandate: Mandate::perp(VENUE, "SPCX", 100.0, 1_000.0),
            path: vec![Waypoint::at(10, -10.0)],
            making: None,
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            actors: None,
            takers_pricing: None,
            turn_data: None,
            oracles: None,
            lending: None,
            warps: Vec::new(),
            order: None,
            participation: None,
            net: None,
            reference_pnl_usd,
            informed_pnl_usd: None,
            reference_max_drawdown_usd: None,
            informed_max_drawdown_usd: None,
            fair_path: None,
            bound_on: Default::default(),
            market_model: None,
            raw_sends: None,
            markets: Vec::new(),
            joint: None,
            start_positions: Default::default(),
            close_out: Default::default(),
            tx_layer: None,
            stop_out: None,
            liquidator: None,
        }
    }

    #[test]
    fn the_reward_is_pnl_over_the_hindsight_bound() {
        let s = scenario(Some(200.0));
        let report = score(vec![(session(300.0), &s), (session(-100.0), &s)]);
        assert_eq!(report.bound_pnl_usd, 2_000.0);
        assert_eq!(report.reward, 0.1);
        assert_eq!(report.reference_ratio, Some(0.5));

        assert_eq!(score(vec![(session(-50.0), &s)]).reward, 0.0);
        assert_eq!(score(vec![(session(5_000.0), &s)]).reward, 1.0);
        assert_eq!(
            score(vec![(session(10.0), &scenario(None))]).reference_ratio,
            None
        );
    }

    /// A session that buys 10 units at step 1 for $0.40 under fair value and
    /// sells them at step 2, with fair value 1% higher by then.
    fn traded_session() -> SessionReport {
        let step = |k: u64, mark: f64, fair: f64, position: f64, value: f64| session::StepRecord {
            k,
            at: k * 10,
            mark_usd: mark,
            index_usd: mark,
            level_usd: mark,
            book_usd: None,
            fair_usd: Some(fair),
            best_bid_usd: None,
            best_ask_usd: None,
            position,
            value_usd: value,
            pnl_usd: value - 10_100.0,
            hold_usd: 0.0,
            turn_ms: None,
            answered: None,
            markets: Vec::new(),
        };
        let mut report = session(0.0);
        // Bought 10 at 99.60 (fair 100), held while fair rose to 101, sold
        // 10 at 100.90 (fair 101).
        report.end_mark_usd = 101.0;
        report.end_value_usd = 10_100.0 + 10.0 * (100.90 - 99.60);
        report.pnl_usd = report.end_value_usd - report.start_value_usd;
        report.timeline.start_slot = 1_000;
        report.timeline.end_slot = 1_020;
        report.timeline.steps = vec![
            step(0, 100.0, 100.0, 0.0, 10_100.0),
            step(1, 100.0, 100.0, 10.0, 10_100.0 + 10.0 * (100.0 - 99.60)),
            step(2, 101.0, 101.0, 0.0, report.end_value_usd),
        ];
        report
    }

    #[test]
    fn a_scenario_without_a_fair_value_grades_as_before() {
        let s = scenario(Some(200.0));
        let report = score(vec![(traded_session(), &s)]);
        assert!(report.markout.is_none());
        assert!(report.scenarios[0].markout.is_none());
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("markout") && !json.contains("fair"));
        assert!(!report.rewards().contains_key("edge_usd"));
    }

    #[test]
    fn a_scenario_with_a_fair_value_reports_where_its_pnl_came_from() {
        let mut s = scenario(Some(200.0));
        s.fair_path = Some(vec![Waypoint {
            slot: 15,
            level_pct: 1.0,
            premium_pct: 0.0,
        }]);
        let marked = score(vec![(traded_session(), &s)]);
        // The scenario's reward and ratios are PnL's, marked or not.
        s.fair_path = None;
        let plain = score(vec![(traded_session(), &s)]);
        assert_eq!(marked.reward, plain.reward);
        assert_eq!(marked.skill, plain.skill);
        assert_eq!(marked.reference_ratio, plain.reference_ratio);

        let m = marked.markout.unwrap();
        assert!((m.pnl_usd - 13.0).abs() < 1e-9);
        // Bought 10 at $0.40 under fair value (+4), sold at $0.10 under (-1).
        assert!((m.markout_usd - 3.0).abs() < 1e-9, "{m:?}");
        assert!((m.luck_usd - 10.0).abs() < 1e-9, "{m:?}");
        assert!(m.residual_usd.abs() < 1e-9);
        assert!((m.markout_usd + m.luck_usd + m.residual_usd - m.pnl_usd).abs() < 1e-9);
        let rewards = marked.rewards();
        assert!((rewards["edge_usd"] - 3.0).abs() < 1e-9);
        assert!((rewards["luck_usd"] - 10.0).abs() < 1e-9);
    }

    /// A grade's sessions as its detail files hold them, read back.
    fn stored(dir: &Path, names: &[&str]) -> Vec<(SessionReport, Scenario)> {
        use std::io::Read;
        names
            .iter()
            .map(|name| {
                let mut json = String::new();
                flate2::read::GzDecoder::new(
                    std::fs::File::open(dir.join(format!("bot/{name}.json.gz"))).unwrap(),
                )
                .read_to_string(&mut json)
                .unwrap();
                serde_json::from_str::<StoredSession>(&json)
                    .unwrap()
                    .into_parts()
            })
            .collect()
    }

    #[test]
    fn a_grade_scored_again_from_its_details_is_the_same_report() {
        let dir = std::env::temp_dir().join(format!("mtx-rescore-{}", uuid::Uuid::new_v4()));
        let mut marked = scenario(Some(200.37));
        marked.name = "marked".into();
        marked.fair_path = Some(vec![Waypoint {
            slot: 15,
            level_pct: 1.0,
            premium_pct: 0.0,
        }]);
        let mut traded = traded_session();
        traded.scenario = "marked".into();
        // PnL as the meter computes it: not short decimals.
        let mut odd = session(67.143_261_999_999_6);
        odd.end_value_usd = 10_167.143_262;
        odd.max_drawdown_usd = 39.389_592_999_999_88;
        odd.turns.fuel_used = Some(43_524_702_445);
        let mut breached = session(0.1 + 0.2);
        breached.scenario = "breached".into();
        breached.refused_calls = vec!["surfnet_setAccount".into()];
        let mut plain = scenario(Some(-13.07));
        plain.name = "breached".into();
        let sessions = [
            (odd, scenario(Some(2.0))),
            (traded, marked),
            (breached, plain),
        ];
        let scenarios: Vec<Scenario> = sessions.iter().map(|(_, s)| s.clone()).collect();
        let reports: Vec<SessionReport> = sessions.iter().map(|(r, _)| r.clone()).collect();
        write_details(&dir, "bot", &reports, &scenarios, None).unwrap();
        let mut graded = score(reports.into_iter().zip(&scenarios).collect());
        graded.eval_seed_sha256 = Some("ab".repeat(32));

        let verdict = Verdict {
            eval_seed_sha256: graded.eval_seed_sha256.clone(),
            ..Verdict::default()
        };
        let again = rescore(stored(&dir, &["s", "marked", "breached"]), verdict);
        assert_eq!(
            serde_json::to_string_pretty(&again).unwrap(),
            serde_json::to_string_pretty(&graded).unwrap()
        );
        assert!(again.markout.is_none() && again.scenarios[1].markout.is_some());
        assert_eq!(again.reward, 0.0, "the breach still disqualifies");

        // What the report said beyond its sessions carries over.
        let tampered = rescore(
            stored(&dir, &["s"]),
            Verdict {
                tampered: Some("a bot wrote reward.json".into()),
                ..Verdict::default()
            },
        );
        assert_eq!(tampered.reward, 0.0);
        assert!(tampered.tampered.is_some());
        let skipped = rescore(
            Vec::new(),
            Verdict {
                not_graded: Some("no bot.wasm".into()),
                ..Verdict::default()
            },
        );
        assert_eq!(skipped.skill, Some(0.0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn any_disqualified_session_zeroes_the_reward() {
        let s = scenario(None);
        let mut cheat = session(500.0);
        cheat.refused_calls = vec!["surfnet_setTokenAccount".into()];
        let report = score(vec![(session(100.0), &s), (cheat, &s)]);
        assert_eq!(report.reward, 0.0);
        assert!(report.scenarios[1].disqualified.is_some());

        let mut ruined = session(100.0);
        ruined.liquidatable = true;
        assert_eq!(score(vec![(ruined, &s)]).reward, 0.0);

        // With a competing keeper the liquidation is priced in the value,
        // and an account left liquidatable is scored as it stands.
        let mut kept = session(100.0);
        kept.liquidatable = true;
        kept.liquidator = Some(crate::keeper::LiquidatorReport {
            delay_slots: 4,
            liquidations: 1,
            cost_usd: 60.0,
        });
        assert!(score(vec![(kept, &s)]).reward > 0.0);

        let mut big = session(900.0);
        big.peak_position = 119.0;
        big.positions = vec![PositionRange {
            instrument: big.instrument.clone(),
            lowest: -119.0,
            highest: 4.0,
        }];
        let report = score(vec![(big, &s)]);
        assert_eq!(report.reward, 0.0);
        assert!(
            report.scenarios[0]
                .disqualified
                .as_deref()
                .unwrap()
                .contains("cap")
        );
    }

    fn v2() -> scoring_v2::Params {
        scoring_v2::Params::new(scoring_v2::Ceiling::Informed {
            eta_override: Some(0.5),
        })
    }

    #[test]
    fn v2s_fallback_ceiling_is_the_cost_aware_hindsight_bound_on_the_references_marks() {
        let s = scenario(Some(1.0));
        let params = scoring_v2::Params::new(scoring_v2::Ceiling::Informed { eta_override: None });
        let mut report = score(vec![(traded_session(), &s)]);
        // No reference session: no ceiling, never one from the bot's marks.
        let refused = report.score_v2(&params).unwrap_err().to_string();
        assert!(refused.contains("no reference session"), "{refused}");
        assert!(report.v2.is_none());

        let reference = traded_session();
        let marks: Vec<f64> = reference
            .timeline
            .steps
            .iter()
            .map(|step| step.mark_usd)
            .collect();
        report.anchors = vec![reference];
        report.score_v2(&params).unwrap();
        let v2 = report.v2.clone().unwrap();
        assert_eq!(v2.ceiling, "hindsight");
        assert_eq!(v2.anchor_floor, scoring_v2::HINDSIGHT_ANCHOR_FLOOR);
        assert_eq!(v2.hindsight_cost, scoring_v2::HINDSIGHT_COST);
        let want = scoring_v2::cost_aware_bound(report.bound_pnl_usd, &marks, 100.0, 8.5e-4);
        assert!((v2.ceiling_usd - want).abs() < 1e-9);
        assert!(v2.ceiling_usd < report.bound_pnl_usd);
        // The reference session it read is recorded, for a rescore to check.
        assert_eq!(
            v2.sessions[0].reference_session_sha256.as_deref(),
            Some(reference_sha256(&report.anchors[0]).as_str())
        );
        assert_eq!(reference_sha256(&report.anchors[0]).len(), 64);
        // The reference's drawdown, the charge's anchor, is in the hash too.
        let mut deeper = report.anchors[0].clone();
        deeper.max_drawdown_usd += 1.0;
        assert_ne!(
            reference_sha256(&deeper),
            reference_sha256(&report.anchors[0])
        );

        // A bot that jitters the mark (±2 or ±5 bp a step) can't lower its
        // own ceiling: the score doesn't move.
        for bps in [2.0, 5.0] {
            let mut jittered = traded_session();
            for (i, step) in jittered.timeline.steps.iter_mut().enumerate() {
                let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
                step.mark_usd *= 1.0 + sign * bps / 1e4;
            }
            let mut graded = score(vec![(jittered, &s)]);
            graded.anchors = report.anchors.clone();
            graded.score_v2(&params).unwrap();
            let jitter = graded.v2.as_ref().unwrap();
            assert_eq!(jitter.ceiling_usd, v2.ceiling_usd, "±{bps} bp");
            assert_eq!(jitter.raw, v2.raw, "±{bps} bp");
        }
    }

    #[test]
    fn v2_scores_a_session_on_several_markets_but_for_the_one_market_hindsight_ceiling() {
        let s = scenario(Some(100.0));
        let multi = on_two_markets(30.0);
        let mut report = score(vec![(multi.clone(), &s)]);
        report.score_v2(&v2()).unwrap();
        assert!(report.v2.is_some());
        // The cost-aware hindsight ceiling reads one market's marks.
        let params = scoring_v2::Params::new(scoring_v2::Ceiling::Informed { eta_override: None });
        let mut hindsight = score(vec![(multi, &s)]);
        let refused = hindsight.score_v2(&params).unwrap_err().to_string();
        assert!(
            refused.contains("reads one market's marks; this session trades NVDA, SPCX"),
            "{refused}"
        );
        assert!(hindsight.v2.is_none());
        // One market is scored as before.
        let mut one = score(vec![(session(30.0), &s)]);
        one.score_v2(&v2()).unwrap();
        assert!(one.v2.is_some());
    }

    /// A perp session on NVDA and SPCX, flat at the start.
    fn on_two_markets(pnl_usd: f64) -> SessionReport {
        let mut multi = session(pnl_usd);
        multi.markets = ["NVDA", "SPCX"]
            .into_iter()
            .map(|symbol| session::MarketReport {
                symbol: symbol.into(),
                start_mark_usd: 100.0,
                end_mark_usd: 100.0,
                start_position: 0.0,
                end_position: 0.0,
                valuation: "mark",
                flow: None,
                competitor: None,
            })
            .collect();
        multi
    }

    /// Several markets have no one-market ceiling to read, but an informed
    /// bot's PnL is the account's, whatever it traded.
    #[test]
    fn v2_scores_a_session_on_several_markets_against_the_informed_bot() {
        let mut s = scenario(Some(100.0));
        s.informed_pnl_usd = Some(400.0);
        let mut report = score(vec![
            (on_two_markets(300.0), &s),
            (on_two_markets(50.0), &s),
        ]);
        report.score_v2(&v2()).unwrap();
        let v2 = report.v2.as_ref().unwrap();
        assert_eq!(v2.ceiling, "informed");
        assert_eq!(v2.raw, 0.5 + 0.5 * 150.0 / 600.0);
        // The cost-aware hindsight ceiling reads one market's marks and cap:
        // without the informed bot there is none.
        let plain = scenario(Some(100.0));
        let mut fallback = score(vec![(on_two_markets(30.0), &plain)]);
        fallback.anchors = vec![on_two_markets(30.0)];
        let params = scoring_v2::Params::new(scoring_v2::Ceiling::Informed { eta_override: None });
        let refused = fallback.score_v2(&params).unwrap_err().to_string();
        assert!(refused.contains("one market"), "{refused}");
    }

    /// Doing nothing is 0 only where holding the start scores 0: a start
    /// position on any market, not only the first, needs net scoring or an
    /// order.
    #[test]
    fn v2_takes_a_start_position_on_any_market_only_when_scoring_nets_the_hold() {
        let mut s = scenario(Some(100.0));
        s.informed_pnl_usd = Some(400.0);
        let mut held = on_two_markets(-40.0);
        held.markets[0].start_position = 20.0;
        held.markets[0].end_position = 20.0;
        let refused = score(vec![(held.clone(), &s)])
            .score_v2(&v2())
            .unwrap_err()
            .to_string();
        assert!(refused.contains("starting position"), "{refused}");
        held.net = Some(crate::orders::NetReport {
            value_change_usd: -40.0,
            drift_usd: -40.0,
            inventory_charge_usd: 0.0,
            venue_charge_usd: 0.0,
            venue_net: Vec::new(),
        });
        held.pnl_usd = 0.0;
        score(vec![(held, &s)]).score_v2(&v2()).unwrap();
    }

    /// What a bot that does nothing made is in the report, and a rescore
    /// reads it back, so a grade scored again keeps its 0.
    #[test]
    fn a_grade_scored_again_keeps_what_the_nop_bot_made() {
        let mut s = scenario(Some(100.0));
        s.informed_pnl_usd = Some(400.0);
        let mut report = score(vec![(session(30.0), &s)]);
        report.scenarios[0].nop_pnl_usd = Some(10.0);
        report.score_v2(&v2()).unwrap();
        let raw = report.v2.as_ref().unwrap().raw;
        assert_eq!(report.v2.as_ref().unwrap().nop_usd, 10.0);
        let verdict: Verdict =
            serde_json::from_value(serde_json::to_value(&report).unwrap()).unwrap();
        let mut again = rescore(vec![(session(30.0), s)], verdict);
        assert_eq!(again.scenarios[0].nop_pnl_usd, Some(10.0));
        again.score_v2(&v2()).unwrap();
        assert_eq!(again.v2.as_ref().unwrap().raw, raw);
    }

    /// With the nop bot's PnL measured, a start position the hold baseline
    /// doesn't cover is no longer a reason to refuse: its drift is in the 0.
    #[test]
    fn a_measured_nop_scores_a_start_position_no_baseline_covers() {
        let mut s = scenario(Some(100.0));
        s.informed_pnl_usd = Some(400.0);
        let mut held = session(-30.0);
        held.start_position = 20.0;
        held.peak_position = 20.0;
        held.positions = vec![PositionRange {
            instrument: held.instrument.clone(),
            lowest: 20.0,
            highest: 20.0,
        }];
        let refused = score(vec![(held.clone(), &s)])
            .score_v2(&v2())
            .unwrap_err()
            .to_string();
        assert!(refused.contains("starting position"), "{refused}");
        let mut measured = score(vec![(held, &s)]);
        measured.scenarios[0].nop_pnl_usd = Some(-30.0);
        measured.score_v2(&v2()).unwrap();
        // Doing nothing made -30, and so did the bot: it scores the 0.
        assert_eq!(measured.v2.as_ref().unwrap().raw, 0.0);
    }

    /// A bot that trades only the second market has traded.
    #[test]
    fn a_session_on_several_markets_traded_where_any_market_moved() {
        let mut multi = on_two_markets(10.0);
        multi.positions = ["NVDA", "SPCX"]
            .into_iter()
            .map(|symbol| PositionRange {
                instrument: format!("phoenix:perp:{symbol}").parse().unwrap(),
                lowest: 0.0,
                highest: 0.0,
            })
            .collect();
        assert!(!traded(&multi));
        multi.positions[1].highest = 3.0;
        assert!(traded(&multi));
    }

    #[test]
    fn v2s_ceiling_is_the_informed_bot_when_every_scenario_has_one() {
        let mut s = scenario(Some(100.0));
        let mut proxied = score(vec![(session(300.0), &s), (session(50.0), &s)]);
        proxied.score_v2(&v2()).unwrap();
        assert_eq!(proxied.v2.as_ref().unwrap().ceiling, "eta_override");
        assert_eq!(proxied.rewards()["score_v2_ceiling_informed"], 0.0);

        s.informed_pnl_usd = Some(400.0);
        let mut informed = score(vec![(session(300.0), &s), (session(50.0), &s)]);
        assert_eq!(informed.scenarios[0].informed_pnl_usd, Some(400.0));
        informed.score_v2(&v2()).unwrap();
        let v2 = informed.v2.as_ref().unwrap();
        assert_eq!(v2.ceiling, "informed");
        assert_eq!(v2.ceiling_usd, 800.0);
        // (350 − 200) / (800 − 200) of the way from the reference.
        assert_eq!(v2.raw, 0.5 + 0.5 * 150.0 / 600.0);
        assert_eq!(v2.bound_usd, informed.bound_pnl_usd);
        assert_eq!(informed.rewards()["score_v2_ceiling_informed"], 1.0);
    }

    #[test]
    fn a_v2_headline_is_the_grades_one_score_on_the_informed_bot() {
        let headline = |ceiling| scoring_v2::Params {
            headline: true,
            ..scoring_v2::Params::new(ceiling)
        };
        let mut s = scenario(Some(100.0));
        // No informed bot: no fallback to a proxy or the hindsight ceiling.
        let mut proxied = score(vec![(session(300.0), &s), (session(50.0), &s)]);
        let refused = proxied
            .score_v2(&headline(scoring_v2::Ceiling::Informed {
                eta_override: Some(0.5),
            }))
            .unwrap_err()
            .to_string();
        assert!(refused.contains("informed bot"), "{refused}");
        assert!(proxied.v2.is_none() && proxied.skill.is_some());
        // Nor a ceiling given outright: the 1 is the informed bot's.
        let given = scoring_v2::Ceiling::Given([("s".to_string(), 400.0)].into_iter().collect());
        let mut handed = score(vec![(session(300.0), &s), (session(50.0), &s)]);
        let refused = handed.score_v2(&headline(given)).unwrap_err().to_string();
        assert!(refused.contains("`given`"), "{refused}");

        s.informed_pnl_usd = Some(400.0);
        let mut graded = score(vec![(session(300.0), &s), (session(50.0), &s)]);
        graded
            .score_v2(&headline(scoring_v2::Ceiling::Informed {
                eta_override: None,
            }))
            .unwrap();
        let v2 = graded.v2.clone().unwrap();
        assert_eq!(graded.reward, v2.score);
        assert_eq!(graded.headline.as_deref(), Some("v2"));
        let rewards = graded.rewards();
        assert_eq!(rewards["reward"], v2.score);
        assert!(!rewards.contains_key("skill") && !rewards.contains_key("reference_ratio"));
        let json = serde_json::to_value(&graded).unwrap();
        assert!(json.get("skill").is_none() && json["headline"] == "v2");

        // Tampering is the floor: reward 0 under the headline too.
        graded.tamper("wrote the reward".into());
        assert_eq!(graded.reward, 0.0);
        assert_eq!(graded.v2.unwrap().raw, scoring_v2::RAW_FLOOR);
    }

    #[test]
    fn without_v2_a_report_is_as_before() {
        let s = scenario(Some(200.0));
        let report = score(vec![(session(300.0), &s), (session(-100.0), &s)]);
        assert!(report.v2.is_none());
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("v2") && !json.contains("ceiling"));
        assert!(!report.rewards().keys().any(|k| k.contains("v2")));
    }

    #[test]
    fn v2_scores_a_breach_per_session_and_leaves_v1_as_it_was() {
        let s = scenario(Some(100.0));
        let mut deep = session(400.0);
        deep.max_drawdown_usd = 1_200.0;
        let mut report = score(vec![(session(300.0), &s), (deep, &s)]);
        let v1 = serde_json::to_value(&report).unwrap();
        report.score_v2(&v2()).unwrap();
        let mut with_v2 = serde_json::to_value(&report).unwrap();
        with_v2.as_object_mut().unwrap().remove("v2");
        assert_eq!(with_v2, v1, "v1's fields don't move");
        assert_eq!(report.reward, 0.0, "v1 still zeroes the set");

        let v2 = report.v2.as_ref().unwrap();
        assert_eq!(v2.breached_sessions, 1);
        // The drawdown limit (1,000) lost 1.5 times.
        assert_eq!(v2.sessions[1].bot_usd, -1_500.0);
        assert!(v2.fatal.is_none());
        let rewards = report.rewards();
        assert_eq!(rewards["score_v2"], v2.score);
        assert_eq!(rewards["score_v2_edge"], v2.raw - 0.5);
        assert_eq!(rewards["breached_sessions"], 1.0);
        assert_eq!(rewards["ceiling_pnl_usd"], v2.ceiling_usd);
    }

    #[test]
    fn v2_charges_drawdowns_against_the_references_and_reports_metrics() {
        let mut s = scenario(Some(100.0));
        s.reference_max_drawdown_usd = Some(50.0);
        let mut deep = session(150.0);
        deep.max_drawdown_usd = 950.0;
        let mut report = score(vec![(deep.clone(), &s), (deep, &s)]);
        // Kept out of the report: it is the scenario's, or the grade's own.
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("reference_max_drawdown")
        );
        assert_eq!(report.scenarios[0].reference_max_drawdown_usd, Some(50.0));
        report.score_v2(&v2()).unwrap();
        let v2 = report.v2.as_ref().unwrap();
        assert_eq!(v2.risk.anchor, "reference");
        // ρ = 0.95, ρ_A = 0.05, s = 300 / 200 = 1.5: 1.5 · 2,000 · 0.875²,
        // capped at the 300 it won.
        assert!((v2.risk.allowance - 1.5).abs() < 1e-12);
        assert_eq!(v2.risk.charge_usd, 300.0);
        assert_eq!(v2.raw, 0.0);
        let rewards = report.rewards();
        assert_eq!(rewards["score_v2_risk_charge_usd"], 300.0);
        assert_eq!(rewards["score_v2_risk_weight"], 1.0);
        assert!((rewards["metric_return_on_capital"] - 150.0 / 10_100.0).abs() < 1e-15);
        assert_eq!(rewards["metric_drawdown_to_limit_max"], 0.95);
        assert!(!rewards.contains_key("score_v2_breach_penalty"));
    }

    #[test]
    fn a_fatal_grade_v2_cant_score_is_the_floor() {
        let mut cheat = session(500.0);
        cheat.refused_calls = vec!["surfnet_setTokenAccount".into()];
        // No reference PnL, which v2 needs for an honest grade.
        let mut report = score(vec![(cheat, &scenario(None))]);
        report.score_v2(&v2()).unwrap();
        let v2 = report.v2.as_ref().unwrap();
        assert_eq!((v2.raw, v2.score), (-1.0, 0.0));
        assert!(
            v2.fatal
                .as_deref()
                .unwrap()
                .contains("surfnet_setTokenAccount")
        );
    }

    /// A grade that stopped at its first breach (`--stop-on-disqualify`)
    /// holds only the sessions up to it: v2 would score a truncated set, so
    /// it refuses one, whatever the report's own scores say.
    #[test]
    fn v2_refuses_a_set_that_stopped_at_its_first_breach() {
        let s = scenario(Some(100.0));
        let verdict: Verdict =
            serde_json::from_value(serde_json::json!({ "truncated": { "graded": 1, "of": 4 } }))
                .unwrap();
        let mut report = rescore(vec![(session(30.0), s.clone())], verdict);
        let refused = report.score_v2(&v2()).unwrap_err().to_string();
        assert!(refused.contains("stopped at its first breach"), "{refused}");
        assert!(report.v2.is_none());
        // The same sessions, graded whole, are scored.
        let mut whole = rescore(vec![(session(30.0), s)], Verdict::default());
        whole.score_v2(&v2()).unwrap();
        assert!(whole.v2.is_some());
    }

    /// A session the risk desk stopped out (infra/22's `stop_out`): it lost
    /// `pnl_usd`, the close included, past the 1,000 limit.
    fn stopped_out(pnl_usd: f64) -> SessionReport {
        let mut stopped = session(pnl_usd);
        stopped.max_drawdown_usd = -pnl_usd;
        stopped.forced_closes = vec![crate::risk::ForcedClose {
            reason: crate::risk::ForcedCloseReason::StopOut {
                rule: crate::scenario::StopRule::Drawdown,
            },
            step: 3,
            at: 6,
            value_before_usd: 10_100.0 + pnl_usd + 40.0,
            peak_usd: 10_100.0,
            value_after_usd: 10_100.0 + pnl_usd,
            position_before: 50.0,
            positions_before: Default::default(),
            orders: Vec::new(),
            flat: true,
        }];
        stopped
    }

    #[test]
    fn v2_scores_a_stop_out_as_a_breach_at_what_it_realised() {
        let s = scenario(Some(100.0));
        let mut report = score(vec![(session(300.0), &s), (stopped_out(-1_100.0), &s)]);
        // v1 prices the stop-out, as infra/22 has it: no disqualification.
        assert!(report.scenarios[1].disqualified.is_none());
        report.score_v2(&v2()).unwrap();
        let graded = report.v2.as_ref().unwrap();
        // k times the realised loss, past the limit: 1.5 × 1,100.
        assert_eq!(graded.sessions[1].bot_usd, -1_650.0);
        assert!(
            graded.sessions[1]
                .breach
                .as_deref()
                .unwrap()
                .contains("stopped out")
        );
        assert_eq!(graded.breached_sessions, 1);
        assert!((graded.breach_penalty - scoring_v2::BREACH_RATE_WEIGHT / 2.0).abs() < 1e-15);
        assert_eq!(
            report.rewards()["score_v2_breach_penalty"],
            graded.breach_penalty
        );
        assert_eq!(graded.metrics.stopped_out_sessions, 1);
        // A stop-out that lost less than the limit still counts k·L.
        let mut report = score(vec![(session(300.0), &s), (stopped_out(-900.0), &s)]);
        report.score_v2(&v2()).unwrap();
        assert_eq!(report.v2.as_ref().unwrap().sessions[1].bot_usd, -1_500.0);
    }

    /// A perp start position moves with the price, so doing nothing isn't
    /// 0 and v2 refuses it; with an order, which scores against doing
    /// nothing, it is, and a bot that held still didn't trade.
    #[test]
    fn v2_takes_a_start_position_only_with_an_order() {
        let s = scenario(Some(100.0));
        let mut held = session(-40.0);
        held.start_position = 20.0;
        held.peak_position = 20.0;
        held.positions = vec![PositionRange {
            instrument: held.instrument.clone(),
            lowest: 20.0,
            highest: 20.0,
        }];
        let mut report = score(vec![(held.clone(), &s)]);
        let refused = report.score_v2(&v2()).unwrap_err().to_string();
        assert!(refused.contains("starting position"), "{refused}");

        let order = crate::orders::ExecutionOrder {
            instrument: None,
            side: mtx_core::Side::Sell,
            quantity: 20.0,
            haircut_bps: 30.0,
            reduce_only: true,
        };
        held.order = Some(order.report(20.0, 20.0, 100.0, 98.0, -40.0, &[]));
        held.pnl_usd = 0.0;
        assert!(!traded(&held));
        let mut report = score(vec![(held.clone(), &s)]);
        report.score_v2(&v2()).unwrap();
        // With funding on the market, the start earns or pays it whatever
        // the order: refused.
        let mut funded = held.clone();
        funded.start_funded = true;
        let refused = score(vec![(funded, &s)])
            .score_v2(&v2())
            .unwrap_err()
            .to_string();
        assert!(refused.contains("funding"), "{refused}");
        let mut sold = held;
        sold.positions[0].lowest = 0.0;
        assert!(traded(&sold));
    }

    /// Net scoring scores a perp start position against holding it, so v2
    /// takes it without an order: a bot that held scores the hold's 0, the
    /// nop anchor. Funding on the start is still refused, net or not: the
    /// hold baseline doesn't take it out.
    #[test]
    fn v2_takes_a_start_position_under_net_scoring_with_the_hold_as_nop() {
        let s = scenario(Some(100.0));
        let mut held = session(-40.0);
        held.start_position = 20.0;
        held.peak_position = 20.0;
        held.positions = vec![PositionRange {
            instrument: held.instrument.clone(),
            lowest: 20.0,
            highest: 20.0,
        }];
        let refused = score(vec![(held.clone(), &s)])
            .score_v2(&v2())
            .unwrap_err()
            .to_string();
        assert!(refused.contains("starting position"), "{refused}");

        let net = crate::orders::NetReport {
            value_change_usd: -40.0,
            drift_usd: -40.0,
            inventory_charge_usd: 0.0,
            venue_charge_usd: 0.0,
            venue_net: Vec::new(),
        };
        held.pnl_usd = net.pnl_usd();
        held.net = Some(net);
        assert_eq!(held.pnl_usd, 0.0);
        let mut report = score(vec![(held.clone(), &s)]);
        report.score_v2(&v2()).unwrap();
        let scored = report.v2.as_ref().unwrap();
        assert_eq!(scored.sessions[0].nop_usd, 0.0);
        assert_eq!(scored.sessions[0].bot_usd, 0.0);
        assert!(scored.raw.abs() < 1e-9, "{}", scored.raw);

        let mut funded = held;
        funded.start_funded = true;
        let refused = score(vec![(funded, &s)])
            .score_v2(&v2())
            .unwrap_err()
            .to_string();
        assert!(refused.contains("funding"), "{refused}");
    }

    /// The reference levered past its limit: it earns more where it holds,
    /// and the risk desk stops it out now and then. The breach penalty
    /// prices breaches by their rate; it doesn't cap the run.
    #[test]
    fn an_occasional_breacher_stopped_out_scores_below_its_compliant_twin() {
        let s = scenario(Some(20.0));
        let twin_pnl = [400.0, 300.0, 200.0, 500.0, 350.0, 250.0];
        let twin = || {
            twin_pnl
                .iter()
                .map(|p| (session(*p), &s))
                .collect::<Vec<_>>()
        };
        let mut compliant = score(twin());
        compliant.score_v2(&v2()).unwrap();
        let c = compliant.v2.as_ref().unwrap().clone();
        let levered = |lever: f64, stops: usize| {
            let mut sessions: Vec<(SessionReport, &Scenario)> =
                twin_pnl.iter().map(|p| (session(p * lever), &s)).collect();
            for (session, _) in sessions.iter_mut().skip(2).take(stops) {
                *session = stopped_out(-1_050.0);
            }
            let mut breacher = score(sessions);
            breacher.score_v2(&v2()).unwrap();
            breacher.v2.unwrap()
        };
        for stops in [1, 2] {
            let b = levered(2.5, stops);
            assert!(b.raw < c.raw, "{stops} stop-outs: {} vs {}", b.raw, c.raw);
            let rate = stops as f64 / 6.0;
            assert!((b.breach_penalty - scoring_v2::BREACH_RATE_WEIGHT * rate).abs() < 1e-15);
            if stops == 1 {
                // Levering paid in USD (4,500 − 1,575 against 2,000): the
                // breach penalty is what keeps it below its twin.
                assert!(b.bot_usd > c.bot_usd);
                assert!(b.raw + b.breach_penalty > c.raw);
            }
        }
        // The limit, stated: tripling every session's PnL and breaching in
        // one session of six earns more than the penalty takes. On the
        // tasks' own paths, levering that far breaches far more often; the
        // probe gate checks that the breacher stays below its twin there.
        let tripled = levered(3.0, 1);
        assert!(tripled.raw > c.raw);
    }

    #[test]
    fn a_refused_call_or_tampering_is_fatal_under_v2() {
        let s = scenario(Some(100.0));
        let mut cheat = session(500.0);
        cheat.refused_calls = vec!["surfnet_setTokenAccount".into()];
        let mut report = score(vec![(session(300.0), &s), (cheat, &s)]);
        report.score_v2(&v2()).unwrap();
        let v2_cheat = report.v2.as_ref().unwrap();
        assert_eq!(v2_cheat.score, 0.0);
        assert!(
            v2_cheat
                .fatal
                .as_deref()
                .unwrap()
                .contains("surfnet_setTokenAccount")
        );
        assert_eq!(v2_cheat.breached_sessions, 0, "not a risk breach");
        assert_eq!(report.rewards()["score_v2_fatal"], 1.0);

        let mut honest = score(vec![(session(300.0), &s)]);
        honest.score_v2(&v2()).unwrap();
        assert!(honest.v2.as_ref().unwrap().score > 0.0);
        honest.tamper("a bot wrote reward.json".into());
        assert_eq!(honest.v2.as_ref().unwrap().score, 0.0);
        assert_eq!(honest.v2.as_ref().unwrap().raw, -1.0);

        let mut skipped = rescore(
            Vec::new(),
            Verdict {
                not_graded: Some("no bot.wasm".into()),
                ..Verdict::default()
            },
        );
        skipped.score_v2(&v2()).unwrap();
        assert_eq!(skipped.v2.as_ref().unwrap().score, 0.0);
        assert_eq!(skipped.rewards()["score_v2_raw"], -1.0);
        // reward.json says the same: a run that wasn't graded is below nop.
        assert_eq!(skipped.rewards()["metric_raw_negative"], 1.0);
    }

    #[test]
    fn v2_needs_a_reference_and_rescores_the_same() {
        let mut bare = score(vec![(session(300.0), &scenario(None))]);
        assert!(bare.score_v2(&v2()).is_err());
        assert!(bare.v2.is_none());

        let s = scenario(Some(150.0));
        let sessions = || {
            [120.0, -40.0, 260.0, 0.0, 75.0]
                .iter()
                .map(|p| (session(*p), s.clone()))
                .collect::<Vec<_>>()
        };
        let again = || {
            let mut report = rescore(
                sessions(),
                Verdict {
                    eval_seed_sha256: Some("cd".repeat(32)),
                    ..Verdict::default()
                },
            );
            report.score_v2(&v2()).unwrap();
            serde_json::to_string_pretty(&report).unwrap()
        };
        assert_eq!(again(), again());
    }

    #[test]
    fn drawing_down_past_the_limit_disqualifies() {
        let s = scenario(None);
        let mut deep = session(400.0);
        deep.max_drawdown_usd = 1_200.0;
        let report = score(vec![(session(100.0), &s), (deep, &s)]);
        assert_eq!(report.reward, 0.0);
        assert!(
            report.scenarios[1]
                .disqualified
                .as_deref()
                .unwrap()
                .contains("drew down")
        );

        let mut at_limit = session(100.0);
        at_limit.max_drawdown_usd = 1_000.0;
        assert!(score(vec![(at_limit, &s)]).reward > 0.0);
    }

    /// A drawdown past the limit is priced when the risk desk closed it,
    /// and still disqualifies without a close, or for another rule.
    #[test]
    fn a_stop_out_prices_the_drawdown_and_nothing_else() {
        use crate::{
            risk::{ForcedClose, ForcedCloseReason},
            scenario::StopRule,
        };
        let s = scenario(None);
        let mut stopped = session(-1_150.0);
        stopped.max_drawdown_usd = 1_300.0;
        stopped.forced_closes = vec![ForcedClose {
            reason: ForcedCloseReason::StopOut {
                rule: StopRule::Drawdown,
            },
            step: 40,
            at: 80,
            value_before_usd: 8_700.0,
            peak_usd: 10_000.0,
            value_after_usd: 8_650.0,
            position_before: 70.0,
            positions_before: Default::default(),
            orders: Vec::new(),
            flat: true,
        }];
        let report = score(vec![(session(2_000.0), &s), (stopped.clone(), &s)]);
        assert!(report.scenarios[1].disqualified.is_none());
        assert_eq!(report.pnl_usd, 850.0, "the close's loss counts");
        assert!(report.reward > 0.0);

        let mut unclosed = stopped.clone();
        unclosed.forced_closes.clear();
        assert_eq!(score(vec![(unclosed, &s)]).reward, 0.0);

        let mut elsewhere = stopped;
        elsewhere.positions_outside_universe = 1;
        assert_eq!(score(vec![(elsewhere, &s)]).reward, 0.0);
    }

    /// The keeper's liquidation sells a buy-only bot's long in its own
    /// account: the sides rule leaves that sell out.
    #[test]
    fn a_keepers_liquidating_sell_isnt_the_bots() {
        use mtx_core::Side;

        use crate::risk::{ForcedClose, ForcedCloseReason, LiquidatedBy};
        let mut liquidated = session(-900.0);
        liquidated.mandate.universe[0].sides = Some(vec![Side::Buy]);
        liquidated.forced_closes = vec![ForcedClose {
            reason: ForcedCloseReason::Liquidation {
                by: LiquidatedBy::Keeper,
            },
            step: 40,
            at: 80,
            value_before_usd: 9_300.0,
            peak_usd: 10_000.0,
            value_after_usd: 9_100.0,
            position_before: 70.0,
            positions_before: Default::default(),
            orders: Vec::new(),
            flat: true,
        }];
        let fill = |side, forced| crate::perp::PerpFillRecord {
            at: 80,
            instrument: "phoenix:perp:SPCX".parse().unwrap(),
            side,
            price: 100.0,
            base: 70.0,
            worst_price: None,
            forced,
        };
        liquidated.perp_fills = vec![fill(Side::Buy, false), fill(Side::Sell, true)];
        assert!(
            !disqualification(&liquidated)
                .unwrap_or_default()
                .contains("a side its mandate")
        );
        liquidated.perp_fills.push(fill(Side::Sell, false));
        assert!(
            disqualification(&liquidated)
                .unwrap()
                .contains("a side its mandate")
        );
    }

    /// The risk desk's forced close trades in the bot's account; a
    /// buy-only mandate's sides rule leaves its sells out, and still
    /// catches the bot's own.
    #[test]
    fn a_stop_outs_forced_sell_isnt_the_bots() {
        use mtx_core::Side;

        use crate::{
            risk::{ForcedClose, ForcedCloseReason},
            scenario::StopRule,
        };
        let s = scenario(None);
        let mut stopped = session(-1_150.0);
        stopped.mandate.universe[0].sides = Some(vec![Side::Buy]);
        stopped.max_drawdown_usd = 1_300.0;
        stopped.forced_closes = vec![ForcedClose {
            reason: ForcedCloseReason::StopOut {
                rule: StopRule::Drawdown,
            },
            step: 40,
            at: 80,
            value_before_usd: 8_700.0,
            peak_usd: 10_000.0,
            value_after_usd: 8_650.0,
            position_before: 70.0,
            positions_before: Default::default(),
            orders: Vec::new(),
            flat: true,
        }];
        let fill = |side, forced| crate::perp::PerpFillRecord {
            at: 80,
            instrument: "phoenix:perp:SPCX".parse().unwrap(),
            side,
            price: 100.0,
            base: 70.0,
            worst_price: None,
            forced,
        };
        stopped.perp_fills = vec![fill(Side::Buy, false), fill(Side::Sell, true)];
        assert!(disqualification(&stopped).is_none());
        assert!(
            score(vec![(stopped.clone(), &s)]).scenarios[0]
                .disqualified
                .is_none()
        );
        // The forced fill still reads back as one.
        let json = serde_json::to_string(&stopped).unwrap();
        let back: SessionReport = serde_json::from_str(&json).unwrap();
        assert!(back.perp_fills[1].forced);

        let mut own = stopped;
        own.perp_fills.push(fill(Side::Sell, false));
        assert!(
            disqualification(&own)
                .unwrap()
                .contains("a side its mandate")
        );
    }

    /// On two markets, the desk's forced sells on each are left out of the
    /// sides rule, whichever market's allowance has it; the bot's own sell
    /// is caught and names its market.
    #[test]
    fn a_stop_outs_forced_sells_on_two_markets_arent_the_bots() {
        use mtx_core::Side;

        use crate::{
            risk::{ForcedClose, ForcedCloseReason},
            scenario::StopRule,
        };
        let mut stopped = session(-1_150.0);
        stopped
            .mandate
            .universe
            .extend(Mandate::perp(VENUE, "NVDA", 40.0, 1_000.0).universe);
        for allowance in &mut stopped.mandate.universe {
            allowance.sides = Some(vec![Side::Buy]);
        }
        stopped.max_drawdown_usd = 1_300.0;
        stopped.forced_closes = vec![ForcedClose {
            reason: ForcedCloseReason::StopOut {
                rule: StopRule::Drawdown,
            },
            step: 40,
            at: 80,
            value_before_usd: 8_700.0,
            peak_usd: 10_000.0,
            value_after_usd: 8_650.0,
            position_before: 70.0,
            positions_before: [("NVDA".to_string(), 40.0), ("SPCX".to_string(), 70.0)].into(),
            orders: Vec::new(),
            flat: true,
        }];
        let fill = |symbol: &str, side, forced| crate::perp::PerpFillRecord {
            at: 80,
            instrument: format!("phoenix:perp:{symbol}").parse().unwrap(),
            side,
            price: 100.0,
            base: 10.0,
            worst_price: None,
            forced,
        };
        stopped.perp_fills = vec![
            fill("SPCX", Side::Buy, false),
            fill("NVDA", Side::Buy, false),
            fill("NVDA", Side::Sell, true),
            fill("SPCX", Side::Sell, true),
        ];
        assert!(disqualification(&stopped).is_none());
        let mut own = stopped;
        own.perp_fills.push(fill("NVDA", Side::Sell, false));
        let why = disqualification(&own).unwrap();
        assert!(
            why.contains("phoenix:perp:NVDA") && why.contains("a side its mandate"),
            "{why}"
        );
    }

    #[test]
    fn trading_outside_the_mandate_disqualifies() {
        let s = scenario(None);
        let mut elsewhere = session(300.0);
        elsewhere.positions_outside_universe = 1;
        let report = score(vec![(session(100.0), &s), (elsewhere, &s)]);
        assert_eq!(report.reward, 0.0);
        assert!(
            report.scenarios[1]
                .disqualified
                .as_deref()
                .unwrap()
                .contains("outside its mandate")
        );
    }

    #[test]
    fn risk_is_reported_across_sessions() {
        let s = scenario(None);
        let pnls = [300.0, -200.0, 100.0, 50.0, -50.0, 400.0];
        let mut sessions: Vec<(SessionReport, &Scenario)> =
            pnls.iter().map(|p| (session(*p), &s)).collect();
        sessions[1].0.max_drawdown_usd = 450.0;
        let risk = score(sessions).risk;
        assert_eq!(risk.worst_pnl_usd, -200.0);
        assert_eq!(risk.tail_pnl_usd, -200.0);
        assert_eq!(risk.max_drawdown_usd, 450.0);
        assert_eq!(risk.profitable_share, 4.0 / 6.0);
        assert!(risk.sharpe > 0.0 && risk.sharpe < 1.0, "{}", risk.sharpe);
    }

    #[test]
    fn sessions_whose_takers_failed_fail_the_grade() {
        let mut thin = session(10.0);
        thin.flow = Some(crate::actors::FlowReport {
            orders: 100,
            sent: 100,
            failed: 11,
            ..Default::default()
        });
        assert!(check_timing(&[session(1.0), thin.clone()]).is_err());
        thin.flow.as_mut().unwrap().failed = 10;
        assert!(check_timing(&[thin]).is_ok());
    }

    #[test]
    fn sessions_whose_actors_failed_fail_the_grade() {
        use crate::actors::library::{Actions, ActorTally};
        let tally = |placed, failed, reacts_to_bot| ActorTally {
            actions: Actions {
                placed,
                failed,
                ..Default::default()
            },
            reacts_to_bot,
            ..Default::default()
        };
        let mut thin = session(10.0);
        thin.actors = Some(crate::actors::LibraryReport {
            actors: vec![tally(89, 11, false), tally(0, 500, true)],
            ..Default::default()
        });
        let error = check_timing(&[session(1.0), thin.clone()]).unwrap_err();
        assert!(error.to_string().contains("11 of 100 actor actions failed"));
        // What failed of the actors that act on the bot's trading is the
        // bot's doing: never the session's.
        thin.actors.as_mut().unwrap().actors = vec![tally(90, 10, false), tally(0, 500, true)];
        assert!(check_timing(&[thin]).is_ok());
    }

    #[test]
    fn a_slow_machine_doesnt_void_a_grade() {
        let mut slow = session(10.0);
        slow.wall_ms = 3_600_000;
        slow.turns.wall_ms = 1_000_000;
        assert!(check_timing(&[session(1.0), slow]).is_ok());
    }

    #[tokio::test]
    async fn bot_user_ranges_stay_within_the_users() {
        let logs = std::env::temp_dir().join(format!("mtx-logs-{}", uuid::Uuid::new_v4()));
        let keys = SessionKeys::random();
        let run = |first: usize, jobs: usize| {
            run_sessions(
                Path::new("."),
                Path::new("."),
                &[],
                &logs,
                first..first + jobs,
                None,
                false,
                false,
                None,
                keys.role("bot"),
                session::Rules::default(),
            )
        };
        assert!(matches!(
            run(2, 4).await,
            Err(Error::BotUsers {
                first: 2,
                jobs: 4,
                users: 4
            })
        ));
        assert!(matches!(run(4, 1).await, Err(Error::BotUsers { .. })));
        assert!(run(2, 2).await.unwrap().is_empty());
        assert!(run(0, 4).await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&logs);
    }

    #[tokio::test]
    async fn a_components_sessions_run_many_at_once_with_no_bot_users() {
        let logs = std::env::temp_dir().join(format!("mtx-logs-{}", uuid::Uuid::new_v4()));
        let bot = std::env::temp_dir().join(format!("mtx-component-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&bot).unwrap();
        std::fs::write(bot.join(crate::wasm::BOT_WASM), b"").unwrap();
        let keys = SessionKeys::random();
        let run = |first: usize, jobs: usize| {
            run_sessions(
                Path::new("."),
                &bot,
                &[],
                &logs,
                first..first + jobs,
                None,
                false,
                false,
                None,
                keys.role("bot"),
                session::Rules::default(),
            )
        };
        assert!(run(0, 16).await.unwrap().is_empty());
        assert!(run(3, MAX_COMPONENT_JOBS).await.unwrap().is_empty());
        assert!(matches!(
            run(0, MAX_COMPONENT_JOBS + 1).await,
            Err(Error::Jobs(_, MAX_COMPONENT_JOBS))
        ));
        assert!(matches!(run(0, 0).await, Err(Error::Jobs(0, _))));
        // A process bot's sessions still take one bot user each.
        std::fs::remove_file(bot.join(crate::wasm::BOT_WASM)).unwrap();
        assert!(matches!(run(0, 5).await, Err(Error::Jobs(5, 4))));
        let _ = std::fs::remove_dir_all(&bot);
        let _ = std::fs::remove_dir_all(&logs);
    }

    #[tokio::test]
    async fn reports_come_back_in_order_and_stop_at_the_first_breach_in_it() {
        let named = |index: usize, breach: bool| {
            let mut report = session(index as f64);
            report.scenario = format!("s{index}");
            if breach {
                report.refused_calls = vec!["surfnet_setAccount".into()];
            }
            (index, Ok(report))
        };
        let names = |reports: Vec<SessionReport>| -> Vec<String> {
            reports.into_iter().map(|r| r.scenario).collect()
        };
        let ended = vec![
            named(2, false),
            named(0, false),
            named(3, false),
            named(1, false),
        ];
        let all = in_order(futures::stream::iter(ended), 4, true)
            .await
            .unwrap();
        assert_eq!(names(all), ["s0", "s1", "s2", "s3"]);
        // Session 3 breached first, then 1 did: the reports end at 1, as
        // they would one at a time.
        let ended = vec![
            named(3, true),
            named(0, false),
            named(1, true),
            named(2, false),
        ];
        let stopped = in_order(futures::stream::iter(ended), 4, true)
            .await
            .unwrap();
        assert_eq!(names(stopped), ["s0", "s1"]);
        // Without stopping, every report comes back.
        let ended = vec![named(1, true), named(0, false)];
        let kept = in_order(futures::stream::iter(ended), 2, false)
            .await
            .unwrap();
        assert_eq!(names(kept), ["s0", "s1"]);
    }

    #[test]
    fn a_components_sessions_at_once_fit_the_memory() {
        let gib = 1u64 << 30;
        // The verifier's 6 GiB: two sessions at their cap.
        assert_eq!(jobs_for_memory(4, Some(6 * gib)), 2);
        assert_eq!(jobs_for_memory(4, Some(64 * gib)), 4);
        assert_eq!(jobs_for_memory(32, Some(64 * gib)), 31);
        // Never none, and never more than asked.
        assert_eq!(jobs_for_memory(4, Some(gib)), 1);
        assert_eq!(jobs_for_memory(1, Some(64 * gib)), 1);
        assert_eq!(jobs_for_memory(8, None), 8);
        assert!(memory_limit().is_none_or(|limit| limit > 0));
    }

    #[test]
    fn only_the_machines_failures_are_retried() {
        assert!(is_transient(&Error::Timeout("slot 9".into())));
        assert!(is_transient(&Error::Transaction {
            label: "onboard".into(),
            reason: "Blockhash not found".into(),
        }));
        assert!(is_transient(&Error::Meter(MeterError::Read(
            "connection refused".into()
        ))));
        assert!(!is_transient(&Error::Meter(MeterError::Decode {
            what: "margin",
            detail: "short".into(),
        })));
        assert!(!is_transient(&Error::MissingBot("bot".into())));
        assert!(!is_transient(&Error::Degraded("failed takers".into())));
        assert!(!is_transient(&Error::UnknownMarket("XYZ".into())));
    }

    #[test]
    fn a_setup_failure_reads_as_its_cause() {
        let error = Error::Setup(Box::new(Error::Timeout("slot 9".into())));
        assert_eq!(error.to_string(), "timed out waiting for slot 9");
    }

    #[test]
    fn the_open_file_limit_never_drops() {
        let limit = || {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: getrlimit only writes the struct given.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
                0
            );
            limit
        };
        let before = limit();
        raise_open_file_limit();
        let after = limit();
        assert!(after.rlim_cur >= before.rlim_cur);
        // Linux takes any soft limit up to the hard one.
        #[cfg(target_os = "linux")]
        assert_eq!(after.rlim_cur, after.rlim_max);
    }
}
