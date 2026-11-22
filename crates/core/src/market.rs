//! A market as a survey sees it: what it is, where it is, and what trading
//! there costs, for any venue. The harness that chooses where to run
//! strategies reads these for every market a venue lists; a strategy that
//! trades a market reads its exact [`crate::Instrument`] instead.
//!
//! Prices and sizes here are `f64` on purpose: a survey compares markets
//! (a tick's width against its price, a fee against a spread), it doesn't
//! settle anything.

use serde::{Deserialize, Serialize};

use crate::mandate::InstrumentRef;

/// One market a venue lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketInfo {
    /// As mandates name it: `phoenix:perp:SOL`.
    pub instrument: InstrumentRef,
    /// Its account on chain, where it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// The smallest price step, in quote units per base unit.
    pub tick: f64,
    /// The smallest size step, in base units.
    pub lot: f64,
    /// What a fill costs, in bps of its notional: the maker's may be
    /// negative (a rebate).
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
    /// Whether the venue lets it trade now.
    pub active: bool,
}

impl MarketInfo {
    /// The tick's width at `price`, in bps: how much of a fill a one-tick
    /// spread is. `None` for a price that isn't one.
    pub fn tick_bps(&self, price: f64) -> Option<f64> {
        (price.is_finite() && price > 0.0).then(|| self.tick / price * 10_000.0)
    }

    /// What a maker keeps of a one-tick spread at `price`, in bps, after
    /// its fee: half the tick less the maker fee. Negative where the fee
    /// is more than half a tick.
    pub fn maker_edge_at_touch_bps(&self, price: f64) -> Option<f64> {
        self.tick_bps(price).map(|t| t / 2.0 - self.maker_fee_bps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sol() -> MarketInfo {
        MarketInfo {
            instrument: "phoenix:perp:SOL".parse().unwrap(),
            address: None,
            tick: 0.01,
            lot: 0.01,
            maker_fee_bps: 0.5,
            taker_fee_bps: 3.5,
            active: true,
        }
    }

    #[test]
    fn a_tick_against_its_price() {
        let m = sol();
        assert!((m.tick_bps(69.0).unwrap() - 1.449).abs() < 1e-3);
        assert!((m.maker_edge_at_touch_bps(69.0).unwrap() - 0.2246).abs() < 1e-3);
        assert_eq!(m.tick_bps(0.0), None);
        assert_eq!(m.tick_bps(f64::NAN), None);
    }

    #[test]
    fn it_reads_and_writes_as_json() {
        let m = sol();
        let text = serde_json::to_string(&m).unwrap();
        assert!(
            text.starts_with(r#"{"instrument":"phoenix:perp:SOL","tick":0.01"#),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<MarketInfo>(&text).unwrap(), m);
    }
}
