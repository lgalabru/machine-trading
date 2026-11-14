//! Market model v2: a fair-value process driven by a calibration file.
//!
//! A session's path is drawn slot by slot:
//!
//! - **Clock.** The session starts at an instant on the calibration's
//!   calendar (drawn, inside a session kind, just before one opens, or
//!   fixed). Each block's volatility is scaled by the calendar's session
//!   multiplier, or by the hour of day on a continuous calendar.
//! - **Volatility level.** The session draws its regime from the regimes'
//!   shares and, for each regime, a level: the regime's multiplier times a
//!   log-normal draw. Real sessions differ in volatility mostly between
//!   sessions, not within one (CALIBRATION.md, section e), so the level is
//!   per session. Regimes switch within the session at their hourly
//!   transition rates.
//! - **Clustering and tails.** Block returns follow a GARCH(1,1) on variance
//!   relative to the level, with unit-variance Student-t innovations.
//! - **Slots.** A Gaussian bridge splits each block's return into slots that
//!   sum to it.
//! - **Events.** Background jumps arrive as a Poisson process; when the
//!   calendar enters a session kind that gaps (the open), the price gaps by
//!   a draw from the fitted gap sizes. Both are recorded.
//! - **Mark.** When the calibration's fair value is a venue's smoothed mark
//!   (SPCX's Phoenix mark), the output trails the path: every
//!   `update_seconds` it closes part of the gap, an exponential average with
//!   the fitted half-life. That smoothing is what gives real 1 m marks their
//!   small positive lag-1 autocorrelation.
//!
//! The path compiles to [`Waypoint`]s, which the existing machinery
//! replays: [`MarketModel::scenario`] fills a template scenario's `path` and
//! `fair_path` and records the draw in [`Scenario::market_model`]. Several
//! assets can be drawn together with correlated block innovations
//! ([`draw_joint`]).

use serde::{Deserialize, Serialize};

use super::{
    calendar::SessionCalendar,
    calibration::{CalendarModel, Calibration, FairValueModel, Regimes, validate_regimes},
};
use crate::scenario::{SLOT_SECONDS, Scenario, SplitMix64, Waypoint, move_slots};

/// z of the 90th percentile of a standard normal.
const Z90: f64 = 1.281_551_565_545;

/// How a scenario draws from a calibration. Each field is a stated
/// departure from the calibration (or the default: none).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    /// Scales volatility beyond the calibration: stress, stated.
    #[serde(default = "one")]
    pub vol_mult: f64,
    /// A regime by name, held for the whole session; without it, drawn and
    /// switching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regime: Option<String>,
    #[serde(default)]
    pub start: Start,
    /// Slots between waypoints. At least 3: a move takes Phoenix two slots,
    /// and the next starts after it.
    #[serde(default = "default_waypoint_slots")]
    pub waypoint_slots: u64,
    /// Background jumps, added on top of the fitted tails. Off by default:
    /// the tails and the clustering are fitted on returns that hold these
    /// moves already, so adding them again counts them twice. A family that
    /// wants explicit jump events turns them on, or designs its own sized
    /// as multiples of the calibration's jump p90.
    #[serde(default)]
    pub jumps: bool,
    /// Gaps when a session opens (on by default).
    #[serde(default = "yes")]
    pub gaps: bool,
    /// Runs the regimes' switching this many times faster than their hourly
    /// rates: a stated departure, so a session minutes long can see a
    /// switch. It changes how often the state changes, not what each state
    /// is. At most the rate at which a block leaves its state half the time.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub regime_clock: f64,
    /// A stated regime chain in place of the calibration's: its states'
    /// multipliers, level spreads, shares and hourly transitions, with a
    /// `source` saying where they come from. For a calibration fitted with
    /// one state, this is how a family switches between stated ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regimes: Option<Regimes>,
}

fn is_one(x: &f64) -> bool {
    *x == 1.0
}

fn one() -> f64 {
    1.0
}

fn yes() -> bool {
    true
}

fn default_waypoint_slots() -> u64 {
    4
}

impl Default for ModelSpec {
    fn default() -> Self {
        Self {
            vol_mult: 1.0,
            regime: None,
            start: Start::Draw,
            waypoint_slots: default_waypoint_slots(),
            jumps: false,
            gaps: true,
            regime_clock: 1.0,
            regimes: None,
        }
    }
}

/// When on the calendar a session starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Start {
    /// Anywhere in the calendar's draw range.
    #[default]
    Draw,
    /// Inside a session of this kind, the whole session inside it when it
    /// fits.
    Session { kind: String },
    /// This long before a session of this kind opens: the open, and its
    /// gap, fall inside the session.
    Before { kind: String, lead_seconds: f64 },
    /// At a fixed instant, in Unix seconds.
    At { unix: i64 },
}

/// What a scenario drawn from a calibration records: which file, the
/// stated departures, and what was drawn, so realism checks can mask the
/// events and compare like with like. Never the seed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MarketModelRecord {
    pub calibration: String,
    pub calibration_sha256: String,
    pub process: FairValueModel,
    pub vol_mult: f64,
    pub slot_seconds: f64,
    /// The session's start on the calendar, in Unix seconds.
    pub start_unix: i64,
    /// Each regime's volatility multiplier this session, level draw
    /// included.
    pub regime_vol: Vec<f64>,
    /// The regime from each slot on.
    pub regimes: Vec<Mark>,
    /// The calendar session kind from each slot on (`closed` between).
    pub sessions: Vec<Mark>,
    /// Gaps and jumps, with their sizes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Event>,
    /// The spec's regime clock, when it departs from 1.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub regime_clock: f64,
    /// The spec's stated regime chain, when it replaces the calibration's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regime_set: Option<Regimes>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Mark {
    pub slot: u64,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub slot: u64,
    pub kind: EventKind,
    /// The log move, in percent.
    pub pct: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A session opened.
    Gap,
    /// A background jump.
    Jump,
}

impl MarketModelRecord {
    pub fn validate(&self, duration_slots: u64) -> std::result::Result<(), String> {
        if self.calibration.is_empty()
            || self.calibration_sha256.len() != 64
            || !self
                .calibration_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err("names its calibration and the file's SHA-256".into());
        }
        let in_session = |slot: u64| slot <= duration_slots;
        if !(self.regimes.iter().all(|m| in_session(m.slot))
            && self.sessions.iter().all(|m| in_session(m.slot))
            && self.events.iter().all(|e| in_session(e.slot)))
        {
            return Err("a recorded slot is past the session end".into());
        }
        Ok(())
    }
}

/// One drawn path, slot by slot.
#[derive(Debug, Clone)]
pub struct ModelPath {
    /// Log price relative to the start, at each slot `0..=duration`.
    pub log_levels: Vec<f64>,
    pub record: MarketModelRecord,
}

impl ModelPath {
    /// The level at a slot, in percent from the start, as waypoints carry it.
    pub fn level_pct(&self, slot: u64) -> f64 {
        (self.log_levels[slot as usize].exp() - 1.0) * 100.0
    }

    /// The path as waypoints, one every `every` slots when the last move has
    /// finished, each moving to the level at its slot. Every move finishes
    /// before the session ends.
    pub fn waypoints(&self, every: u64) -> Vec<Waypoint> {
        let duration = (self.log_levels.len() - 1) as u64;
        let mut out = Vec::new();
        let mut last = 0.0;
        let mut free_from = 0;
        let mut slot = every;
        while slot < duration {
            if slot >= free_from {
                let level = round(self.level_pct(slot));
                let done = slot + move_slots(last, level) + 1;
                if done > duration {
                    break;
                }
                if level != last {
                    out.push(Waypoint::at(slot, level));
                    free_from = done;
                    last = level;
                }
            }
            slot += every;
        }
        out
    }
}

fn round(level_pct: f64) -> f64 {
    (level_pct * 1000.0).round() / 1000.0
}

/// A calibration ready to draw sessions from.
#[derive(Debug, Clone)]
pub struct MarketModel {
    pub calibration: Calibration,
    pub spec: ModelSpec,
    calendar: SessionCalendar,
    fixed_regime: Option<usize>,
    /// The median session's volatility multiplier before normalizing (see
    /// [`median_level`]): sessions are scaled by its inverse, so the median
    /// session runs at the calibration's `per_second_sd`.
    level_norm: f64,
}

impl MarketModel {
    pub fn new(mut calibration: Calibration, spec: ModelSpec) -> std::result::Result<Self, String> {
        if !(spec.vol_mult.is_finite() && spec.vol_mult > 0.0) {
            return Err(format!("vol_mult {} must be positive", spec.vol_mult));
        }
        if let Some(regimes) = &spec.regimes {
            validate_regimes(regimes, "regimes")?;
            calibration.fair_value.regimes = regimes.clone();
        }
        check_regime_clock(&calibration, spec.regime_clock)?;
        if spec.waypoint_slots < 3 {
            return Err("waypoint_slots must be at least 3".into());
        }
        let calendar = SessionCalendar::new(&calibration.calendar)?;
        let fixed_regime = match &spec.regime {
            None => None,
            Some(name) => Some(
                calibration
                    .fair_value
                    .regimes
                    .names
                    .iter()
                    .position(|n| n == name)
                    .ok_or_else(|| format!("the calibration has no regime {name}"))?,
            ),
        };
        match &spec.start {
            Start::Session { kind } | Start::Before { kind, .. } if !calendar.has_kind(kind) => {
                return Err(format!("the calendar has no session kind {kind}"));
            }
            Start::Before { lead_seconds, .. } if lead_seconds.is_nan() || *lead_seconds < 0.0 => {
                return Err("start.lead_seconds must be non-negative".into());
            }
            _ => {}
        }
        let level_norm = median_level(&calibration);
        Ok(Self {
            level_norm,
            calibration,
            spec,
            calendar,
            fixed_regime,
        })
    }

    pub fn calendar(&self) -> &SessionCalendar {
        &self.calendar
    }

    /// Draw one session of `duration_slots`.
    pub fn draw(&self, seed: u64, duration_slots: u64) -> ModelPath {
        let mut rng = SplitMix64(seed ^ 0x6d61_726b_6574_7632);
        let start = self.draw_start(&mut rng, duration_slots);
        let mut walker = Walker::new(self, start, duration_slots, &mut rng);
        while !walker.done() {
            let z = rng.normal();
            walker.block(&mut rng, z);
        }
        walker.finish()
    }

    /// A template scenario with its path drawn from the model: `path` and
    /// `fair_path` are the drawn path's waypoints, and `market_model` records
    /// the draw. Everything else is the template's.
    pub fn scenario(
        &self,
        template: &Scenario,
        name: &str,
        seed: u64,
    ) -> std::result::Result<Scenario, String> {
        let drawn = self.draw(seed, template.duration_slots);
        self.fill(template, name, drawn)
    }

    fn fill(
        &self,
        template: &Scenario,
        name: &str,
        drawn: ModelPath,
    ) -> std::result::Result<Scenario, String> {
        let mut scenario = template.clone();
        let path = drawn.waypoints(self.spec.waypoint_slots);
        scenario.name = name.into();
        scenario.fair_path = Some(path.clone());
        scenario.path = path;
        scenario.market_model = Some(drawn.record);
        scenario.validate()?;
        Ok(scenario)
    }

    /// `count` scenarios from a template, named `<prefix>-<n>`.
    pub fn sample_set(
        &self,
        template: &Scenario,
        prefix: &str,
        count: usize,
        seed: u64,
    ) -> std::result::Result<Vec<Scenario>, String> {
        (0..count)
            .map(|i| {
                let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(i as u64);
                self.scenario(template, &format!("{prefix}-{}", i + 1), draw_seed)
            })
            .collect()
    }

    fn draw_start(&self, rng: &mut SplitMix64, duration_slots: u64) -> i64 {
        let cal = &self.calendar;
        let (from, to) = cal.draw_days();
        let day_seconds = 86_400;
        let any = |rng: &mut SplitMix64| {
            let span = (to - from + 1) * day_seconds;
            from * day_seconds + (rng.next_u64() % span as u64) as i64
        };
        let length = (duration_slots as f64 * SLOT_SECONDS).ceil() as i64;
        match &self.spec.start {
            Start::Draw => any(rng),
            Start::At { unix } => *unix,
            Start::Session { kind } => {
                let at = any(rng);
                // Kinds exist (checked in new), and every kind recurs within a
                // year.
                let (open, close) = cal.next_session(kind, at).unwrap_or((at, at));
                let room = (close - open - length).max(0);
                open + (rng.next_u64() % (room as u64 + 1)) as i64
            }
            Start::Before { kind, lead_seconds } => {
                let at = any(rng);
                let (open, _) = cal.next_session(kind, at).unwrap_or((at, at));
                open - lead_seconds.round() as i64
            }
        }
    }
}

/// A smoothed mark trailing the path (see
/// [`crate::market::calibration::MarkSmoothing`]).
struct SmoothedMark {
    level: f64,
    /// Slots between updates, and the share of the gap each closes.
    every: u64,
    weight: f64,
}

/// One asset's state through a draw, block by block.
struct Walker<'a> {
    model: &'a MarketModel,
    start: i64,
    duration: u64,
    slot_seconds: f64,
    block_slots: u64,
    slot: u64,
    /// The path's log level; the output is its mark when the calibration
    /// has one.
    log_level: f64,
    mark: Option<SmoothedMark>,
    log_levels: Vec<f64>,
    regime: usize,
    regime_vol: Vec<f64>,
    /// GARCH variance relative to its target.
    h: f64,
    kind: Option<String>,
    regimes: Vec<Mark>,
    sessions: Vec<Mark>,
    events: Vec<Event>,
}

impl<'a> Walker<'a> {
    fn new(model: &'a MarketModel, start: i64, duration: u64, rng: &mut SplitMix64) -> Self {
        let fv = &model.calibration.fair_value;
        let regimes = &fv.regimes;
        let regime_vol: Vec<f64> = regimes
            .vol_mult
            .iter()
            .zip(&regimes.log_sd_within)
            .map(|(mult, sd)| mult * (sd * rng.normal()).exp() / model.level_norm)
            .collect();
        let regime = model
            .fixed_regime
            .unwrap_or_else(|| pick(rng.next_f64(), &regimes.share));
        let slot_seconds = SLOT_SECONDS;
        let block_slots = ((fv.block_seconds / slot_seconds).round() as u64).max(1);
        let kind = Some(kind_name(&model.calendar, start));
        let mark = fv.mark.as_ref().map(|m| SmoothedMark {
            level: 0.0,
            every: ((m.update_seconds / slot_seconds).round() as u64).max(1),
            weight: 1.0 - 0.5f64.powf(m.update_seconds / m.half_life_seconds),
        });
        Self {
            model,
            start,
            duration,
            slot_seconds,
            block_slots,
            slot: 0,
            log_level: 0.0,
            mark,
            log_levels: vec![0.0],
            regime,
            regime_vol,
            h: 1.0,
            regimes: vec![Mark {
                slot: 0,
                name: regimes.names[regime].clone(),
            }],
            sessions: vec![Mark {
                slot: 0,
                name: kind.clone().unwrap_or_default(),
            }],
            kind,
            events: Vec::new(),
        }
    }

    fn done(&self) -> bool {
        self.slot >= self.duration
    }

    fn unix_at(&self, slot: u64) -> i64 {
        self.start + (slot as f64 * self.slot_seconds).floor() as i64
    }

    /// One block, its innovation's normal part given (correlated across
    /// assets in a joint draw).
    fn block(&mut self, rng: &mut SplitMix64, normal: f64) {
        let model = self.model;
        let cal = &model.calibration;
        let fv = &cal.fair_value;
        let n = self.block_slots.min(self.duration - self.slot);
        let at = self.unix_at(self.slot);
        let clock = model.calendar.vol_mult_at(at) * intraday(cal, at);
        let sd = cal.per_slot_sd(self.slot_seconds)
            * (n as f64).sqrt()
            * self.regime_vol[self.regime]
            * clock
            * model.spec.vol_mult;
        // A unit-variance Student-t: the normal part times a scale. The
        // normal part spreads over the block's slots (a Gaussian bridge);
        // what the scale adds lands in one slot, so a tail move arrives at
        // once instead of evenly, as real ones do.
        let df = fv.tails.df;
        let scale = ((df - 2.0) / chi_square(rng, df)).sqrt();
        let e = self.h.sqrt() * normal * scale;
        let diffusive = sd * self.h.sqrt() * normal;
        let excess = diffusive * (scale - 1.0);
        let excess_at = rng.next_u64() % n;
        let pieces: Vec<f64> = (0..n).map(|_| rng.normal()).collect();
        let mean = pieces.iter().sum::<f64>() / n as f64;
        let piece_sd = sd * self.h.sqrt() / (n as f64).sqrt();
        for (i, piece) in pieces.into_iter().enumerate() {
            let mut step = diffusive / n as f64 + piece_sd * (piece - mean);
            if i as u64 == excess_at {
                step += excess;
            }
            self.slot += 1;
            let slot = self.slot;
            let unix = self.unix_at(slot);
            // The calendar: a new session kind, and its gap.
            if !model.calendar.is_continuous() {
                let kind = kind_name(&model.calendar, unix);
                if self.kind.as_deref() != Some(kind.as_str()) {
                    if model.spec.gaps
                        && let Some(gap) = cal.gap(&kind)
                    {
                        let pct = signed_lognormal(rng, gap.abs_median_pct, gap.abs_p90_pct);
                        step += pct / 100.0;
                        self.events.push(Event {
                            slot,
                            kind: EventKind::Gap,
                            pct,
                        });
                    }
                    self.sessions.push(Mark {
                        slot,
                        name: kind.clone(),
                    });
                    self.kind = Some(kind);
                }
            }
            let jumps = &fv.jumps;
            if model.spec.jumps
                && jumps.per_hour > 0.0
                && rng.next_f64() < jumps.per_hour * self.slot_seconds / 3600.0
            {
                let pct = signed_lognormal(rng, jumps.abs_size_median_pct, jumps.abs_size_p90_pct)
                    * model.spec.vol_mult;
                step += pct / 100.0;
                self.events.push(Event {
                    slot,
                    kind: EventKind::Jump,
                    pct,
                });
            }
            self.log_level += step;
            match &mut self.mark {
                Some(mark) => {
                    if slot.is_multiple_of(mark.every) {
                        mark.level += mark.weight * (self.log_level - mark.level);
                    }
                    self.log_levels.push(mark.level);
                }
                None => self.log_levels.push(self.log_level),
            }
        }
        let c = &fv.clustering;
        self.h = (1.0 - c.alpha - c.beta) + c.alpha * e * e + c.beta * self.h;
        // Regime switches, at the hourly rates scaled to the block.
        if model.fixed_regime.is_none() && self.slot < self.duration {
            let row = &fv.regimes.transition_per_hour[self.regime];
            let scale = n as f64 * self.slot_seconds / 3600.0;
            let stay = 1.0 - (1.0 - row[self.regime]) * scale * model.spec.regime_clock;
            let u = rng.next_f64();
            if u >= stay {
                let others: Vec<f64> = row
                    .iter()
                    .enumerate()
                    .map(|(j, p)| if j == self.regime { 0.0 } else { *p })
                    .collect();
                let next = pick((u - stay) / (1.0 - stay), &normalize(&others));
                if next != self.regime {
                    self.regime = next;
                    self.regimes.push(Mark {
                        slot: self.slot,
                        name: fv.regimes.names[next].clone(),
                    });
                }
            }
        }
    }

    fn finish(self) -> ModelPath {
        let cal = &self.model.calibration;
        ModelPath {
            log_levels: self.log_levels,
            record: MarketModelRecord {
                calibration: cal.meta.calibration_id.clone(),
                calibration_sha256: cal.sha256.clone(),
                process: cal.fair_value.model,
                vol_mult: self.model.spec.vol_mult,
                slot_seconds: self.slot_seconds,
                start_unix: self.start,
                regime_vol: self.regime_vol,
                regimes: self.regimes,
                sessions: self.sessions,
                events: self.events,
                regime_clock: self.model.spec.regime_clock,
                regime_set: self.model.spec.regimes.clone(),
            },
        }
    }
}

/// Draw several assets over the same clock, their block innovations
/// correlated by `correlation` (a symmetric, positive-definite matrix with
/// a unit diagonal). The assets' calibrations must share a block length.
/// The start is drawn by the first model's spec. Each asset's Student-t
/// divisor is its own, so the realized correlation of block returns is a
/// little below the matrix's.
pub fn draw_joint(
    models: &[&MarketModel],
    correlation: &[Vec<f64>],
    seed: u64,
    duration_slots: u64,
) -> std::result::Result<Vec<ModelPath>, String> {
    let k = models.len();
    if k == 0 || correlation.len() != k || correlation.iter().any(|row| row.len() != k) {
        return Err(format!("the correlation matrix must be {k} by {k}"));
    }
    let blocks: Vec<f64> = models
        .iter()
        .map(|m| m.calibration.fair_value.block_seconds)
        .collect();
    if blocks.iter().any(|b| *b != blocks[0]) {
        return Err(format!("the assets' block_seconds differ: {blocks:?}"));
    }
    let lower = cholesky(correlation)?;
    let mut rng = SplitMix64(seed ^ 0x6a6f_696e_7476_3200);
    let start = models[0].draw_start(&mut rng, duration_slots);
    let mut walkers: Vec<Walker> = models
        .iter()
        .map(|m| Walker::new(m, start, duration_slots, &mut rng))
        .collect();
    while !walkers[0].done() {
        let g: Vec<f64> = (0..k).map(|_| rng.normal()).collect();
        for (i, walker) in walkers.iter_mut().enumerate() {
            let z: f64 = (0..=i).map(|j| lower[i][j] * g[j]).sum();
            walker.block(&mut rng, z);
        }
    }
    Ok(walkers.into_iter().map(Walker::finish).collect())
}

/// A template scenario on several markets with every market's path drawn
/// jointly ([`draw_joint`]): `models` names each market's model by symbol,
/// in the matrix's order, and the template's markets are those symbols.
/// Each market's `path` and `fair_path` are its drawn waypoints;
/// [`Scenario::joint`] records the draw and where the matrix came from
/// (`matrix_source`: `design` for a matrix the author states). Everything
/// else is the template's.
pub fn joint_scenario(
    models: &[(&str, &MarketModel)],
    correlation: &[Vec<f64>],
    matrix_source: &str,
    template: &Scenario,
    name: &str,
    seed: u64,
) -> std::result::Result<Scenario, String> {
    let mut scenario = template.clone();
    let mut symbols: Vec<&str> = models.iter().map(|(symbol, _)| *symbol).collect();
    symbols.sort();
    let markets: Vec<&str> = scenario.markets.iter().map(|m| m.symbol.as_str()).collect();
    if symbols != markets {
        return Err(format!(
            "the models are for {symbols:?}, and the template's markets are {markets:?}"
        ));
    }
    let only: Vec<&MarketModel> = models.iter().map(|(_, model)| *model).collect();
    let drawn = draw_joint(&only, correlation, seed, template.duration_slots)?;
    let mut records = std::collections::BTreeMap::new();
    for ((symbol, model), drawn) in models.iter().zip(drawn) {
        let path = drawn.waypoints(model.spec.waypoint_slots);
        let market = scenario
            .markets
            .iter_mut()
            .find(|m| m.symbol == *symbol)
            .expect("checked above");
        market.fair_path = Some(path.clone());
        market.path = path;
        market.market_model = None;
        records.insert(symbol.to_string(), drawn.record);
    }
    scenario.name = name.into();
    scenario.joint = Some(crate::scenario::JointDraw {
        matrix_source: matrix_source.into(),
        matrix_sha256: None,
        assets: models
            .iter()
            .map(|(symbol, _)| symbol.to_string())
            .collect(),
        matrix: correlation.to_vec(),
        records,
    });
    scenario.validate()?;
    Ok(scenario)
}

/// `count` [`joint_scenario`]s from a template, named `<prefix>-<n>`, each
/// seed drawn as [`MarketModel::sample_set`] draws its.
pub fn joint_sample_set(
    models: &[(&str, &MarketModel)],
    correlation: &[Vec<f64>],
    matrix_source: &str,
    template: &Scenario,
    prefix: &str,
    count: usize,
    seed: u64,
) -> std::result::Result<Vec<Scenario>, String> {
    (0..count)
        .map(|i| {
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(i as u64);
            joint_scenario(
                models,
                correlation,
                matrix_source,
                template,
                &format!("{prefix}-{}", i + 1),
                draw_seed,
            )
        })
        .collect()
}

fn cholesky(a: &[Vec<f64>]) -> std::result::Result<Vec<Vec<f64>>, String> {
    let n = a.len();
    let mut l = vec![vec![0.0; n]; n];
    for i in 0..n {
        if (a[i][i] - 1.0).abs() > 1e-9 {
            return Err("the correlation matrix needs a unit diagonal".into());
        }
        for j in 0..=i {
            if (a[i][j] - a[j][i]).abs() > 1e-9 {
                return Err("the correlation matrix must be symmetric".into());
            }
            let sum: f64 = (0..j).map(|k| l[i][k] * l[j][k]).sum();
            if i == j {
                let d = a[i][i] - sum;
                if d <= 0.0 {
                    return Err("the correlation matrix must be positive definite".into());
                }
                l[i][j] = d.sqrt();
            } else {
                l[i][j] = (a[i][j] - sum) / l[j][j];
            }
        }
    }
    Ok(l)
}

/// A regime clock must be positive, and no faster than leaves any state
/// within one block half the time: a faster clock would switch nearly every
/// block, and a block's leave chance is linear in it only while small.
fn check_regime_clock(cal: &Calibration, clock: f64) -> std::result::Result<(), String> {
    if !(clock.is_finite() && clock > 0.0) {
        return Err(format!("regime_clock {clock} must be positive"));
    }
    let fv = &cal.fair_value;
    let per_block = fv.block_seconds / 3600.0;
    let fastest = (0..fv.regimes.names.len())
        .map(|i| 1.0 - fv.regimes.transition_per_hour[i][i])
        .fold(0.0, f64::max);
    if fastest > 0.0 && fastest * per_block * clock > 0.5 {
        return Err(format!(
            "regime_clock {clock} leaves a state more than half the time in one block; at most {:.4}",
            0.5 / (fastest * per_block)
        ));
    }
    Ok(())
}

fn kind_name(calendar: &SessionCalendar, unix: i64) -> String {
    calendar.kind_at(unix).unwrap_or("closed").to_string()
}

/// The median of a session's volatility multiplier, before normalizing:
/// the regime's multiplier times its log-normal level draw, times the hour's
/// intraday multiplier on a continuous calendar (sessions start at any
/// hour), mixed by the regimes' shares. The calibration's `per_second_sd`
/// is the median session's level, so the model divides by this.
fn median_level(cal: &Calibration) -> f64 {
    let regimes = &cal.fair_value.regimes;
    let hours: Vec<f64> = match (&cal.fair_value.intraday_vol_mult_utc, cal.calendar.model) {
        (Some(shape), CalendarModel::Continuous) => shape.iter().map(|m| m.ln()).collect(),
        _ => vec![0.0],
    };
    // (weight, log-mean, log-sd) of each log-normal in the mixture.
    let parts: Vec<(f64, f64, f64)> = (0..regimes.names.len())
        .flat_map(|i| {
            let hours = &hours;
            hours.iter().map(move |h| {
                (
                    regimes.share[i] / hours.len() as f64,
                    regimes.vol_mult[i].ln() + h,
                    regimes.log_sd_within[i],
                )
            })
        })
        .collect();
    let cdf = |x: f64| -> f64 {
        parts
            .iter()
            .map(|(w, mu, sd)| {
                let step = if x >= *mu { 1.0 } else { 0.0 };
                w * if *sd > 0.0 {
                    normal_cdf((x - mu) / sd)
                } else {
                    step
                }
            })
            .sum()
    };
    let (mut lo, mut hi) = (-20.0, 20.0);
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if cdf(mid) < 0.5 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (0.5 * (lo + hi)).exp()
}

/// The standard normal CDF, from Abramowitz and Stegun's 7.1.26 erf
/// (error below 1.5e-7).
fn normal_cdf(z: f64) -> f64 {
    let x = z.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-x * x).exp();
    if z >= 0.0 {
        0.5 * (1.0 + erf)
    } else {
        0.5 * (1.0 - erf)
    }
}

/// The intraday multiplier of a continuous calendar at a UTC instant.
fn intraday(cal: &Calibration, unix: i64) -> f64 {
    match (&cal.fair_value.intraday_vol_mult_utc, cal.calendar.model) {
        (Some(shape), CalendarModel::Continuous) => {
            shape[(unix.rem_euclid(86_400) / 3600) as usize]
        }
        _ => 1.0,
    }
}

/// An index drawn from `weights` (summing to 1) by a uniform `u`.
fn pick(u: f64, weights: &[f64]) -> usize {
    let mut acc = 0.0;
    for (i, w) in weights.iter().enumerate() {
        acc += w;
        if u < acc {
            return i;
        }
    }
    weights.len() - 1
}

fn normalize(weights: &[f64]) -> Vec<f64> {
    let sum: f64 = weights.iter().sum();
    if sum <= 0.0 {
        return weights.to_vec();
    }
    weights.iter().map(|w| w / sum).collect()
}

/// A size in percent, log-normal with this median and 90th percentile,
/// either sign.
fn signed_lognormal(rng: &mut SplitMix64, median: f64, p90: f64) -> f64 {
    let sigma = (p90 / median).ln() / Z90;
    let size = median * (sigma * rng.normal()).exp();
    if rng.next_f64() < 0.5 { -size } else { size }
}

/// A chi-square draw with `df` degrees of freedom: twice a Gamma(df / 2).
fn chi_square(rng: &mut SplitMix64, df: f64) -> f64 {
    2.0 * gamma(rng, df / 2.0)
}

/// Gamma(shape, 1), Marsaglia and Tsang (2000).
fn gamma(rng: &mut SplitMix64, shape: f64) -> f64 {
    if shape < 1.0 {
        let u = rng.next_f64().max(f64::MIN_POSITIVE);
        return gamma(rng, shape + 1.0) * u.powf(1.0 / shape);
    }
    let d = shape - 1.0 / 3.0;
    let c = 1.0 / (9.0 * d).sqrt();
    loop {
        let x = rng.normal();
        let v = (1.0 + c * x).powi(3);
        if v <= 0.0 {
            continue;
        }
        let u = rng.next_f64().max(f64::MIN_POSITIVE);
        if u.ln() < 0.5 * x * x + d - d * v + d * v.ln() {
            return d * v;
        }
    }
}

#[cfg(test)]
mod tests {
    use mtx_core::mandate::Mandate;

    use super::*;
    use crate::market::tests::{sol, spcx, template};

    #[test]
    fn draws_are_deterministic_and_seeded() {
        let model = MarketModel::new(sol(), ModelSpec::default()).unwrap();
        let a = model.draw(7, 1200);
        let b = model.draw(7, 1200);
        let c = model.draw(8, 1200);
        assert_eq!(a.log_levels, b.log_levels);
        assert_eq!(a.record, b.record);
        assert_ne!(a.log_levels, c.log_levels);
        assert_eq!(a.log_levels.len(), 1201);
    }

    #[test]
    fn volatility_matches_the_calibration() {
        // With no clustering surprises, regimes or calendar, the slot sd is
        // the calibrated one times the regime level: the path's, before any
        // mark smooths it.
        let mut cal = sol();
        cal.fair_value.mark = None;
        let model = MarketModel::new(
            cal.clone(),
            ModelSpec {
                regime: Some("calm".into()),
                jumps: false,
                start: Start::At {
                    unix: 1_798_761_600 + 12 * 3600,
                },
                ..ModelSpec::default()
            },
        )
        .unwrap();
        let mut ratios = Vec::new();
        for seed in 0..200 {
            let path = model.draw(seed, 2400);
            let r: Vec<f64> = path.log_levels.windows(2).map(|w| w[1] - w[0]).collect();
            let sd = (r.iter().map(|x| x * x).sum::<f64>() / r.len() as f64).sqrt();
            let expected = cal.per_slot_sd(SLOT_SECONDS)
                * path.record.regime_vol[0]
                * cal.fair_value.intraday_vol_mult_utc.as_ref().unwrap()[12];
            ratios.push(sd / expected);
        }
        let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
        assert!(
            (0.9..1.1).contains(&mean),
            "slot sd over calibrated: {mean}"
        );
    }

    #[test]
    fn vol_mult_scales_volatility() {
        let spec = |vol_mult| ModelSpec {
            vol_mult,
            jumps: false,
            ..ModelSpec::default()
        };
        let calm = MarketModel::new(sol(), spec(1.0)).unwrap().draw(3, 800);
        let hot = MarketModel::new(sol(), spec(2.0)).unwrap().draw(3, 800);
        let last = |p: &ModelPath| *p.log_levels.last().unwrap();
        assert!((last(&hot) - 2.0 * last(&calm)).abs() < 1e-12);
    }

    #[test]
    fn the_open_gaps_and_is_recorded() {
        let model = MarketModel::new(
            spcx(),
            ModelSpec {
                start: Start::Before {
                    kind: "regular".into(),
                    lead_seconds: 60.0,
                },
                ..ModelSpec::default()
            },
        )
        .unwrap();
        for seed in 0..20 {
            let path = model.draw(seed, 480);
            let rec = &path.record;
            let gap = rec
                .events
                .iter()
                .find(|e| e.kind == EventKind::Gap)
                .expect("a gap at the open");
            // 60 s at 0.25 s a slot.
            assert_eq!(gap.slot, 240);
            assert_eq!(rec.sessions.last().unwrap().name, "regular");
            assert_eq!(rec.sessions.last().unwrap().slot, 240);
            assert_eq!(rec.sessions[0].name, "pre");
            let jump = path.log_levels[240] - path.log_levels[239];
            assert!(jump.abs() > gap.pct.abs() / 100.0 * 0.5);
        }
    }

    #[test]
    fn closed_hours_are_quieter() {
        let spec = |kind: &str| ModelSpec {
            start: Start::Session { kind: kind.into() },
            jumps: false,
            gaps: false,
            ..ModelSpec::default()
        };
        let sd = |model: &MarketModel| {
            let mut total = 0.0;
            for seed in 0..100 {
                let p = model.draw(seed, 1200);
                total += p
                    .log_levels
                    .windows(2)
                    .map(|w| (w[1] - w[0]).powi(2))
                    .sum::<f64>();
            }
            total.sqrt()
        };
        let open = sd(&MarketModel::new(spcx(), spec("regular")).unwrap());
        let night = sd(&MarketModel::new(spcx(), spec("overnight")).unwrap());
        let ratio = night / open;
        let expected = spcx().calendar.closed_vol_mult.unwrap();
        assert!(
            (ratio / expected - 1.0).abs() < 0.15,
            "{ratio} vs {expected}"
        );
    }

    #[test]
    fn regimes_switch_and_are_recorded() {
        let model = MarketModel::new(sol(), ModelSpec::default()).unwrap();
        let mut switched = 0;
        let mut stressed_start = 0;
        for seed in 0..400 {
            let rec = model.draw(seed, 14_400).record;
            switched += usize::from(rec.regimes.len() > 1);
            stressed_start += usize::from(rec.regimes[0].name == "stressed");
        }
        // An hour-long session leaves calm about 10% of the time.
        assert!((10..120).contains(&switched), "{switched}");
        // About 21% start stressed.
        assert!((50..130).contains(&stressed_start), "{stressed_start}");
    }

    /// Without departures a record says nothing about them, so scenarios
    /// drawn before the regime clock serialise as they did.
    #[test]
    fn a_record_without_departures_names_none() {
        let model = MarketModel::new(spcx(), ModelSpec::default()).unwrap();
        let path = model.draw(3, 1_600);
        assert_eq!(path.record.regime_clock, 1.0);
        let text = serde_json::to_string(&path.record).unwrap();
        assert!(!text.contains("regime_clock") && !text.contains("regime_set"));
    }

    /// The two-state chain card 8 states for SPCX: SPCX's one fitted state
    /// split into a calm and a stressed one.
    fn two_states() -> Regimes {
        Regimes {
            model: crate::market::calibration::RegimeModel::Markov,
            names: vec!["calm".into(), "stressed".into()],
            vol_mult: vec![0.85, 1.7],
            log_sd_within: vec![0.4, 0.4],
            share: vec![0.7, 0.3],
            transition_per_hour: vec![vec![0.9, 0.1], vec![0.25, 0.75]],
            source: "test".into(),
        }
    }

    #[test]
    fn a_stated_regime_set_replaces_the_calibrations_and_is_recorded() {
        let spec = ModelSpec {
            regimes: Some(two_states()),
            ..ModelSpec::default()
        };
        let model = MarketModel::new(spcx(), spec).unwrap();
        assert_eq!(model.calibration.fair_value.regimes, two_states());
        let mut names = std::collections::BTreeSet::new();
        for seed in 0..200 {
            let rec = model.draw(seed, 1_600).record;
            assert_eq!(rec.regime_set.as_ref(), Some(&two_states()));
            assert_eq!(rec.regime_vol.len(), 2);
            names.extend(rec.regimes.iter().map(|m| m.name.clone()));
        }
        assert!(names.contains("calm") && names.contains("stressed"));

        let mut bad = two_states();
        bad.share = vec![0.5, 0.2];
        let refused = MarketModel::new(
            spcx(),
            ModelSpec {
                regimes: Some(bad),
                ..ModelSpec::default()
            },
        );
        assert!(refused.unwrap_err().contains("regimes.share"));
    }

    /// The clock scales how often a session switches, and refuses a clock
    /// that would leave a state more than half the blocks.
    #[test]
    fn the_regime_clock_scales_switching() {
        let switches = |clock: f64| -> usize {
            let spec = ModelSpec {
                regimes: Some(two_states()),
                regime_clock: clock,
                ..ModelSpec::default()
            };
            let model = MarketModel::new(spcx(), spec).unwrap();
            (0..300)
                .map(|seed| model.draw(seed, 1_600).record.regimes.len() - 1)
                .sum()
        };
        let (slow, fast) = (switches(1.0), switches(12.0));
        assert!(fast > 5 * slow.max(1), "{slow} switches at 1, {fast} at 12");
        let spec = ModelSpec {
            regimes: Some(two_states()),
            regime_clock: 200.0,
            ..ModelSpec::default()
        };
        let error = MarketModel::new(spcx(), spec).unwrap_err();
        assert!(error.contains("half the time"), "{error}");
        let spec = ModelSpec {
            regime_clock: 0.0,
            ..ModelSpec::default()
        };
        assert!(MarketModel::new(spcx(), spec).is_err());
    }
    #[test]
    fn waypoints_fit_phoenix_moves() {
        let model = MarketModel::new(sol(), ModelSpec::default()).unwrap();
        let path = model.draw(1, 1000);
        let points = path.waypoints(4);
        assert!(points.len() > 100);
        let mut free = 0;
        let mut last = 0.0;
        for w in &points {
            assert!(w.slot >= free && w.slot % 4 == 0);
            free = w.slot + move_slots(last, w.level_pct) + 1;
            last = w.level_pct;
            assert!((w.level_pct - round(path.level_pct(w.slot))).abs() < 1e-12);
        }
        assert!(free <= 1000);
    }

    #[test]
    fn scenarios_carry_the_path_and_the_record() {
        let model = MarketModel::new(sol(), ModelSpec::default()).unwrap();
        let template = crate::market::tests::template();
        let set = model.sample_set(&template, "cal", 3, 11).unwrap();
        assert_eq!(set.len(), 3);
        for s in &set {
            s.validate().unwrap();
            assert_eq!(s.fair_path.as_ref(), Some(&s.path));
            let rec = s.market_model.as_ref().unwrap();
            assert_eq!(rec.calibration, model.calibration.meta.calibration_id);
            assert_eq!(s.mandate, template.mandate);
        }
        assert_eq!(set[0].name, "cal-1");
        let json = serde_json::to_string(&set[0]).unwrap();
        let back: Scenario = serde_json::from_str(&json).unwrap();
        assert_eq!(back, set[0]);
    }

    #[test]
    fn joint_draws_are_correlated() {
        let a = MarketModel::new(
            sol(),
            ModelSpec {
                jumps: false,
                ..ModelSpec::default()
            },
        )
        .unwrap();
        let b = a.clone();
        let rho = 0.7;
        let corr = vec![vec![1.0, rho], vec![rho, 1.0]];
        // Block returns' signs are the correlated normals' signs (each
        // asset's t divisor is positive), so their concordance is
        // 1/2 + asin(rho) / pi whatever the tails: about 0.747 at 0.7.
        let block = 40;
        let (mut same, mut total) = (0, 0);
        for seed in 0..60 {
            let paths = draw_joint(&[&a, &b], &corr, seed, 4000).unwrap();
            assert_eq!(paths[0].record.start_unix, paths[1].record.start_unix);
            let ret =
                |p: &ModelPath, i: usize| p.log_levels[(i + 1) * block] - p.log_levels[i * block];
            for i in 0..4000 / block {
                same += usize::from((ret(&paths[0], i) > 0.0) == (ret(&paths[1], i) > 0.0));
                total += 1;
            }
        }
        let concordance = same as f64 / total as f64;
        let expected = 0.5 + rho.asin() / std::f64::consts::PI;
        assert!(
            (concordance - expected).abs() < 0.03,
            "sign concordance {concordance}, expected {expected}"
        );
        assert!(draw_joint(&[&a, &b], &[vec![1.0, 1.2], vec![1.2, 1.0]], 1, 100).is_err());
        let spcx = MarketModel::new(spcx(), ModelSpec::default()).unwrap();
        assert!(draw_joint(&[&a, &spcx], &corr, 1, 100).is_err());
    }

    #[test]
    fn a_joint_draw_fills_each_market_and_records_the_draw() {
        let model = MarketModel::new(spcx(), ModelSpec::default()).unwrap();
        let mut template = template();
        template.symbol = "NVDA".into();
        template.mandate = Mandate::perp(crate::scenario::VENUE, "NVDA", 40.0, 1_000.0);
        template
            .mandate
            .universe
            .extend(Mandate::perp(crate::scenario::VENUE, "SPCX", 70.0, 1_000.0).universe);
        template.markets = ["NVDA", "SPCX"]
            .into_iter()
            .map(|symbol| crate::scenario::MarketScenario {
                symbol: symbol.into(),
                path: Vec::new(),
                fair_path: None,
                funding: None,
                making: None,
                tape: Vec::new(),
                takers_pricing: None,
                market_model: None,
            })
            .collect();
        let corr = vec![vec![1.0, 0.6], vec![0.6, 1.0]];
        let models = [("SPCX", &model), ("NVDA", &model)];
        let set = joint_sample_set(&models, &corr, "design", &template, "j", 3, 9).unwrap();
        let again = joint_sample_set(&models, &corr, "design", &template, "j", 3, 9).unwrap();
        assert_eq!(set, again);
        for s in &set {
            s.validate().unwrap();
            let joint = s.joint.as_ref().unwrap();
            assert_eq!(joint.assets, ["SPCX", "NVDA"]);
            assert_eq!(joint.matrix_source, "design");
            assert_eq!(
                joint.records["NVDA"].start_unix,
                joint.records["SPCX"].start_unix
            );
            for market in &s.markets {
                assert!(!market.path.is_empty());
                assert_eq!(market.fair_path.as_ref(), Some(&market.path));
            }
            assert_ne!(s.markets[0].path, s.markets[1].path);
            // The v2 file says it and reads back the same.
            let v2 = crate::scenario_v2::ScenarioV2::try_from(s).unwrap();
            let text = serde_json::to_string(&v2).unwrap();
            assert_eq!(&crate::scenario_v2::parse(&text).unwrap(), s);
        }
        assert!(joint_scenario(&models[..1], &corr[..1], "design", &template, "x", 1).is_err());
    }

    #[test]
    fn student_t_has_unit_variance_and_fat_tails() {
        let mut rng = SplitMix64(5);
        let df = 5.0;
        let xs: Vec<f64> = (0..200_000)
            .map(|_| {
                rng.normal() / (chi_square(&mut rng, df) / df).sqrt() * ((df - 2.0) / df).sqrt()
            })
            .collect();
        let var = xs.iter().map(|x| x * x).sum::<f64>() / xs.len() as f64;
        let kurt = xs.iter().map(|x| x.powi(4)).sum::<f64>() / xs.len() as f64 / (var * var) - 3.0;
        assert!((var - 1.0).abs() < 0.03, "{var}");
        // 6 / (df - 4) = 6, noisy at this df.
        assert!(kurt > 3.0, "{kurt}");
    }

    #[test]
    fn bad_specs_fail() {
        let bad = |spec: ModelSpec| MarketModel::new(sol(), spec).is_err();
        assert!(bad(ModelSpec {
            vol_mult: 0.0,
            ..ModelSpec::default()
        }));
        assert!(bad(ModelSpec {
            regime: Some("panic".into()),
            ..ModelSpec::default()
        }));
        assert!(bad(ModelSpec {
            start: Start::Session {
                kind: "regular".into()
            },
            ..ModelSpec::default()
        }));
        assert!(bad(ModelSpec {
            waypoint_slots: 2,
            ..ModelSpec::default()
        }));
    }
}
