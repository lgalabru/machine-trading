//! Makers: a ladder of quotes on each venue, re-placed after every step's
//! blocks around the session's reference.
//!
//! Each venue's maker quotes `sizes` on both sides, the nearest
//! `half_spread_bps` from its reference and each next one `spacing_bps`
//! further. What the step traded against it comes off its ladder, and comes
//! back at `refill` of the full ladder a step: a book the bot sold into
//! stays thin for a while. What it bought or sold it holds as inventory,
//! leaning its quotes against it (`skew_bps` at one full ladder held) and
//! shedding `hedge` of it a step. The flow all the makers absorb moves the
//! reference itself (see [`super::Impact`]).
//!
//! A `schedule` reshapes the ladder over stretches of the session, per venue
//! or on all of them: thinner, wider, fewer levels showing, leaning to one
//! side. `passive_impact` lets the bot's own resting orders, when they
//! fill, move the reference too, at that share of the usual impact.
//!
//! With `clear_stale`, the makers first take out of the snapshot's book
//! every order resting inside their quotes, so the session's book starts at
//! the scenario's level. A snapshot's books are mainnet's as captured: the
//! level a scenario sets can sit far from them.

use std::sync::Arc;

use futures::future::BoxFuture;
use mtx_core::Side;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{Actions, Actor, ActorsReport, Moment, StepFlow, seats::Seat};

/// How many rounds the makers take stale orders in: a venue matches a
/// limited number of orders per take.
const CLEAR_ROUNDS: usize = 6;
/// The most levels a side: an OpenBook open orders account holds 24 orders.
pub const MAX_LEVELS: usize = 12;

/// The makers' parameters; see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MakerModel {
    /// The nearest quote's distance from the reference, and each next
    /// one's from the last, in basis points.
    pub half_spread_bps: f64,
    pub spacing_bps: f64,
    /// Each level's size on each side, nearest first, in base units.
    pub sizes: Vec<f64>,
    /// The share of a full ladder that comes back each step.
    pub refill: f64,
    /// How far the quotes lean at one full ladder of inventory, in basis
    /// points, and the share of inventory shed each step.
    pub skew_bps: f64,
    pub hedge: f64,
    /// How far each impact unit of net selling into the makers moves the
    /// reference, for good and for a while, in basis points; and the share
    /// of the passing move that decays each step.
    pub permanent_bps: f64,
    pub transient_bps: f64,
    pub transient_decay: f64,
    /// The impact unit, in base units. Without it, one full side of every
    /// venue's ladder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact_unit: Option<f64>,
    /// Take the snapshot's resting orders inside the first quotes at setup.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_stale: bool,
    /// How much the bot's fills against its own resting orders move the
    /// reference, as a share of what the same flow into the makers would.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub passive_impact: f64,
    /// Stretches of the session where the makers quote differently: thin
    /// books, wide spreads, makers away, a lean.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule: Vec<Episode>,
}

fn is_zero(x: &f64) -> bool {
    *x == 0.0
}

fn one() -> f64 {
    1.0
}

fn is_one(x: &f64) -> bool {
    *x == 1.0
}

/// From `from_slot` to just before `to_slot` (slots into the session), on
/// `venue` or every venue, the makers' ladder is `depth` times as deep, its
/// gaps `spread` times as wide, only the nearest `quoting` share of its
/// levels shows, its bids are `1 + lean` times their size and its asks
/// `1 - lean`, and the whole ladder sits `offset_bps` off the reference (a
/// venue quoting away from fair). Overlapping episodes multiply (their leans
/// and offsets add).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Episode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue: Option<String>,
    pub from_slot: u64,
    pub to_slot: u64,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub depth: f64,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub spread: f64,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub quoting: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lean: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset_bps: f64,
}

/// The makers' shape at one step on one venue: the product of the episodes
/// in force.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shape {
    pub depth: f64,
    pub spread: f64,
    pub quoting: f64,
    pub lean: f64,
    pub offset_bps: f64,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            depth: 1.0,
            spread: 1.0,
            quoting: 1.0,
            lean: 0.0,
            offset_bps: 0.0,
        }
    }
}

impl MakerModel {
    pub fn validate(&self) -> std::result::Result<(), String> {
        let bad = |x: f64| !(x.is_finite() && x >= 0.0);
        if self.sizes.is_empty() || self.sizes.iter().any(|s| !(s.is_finite() && *s > 0.0)) {
            return Err("makers: sizes must be positive numbers".into());
        }
        if [
            self.half_spread_bps,
            self.spacing_bps,
            self.skew_bps,
            self.permanent_bps,
            self.transient_bps,
        ]
        .into_iter()
        .any(bad)
        {
            return Err("makers: basis points must be numbers of at least 0".into());
        }
        if self.sizes.len() > MAX_LEVELS {
            return Err(format!(
                "makers: at most {MAX_LEVELS} levels a side: an OpenBook account rests 24 orders"
            ));
        }
        if self.half_spread_bps == 0.0 {
            return Err("makers: half_spread_bps must be above 0".into());
        }
        if [self.refill, self.hedge, self.transient_decay]
            .into_iter()
            .any(|x| bad(x) || x > 1.0)
        {
            return Err("makers: refill, hedge and transient_decay are shares in [0, 1]".into());
        }
        if self
            .impact_unit
            .is_some_and(|u| !(u.is_finite() && u > 0.0))
        {
            return Err("makers: impact_unit must be above 0".into());
        }
        if bad(self.passive_impact) {
            return Err("makers: passive_impact must be a number of at least 0".into());
        }
        for (i, e) in self.schedule.iter().enumerate() {
            if e.from_slot >= e.to_slot
                || bad(e.depth)
                || !(e.spread.is_finite() && e.spread > 0.0)
                || bad(e.quoting)
                || e.quoting > 1.0
                || !(e.lean.is_finite() && e.lean.abs() <= 1.0)
                || !(e.offset_bps.is_finite() && e.offset_bps.abs() < 1e4)
            {
                return Err(format!(
                    "makers: schedule episode {i} must end after it starts, with depth and \
                     quoting at least 0 (quoting at most 1), spread above 0 and lean in [-1, 1]"
                ));
            }
        }
        Ok(())
    }

    /// The makers' shape on `venue`, `at` slots into the session.
    pub fn shape(&self, venue: &str, at: u64) -> Shape {
        let mut shape = Shape::default();
        for e in &self.schedule {
            if (e.from_slot..e.to_slot).contains(&at)
                && e.venue.as_deref().is_none_or(|v| v == venue)
            {
                shape.depth *= e.depth;
                shape.spread *= e.spread;
                shape.quoting *= e.quoting;
                shape.lean += e.lean;
                shape.offset_bps += e.offset_bps;
            }
        }
        shape.lean = shape.lean.clamp(-1.0, 1.0);
        shape
    }

    /// One side's full ladder, in base units.
    pub fn ladder(&self) -> f64 {
        self.sizes.iter().sum()
    }

    /// A maker's quotes around `reference_usd`, shaped by `shape`: bids
    /// then asks, each (price, size), nearest first.
    pub fn quotes(&self, book: &Book, reference_usd: f64, shape: &Shape) -> (Ladder, Ladder) {
        let lean = (book.inventory / self.ladder()).clamp(-1.0, 1.0);
        let center =
            reference_usd * (1.0 + shape.offset_bps / 1e4) * (1.0 - self.skew_bps * lean / 1e4);
        let levels =
            ((shape.quoting * self.sizes.len() as f64).ceil() as usize).min(self.sizes.len());
        let side = |sign: f64, available: f64, weight: f64| {
            self.sizes[..levels]
                .iter()
                .enumerate()
                .map(|(i, size)| {
                    let bps = (self.half_spread_bps + i as f64 * self.spacing_bps) * shape.spread;
                    (
                        center * (1.0 + sign * bps / 1e4),
                        size * available * shape.depth * weight,
                    )
                })
                .collect()
        };
        (
            side(-1.0, book.bid, 1.0 + shape.lean),
            side(1.0, book.ask, 1.0 - shape.lean),
        )
    }

    /// One side's ladder in full at `shape`, in base units: what a fill
    /// takes its share of.
    pub fn full(&self, shape: &Shape, weight: f64) -> f64 {
        let levels =
            ((shape.quoting * self.sizes.len() as f64).ceil() as usize).min(self.sizes.len());
        self.sizes[..levels].iter().sum::<f64>() * shape.depth * weight
    }
}

/// One side's quotes, nearest first: price and size.
pub type Ladder = Vec<(f64, f64)>;

/// One maker's state: the share of its ladder left on each side, and its
/// inventory in base units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Book {
    pub bid: f64,
    pub ask: f64,
    pub inventory: f64,
}

impl Default for Book {
    fn default() -> Self {
        Self {
            bid: 1.0,
            ask: 1.0,
            inventory: 0.0,
        }
    }
}

impl Book {
    /// A step's fills against it (`bought` on its bids, `sold` on its
    /// asks), each a share of that side's full ladder as last quoted
    /// (`full_bid`, `full_ask`), then the step's refill and hedge.
    pub fn step(&mut self, model: &MakerModel, bought: f64, sold: f64, full: (f64, f64)) {
        let share = |traded: f64, full: f64| {
            if traded <= 0.0 {
                0.0
            } else if full > 0.0 {
                traded / full
            } else {
                1.0
            }
        };
        self.bid = (self.bid - share(bought, full.0)).max(0.0);
        self.ask = (self.ask - share(sold, full.1)).max(0.0);
        self.inventory += bought - sold;
        self.bid = (self.bid + model.refill).min(1.0);
        self.ask = (self.ask + model.refill).min(1.0);
        self.inventory *= 1.0 - model.hedge;
    }
}

/// A maker's quotes after one step.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MakerStep {
    pub at: u64,
    /// Its best bid and offer, and the size of each side's ladder, in base
    /// units.
    pub bid_usd: Option<f64>,
    pub ask_usd: Option<f64>,
    pub bid_base: f64,
    pub ask_base: f64,
    pub inventory: f64,
}

/// What one venue's maker did in a session.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MakerReport {
    pub venue: String,
    /// Base units of the snapshot's resting orders it took at setup: asks
    /// it bought, bids it sold.
    pub cleared_bought: f64,
    pub cleared_sold: f64,
    /// Base units traded against its quotes in the session.
    pub bought: f64,
    pub sold: f64,
    pub steps: Vec<MakerStep>,
    pub actions: Actions,
}

/// The makers of a session: one seat per venue.
pub struct Makers {
    model: MakerModel,
    seats: Vec<(usize, Arc<Seat>)>,
    books: Vec<Book>,
    /// Each maker's full ladder on each side as last quoted.
    full: Vec<(f64, f64)>,
    reports: Vec<MakerReport>,
}

impl Makers {
    /// Makers on `seats` (seat number and seat), quoting first around
    /// `reference_usd`. Setup: after clearing stale orders if asked, every
    /// maker quotes, so the bot's first look finds the book.
    pub async fn prepare(
        model: MakerModel,
        seats: Vec<(usize, Arc<Seat>)>,
        reference_usd: f64,
    ) -> Self {
        let reports = seats
            .iter()
            .map(|(_, seat)| MakerReport {
                venue: seat.venue.clone(),
                ..MakerReport::default()
            })
            .collect();
        let mut makers = Self {
            books: vec![Book::default(); seats.len()],
            full: vec![(model.ladder(), model.ladder()); seats.len()],
            model,
            seats,
            reports,
        };
        for i in 0..makers.seats.len() {
            if makers.model.clear_stale {
                makers.clear(i, reference_usd).await;
            }
            makers.quote(i, 0, reference_usd).await;
        }
        makers
    }

    pub fn model(&self) -> &MakerModel {
        &self.model
    }

    pub fn seat_numbers(&self) -> Vec<usize> {
        self.seats.iter().map(|(n, _)| *n).collect()
    }

    /// The reference moves by `permanent_bps` per this much net selling.
    pub fn impact_unit(&self) -> f64 {
        self.model
            .impact_unit
            .unwrap_or(self.model.ladder() * self.seats.len() as f64)
    }

    /// Take the snapshot's orders resting inside the first quotes around
    /// `reference_usd`: asks below its best offer, bids above its best bid.
    async fn clear(&mut self, i: usize, reference_usd: f64) {
        let seat = self.seats[i].1.clone();
        let (bids, asks) = self.model.quotes(
            &Book::default(),
            reference_usd,
            &self.model.shape(&seat.venue, 0),
        );
        let tick = seat.tick_usd();
        let report = &mut self.reports[i];
        for (side, inside) in [
            // Buy the asks below our offer, sell into the bids above our bid.
            (Side::Buy, asks[0].0 - tick),
            (Side::Sell, bids[0].0 + tick),
        ] {
            for _ in 0..CLEAR_ROUNDS {
                let resting = match seat.levels(side.opposite()).await {
                    Ok(levels) => levels,
                    Err(error) => {
                        warn!(venue = seat.venue, %error, "a maker couldn't read the book");
                        report.actions.failed += 1;
                        break;
                    }
                };
                let stale: f64 = resting
                    .iter()
                    .filter(|(price, _)| match side {
                        Side::Buy => *price <= inside,
                        Side::Sell => *price >= inside,
                    })
                    .map(|(_, size)| size)
                    .sum();
                if stale.is_nan() || stale <= 0.0 {
                    break;
                }
                report.actions.took += 1;
                match seat.take(side, stale, inside).await {
                    Ok(()) => match side {
                        Side::Buy => report.cleared_bought += stale,
                        Side::Sell => report.cleared_sold += stale,
                    },
                    Err(error) => {
                        warn!(venue = seat.venue, %error, "a maker's clearing take failed");
                        report.actions.failed += 1;
                        break;
                    }
                }
            }
        }
    }

    /// Re-place maker `i`'s ladder: cancel, then rest each side no closer
    /// than one tick inside the other side's best price.
    async fn quote(&mut self, i: usize, at: u64, reference_usd: f64) {
        let seat = self.seats[i].1.clone();
        let shape = self.model.shape(&seat.venue, at);
        let (mut bids, mut asks) = self.model.quotes(&self.books[i], reference_usd, &shape);
        self.full[i] = (
            self.model.full(&shape, 1.0 + shape.lean),
            self.model.full(&shape, 1.0 - shape.lean),
        );
        let report = &mut self.reports[i];
        report.actions.cancelled += 1;
        if let Err(error) = seat.cancel().await {
            warn!(venue = seat.venue, %error, "a maker's cancel failed");
            report.actions.failed += 1;
        }
        let tick = seat.tick_usd();
        let best_ask = seat.best(Side::Sell).await.ok().flatten();
        let best_bid = seat.best(Side::Buy).await.ok().flatten();
        if let Some(ask) = best_ask {
            for bid in &mut bids {
                bid.0 = bid.0.min(ask - tick);
            }
        }
        if let Some(bid) = best_bid {
            for ask in &mut asks {
                ask.0 = ask.0.max(bid + tick);
            }
        }
        let orders: Vec<(Side, f64, f64)> = bids
            .iter()
            .map(|(p, s)| (Side::Buy, *p, *s))
            .chain(asks.iter().map(|(p, s)| (Side::Sell, *p, *s)))
            .filter(|(_, p, s)| *p > 0.0 && *s > 0.0)
            .collect();
        let rested = seat.rest(&orders).await;
        report.actions.placed += rested.sent;
        report.actions.failed += rested.failed;
        if let Some(error) = rested.error {
            warn!(venue = seat.venue, %error, "a maker's quotes failed");
        }
        let book = self.books[i];
        report.steps.push(MakerStep {
            at,
            bid_usd: bids.first().map(|b| b.0),
            ask_usd: asks.first().map(|a| a.0),
            bid_base: bids.iter().map(|b| b.1).sum(),
            ask_base: asks.iter().map(|a| a.1).sum(),
            inventory: book.inventory,
        });
    }
}

impl Actor for Makers {
    fn before_flow<'a>(&'a mut self, _now: &'a Moment) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn after_blocks<'a>(&'a mut self, now: &'a Moment, flow: &'a StepFlow) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for i in 0..self.seats.len() {
                let (bought, sold) = flow
                    .seats
                    .get(self.seats[i].0)
                    .map_or((0.0, 0.0), |f| (f.bought, f.sold));
                self.reports[i].bought += bought;
                self.reports[i].sold += sold;
                self.books[i].step(&self.model, bought, sold, self.full[i]);
                self.quote(i, now.at, now.reference_usd).await;
            }
        })
    }

    fn report<'a>(&'a self, report: &'a mut ActorsReport) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for maker in &self.reports {
                report.actions.placed += maker.actions.placed;
                report.actions.cancelled += maker.actions.cancelled;
                report.actions.took += maker.actions.took;
                report.actions.failed += maker.actions.failed;
                report.makers.push(maker.clone());
            }
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn model() -> MakerModel {
        MakerModel {
            half_spread_bps: 10.0,
            spacing_bps: 5.0,
            sizes: vec![2.0, 3.0, 5.0],
            refill: 0.25,
            skew_bps: 20.0,
            hedge: 0.5,
            permanent_bps: 0.0,
            transient_bps: 0.0,
            transient_decay: 0.0,
            impact_unit: None,
            clear_stale: true,
            passive_impact: 0.0,
            schedule: Vec::new(),
        }
    }

    const FULL: (f64, f64) = (10.0, 10.0);

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn the_ladder_steps_out_from_the_reference() {
        let (bids, asks) = model().quotes(&Book::default(), 100.0, &Shape::default());
        let prices: Vec<f64> = bids.iter().map(|b| b.0).collect();
        assert!(close(prices[0], 99.90) && close(prices[1], 99.85) && close(prices[2], 99.80));
        assert!(close(asks[0].0, 100.10) && close(asks[2].0, 100.20));
        assert_eq!(
            bids.iter().map(|b| b.1).collect::<Vec<_>>(),
            [2.0, 3.0, 5.0]
        );
    }

    #[test]
    fn a_ladder_sold_into_refills_and_leans() {
        let m = model();
        let mut book = Book::default();
        // The bot sells 6 into the bids: 60% of the ladder gone, 25% back.
        book.step(&m, 6.0, 0.0, FULL);
        assert!(close(book.bid, 0.65));
        assert_eq!(book.ask, 1.0);
        // Long 6, half hedged away.
        assert!(close(book.inventory, 3.0));
        let (bids, asks) = m.quotes(&book, 100.0, &Shape::default());
        assert!(close(bids[0].1, 2.0 * 0.65));
        // 3 of a 10 ladder: the quotes lean 6 bps lower.
        assert!(close(asks[0].0, 100.0 * (1.0 - 0.0006) * 1.001));
        book.step(&m, 0.0, 0.0, FULL);
        assert!(close(book.bid, 0.90));
        book.step(&m, 0.0, 0.0, FULL);
        assert_eq!(book.bid, 1.0, "never more than the ladder");
        assert!(close(book.inventory, 0.75));
        // Selling more than the ladder empties it, no further.
        book.step(&m, 50.0, 0.0, FULL);
        assert!(close(book.bid, 0.25));
        let (_, asks) = m.quotes(&book, 100.0, &Shape::default());
        assert!(
            close(asks[0].0, 100.0 * (1.0 - 0.0020) * 1.001),
            "lean capped at one ladder"
        );
    }

    #[test]
    fn a_schedule_thins_widens_hides_and_leans_the_ladder() {
        let mut m = model();
        m.schedule = vec![
            Episode {
                venue: Some("openbook".into()),
                from_slot: 10,
                to_slot: 20,
                depth: 0.5,
                spread: 2.0,
                quoting: 0.5,
                lean: 0.0,
                offset_bps: 0.0,
            },
            Episode {
                venue: None,
                from_slot: 14,
                to_slot: 30,
                depth: 1.0,
                spread: 1.0,
                quoting: 1.0,
                lean: -0.1,
                offset_bps: 0.0,
            },
        ];
        assert_eq!(m.validate(), Ok(()));
        assert_eq!(m.shape("openbook", 9), Shape::default());
        assert_eq!(m.shape("phoenix", 12), Shape::default());
        let thin = m.shape("openbook", 15);
        assert_eq!((thin.depth, thin.spread, thin.quoting), (0.5, 2.0, 0.5));
        assert!(close(thin.lean, -0.1));
        let (bids, asks) = m.quotes(&Book::default(), 100.0, &thin);
        // Two of three levels show (half, rounded up), twice as far out,
        // half as deep, the asks 10% heavier and the bids 10% lighter.
        assert_eq!(bids.len(), 2);
        assert!(close(bids[0].0, 99.80) && close(asks[1].0, 100.30));
        assert!(close(bids[0].1, 2.0 * 0.5 * 0.9) && close(asks[1].1, 3.0 * 0.5 * 1.1));
        assert!(close(m.full(&thin, 1.1), 5.0 * 0.5 * 1.1));
        // A fill takes its share of the ladder as quoted: 1 of a 2.5 ladder.
        let mut book = Book::default();
        book.step(&m, 1.0, 0.0, (2.5, 2.5));
        assert!(close(book.bid, 1.0 - 0.4 + 0.25));
        let mut bad = m.clone();
        bad.schedule[0].to_slot = 10;
        assert!(bad.validate().is_err());
        let json = serde_json::to_value(&m.schedule[1]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "from_slot": 14, "to_slot": 30, "lean": -0.1 })
        );
    }

    #[test]
    fn an_offset_moves_one_venues_ladder_off_the_reference() {
        let mut m = model();
        let episode = |venue: &str, offset_bps| Episode {
            venue: Some(venue.into()),
            from_slot: 0,
            to_slot: 10,
            depth: 1.0,
            spread: 1.0,
            quoting: 1.0,
            lean: 0.0,
            offset_bps,
        };
        m.schedule = vec![episode("phoenix", 50.0), episode("phoenix", -20.0)];
        assert_eq!(m.validate(), Ok(()));
        let phoenix = m.shape("phoenix", 4);
        assert!(close(phoenix.offset_bps, 30.0), "offsets add");
        let (bids, asks) = m.quotes(&Book::default(), 100.0, &phoenix);
        assert!(close(bids[0].0, 100.3 * 0.999) && close(asks[0].0, 100.3 * 1.001));
        let (bids, _) = m.quotes(&Book::default(), 100.0, &m.shape("openbook", 4));
        assert!(
            close(bids[0].0, 99.9),
            "the other venue stays on the reference"
        );
        m.schedule[0].offset_bps = f64::NAN;
        assert!(m.validate().is_err());
    }

    #[test]
    fn the_model_is_checked() {
        assert_eq!(model().validate(), Ok(()));
        let mut bad = model();
        bad.refill = 1.5;
        assert!(bad.validate().is_err());
        bad = model();
        bad.sizes.clear();
        assert!(bad.validate().is_err());
        bad = model();
        bad.sizes = vec![1.0; MAX_LEVELS + 1];
        assert!(bad.validate().is_err());
        bad = model();
        bad.half_spread_bps = 0.0;
        assert!(bad.validate().is_err());
        let json = serde_json::to_value(model()).unwrap();
        assert!(json.get("impact_unit").is_none());
        let mut quiet = model();
        quiet.clear_stale = false;
        assert!(
            serde_json::to_value(quiet)
                .unwrap()
                .get("clear_stale")
                .is_none()
        );
    }
}
