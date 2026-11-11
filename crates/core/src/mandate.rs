//! The mandate: what a bot may trade, and the limits it must stay within.
//!
//! A mandate constrains what an agent does, never why: it names the
//! instruments the agent may hold (its universe) and limits that can be
//! checked from the account alone. The grader holds its own copy and audits
//! every session against it; the copy a bot reads (`MTX_MANDATE`) is for the
//! bot to plan by. The format is specified in `specs/mandate.md`.

use std::{fmt, path::Path, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{Error, Holding, InstrumentId, Result, error::io_error};

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
    /// The largest position, long or short, in base units.
    pub max_position: f64,
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
    /// instead of after a session. This reader has no `min_position`:
    /// `max_position` bounds a short as much as a long.
    pub fn room(&self, position: f64, resting_bids: f64, resting_asks: f64) -> (f64, f64) {
        position_room(self.max_position, position, resting_bids, resting_asks)
    }

    /// The breach, if holding anywhere from `lowest` to `highest` breaks
    /// the allowance's cap.
    pub fn breach(&self, lowest: f64, highest: f64) -> Option<Breach> {
        let held = highest.max(-lowest);
        (held > self.max_position * (1.0 + POSITION_TOLERANCE)).then(|| Breach::Position {
            instrument: self.instrument.clone(),
            held,
            cap: self.max_position,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// The most, in USD, the account's value may fall below its running peak.
    pub max_drawdown_usd: f64,
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

/// Instrument kinds this version defines. `specs/instruments/` reserves the
/// others (spot, future, option, money market) for later versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A perpetual future: no expiry, held near spot by funding payments.
    Perp,
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
            }],
            limits: Limits { max_drawdown_usd },
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
            positive(
                &format!("{} max_position", allowance.instrument),
                allowance.max_position,
            )?;
        }
        positive("max_drawdown_usd", self.limits.max_drawdown_usd)
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
        if observed.max_drawdown_usd > self.limits.max_drawdown_usd {
            return Some(Breach::Drawdown {
                drawn: observed.max_drawdown_usd,
                limit: self.limits.max_drawdown_usd,
            });
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
        })
    }
}

impl FromStr for Kind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "perp" => Ok(Self::Perp),
            "spot" | "future" | "option" | "money-market" => Err(format!(
                "instrument kind {s:?} is reserved; {VERSION} defines perp only"
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
        let reserved =
            InstrumentRef::of_holding(&InstrumentId::new("openbook:spot:SOL-USDC"), &spot);
        assert!(reserved.unwrap_err().contains("reserved"));
        assert!(InstrumentRef::of_holding(&InstrumentId::new("SPCX"), &perp).is_err());
        let mislabelled = InstrumentRef::of_holding(&InstrumentId::new("phoenix:spot:SPCX"), &perp);
        assert!(mislabelled.unwrap_err().contains("names a spot"));
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
}
