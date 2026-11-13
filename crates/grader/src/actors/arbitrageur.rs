//! The grader's arbitrageur: it keeps markets' prices tied to the
//! reference price, as arbitrage against the wider market would.
//!
//! After each step's blocks, before the bot's turn, it reads each of its
//! markets' top through its own seat. Where buying costs less than the
//! reference by more than its band, it buys, up to its clip, until buying
//! would cost the reference; where selling pays more than the reference by
//! more than the band, it sells down to it. Each is one taker's order on the
//! market (on a pool, a swap that stops at the price), so the arbitrageur
//! pays the market's fee like anyone: a market stays within its fee of the
//! reference, and a trade that moves it further is pulled back next step.
//! The same session trades the same way every time.

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{Actor, ActorsReport, Moment};
use crate::venue::Seat;

/// How a scenario's arbitrageur trades.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArbitrageSpec {
    /// How far, in basis points, a market's price for a trade may stray from
    /// the reference before the arbitrageur trades it back.
    pub band_bps: f64,
    /// The most base it trades on one market in one step.
    pub clip: f64,
    /// What each of its accounts starts with, one per market: base units
    /// and quote units.
    pub base: f64,
    pub quote: f64,
}

impl ArbitrageSpec {
    pub fn validate(&self) -> std::result::Result<(), String> {
        let ok = |x: f64| x.is_finite() && x >= 0.0;
        if !(ok(self.band_bps) && ok(self.base) && ok(self.quote))
            || !(self.clip.is_finite() && self.clip > 0.0)
        {
            return Err(
                "arbitrage: band_bps, base and quote are numbers of at least 0, and clip is above 0"
                    .into(),
            );
        }
        Ok(())
    }
}

/// What the arbitrageur did in a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ArbitrageReport {
    /// Orders it sent, and how many failed.
    pub sent: usize,
    pub failed: usize,
    /// Base it bought and sold, at most what it sent for.
    pub bought: f64,
    pub sold: f64,
}

/// The arbitrageur through a session: one seat per market it keeps in line.
pub struct Arbitrageur {
    seats: Vec<Box<dyn Seat>>,
    spec: ArbitrageSpec,
    report: ArbitrageReport,
}

impl Arbitrageur {
    pub fn new(seats: Vec<Box<dyn Seat>>, spec: ArbitrageSpec) -> Self {
        Self {
            seats,
            spec,
            report: ArbitrageReport::default(),
        }
    }

    /// The order that brings a market whose top is `(bid, ask)` back to
    /// `reference`: a side and a limit, if it strays past the band.
    fn order(&self, (bid, ask): (Option<f64>, Option<f64>), reference: f64) -> Option<(Side, f64)> {
        let band = self.spec.band_bps / 10_000.0;
        match (bid, ask) {
            (_, Some(ask)) if ask < reference * (1.0 - band) => Some((Side::Bid, reference)),
            (Some(bid), _) if bid > reference * (1.0 + band) => Some((Side::Ask, reference)),
            _ => None,
        }
    }
}

#[async_trait]
impl Actor for Arbitrageur {
    async fn after_blocks(&mut self, now: &Moment) {
        if !(now.level_usd.is_finite() && now.level_usd > 0.0) {
            return;
        }
        for seat in &self.seats {
            let top = match seat.top().await {
                Ok(top) => top,
                Err(error) => {
                    warn!(market = seat.market(), %error, "the arbitrageur couldn't read the market");
                    continue;
                }
            };
            let Some((side, limit)) = self.order(top, now.level_usd) else {
                continue;
            };
            self.report.sent += 1;
            match seat.take(side, self.spec.clip, limit).await {
                Ok(()) => match side {
                    Side::Bid => self.report.bought += self.spec.clip,
                    Side::Ask => self.report.sold += self.spec.clip,
                },
                Err(error) => {
                    self.report.failed += 1;
                    warn!(market = seat.market(), %error, "the arbitrageur's order failed");
                }
            }
        }
    }

    async fn report(&mut self, report: &mut ActorsReport) {
        report.arbitrage = Some(self.report.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_trades_a_market_back_only_past_its_band() {
        let spec = ArbitrageSpec {
            band_bps: 50.0,
            clip: 5.0,
            base: 10.0,
            quote: 1_000.0,
        };
        assert_eq!(spec.validate(), Ok(()));
        let arb = Arbitrageur::new(Vec::new(), spec);
        // Cheap by 1%: buy up to the reference.
        assert_eq!(
            arb.order((Some(98.0), Some(99.0)), 100.0),
            Some((Side::Bid, 100.0))
        );
        // Rich by 1%: sell down to it.
        assert_eq!(
            arb.order((Some(101.0), Some(102.0)), 100.0),
            Some((Side::Ask, 100.0))
        );
        // Within the band, or unread: nothing.
        assert_eq!(arb.order((Some(99.7), Some(100.3)), 100.0), None);
        assert_eq!(arb.order((None, None), 100.0), None);
        let mut bad = spec;
        bad.clip = 0.0;
        assert!(bad.validate().is_err());
    }
}
