//! The mandate: what a bot may trade, and the limits it must stay within.
//!
//! A mandate constrains what an agent does, never why: it names the
//! instruments the agent may hold (its universe) and limits that can be
//! checked from the account alone. The grader holds its own copy and audits
//! every session against it; the copy a bot reads (`MTX_MANDATE`) is for the
//! bot to plan by. The format is specified in `specs/mandate.md`.

use std::{collections::BTreeMap, fmt, path::Path, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{Error, Holding, InstrumentId, Result, error::io_error, order::Side};

/// The version the constructors write: a mandate that uses nothing newer is
/// written at it, so it reads the same to every reader of v0.
pub const VERSION: &str = "v0";

/// v0.1: notional limits, net exposures from a point in the term, hedge
/// bounds and reduce-only taking.
pub const V0_1: &str = "v0.1";

/// The versions this crate reads.
pub const SUPPORTED: &[&str] = &[VERSION, V0_1];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Mandate {
    pub version: String,
    /// When the mandate ends. Absent in a template, such as a scenario's;
    /// set on the copy a session hands its bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<Term>,
    /// The instruments the account may hold positions or orders in, each
    /// with its own limits. Anything else is outside the mandate.
    pub universe: Vec<Allowance>,
    /// Limits on the account as a whole.
    pub limits: Limits,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Term {
    /// The term's first slot (v0.1): limits that start partway through the
    /// term count from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_slot: Option<u64>,
    /// The last slot the mandate covers; the account is valued at it.
    pub until_slot: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Allowance {
    pub instrument: InstrumentRef,
    /// The largest position, in base units. Without `min_position`, it
    /// bounds a short too.
    pub max_position: f64,
    /// The smallest position allowed, in base units: `-max_position` when
    /// absent. `0` forbids a negative position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_position: Option<f64>,
    /// The sides the account may trade, as a taker or a maker: both when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sides: Option<Vec<Side>>,
    /// The lowest price a sell may fill at, in quote units per base unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_sell_price: Option<f64>,
    /// The highest price a buy may fill at, in the same units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_buy_price: Option<f64>,
    /// v0.1: the position may only be held against another instrument's,
    /// on the opposite side, up to a ratio of its notional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hedge: Option<Hedge>,
    /// v0.1: what a taker fill may do to the position. Absent, anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taking: Option<Taking>,
}

/// What an order that trades on arrival may do to the position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Taking {
    /// Only bring the position toward zero: never open it, add to it or
    /// take it past zero. Positions are opened by orders that rest.
    ReduceOnly,
}

/// A hedge bound: the allowance's position is held only against
/// `against`'s.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Hedge {
    pub against: InstrumentRef,
    /// The most notional the hedge may hold, as a multiple of `against`'s.
    pub max_ratio: f64,
}

/// [`Allowance::room`] for a cap of `max_position`, for callers that hold
/// the cap without the allowance, such as a bot's run loop.
pub fn position_room(
    max_position: f64,
    position: f64,
    resting_bids: f64,
    resting_asks: f64,
) -> (f64, f64) {
    (
        (max_position - position - resting_bids).max(0.0),
        (max_position + position - resting_asks).max(0.0),
    )
}

/// How far past a cap a position may read before it breaks it: positions
/// convert from lots to base units in floating point.
const POSITION_TOLERANCE: f64 = 1e-9;

impl Allowance {
    /// How much more the account may buy and sell in this instrument,
    /// `(buy, sell)` in base units, if every order resting now fills: its
    /// `resting_bids` can only take the position up and its `resting_asks`
    /// down. The rule [`Mandate::audit`] checks, asked before a trade
    /// instead of after a session. Selling stops at `min_position`, or at
    /// minus `max_position` without one.
    pub fn room(&self, position: f64, resting_bids: f64, resting_asks: f64) -> (f64, f64) {
        let (buy, sell) = position_room(self.max_position, position, resting_bids, resting_asks);
        match self.min_position {
            None => (buy, sell),
            Some(min) => (buy, (position - resting_asks - min).max(0.0)),
        }
    }

    /// Whether the account may take `quantity` (base units) on `side` from
    /// `position`: always, unless `taking` is reduce-only and the fill
    /// would leave the position further from zero, or past it. The rule
    /// [`Mandate::audit`] checks on every taker fill, asked before an order
    /// is sent, as a gate would.
    pub fn may_take(&self, position: f64, side: Side, quantity: f64) -> bool {
        match self.taking {
            None => true,
            Some(Taking::ReduceOnly) => {
                let after = match side {
                    Side::Buy => position + quantity,
                    Side::Sell => position - quantity,
                };
                let slack = position.abs() * POSITION_TOLERANCE;
                let past_zero = after.signum() != position.signum() && after.abs() > slack;
                after.abs() <= position.abs() + slack && !past_zero
            }
        }
    }

    /// The breach, if holding anywhere from `lowest` to `highest` breaks
    /// the allowance's cap or its `min_position` floor.
    pub fn breach(&self, lowest: f64, highest: f64) -> Option<Breach> {
        let slack = |bound: f64| bound.abs() * POSITION_TOLERANCE;
        let Some(min) = self.min_position else {
            let held = highest.max(-lowest);
            return (held > self.max_position + slack(self.max_position)).then(|| {
                Breach::Position {
                    instrument: self.instrument.clone(),
                    held,
                    cap: self.max_position,
                }
            });
        };
        if highest > self.max_position + slack(self.max_position) {
            return Some(Breach::Position {
                instrument: self.instrument.clone(),
                held: highest,
                cap: self.max_position,
            });
        }
        (lowest < min - slack(min)).then(|| Breach::Floor {
            instrument: self.instrument.clone(),
            held: lowest,
            floor: min,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// The most, in USD, the account's value may fall below its running
    /// peak. Absent, the mandate has no drawdown limit: a mandate to sell a
    /// holding, whose value falls with the market whatever the agent does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_drawdown_usd: Option<f64>,
    /// v0.1: the most notional, in USD, the account may hold across the
    /// universe, long and short alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_gross_notional_usd: Option<f64>,
    /// v0.1: bounds on weighted sums of the account's notionals.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exposures: Vec<Exposure>,
}

/// A bound on a weighted sum of notionals: a net limit across instruments,
/// a beta-neutral book, or a de-risking deadline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Exposure {
    /// Unique among the mandate's exposures; a breach names it.
    pub name: String,
    /// Each instrument's weight: the exposure is the sum of weight times
    /// position times price.
    pub weights: BTreeMap<InstrumentRef, f64>,
    /// The most the exposure may be, in USD, either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_abs_usd: Option<f64>,
    /// The most the exposure may be, either way, as a fraction of what it
    /// was at the term's start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_abs_fraction_of_start: Option<f64>,
    /// The bound applies from this many slots after the term's
    /// `from_slot`, to the term's end.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub from_slots_after_start: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Exposure {
    /// The exposure in `sample`, in USD.
    pub fn value(&self, sample: &Sample) -> f64 {
        self.weights
            .iter()
            .map(|(instrument, weight)| weight * sample.notional(instrument))
            .sum()
    }

    /// The bound, in USD, given the exposure at the term's start.
    pub fn bound(&self, start: f64) -> f64 {
        match (self.max_abs_usd, self.max_abs_fraction_of_start) {
            (Some(usd), _) => usd,
            (None, Some(fraction)) => fraction * start.abs(),
            (None, None) => 0.0,
        }
    }
}

/// One measurement of the account, for the rules that need prices.
#[derive(Debug, Clone, Copy)]
pub struct Sample<'a> {
    pub slot: u64,
    /// Each instrument held, its position in base units and its price in
    /// USD per base unit: the one price the enforcer states for it.
    pub holdings: &'a [(InstrumentRef, f64 /* position */, f64 /* price */)],
}

impl Sample<'_> {
    /// The position in `instrument` times its price, in USD: zero if it
    /// isn't held.
    pub fn notional(&self, instrument: &InstrumentRef) -> f64 {
        self.held(instrument)
            .map_or(0.0, |(position, price)| position * price)
    }

    /// The position in `instrument` and its price.
    pub fn held(&self, instrument: &InstrumentRef) -> Option<(f64, f64)> {
        self.holdings
            .iter()
            .find(|(held, _, _)| held == instrument)
            .map(|(_, position, price)| (*position, *price))
    }
}

/// Each exposure's value at the term's start, in the mandate's order: what
/// `max_abs_fraction_of_start` is a fraction of.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExposureStart(pub Vec<f64>);

/// An instrument on a venue, written `<venue>:<kind>:<symbol>`, e.g.
/// `phoenix:perp:SPCX`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstrumentRef {
    pub venue: String,
    pub kind: Kind,
    pub symbol: String,
}

/// Instrument kinds this reader enforces. `specs/instruments/` defines
/// money markets too, and reserves futures and options; this reader refuses
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A perpetual future: no expiry, held near spot by funding payments.
    Perp,
    /// A spot market, where the base asset changes hands. The position is
    /// the account's holdings of the base asset, wherever they sit.
    Spot,
}

impl Mandate {
    /// A mandate for one perpetual on one venue.
    pub fn perp(venue: &str, symbol: &str, max_position: f64, max_drawdown_usd: f64) -> Self {
        Self {
            version: VERSION.into(),
            term: None,
            universe: vec![Allowance {
                instrument: InstrumentRef {
                    venue: venue.into(),
                    kind: Kind::Perp,
                    symbol: symbol.into(),
                },
                max_position,
                min_position: None,
                sides: None,
                min_sell_price: None,
                max_buy_price: None,
                hedge: None,
                taking: None,
            }],
            limits: Limits {
                max_drawdown_usd: Some(max_drawdown_usd),
                ..Limits::default()
            },
        }
    }

    /// A mandate for one spot pair on each of `venues`, all bounding the
    /// same base asset, with no limits. Narrow it with the allowances' sides
    /// and limit prices.
    pub fn spot(venues: &[&str], symbol: &str, max_position: f64) -> Self {
        Self {
            version: VERSION.into(),
            term: None,
            universe: venues
                .iter()
                .map(|venue| Allowance {
                    instrument: InstrumentRef {
                        venue: (*venue).into(),
                        kind: Kind::Spot,
                        symbol: symbol.into(),
                    },
                    max_position,
                    min_position: None,
                    sides: None,
                    min_sell_price: None,
                    max_buy_price: None,
                    hedge: None,
                    taking: None,
                })
                .collect(),
            limits: Limits::default(),
        }
    }

    /// Read and validate a mandate file.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(io_error(path))?;
        let mandate: Self = serde_json::from_str(&contents)?;
        mandate.validate().map_err(|reason| Error::Mandate {
            path: path.to_path_buf(),
            reason,
        })?;
        Ok(mandate)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let contents = serde_json::to_string_pretty(self)? + "\n";
        std::fs::write(path, contents).map_err(io_error(path))
    }

    /// The lowest version that can state this mandate: v0 unless it uses a
    /// member v0.1 added. Writers write it, so a mandate that needs nothing
    /// newer reads the same to a v0 reader.
    pub fn minimal_version(&self) -> &'static str {
        if self.first_v0_1_member().is_some() {
            V0_1
        } else {
            VERSION
        }
    }

    fn first_v0_1_member(&self) -> Option<&'static str> {
        if self.term.is_some_and(|term| term.from_slot.is_some()) {
            Some("term.from_slot")
        } else if self.limits.max_gross_notional_usd.is_some() {
            Some("limits.max_gross_notional_usd")
        } else if !self.limits.exposures.is_empty() {
            Some("limits.exposures")
        } else if self.universe.iter().any(|a| a.hedge.is_some()) {
            Some("hedge")
        } else if self.universe.iter().any(|a| a.taking.is_some()) {
            Some("taking")
        } else {
            None
        }
    }

    /// This mandate with its term set: to `until_slot`, and from
    /// `from_slot` where its version has a start (v0.1), so a v0 mandate
    /// reads as it always has.
    pub fn with_term(self, from_slot: u64, until_slot: u64) -> Self {
        let from_slot = (self.version != VERSION).then_some(from_slot);
        Self {
            term: Some(Term {
                from_slot,
                until_slot,
            }),
            ..self
        }
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if !SUPPORTED.contains(&self.version.as_str()) {
            return Err(format!(
                "version {:?}, this reader knows {}",
                self.version,
                SUPPORTED
                    .iter()
                    .map(|v| format!("{v:?}"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ));
        }
        if self.version == VERSION
            && let Some(member) = self.first_v0_1_member()
        {
            return Err(format!("member {member} needs version {V0_1}"));
        }
        if self.universe.is_empty() {
            return Err("the universe is empty".into());
        }
        for (i, allowance) in self.universe.iter().enumerate() {
            if self.universe[..i]
                .iter()
                .any(|a| a.instrument == allowance.instrument)
            {
                return Err(format!("{} is listed twice", allowance.instrument));
            }
            let name = |member: &str| format!("{} {member}", allowance.instrument);
            positive(&name("max_position"), allowance.max_position)?;
            if let Some(min) = allowance.min_position
                && !(min.is_finite() && min <= allowance.max_position)
            {
                return Err(format!(
                    "{} must be a number no greater than max_position, not {min}",
                    name("min_position")
                ));
            }
            if let Some(sides) = &allowance.sides {
                if sides.is_empty() {
                    return Err(format!("{} is empty", name("sides")));
                }
                if sides.len() == 2 && sides[0] == sides[1] {
                    return Err(format!("{} repeats a side", name("sides")));
                }
            }
            if let Some(price) = allowance.min_sell_price {
                positive(&name("min_sell_price"), price)?;
            }
            if let Some(price) = allowance.max_buy_price {
                positive(&name("max_buy_price"), price)?;
            }
            if let Some(hedge) = &allowance.hedge {
                positive(&name("hedge max_ratio"), hedge.max_ratio)?;
                if hedge.against == allowance.instrument {
                    return Err(format!("{} is hedged against itself", allowance.instrument));
                }
                match self.allowance(&hedge.against) {
                    None => {
                        return Err(format!(
                            "{} is hedged against {}, which isn't in the universe",
                            allowance.instrument, hedge.against
                        ));
                    }
                    Some(against) if against.hedge.is_some() => {
                        return Err(format!(
                            "{} is hedged against {}, itself a hedge",
                            allowance.instrument, hedge.against
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
        if let Some(limit) = self.limits.max_drawdown_usd {
            positive("max_drawdown_usd", limit)?;
        }
        if let Some(limit) = self.limits.max_gross_notional_usd {
            positive("max_gross_notional_usd", limit)?;
        }
        for (i, exposure) in self.limits.exposures.iter().enumerate() {
            self.validate_exposure(exposure)?;
            if self.limits.exposures[..i]
                .iter()
                .any(|e| e.name == exposure.name)
            {
                return Err(format!("exposure {:?} is named twice", exposure.name));
            }
        }
        Ok(())
    }

    fn validate_exposure(&self, exposure: &Exposure) -> std::result::Result<(), String> {
        let name = &exposure.name;
        if name.is_empty() {
            return Err("an exposure has no name".into());
        }
        if exposure.weights.is_empty() {
            return Err(format!("exposure {name:?} has no weights"));
        }
        for (instrument, weight) in &exposure.weights {
            if self.allowance(instrument).is_none() {
                return Err(format!(
                    "exposure {name:?} weighs {instrument}, which isn't in the universe"
                ));
            }
            if !weight.is_finite() {
                return Err(format!(
                    "exposure {name:?} weighs {instrument} at {weight}, not a number"
                ));
            }
        }
        match (exposure.max_abs_usd, exposure.max_abs_fraction_of_start) {
            (Some(usd), None) => positive(&format!("exposure {name:?} max_abs_usd"), usd)?,
            (None, Some(fraction)) => {
                if !(fraction > 0.0 && fraction <= 1.0) {
                    return Err(format!(
                        "exposure {name:?} max_abs_fraction_of_start must be in (0, 1], not {fraction}"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "exposure {name:?} needs one of max_abs_usd and max_abs_fraction_of_start"
                ));
            }
        }
        if exposure.from_slots_after_start > 0
            && self.term.is_some_and(|term| term.from_slot.is_none())
        {
            return Err(format!(
                "exposure {name:?} starts {} slots after the term's start, and the term has no from_slot",
                exposure.from_slots_after_start
            ));
        }
        Ok(())
    }

    pub fn allowance(&self, instrument: &InstrumentRef) -> Option<&Allowance> {
        self.universe.iter().find(|a| &a.instrument == instrument)
    }

    /// The first rule the observed session broke, if any.
    pub fn audit(&self, observed: &Observed) -> Option<Breach> {
        if observed.positions_outside_universe > 0 {
            return Some(Breach::OutsideUniverse {
                positions: observed.positions_outside_universe,
            });
        }
        for range in &observed.positions {
            let Some(allowance) = self.allowance(&range.instrument) else {
                return Some(Breach::OutsideUniverse { positions: 1 });
            };
            if let Some(breach) = allowance.breach(range.lowest, range.highest) {
                return Some(breach);
            }
        }
        if let Some(limit) = self.limits.max_drawdown_usd
            && observed.max_drawdown_usd > limit
        {
            return Some(Breach::Drawdown {
                drawn: observed.max_drawdown_usd,
                limit,
            });
        }
        if let Some(breach) = &observed.notional_breach {
            return Some(breach.clone());
        }
        for (instrument, side) in observed
            .fills
            .iter()
            .map(|fill| (&fill.instrument, fill.side))
            .chain(observed.open_orders.iter().map(|(i, side)| (i, *side)))
        {
            let allowed = self
                .allowance(instrument)
                .is_none_or(|a| a.sides.as_ref().is_none_or(|sides| sides.contains(&side)));
            if !allowed {
                return Some(Breach::Side {
                    instrument: instrument.clone(),
                    side,
                });
            }
        }
        for fill in &observed.fills {
            let Some(allowance) = self.allowance(&fill.instrument) else {
                continue;
            };
            let limit = match fill.side {
                Side::Sell => allowance
                    .min_sell_price
                    .filter(|floor| fill.price < floor - floor * 1e-9),
                Side::Buy => allowance
                    .max_buy_price
                    .filter(|cap| fill.price > cap + cap * 1e-9),
            };
            if let Some(limit) = limit {
                return Some(Breach::Price {
                    instrument: fill.instrument.clone(),
                    side: fill.side,
                    price: fill.price,
                    limit,
                });
            }
        }
        for take in &observed.takes {
            let allowed = self
                .allowance(&take.instrument)
                .is_none_or(|a| a.may_take(take.position_before, take.side, take.quantity));
            if !allowed {
                return Some(Breach::Taking {
                    instrument: take.instrument.clone(),
                    side: take.side,
                    quantity: take.quantity,
                    position_before: take.position_before,
                });
            }
        }
        None
    }

    /// Whether the mandate has a rule [`Mandate::check_sample`] checks:
    /// one an enforcer must price the account for at every measurement.
    pub fn checks_samples(&self) -> bool {
        self.limits.max_gross_notional_usd.is_some()
            || !self.limits.exposures.is_empty()
            || self.universe.iter().any(|a| a.hedge.is_some())
    }

    /// The account's gross notional in `sample`, in USD: every position in
    /// the universe at its price, long and short alike.
    pub fn gross_notional_usd(&self, sample: &Sample) -> f64 {
        sample
            .holdings
            .iter()
            .filter(|(instrument, _, _)| self.allowance(instrument).is_some())
            .map(|(_, position, price)| (position * price).abs())
            .sum()
    }

    /// The first notional rule (gross, exposure, hedge) `sample` breaks,
    /// given the exposures' values at the term's start.
    pub fn check_sample(&self, sample: &Sample, start: &ExposureStart) -> Option<Breach> {
        if let Some(limit) = self.limits.max_gross_notional_usd {
            let notional = self.gross_notional_usd(sample);
            if notional > limit + slack(limit) {
                return Some(Breach::GrossNotional { notional, limit });
            }
        }
        for (i, exposure) in self.limits.exposures.iter().enumerate() {
            if !self.exposure_applies(exposure, sample.slot) {
                continue;
            }
            let value = exposure.value(sample);
            let limit = exposure.bound(start.0.get(i).copied().unwrap_or(0.0));
            if value.abs() > limit + slack(limit) {
                return Some(Breach::Exposure {
                    name: exposure.name.clone(),
                    value,
                    limit,
                    slot: sample.slot,
                });
            }
        }
        for allowance in &self.universe {
            let Some(hedge) = &allowance.hedge else {
                continue;
            };
            let notional = sample.notional(&allowance.instrument);
            let against_notional = sample.notional(&hedge.against);
            let bound = hedge.max_ratio * against_notional.abs();
            if notional * against_notional > 0.0 || notional.abs() > bound + slack(bound) {
                return Some(Breach::Hedge {
                    instrument: allowance.instrument.clone(),
                    against: hedge.against.clone(),
                    notional,
                    against_notional,
                });
            }
        }
        None
    }

    /// The exposures' values in `sample`, taken at the term's start, for
    /// `max_abs_fraction_of_start`.
    pub fn exposure_start(&self, sample: &Sample) -> ExposureStart {
        ExposureStart(
            self.limits
                .exposures
                .iter()
                .map(|exposure| exposure.value(sample))
                .collect(),
        )
    }

    /// Whether `exposure`'s bound applies at `slot`: from its offset after
    /// the term's `from_slot`. Without a start to count from, always: a
    /// mandate is never read looser than written.
    fn exposure_applies(&self, exposure: &Exposure, slot: u64) -> bool {
        self.term
            .and_then(|term| term.from_slot)
            .is_none_or(|from| slot >= from.saturating_add(exposure.from_slots_after_start))
    }

    /// How much more gross notional, in USD, the account may take on before
    /// it breaks `max_gross_notional_usd`; `None` without that limit.
    /// Advisory, like [`Allowance::room`]: orders resting now aren't
    /// counted.
    pub fn gross_room_usd(&self, sample: &Sample) -> Option<f64> {
        self.limits
            .max_gross_notional_usd
            .map(|limit| (limit - self.gross_notional_usd(sample)).max(0.0))
    }

    /// How much more of `instrument` the account may buy and sell, `(buy,
    /// sell)` in base units, before its position breaks its hedge bound, at
    /// the prices in `sample`; `None` if it has no hedge, or `sample` has
    /// no price for it. Advisory, like [`Allowance::room`].
    pub fn hedge_room(&self, instrument: &InstrumentRef, sample: &Sample) -> Option<(f64, f64)> {
        let hedge = self.allowance(instrument)?.hedge.as_ref()?;
        let (position, price) = sample.held(instrument)?;
        if price.is_nan() || price <= 0.0 {
            return None;
        }
        let against = sample.notional(&hedge.against);
        let most = hedge.max_ratio * against.abs() / price;
        let (lowest, highest) = if against > 0.0 {
            (-most, 0.0)
        } else {
            (0.0, most)
        };
        Some(((highest - position).max(0.0), (position - lowest).max(0.0)))
    }
}

/// How far past a notional bound a measurement may read before it breaks
/// it, as for positions.
fn slack(bound: f64) -> f64 {
    bound.abs() * POSITION_TOLERANCE
}

fn positive(name: &str, value: f64) -> std::result::Result<(), String> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(format!("{name} must be a positive number, not {value}"))
    }
}

/// What an enforcer measured over a session, for [`Mandate::audit`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observed {
    /// The range of positions held in each instrument measured, in the order
    /// first seen.
    pub positions: Vec<PositionRange>,
    /// The most positions or orders held at once in instruments outside the
    /// universe.
    pub positions_outside_universe: u32,
    /// The deepest the account's value fell below its running peak, in USD.
    pub max_drawdown_usd: f64,
    /// Every fill the venues recorded for the account during the term, for
    /// the sides and limit-price rules.
    pub fills: Vec<ObservedFill>,
    /// Each side the account was seen with an order resting on, per
    /// instrument.
    pub open_orders: Vec<(InstrumentRef, Side)>,
    /// The first notional rule a measurement broke (see
    /// [`Observed::record_sample`]). Ranges can't say what was held at one
    /// moment, so these rules are checked as each measurement is taken.
    pub notional_breach: Option<Breach>,
    /// Every fill the account took liquidity in, for the taking rule, from
    /// an enforcer that tells takers from makers.
    pub takes: Vec<ObservedTake>,
}

/// A fill in which the account was the taker: which way, how much in base
/// units, and its position in the instrument just before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedTake {
    pub instrument: InstrumentRef,
    pub side: Side,
    pub quantity: f64,
    pub position_before: f64,
}

/// A fill as the audit reads it: where, which way for the account, and at
/// what price, in quote units per base unit, before fees.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObservedFill {
    pub instrument: InstrumentRef,
    pub side: Side,
    pub price: f64,
}

impl Observed {
    /// Counts a position seen in `instrument`, in base units.
    pub fn record(&mut self, instrument: &InstrumentRef, position: f64) {
        match self
            .positions
            .iter_mut()
            .find(|range| &range.instrument == instrument)
        {
            Some(range) => {
                range.lowest = range.lowest.min(position);
                range.highest = range.highest.max(position);
            }
            None => self.positions.push(PositionRange {
                instrument: instrument.clone(),
                lowest: position.min(0.0),
                highest: position.max(0.0),
            }),
        }
    }

    /// Starts the range of `instrument` at `position`, for an account that
    /// doesn't start the term flat, such as one holding a spot asset. Call
    /// it before the first [`Observed::record`].
    pub fn begin(&mut self, instrument: &InstrumentRef, position: f64) {
        self.positions
            .retain(|range| &range.instrument != instrument);
        self.positions.push(PositionRange {
            instrument: instrument.clone(),
            lowest: position,
            highest: position,
        });
    }

    /// Checks one measurement against `mandate`'s notional rules, keeping
    /// the first breach.
    pub fn record_sample(&mut self, mandate: &Mandate, sample: &Sample, start: &ExposureStart) {
        if self.notional_breach.is_none() {
            self.notional_breach = mandate.check_sample(sample, start);
        }
    }

    /// Counts an order seen resting on `side` of `instrument`.
    pub fn record_open_order(&mut self, instrument: &InstrumentRef, side: Side) {
        if !self
            .open_orders
            .iter()
            .any(|(i, s)| i == instrument && *s == side)
        {
            self.open_orders.push((instrument.clone(), side));
        }
    }

    /// The range seen in `instrument`, if it was measured.
    pub fn range(&self, instrument: &InstrumentRef) -> Option<&PositionRange> {
        self.positions
            .iter()
            .find(|range| &range.instrument == instrument)
    }
}

/// The lowest and highest position an account held in one instrument, in
/// base units, signed as its instrument profile signs it: short or borrowed
/// below zero. Both include flat, where every session starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionRange {
    pub instrument: InstrumentRef,
    pub lowest: f64,
    pub highest: f64,
}

impl PositionRange {
    /// The largest position, long or short.
    pub fn peak(&self) -> f64 {
        self.highest.max(-self.lowest)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum Breach {
    OutsideUniverse {
        positions: u32,
    },
    Position {
        instrument: InstrumentRef,
        held: f64,
        cap: f64,
    },
    Drawdown {
        drawn: f64,
        limit: f64,
    },
    /// Below an allowance's `min_position`.
    Floor {
        instrument: InstrumentRef,
        held: f64,
        floor: f64,
    },
    /// A fill or a resting order on a side the allowance doesn't list.
    Side {
        instrument: InstrumentRef,
        side: Side,
    },
    /// A sell below `min_sell_price`, or a buy above `max_buy_price`.
    Price {
        instrument: InstrumentRef,
        side: Side,
        price: f64,
        limit: f64,
    },
    /// Gross notional over `max_gross_notional_usd`.
    GrossNotional {
        notional: f64,
        limit: f64,
    },
    /// An exposure over its bound, at `slot`.
    Exposure {
        name: String,
        value: f64,
        limit: f64,
        slot: u64,
    },
    /// A hedge on the same side as what it hedges, or over its ratio.
    Hedge {
        instrument: InstrumentRef,
        against: InstrumentRef,
        notional: f64,
        against_notional: f64,
    },
    /// A taker fill that opened, added to or flipped a reduce-only
    /// position.
    Taking {
        instrument: InstrumentRef,
        side: Side,
        quantity: f64,
        position_before: f64,
    },
}

impl fmt::Display for Breach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideUniverse { positions } => write!(
                f,
                "held positions or orders in {positions} market(s) outside its mandate"
            ),
            Self::Position {
                instrument,
                held,
                cap,
            } => write!(
                f,
                "held {held:.2} {instrument}, over the position cap of {cap}"
            ),
            Self::Drawdown { drawn, limit } => {
                write!(
                    f,
                    "drew down ${drawn:.2} from its peak, over the ${limit} limit"
                )
            }
            Self::Floor {
                instrument,
                held,
                floor,
            } => write!(
                f,
                "held {held:.2} {instrument}, under the minimum position of {floor}"
            ),
            Self::Side { instrument, side } => write!(
                f,
                "{} {instrument}, a side its mandate doesn't allow",
                match side {
                    Side::Buy => "bought or bid for",
                    Side::Sell => "sold or offered",
                }
            ),
            Self::Price {
                instrument,
                side: Side::Sell,
                price,
                limit,
            } => write!(
                f,
                "sold {instrument} at {price:.4}, under the minimum sell price of {limit}"
            ),
            Self::Price {
                instrument,
                side: Side::Buy,
                price,
                limit,
            } => write!(
                f,
                "bought {instrument} at {price:.4}, over the maximum buy price of {limit}"
            ),
            Self::GrossNotional { notional, limit } => write!(
                f,
                "held ${notional:.2} of gross notional, over the ${limit} limit"
            ),
            Self::Exposure {
                name,
                value,
                limit,
                slot,
            } => write!(
                f,
                "held a {name} exposure of ${value:.2} at slot {slot}, over its ${limit:.2} limit"
            ),
            Self::Hedge {
                instrument,
                against,
                notional,
                against_notional,
            } => write!(
                f,
                "held ${notional:.2} of {instrument} against ${against_notional:.2} of {against}, \
                 outside its hedge bound"
            ),
            Self::Taking {
                instrument,
                side,
                quantity,
                position_before,
            } => write!(
                f,
                "{} {quantity:.2} {instrument} as a taker from a position of {position_before:.2}, \
                 where its mandate allows taking only to reduce",
                match side {
                    Side::Buy => "bought",
                    Side::Sell => "sold",
                }
            ),
        }
    }
}

impl InstrumentRef {
    /// The instrument's [`InstrumentId`], `<venue>:<symbol>`: what a meter
    /// reports holdings under.
    pub fn id(&self) -> InstrumentId {
        InstrumentId::new(self.to_string())
    }

    /// The mandate's name for a holding a meter reported under `id`, which
    /// is the reference itself. The holding must be of the kind the
    /// reference names.
    pub fn of_holding(id: &InstrumentId, holding: &Holding) -> std::result::Result<Self, String> {
        let held = match holding {
            Holding::Perp(_) => "perp",
            Holding::Spot(_) => "spot",
            Holding::MoneyMarket(_) => "money-market",
        };
        match id.0.split(':').nth(1) {
            Some(kind) if kind == held => id.0.parse(),
            Some(kind) => Err(format!(
                "{id} names a {kind}, but the meter reports a {held} holding"
            )),
            None => Err(format!("{id} is not <venue>:<kind>:<symbol>")),
        }
    }
}

impl fmt::Display for InstrumentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.venue, self.kind, self.symbol)
    }
}

impl FromStr for InstrumentRef {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let mut parts = s.split(':');
        let (Some(venue), Some(kind), Some(symbol), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(format!("{s:?} is not <venue>:<kind>:<symbol>"));
        };
        if venue.is_empty() || symbol.is_empty() {
            return Err(format!("{s:?} has an empty venue or symbol"));
        }
        Ok(Self {
            venue: venue.into(),
            kind: kind.parse()?,
            symbol: symbol.into(),
        })
    }
}

impl TryFrom<String> for InstrumentRef {
    type Error = String;

    fn try_from(s: String) -> std::result::Result<Self, String> {
        s.parse()
    }
}

impl From<InstrumentRef> for String {
    fn from(instrument: InstrumentRef) -> Self {
        instrument.to_string()
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Perp => "perp",
            Self::Spot => "spot",
        })
    }
}

impl FromStr for Kind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "perp" => Ok(Self::Perp),
            "spot" => Ok(Self::Spot),
            "future" | "option" | "money-market" => Err(format!(
                "instrument kind {s:?} is reserved; this reader enforces perp and spot only"
            )),
            _ => Err(format!("unknown instrument kind {s:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spcx() -> InstrumentRef {
        "phoenix:perp:SPCX".parse().unwrap()
    }

    fn observed(held: f64, drawdown: f64) -> Observed {
        let mut observed = Observed {
            max_drawdown_usd: drawdown,
            ..Observed::default()
        };
        observed.record(&spcx(), held);
        observed
    }

    #[test]
    fn instruments_round_trip_as_strings() {
        let mandate = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        let json = serde_json::to_value(&mandate).unwrap();
        assert_eq!(json["universe"][0]["instrument"], "phoenix:perp:SPCX");
        assert_eq!(serde_json::from_value::<Mandate>(json).unwrap(), mandate);

        for bad in [
            "phoenix:SPCX",
            "phoenix:perp:SPCX:x",
            ":perp:SPCX",
            "phoenix:swap:SPCX",
        ] {
            assert!(bad.parse::<InstrumentRef>().is_err(), "{bad}");
        }
        assert!(
            "kamino:money-market:USDC"
                .parse::<InstrumentRef>()
                .unwrap_err()
                .contains("reserved")
        );
    }

    #[test]
    fn validation_rejects_what_cannot_be_enforced() {
        assert!(
            Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0)
                .validate()
                .is_ok()
        );
        assert!(
            Mandate::perp("phoenix", "SPCX", 0.0, 1_000.0)
                .validate()
                .is_err()
        );
        assert!(
            Mandate::perp("phoenix", "SPCX", 70.0, f64::NAN)
                .validate()
                .is_err()
        );

        let mut twice = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        twice.universe.push(twice.universe[0].clone());
        assert!(twice.validate().unwrap_err().contains("twice"));

        let mut empty = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        empty.universe.clear();
        assert!(empty.validate().is_err());

        let mut future = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        future.version = "v1".into();
        assert!(future.validate().is_err());
    }

    #[test]
    fn unknown_fields_are_refused() {
        // A limit this reader doesn't know is one it can't enforce.
        let json =
            r#"{"version":"v0","universe":[],"limits":{"max_drawdown_usd":1,"max_leverage":3}}"#;
        assert!(serde_json::from_str::<Mandate>(json).is_err());
    }

    #[test]
    fn the_audit_reports_the_first_breach() {
        let mandate = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        assert_eq!(mandate.audit(&observed(70.0, 1_000.0)), None);

        let over = mandate.audit(&observed(70.5, 0.0)).unwrap();
        assert!(matches!(over, Breach::Position { .. }));
        assert!(over.to_string().contains("cap"), "{over}");

        let deep = mandate.audit(&observed(10.0, 1_000.01)).unwrap();
        assert!(deep.to_string().contains("drew down"), "{deep}");

        let mut elsewhere = observed(10.0, 0.0);
        elsewhere.positions_outside_universe = 1;
        assert!(matches!(
            mandate.audit(&elsewhere),
            Some(Breach::OutsideUniverse { positions: 1 })
        ));

        let mut sol = Observed::default();
        sol.record(&"phoenix:perp:SOL".parse().unwrap(), 1.0);
        assert!(matches!(
            mandate.audit(&sol),
            Some(Breach::OutsideUniverse { .. })
        ));
    }

    #[test]
    fn a_short_counts_against_the_cap_like_a_long() {
        let mandate = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        let mut observed = observed(40.0, 0.0);
        observed.record(&spcx(), -12.0);
        assert_eq!(mandate.audit(&observed), None);
        observed.record(&spcx(), -71.0);
        observed.record(&spcx(), 3.0);
        let range = observed.range(&spcx()).unwrap();
        assert_eq!(
            (range.lowest, range.highest, range.peak()),
            (-71.0, 40.0, 71.0)
        );
        assert!(matches!(
            mandate.audit(&observed),
            Some(Breach::Position { held: 71.0, .. })
        ));
    }

    #[test]
    fn holdings_are_named_as_the_mandate_names_them() {
        use crate::{Asset, Holdings, Position, VenueId};

        let id = spcx().id();
        assert_eq!(id.0, "phoenix:perp:SPCX");
        let perp = Holding::Perp(Position::flat(id.clone(), VenueId::new("phoenix")));
        assert_eq!(InstrumentRef::of_holding(&id, &perp).unwrap(), spcx());

        let usdc = Asset::new("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC", 6).unwrap();
        let spot = Holding::Spot(Holdings::empty(usdc));
        let sol = InstrumentRef::of_holding(&InstrumentId::new("openbook:spot:SOL-USDC"), &spot);
        assert_eq!(sol.unwrap().kind, Kind::Spot);
        let lending = InstrumentRef::of_holding(&InstrumentId::new("kamino:spot:USDC"), &perp);
        assert!(lending.unwrap_err().contains("names a spot"));
        assert!(InstrumentRef::of_holding(&InstrumentId::new("SPCX"), &perp).is_err());
        let mislabelled = InstrumentRef::of_holding(&InstrumentId::new("phoenix:spot:SPCX"), &perp);
        assert!(mislabelled.unwrap_err().contains("names a spot"));
    }

    fn sale() -> Mandate {
        let mut mandate = Mandate::spot(&["openbook", "phoenix"], "SOL-USDC", 30_000.0);
        for allowance in &mut mandate.universe {
            allowance.sides = Some(vec![Side::Sell]);
            allowance.min_sell_price = Some(94.0);
        }
        mandate
    }

    fn fill(venue: &str, side: Side, price: f64) -> ObservedFill {
        ObservedFill {
            instrument: format!("{venue}:spot:SOL-USDC").parse().unwrap(),
            side,
            price,
        }
    }

    #[test]
    fn the_specs_sale_example_reads_as_written() {
        let json = r#"{
          "version": "v0",
          "term": { "until_slot": 412600 },
          "universe": [
            { "instrument": "openbook:spot:SOL-USDC", "max_position": 30000, "sides": ["sell"], "min_sell_price": 94.0 },
            { "instrument": "phoenix:spot:SOL-USDC", "max_position": 30000, "sides": ["sell"], "min_sell_price": 94.0 }
          ],
          "limits": {}
        }"#;
        let mandate: Mandate = serde_json::from_str(json).unwrap();
        assert_eq!(mandate.validate(), Ok(()));
        assert_eq!(
            mandate,
            Mandate {
                term: Some(Term {
                    from_slot: None,
                    until_slot: 412_600
                }),
                ..sale()
            }
        );
        assert_eq!(
            serde_json::to_value(mandate.limits).unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn a_perp_mandate_serializes_as_before() {
        let json = serde_json::to_string(&Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0)).unwrap();
        assert_eq!(
            json,
            r#"{"version":"v0","universe":[{"instrument":"phoenix:perp:SPCX","max_position":70.0}],"limits":{"max_drawdown_usd":1000.0}}"#
        );
    }

    #[test]
    fn new_members_are_validated() {
        let check = |edit: &dyn Fn(&mut Allowance)| {
            let mut mandate = sale();
            edit(&mut mandate.universe[0]);
            mandate.validate()
        };
        assert!(check(&|a| a.sides = Some(vec![])).is_err());
        assert!(check(&|a| a.sides = Some(vec![Side::Buy, Side::Buy])).is_err());
        assert!(check(&|a| a.sides = Some(vec![Side::Buy, Side::Sell])).is_ok());
        assert!(check(&|a| a.min_sell_price = Some(0.0)).is_err());
        assert!(check(&|a| a.max_buy_price = Some(f64::NAN)).is_err());
        assert!(check(&|a| a.min_position = Some(30_001.0)).is_err());
        assert!(check(&|a| a.min_position = Some(0.0)).is_ok());
        let mut limited = sale();
        limited.limits.max_drawdown_usd = Some(-1.0);
        assert!(limited.validate().is_err());
        let unknown = r#"{"instrument":"phoenix:spot:SOL-USDC","max_position":1,"sides":["hold"]}"#;
        assert!(serde_json::from_str::<Allowance>(unknown).is_err());
    }

    #[test]
    fn fills_on_a_forbidden_side_or_past_a_limit_price_breach() {
        let mandate = sale();
        let mut observed = Observed::default();
        observed.begin(&"phoenix:spot:SOL-USDC".parse().unwrap(), 30_000.0);
        observed.fills.push(fill("phoenix", Side::Sell, 94.0));
        observed.fills.push(fill("openbook", Side::Sell, 120.0));
        assert_eq!(mandate.audit(&observed), None);

        let mut bought = observed.clone();
        bought.fills.push(fill("openbook", Side::Buy, 100.0));
        let breach = mandate.audit(&bought).unwrap();
        assert!(matches!(
            breach,
            Breach::Side {
                side: Side::Buy,
                ..
            }
        ));
        assert!(breach.to_string().contains("bought"), "{breach}");

        let mut resting = observed.clone();
        resting.record_open_order(&"openbook:spot:SOL-USDC".parse().unwrap(), Side::Buy);
        assert!(matches!(mandate.audit(&resting), Some(Breach::Side { .. })));

        let mut cheap = observed.clone();
        cheap.fills.push(fill("phoenix", Side::Sell, 93.99));
        let breach = mandate.audit(&cheap).unwrap();
        assert!(matches!(breach, Breach::Price { limit: 94.0, .. }));
        assert!(breach.to_string().contains("under the minimum"), "{breach}");

        let mut capped = Mandate::spot(&["phoenix"], "SOL-USDC", 100.0);
        capped.universe[0].max_buy_price = Some(150.0);
        let mut dear = Observed::default();
        dear.fills.push(fill("phoenix", Side::Buy, 150.5));
        assert!(matches!(
            capped.audit(&dear),
            Some(Breach::Price {
                side: Side::Buy,
                ..
            })
        ));
    }

    #[test]
    fn min_position_replaces_the_symmetric_bound() {
        let sol: InstrumentRef = "phoenix:spot:SOL-USDC".parse().unwrap();
        let mut mandate = Mandate::spot(&["phoenix"], "SOL-USDC", 100.0);
        mandate.universe[0].min_position = Some(10.0);
        let mut observed = Observed::default();
        observed.begin(&sol, 100.0);
        observed.record(&sol, 10.0);
        assert_eq!(mandate.audit(&observed), None);
        observed.record(&sol, 9.0);
        assert!(matches!(
            mandate.audit(&observed),
            Some(Breach::Floor { floor: 10.0, .. })
        ));
        // Without a drawdown limit, any drawdown is allowed.
        let deep = Observed {
            max_drawdown_usd: 1e9,
            ..Observed::default()
        };
        assert_eq!(mandate.audit(&deep), None);
    }

    #[test]
    fn room_counts_resting_orders_as_filled() {
        let mandate = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        let allowance = &mandate.universe[0];
        assert_eq!(allowance.room(0.0, 0.0, 0.0), (70.0, 70.0));
        // Long 20, bids of 30 resting and asks of 10: 20 more to buy, 80 to sell.
        assert_eq!(allowance.room(20.0, 30.0, 10.0), (20.0, 80.0));
        // Already over in one direction: no room that way, never negative.
        assert_eq!(allowance.room(60.0, 30.0, 0.0), (0.0, 130.0));
        // What room allows, breach accepts; one more lot, it doesn't.
        let (buy, _) = allowance.room(20.0, 30.0, 10.0);
        assert_eq!(allowance.breach(20.0 - 10.0, 20.0 + 30.0 + buy), None);
        assert!(allowance.breach(0.0, 70.5).is_some());
        assert!(allowance.breach(-70.5, 0.0).is_some());
    }

    #[test]
    fn room_stops_selling_at_the_floor() {
        let mut mandate = Mandate::spot(&["phoenix"], "SOL-USDC", 100.0);
        let allowance = &mut mandate.universe[0];
        allowance.min_position = Some(10.0);
        // Holding 60 with asks of 20 resting: 30 more to sell, 40 to buy.
        assert_eq!(allowance.room(60.0, 0.0, 20.0), (40.0, 30.0));
        assert_eq!(allowance.room(15.0, 0.0, 20.0), (85.0, 0.0));
        let (_, sell) = allowance.room(60.0, 0.0, 20.0);
        assert_eq!(allowance.breach(60.0 - 20.0 - sell, 60.0), None);
        assert!(allowance.breach(9.5, 60.0).is_some());
    }

    fn qqq() -> InstrumentRef {
        "phoenix:perp:QQQ".parse().unwrap()
    }

    fn spy() -> InstrumentRef {
        "phoenix:perp:SPY".parse().unwrap()
    }

    /// The spec's v0.1 example: QQQ hedged with SPY, net down to a quarter
    /// of the start 600 slots into a term that starts at slot 1000.
    const BOOK: &str = r#"{
      "version": "v0.1",
      "term": { "from_slot": 1000, "until_slot": 2000 },
      "universe": [
        { "instrument": "phoenix:perp:QQQ", "max_position": 30 },
        { "instrument": "phoenix:perp:SPY", "max_position": 30,
          "hedge": { "against": "phoenix:perp:QQQ", "max_ratio": 1.2 } }
      ],
      "limits": {
        "exposures": [
          { "name": "deadline", "weights": { "phoenix:perp:QQQ": 1.0, "phoenix:perp:SPY": 1.2 },
            "max_abs_fraction_of_start": 0.25, "from_slots_after_start": 600 }
        ]
      }
    }"#;

    fn book() -> Mandate {
        let mandate: Mandate = serde_json::from_str(BOOK).unwrap();
        assert_eq!(mandate.validate(), Ok(()));
        mandate
    }

    fn held(qqq_usd: f64, spy_usd: f64) -> Vec<(InstrumentRef, f64, f64)> {
        vec![
            (qqq(), qqq_usd / 500.0, 500.0),
            (spy(), spy_usd / 600.0, 600.0),
        ]
    }

    fn at(slot: u64, holdings: &[(InstrumentRef, f64, f64)]) -> Sample<'_> {
        Sample { slot, holdings }
    }

    #[test]
    fn a_v0_mandate_with_a_v0_1_member_is_refused() {
        let perp = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        assert_eq!(perp.minimal_version(), "v0");
        type Edit<'a> = (&'a str, &'a dyn Fn(&mut Mandate));
        let edits: [Edit; 5] = [
            ("term.from_slot", &|m: &mut Mandate| {
                m.term = Some(Term {
                    from_slot: Some(1),
                    until_slot: 2,
                })
            }),
            ("limits.max_gross_notional_usd", &|m: &mut Mandate| {
                m.limits.max_gross_notional_usd = Some(10_000.0)
            }),
            ("limits.exposures", &|m: &mut Mandate| {
                m.limits.exposures.push(Exposure {
                    name: "net".into(),
                    weights: [(spcx(), 1.0)].into(),
                    max_abs_usd: Some(1_000.0),
                    max_abs_fraction_of_start: None,
                    from_slots_after_start: 0,
                })
            }),
            ("hedge", &|m: &mut Mandate| {
                m.universe.push(Allowance {
                    hedge: Some(Hedge {
                        against: spcx(),
                        max_ratio: 1.0,
                    }),
                    ..Mandate::perp("phoenix", "SPY", 10.0, 1.0).universe[0].clone()
                })
            }),
            ("taking", &|m: &mut Mandate| {
                m.universe[0].taking = Some(Taking::ReduceOnly)
            }),
        ];
        for (member, edit) in edits {
            let mut mandate = perp.clone();
            edit(&mut mandate);
            assert_eq!(mandate.minimal_version(), "v0.1", "{member}");
            assert_eq!(
                mandate.validate(),
                Err(format!("member {member} needs version v0.1"))
            );
            mandate.version = "v0.1".into();
            assert_eq!(mandate.validate(), Ok(()), "{member}");
        }
        let mut later = perp;
        later.version = "v0.2".into();
        assert!(later.validate().unwrap_err().contains(r#""v0" and "v0.1""#));
    }

    #[test]
    fn a_v0_mandate_with_its_term_set_serializes_as_before() {
        let mandate = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0).with_term(400_000, 412_480);
        assert_eq!(
            serde_json::to_string(&mandate).unwrap(),
            r#"{"version":"v0","term":{"until_slot":412480},"universe":[{"instrument":"phoenix:perp:SPCX","max_position":70.0}],"limits":{"max_drawdown_usd":1000.0}}"#
        );
        // A v0.1 mandate gets the start its exposures count from.
        let book = book().with_term(5, 9);
        assert_eq!(
            book.term,
            Some(Term {
                from_slot: Some(5),
                until_slot: 9
            })
        );
        let json = serde_json::to_string(&book).unwrap();
        assert_eq!(serde_json::from_str::<Mandate>(&json).unwrap(), book);
    }

    #[test]
    fn unknown_v0_1_members_are_refused_at_every_level() {
        let replace = |from: &str, to: &str| {
            assert!(BOOK.contains(from), "{from}");
            serde_json::from_str::<Mandate>(&BOOK.replace(from, to)).is_err()
        };
        assert!(replace(
            r#""from_slot": 1000"#,
            r#""from_slot": 1000, "to_slot": 3"#
        ));
        assert!(replace(
            r#""max_ratio": 1.2"#,
            r#""max_ratio": 1.2, "min_ratio": 0.5"#
        ));
        assert!(replace(
            r#""name": "deadline""#,
            r#""name": "deadline", "decay": 1"#
        ));
        assert!(replace(
            r#""exposures""#,
            r#""max_notional_usd": 5, "exposures""#
        ));
    }

    #[test]
    fn v0_1_members_are_validated() {
        let check = |edit: &dyn Fn(&mut Mandate)| {
            let mut mandate = book();
            edit(&mut mandate);
            mandate.validate()
        };
        fn exposure(m: &mut Mandate) -> &mut Exposure {
            &mut m.limits.exposures[0]
        }
        fn hedge(m: &mut Mandate) -> &mut Hedge {
            m.universe[1].hedge.as_mut().unwrap()
        }
        assert!(check(&|m| m.limits.max_gross_notional_usd = Some(0.0)).is_err());
        assert!(check(&|m| m.limits.max_gross_notional_usd = Some(1.0)).is_ok());
        assert!(check(&|m| exposure(m).name = String::new()).is_err());
        assert!(check(&|m| exposure(m).weights.clear()).is_err());
        assert!(
            check(&|m| {
                exposure(m).weights.insert(spcx(), 1.0);
            })
            .unwrap_err()
            .contains("universe")
        );
        assert!(
            check(&|m| {
                exposure(m).weights.insert(qqq(), f64::NAN);
            })
            .is_err()
        );
        assert!(check(&|m| exposure(m).max_abs_fraction_of_start = Some(1.5)).is_err());
        assert!(check(&|m| exposure(m).max_abs_fraction_of_start = Some(1.0)).is_ok());
        assert!(
            check(&|m| exposure(m).max_abs_usd = Some(10.0))
                .unwrap_err()
                .contains("one of")
        );
        assert!(
            check(&|m| exposure(m).max_abs_fraction_of_start = None)
                .unwrap_err()
                .contains("one of")
        );
        assert!(
            check(&|m| {
                let twin = m.limits.exposures[0].clone();
                m.limits.exposures.push(twin);
            })
            .unwrap_err()
            .contains("named twice")
        );
        // An offset counts from the term's start: a term without one can't place it.
        assert!(
            check(&|m| m.term.as_mut().unwrap().from_slot = None)
                .unwrap_err()
                .contains("from_slot")
        );
        assert!(check(&|m| m.term = None).is_ok());
        assert!(check(&|m| hedge(m).max_ratio = 0.0).is_err());
        assert!(
            check(&|m| hedge(m).against = spy())
                .unwrap_err()
                .contains("itself")
        );
        assert!(
            check(&|m| hedge(m).against = spcx())
                .unwrap_err()
                .contains("universe")
        );
        assert!(
            check(&|m| {
                m.universe[0].hedge = Some(Hedge {
                    against: spy(),
                    max_ratio: 1.0,
                });
            })
            .unwrap_err()
            .contains("itself a hedge")
        );
    }

    #[test]
    fn gross_notional_breaches_past_its_limit() {
        let mut mandate = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        mandate.version = "v0.1".into();
        mandate.limits.max_gross_notional_usd = Some(10_000.0);
        let start = ExposureStart::default();
        // 25 short at 400: exactly the limit.
        let at_limit = [(spcx(), -25.0, 400.0)];
        assert_eq!(mandate.check_sample(&at(0, &at_limit), &start), None);
        assert_eq!(mandate.gross_room_usd(&at(0, &at_limit)), Some(0.0));
        let over = [(spcx(), -25.01, 400.0)];
        let breach = mandate.check_sample(&at(0, &over), &start).unwrap();
        assert!(matches!(
            breach,
            Breach::GrossNotional {
                limit: 10_000.0,
                ..
            }
        ));
        assert!(breach.to_string().contains("gross notional"), "{breach}");
        // Outside the universe is the universe rule's, not counted twice.
        let elsewhere = [(spcx(), 10.0, 400.0), (qqq(), 100.0, 500.0)];
        assert_eq!(mandate.gross_notional_usd(&at(0, &elsewhere)), 4_000.0);
        assert_eq!(mandate.gross_room_usd(&at(0, &elsewhere)), Some(6_000.0));
        assert_eq!(
            Mandate::perp("phoenix", "SPCX", 1.0, 1.0).gross_room_usd(&at(0, &over)),
            None
        );
    }

    #[test]
    fn an_exposure_applies_from_its_start_and_scales_with_the_start() {
        let mandate = book();
        let opening = held(-10_000.0, 6_000.0);
        let start = mandate.exposure_start(&at(1_000, &opening));
        // QQQ −10,000 + 1.2 × SPY 6,000: −2,800, so the bound is 700.
        assert!((start.0[0] + 2_800.0).abs() < 1e-9, "{start:?}");
        assert_eq!(mandate.check_sample(&at(1_000, &opening), &start), None);
        // Before slot 1600 the deadline isn't enforced.
        assert_eq!(mandate.check_sample(&at(1_599, &opening), &start), None);
        let breach = mandate.check_sample(&at(1_600, &opening), &start).unwrap();
        assert!(
            matches!(&breach, Breach::Exposure { name, slot: 1_600, .. } if name == "deadline")
        );
        assert!(breach.to_string().contains("deadline exposure"), "{breach}");
        // −3,000 + 1.2 × 1,916.67 = −700: on the bound passes, past it doesn't.
        let within = held(-3_000.0, 2_300.0 / 1.2);
        assert_eq!(mandate.check_sample(&at(1_600, &within), &start), None);
        let past = held(-3_000.0, 2_290.0 / 1.2);
        assert!(matches!(
            mandate.check_sample(&at(1_999, &past), &start),
            Some(Breach::Exposure { .. })
        ));
        // The bound is a fraction of the start's size, either way.
        assert!(matches!(
            mandate.check_sample(
                &at(1_600, &held(-1_000.0, 3_000.0 / 1.2 + 700.0 / 1.2)),
                &start
            ),
            Some(Breach::Exposure { .. })
        ));
    }

    #[test]
    fn an_exposure_without_a_start_to_count_from_always_applies() {
        let mut mandate = book();
        mandate.term = None;
        let start = ExposureStart(vec![-2_800.0]);
        assert!(matches!(
            mandate.check_sample(&at(0, &held(-10_000.0, 6_000.0)), &start),
            Some(Breach::Exposure { .. })
        ));
        // max_abs_usd ignores the start.
        mandate.limits.exposures[0].max_abs_fraction_of_start = None;
        mandate.limits.exposures[0].max_abs_usd = Some(3_000.0);
        assert_eq!(
            mandate.check_sample(&at(0, &held(-10_000.0, 6_000.0)), &ExposureStart::default()),
            None
        );
    }

    #[test]
    fn a_hedge_must_face_what_it_hedges_within_its_ratio() {
        let mandate = book();
        let start = ExposureStart(vec![-1e9]);
        let check =
            |qqq_usd, spy_usd| mandate.check_sample(&at(0, &held(qqq_usd, spy_usd)), &start);
        assert_eq!(check(-10_000.0, 12_000.0), None);
        assert_eq!(check(0.0, 0.0), None);
        assert_eq!(check(-10_000.0, 0.0), None);
        // Over the ratio.
        assert!(matches!(
            check(-10_000.0, 12_001.0),
            Some(Breach::Hedge { .. })
        ));
        // With QQQ flat, the hedge must be flat.
        assert!(matches!(check(0.0, 600.0), Some(Breach::Hedge { .. })));
        // The same side as QQQ: a flip, however small.
        let breach = mandate
            .check_sample(&at(0, &held(-10_000.0, -600.0)), &start)
            .unwrap();
        assert!(matches!(
            &breach,
            Breach::Hedge { instrument, against, .. } if *instrument == spy() && *against == qqq()
        ));
        assert!(
            breach.to_string().contains("outside its hedge bound"),
            "{breach}"
        );
    }

    #[test]
    fn hedge_room_stops_at_the_bound() {
        let mandate = book();
        let holdings = held(-10_000.0, 6_000.0);
        // SPY is 10 long of a 20 bound at 600 a share: 10 more to buy, 10 to sell.
        let (buy, sell) = mandate.hedge_room(&spy(), &at(0, &holdings)).unwrap();
        assert!(
            (buy - 10.0).abs() < 1e-9 && (sell - 10.0).abs() < 1e-9,
            "{buy} {sell}"
        );
        let start = ExposureStart(vec![-1e9]);
        let full = held(-10_000.0, 6_000.0 + buy * 600.0);
        assert_eq!(mandate.check_sample(&at(0, &full), &start), None);
        assert_eq!(mandate.hedge_room(&qqq(), &at(0, &holdings)), None);
        assert_eq!(mandate.hedge_room(&spy(), &at(0, &holdings[..1])), None);
    }

    #[test]
    fn the_audit_reports_a_notional_breach_after_drawdown() {
        let mut mandate = book();
        mandate.limits.max_drawdown_usd = Some(100.0);
        let mut observed = Observed::default();
        let start = ExposureStart(vec![-1e9]);
        observed.record_sample(&mandate, &at(0, &held(-1_000.0, 0.0)), &start);
        assert_eq!(mandate.audit(&observed), None);
        observed.record_sample(&mandate, &at(1, &held(-1_000.0, 2_000.0)), &start);
        observed.record_sample(&mandate, &at(2, &held(0.0, 600.0)), &start);
        // The first breach is kept.
        assert!(matches!(
            &observed.notional_breach,
            Some(Breach::Hedge { notional, .. }) if (notional - 2_000.0).abs() < 1e-9
        ));
        assert!(matches!(
            mandate.audit(&observed),
            Some(Breach::Hedge { .. })
        ));
        observed.max_drawdown_usd = 101.0;
        assert!(matches!(
            mandate.audit(&observed),
            Some(Breach::Drawdown { .. })
        ));
    }

    #[test]
    fn breaches_read_back_as_written() {
        for breach in [
            Breach::GrossNotional {
                notional: 1.5,
                limit: 1.0,
            },
            Breach::Exposure {
                name: "net".into(),
                value: -2.0,
                limit: 1.0,
                slot: 7,
            },
            Breach::Hedge {
                instrument: spy(),
                against: qqq(),
                notional: 1.0,
                against_notional: 2.0,
            },
            Breach::Side {
                instrument: spy(),
                side: Side::Buy,
            },
        ] {
            let json = serde_json::to_value(&breach).unwrap();
            assert!(json["rule"].is_string(), "{json}");
            assert_eq!(serde_json::from_value::<Breach>(json).unwrap(), breach);
        }
    }

    #[test]
    fn reduce_only_taking_never_opens_adds_or_flips() {
        let mut mandate = Mandate::perp("phoenix", "SPCX", 20.0, 250.0);
        mandate.version = "v0.1".into();
        let json =
            r#"{"instrument":"phoenix:perp:SPCX","max_position":20.0,"taking":"reduce_only"}"#;
        let allowance: Allowance = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&allowance).unwrap(), json);
        assert!(
            serde_json::from_str::<Allowance>(&json.replace("reduce_only", "post_only")).is_err()
        );
        mandate.universe[0] = allowance;
        assert_eq!(mandate.validate(), Ok(()));
        let allowance = &mandate.universe[0];
        // Toward zero, all the way: allowed.
        assert!(allowance.may_take(10.0, Side::Sell, 4.0));
        assert!(allowance.may_take(10.0, Side::Sell, 10.0));
        assert!(allowance.may_take(-3.0, Side::Buy, 3.0));
        // Opening, adding or flipping: not.
        assert!(!allowance.may_take(0.0, Side::Buy, 1.0));
        assert!(!allowance.may_take(10.0, Side::Buy, 0.5));
        assert!(!allowance.may_take(10.0, Side::Sell, 10.5));
        assert!(!allowance.may_take(-3.0, Side::Sell, 1.0));
        // Without the member, taking is free.
        assert!(
            Mandate::perp("phoenix", "SPCX", 20.0, 250.0).universe[0].may_take(0.0, Side::Buy, 5.0)
        );

        let take = |side, quantity, position_before| ObservedTake {
            instrument: spcx(),
            side,
            quantity,
            position_before,
        };
        let mut observed = Observed::default();
        observed.takes.push(take(Side::Sell, 4.0, 10.0));
        assert_eq!(mandate.audit(&observed), None);
        observed.takes.push(take(Side::Sell, 12.0, 6.0));
        let breach = mandate.audit(&observed).unwrap();
        assert!(matches!(breach, Breach::Taking { quantity: 12.0, .. }));
        assert!(breach.to_string().contains("only to reduce"), "{breach}");
        let json = serde_json::to_value(&breach).unwrap();
        assert_eq!(json["rule"], "taking");
        assert_eq!(serde_json::from_value::<Breach>(json).unwrap(), breach);
    }
}
