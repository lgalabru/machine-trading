//! The mandate: what a bot may trade, and the limits it must stay within.
//!
//! A mandate constrains what an agent does, never why: it names the
//! instruments the agent may hold (its universe) and limits that can be
//! checked from the account alone. The grader holds its own copy and audits
//! every session against it; the copy a bot reads (`MTH_MANDATE`) is for the
//! bot to plan by. The format is specified in `specs/mandate.md`.

use std::{fmt, path::Path, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, error::io_error};

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
        for (instrument, held) in &observed.peak_positions {
            let Some(allowance) = self.allowance(instrument) else {
                return Some(Breach::OutsideUniverse { positions: 1 });
            };
            // Lots convert to base units in floating point.
            if *held > allowance.max_position * (1.0 + 1e-9) {
                return Some(Breach::Position {
                    instrument: instrument.clone(),
                    held: *held,
                    cap: allowance.max_position,
                });
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
    /// The largest position, long or short, held in each instrument measured.
    pub peak_positions: Vec<(InstrumentRef, f64)>,
    /// The most positions or orders held at once in instruments outside the
    /// universe.
    pub positions_outside_universe: u32,
    /// The deepest the account's value fell below its running peak, in USD.
    pub max_drawdown_usd: f64,
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
        Observed {
            peak_positions: vec![(spcx(), held)],
            positions_outside_universe: 0,
            max_drawdown_usd: drawdown,
        }
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

        let sol = Observed {
            peak_positions: vec![("phoenix:perp:SOL".parse().unwrap(), 1.0)],
            ..Observed::default()
        };
        assert!(matches!(
            mandate.audit(&sol),
            Some(Breach::OutsideUniverse { .. })
        ));
    }
}
