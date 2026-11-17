//! How a grade is scored, as flags: `mtx grade`'s, and `mtx rescore`'s and
//! `mtx bench rescore`'s on the host.

use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, Result, bail};
use mtx_grader::scoring_v2::{self, Ceiling, Params};

/// Which scores a grade reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Scoring {
    /// `reward` and `skill`, as always.
    #[default]
    V1,
    /// Those, and scoring v2 beside them (`v2` in the report, `score_v2*`
    /// in the rewards): 0 = doing nothing, 0.5 = the reference, 1 = the
    /// task's informed bot, losses counted 1.5 times, a breach scored per
    /// session and capping the run, and a charge for drawing down far more
    /// than the anchor. Report-only: `reward` and `skill` don't change.
    V2,
    /// Scoring v2 as the grade's one score, what a released task grades by:
    /// `reward` is v2's clamped score, `skill` and `reference_ratio` are
    /// left out, and the 1 must be the task's informed bot: a grade without
    /// it on every session fails rather than fall back to the hindsight
    /// ceiling. `--eta` and `--ceiling` are refused.
    #[value(name = "v2-headline")]
    V2Headline,
}

/// How a grade is scored: `mtx grade`, `mtx rescore` and `mtx bench rescore`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ScoringArgs {
    #[arg(long, value_enum, default_value_t = Scoring::V1)]
    pub scoring: Scoring,
    /// For analysis only, never a task's setting: force v2's ceiling, for a
    /// task whose scenarios carry no informed bot's PnL, to this share of
    /// each session's hindsight bound instead of the cost-aware hindsight
    /// bound (the score says `ceiling: eta_override`). `<task>=<eta>` gives
    /// one task's (as named in bench/tasks), for a batch of several.
    #[arg(long, value_name = "[TASK=]ETA")]
    pub eta: Vec<String>,
    /// v2's ceiling given outright instead: a JSON object from scenario
    /// name to ceiling PnL in USD.
    #[arg(long, conflicts_with = "eta")]
    pub ceiling: Option<PathBuf>,
    /// v2: how many times a loss counts.
    #[arg(long, default_value_t = scoring_v2::LOSS_WEIGHT)]
    pub loss_weight: f64,
    /// v2: the least the reference anchor can be, as a share of the
    /// ceiling; a weaker reference is floored there and flagged (default
    /// 0.10, or 0.05 of the cost-aware hindsight ceiling).
    #[arg(long)]
    pub anchor_floor: Option<f64>,
    /// v2: what the cost-aware hindsight ceiling pays per trade, in basis
    /// points of the price: the venue's taker fee plus half-spread.
    #[arg(long, default_value_t = scoring_v2::HINDSIGHT_COST * 1e4)]
    pub ceiling_cost_bps: f64,
    /// v2: the near-breach charge's weight λ (1 by default; 0 turns it
    /// off). `<task>=<λ>` gives one task's, as `--eta` does.
    #[arg(long, value_name = "[TASK=]LAMBDA")]
    pub risk_weight: Vec<String>,
}

impl ScoringArgs {
    /// v2's parameters for a grade of `task`, or `None` under v1.
    pub fn params(&self, task: Option<&str>) -> Result<Option<Params>> {
        if self.scoring == Scoring::V1 {
            if !self.eta.is_empty() || self.ceiling.is_some() || !self.risk_weight.is_empty() {
                bail!("--eta, --ceiling and --risk-weight are for --scoring v2");
            }
            return Ok(None);
        }
        let headline = self.scoring == Scoring::V2Headline;
        if headline && (!self.eta.is_empty() || self.ceiling.is_some()) {
            bail!("--eta and --ceiling replace the 1, and a v2 headline takes the informed bot's");
        }
        let ceiling = match &self.ceiling {
            Some(path) => Ceiling::Given(
                serde_json::from_slice::<BTreeMap<String, f64>>(
                    &std::fs::read(path).with_context(|| path.display().to_string())?,
                )
                .with_context(|| format!("{}: not a map of ceilings", path.display()))?,
            ),
            None => Ceiling::Informed {
                eta_override: self.eta(task)?,
            },
        };
        Ok(Some(Params {
            loss_weight: self.loss_weight,
            anchor_floor: self.anchor_floor,
            hindsight_cost: self.ceiling_cost_bps / 1e4,
            risk_weight: per_task("--risk-weight", &self.risk_weight, task)?
                .unwrap_or(scoring_v2::RISK_WEIGHT),
            headline,
            ..Params::new(ceiling)
        }))
    }

    /// The `--eta` for `task`: its own, else the one without a task.
    fn eta(&self, task: Option<&str>) -> Result<Option<f64>> {
        let eta = per_task("--eta", &self.eta, task)?;
        if let Some(eta) = eta
            && !(eta > 0.0 && eta <= 1.0)
        {
            bail!("--eta {eta}: a share of the bound, in (0, 1]");
        }
        Ok(eta)
    }
}

/// A `[TASK=]VALUE` option's value for `task`: its own, else the one
/// without a task.
fn per_task(flag: &str, given: &[String], task: Option<&str>) -> Result<Option<f64>> {
    let mut any = None;
    for given in given {
        let (name, value) = match given.split_once('=') {
            Some((name, value)) => (Some(name), value),
            None => (None, given.as_str()),
        };
        let value: f64 = value
            .parse()
            .with_context(|| format!("{flag} {given}: not a number"))?;
        match name {
            Some(name) if Some(name) == task => return Ok(Some(value)),
            Some(_) => {}
            None => any = Some(value),
        }
    }
    Ok(any)
}

/// The flags as given with nothing but `--scoring` and `--eta`.
#[cfg(test)]
pub(crate) fn args(scoring: Scoring, eta: &[&str]) -> ScoringArgs {
    ScoringArgs {
        scoring,
        eta: eta.iter().map(|e| e.to_string()).collect(),
        ceiling: None,
        loss_weight: scoring_v2::LOSS_WEIGHT,
        anchor_floor: None,
        ceiling_cost_bps: scoring_v2::HINDSIGHT_COST * 1e4,
        risk_weight: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2s_risk_weight_defaults_to_one_and_can_be_set_per_task() {
        let lambda = |given: &ScoringArgs, task| given.params(task).unwrap().unwrap().risk_weight;
        let mut given = args(Scoring::V2, &["0.3"]);
        assert_eq!(lambda(&given, Some("flash-crash-spcx")), 1.0);
        given.risk_weight = vec!["2".into(), "market-making-spcx=0".into()];
        assert_eq!(lambda(&given, Some("flash-crash-spcx")), 2.0);
        assert_eq!(lambda(&given, Some("market-making-spcx")), 0.0);
        let mut v1 = args(Scoring::V1, &[]);
        v1.risk_weight = vec!["2".into()];
        assert!(v1.params(None).is_err());
    }

    #[test]
    fn v2_takes_a_tasks_own_eta_before_the_default() {
        let given = args(Scoring::V2, &["0.3", "flash-crash-spcx=0.37"]);
        let eta = |given: &ScoringArgs, task| match given.params(task).unwrap().unwrap().ceiling {
            Ceiling::Informed { eta_override } => eta_override,
            Ceiling::Given(_) => unreachable!(),
        };
        assert_eq!(eta(&given, Some("flash-crash-spcx")), Some(0.37));
        assert_eq!(eta(&given, Some("weekend-gap-spcx")), Some(0.3));
        assert_eq!(eta(&given, None), Some(0.3));
        let only = args(Scoring::V2, &["flash-crash-spcx=0.37"]);
        assert_eq!(eta(&only, Some("weekend-gap-spcx")), None);
        // Without η, only an informed bot's PnL can be the ceiling.
        assert_eq!(eta(&args(Scoring::V2, &[]), None), None);
        assert!(args(Scoring::V2, &["2"]).params(None).is_err());
        assert!(args(Scoring::V1, &[]).params(None).unwrap().is_none());
        assert!(args(Scoring::V1, &["0.3"]).params(None).is_err());
    }
}
