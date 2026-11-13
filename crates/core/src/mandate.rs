//! The mandate: what a bot may trade, and the limits it must stay within.
//!
//! A mandate constrains what an agent does, never why: it names the
//! instruments the agent may hold (its universe) and limits that can be
//! checked from the account alone. The grader holds its own copy and audits
//! every session against it; the copy a bot reads (`MTX_MANDATE`) is for the
//! bot to plan by. The format is specified in `specs/mandate.md`.

use std::{fmt, path::Path, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{Error, Holding, InstrumentId, Result, error::io_error, order::Side};

/// The only version this crate reads.
pub const VERSION: &str = "v0";

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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// The most, in USD, the account's value may fall below its running
    /// peak. Absent, the mandate has no drawdown limit: a mandate to sell a
    /// holding, whose value falls with the market whatever the agent does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_drawdown_usd: Option<f64>,
}

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
    /// Liquidity in a pool. The position is the base asset its positions
    /// hold, fees included.
    Amm,
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
            }],
            limits: Limits {
                max_drawdown_usd: Some(max_drawdown_usd),
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

    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.version != VERSION {
            return Err(format!(
                "version {:?}, this reader knows {VERSION:?}",
                self.version
            ));
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
        }
        match self.limits.max_drawdown_usd {
            Some(limit) => positive("max_drawdown_usd", limit),
            None => Ok(()),
        }
    }

    pub fn allowance(&self, instrument: &InstrumentRef) -> Option<&Allowance> {
        self.universe.iter().find(|a| &a.instrument == instrument)
    }

    /// Whether `instrument`'s allowance lets the account trade `side`: an
    /// instrument outside the universe has no sides to check.
    pub fn allows_side(&self, instrument: &InstrumentRef, side: Side) -> bool {
        self.allowance(instrument)
            .is_none_or(|a| a.sides.as_ref().is_none_or(|sides| sides.contains(&side)))
    }

    /// The first rule the observed session broke, if any.
    pub fn audit(&self, observed: &Observed) -> Option<Breach> {
        if let Some(what) = observed.unsupported.first() {
            return Some(Breach::Unsupported { what: what.clone() });
        }
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
        for (instrument, side) in observed
            .fills
            .iter()
            .map(|fill| (&fill.instrument, fill.side))
            .chain(observed.open_orders.iter().map(|(i, side)| (i, *side)))
            .chain(observed.liquidity.iter().map(|(i, side)| (i, *side)))
        {
            if !self.allows_side(instrument, side) {
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
        None
    }
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
    /// Each side the account's liquidity deposits and withdrawals moved base
    /// on, per `amm` instrument: a deposit that takes base in is a buy, a
    /// withdrawal that returns base a sell. Only the sides rule reads them.
    pub liquidity: Vec<(InstrumentRef, Side)>,
    /// What the account ran on a venue's program that a bot may not (a
    /// pool's limit order, say), each as the breach it is.
    pub unsupported: Vec<String>,
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

    /// Counts a deposit or withdrawal that moved base on `side` of the `amm`
    /// instrument.
    pub fn record_liquidity(&mut self, instrument: &InstrumentRef, side: Side) {
        if !self
            .liquidity
            .iter()
            .any(|(i, s)| i == instrument && *s == side)
        {
            self.liquidity.push((instrument.clone(), side));
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
#[derive(Debug, Clone, PartialEq, Serialize)]
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

#[derive(Debug, Clone, PartialEq)]
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
    /// An instruction a bot may not run on its venue, said as what the
    /// account did: "ran meteora PlaceLimitOrder, which this benchmark
    /// doesn't support".
    Unsupported {
        what: String,
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
            Self::Unsupported { what } => f.write_str(what),
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
            Holding::Amm(_) => "amm",
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
            Self::Amm => "amm",
        })
    }
}

impl FromStr for Kind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "perp" => Ok(Self::Perp),
            "spot" => Ok(Self::Spot),
            "amm" => Ok(Self::Amm),
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
    fn a_pools_liquidity_is_an_amm_instrument() {
        let pool: InstrumentRef = "raydium:amm:SPCX-USDC".parse().unwrap();
        assert_eq!(pool.kind, Kind::Amm);
        assert_eq!(pool.to_string(), "raydium:amm:SPCX-USDC");
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
    fn liquidity_moves_count_as_trades_for_the_sides_rule() {
        let pool: InstrumentRef = "orca:amm:SPCX-USDC".parse().unwrap();
        let mut mandate = Mandate::spot(&["orca"], "SPCX-USDC", 10.0);
        mandate.universe.push(Allowance {
            instrument: pool.clone(),
            max_position: 10.0,
            min_position: None,
            sides: Some(vec![Side::Sell]),
            min_sell_price: None,
            max_buy_price: None,
        });
        let mut observed = Observed::default();
        observed.record_liquidity(&pool, Side::Sell);
        observed.record_liquidity(&pool, Side::Sell);
        assert_eq!(observed.liquidity.len(), 1);
        // A withdrawal returning base is a sell: a sell-only mandate allows it.
        assert_eq!(mandate.audit(&observed), None);
        assert!(mandate.allows_side(&pool, Side::Sell) && !mandate.allows_side(&pool, Side::Buy));
        // A deposit taking base in is a buy.
        observed.record_liquidity(&pool, Side::Buy);
        assert_eq!(
            mandate.audit(&observed),
            Some(Breach::Side {
                instrument: pool.clone(),
                side: Side::Buy
            })
        );
        // Outside the universe, nothing to check.
        let other: InstrumentRef = "raydium:amm:SPCX-USDC".parse().unwrap();
        assert!(mandate.allows_side(&other, Side::Buy));
    }

    #[test]
    fn an_unsupported_instruction_is_a_breach_whatever_else_holds() {
        let mandate = Mandate::spot(&["meteora"], "SPCX-USDC", 10.0);
        let mut observed = Observed::default();
        assert_eq!(mandate.audit(&observed), None);
        let what = "ran meteora PlaceLimitOrder, which this benchmark doesn't support";
        observed.unsupported.push(what.into());
        let breach = mandate.audit(&observed).unwrap();
        assert_eq!(breach, Breach::Unsupported { what: what.into() });
        assert_eq!(breach.to_string(), what);
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
}
