//! Scenario actors: participants the grader runs, with funded accounts of
//! their own, that trade the scenario's market at fixed points of each step
//! and react to what the bot did.
//!
//! A scenario opts in with [`Actors`]. The session then onboards every seat
//! the actors need before the cheatcodes lock, whether or not an actor ever
//! uses it, so sessions of a scenario replay the same. While the bot runs,
//! each actor acts at two points of every step:
//!
//! 1. **Before the flow** ([`Actor::before_flow`]), at the start of the step,
//!    where a market-making session's takers trade.
//! 2. **After the blocks** ([`Actor::after_blocks`]), once the step's blocks
//!    have landed and before the bot's turn, with what everyone traded in
//!    the step ([`StepFlow`]).
//!
//! An actor that must also act around each of the bot's transactions gives
//! the bot's guard a [`crate::guard::SendHook`].
//!
//! Three actors come with the engine: [`makers`], a ladder of quotes per
//! venue that refills, leans with inventory and moves with the flow it
//! absorbs; [`ahead`], a trader that trades the bot's way before it; and
//! [`tape`], a spot scenario's listed taker orders. A family sets their
//! parameters, and may add actors of its own ([`Cast::add`]) on seats it
//! onboards ([`Seat::spot`], [`Cast::add_seat`]).
//!
//! Every actor follows the same rules:
//!
//! - It prices off the session's reference ([`Moment::reference_usd`]: the
//!   scenario's level, moved by the flow the makers absorbed) and the makers'
//!   gaps around it, set once at setup. Never off the live book, which lags
//!   in a move and which resting orders, the bot's included, could steer.
//! - A resting order never crosses the other side's best price: it is
//!   clamped one tick inside it.
//! - A take by an account that also rests orders cancels its own resting
//!   order rather than trade with it (`CancelProvide`).
//! - Every place, cancel and take counts as an action. A session where more
//!   than [`MAX_FAILED_SHARE`] of them failed didn't see the market its
//!   scenario describes, and the grade fails as infrastructure.

pub mod ahead;
pub mod family;
pub mod makers;
pub mod seats;
pub mod tape;

use std::{future::Future, sync::Arc};

pub use ahead::{Ahead, Counterparty, CounterpartyReport};
pub use family::{FamilyPlan, FamilySetup};
use futures::future::BoxFuture;
pub use makers::{ImpactSource, MakerModel, MakerReport, Makers};
use mtx_core::Side;
pub use seats::Seat;
use serde::{Deserialize, Serialize};
pub use tape::{Tape, TapeReport};

use crate::{Result, making::TakerOrder};

/// The share of the actors' actions that may fail before the session isn't
/// scored.
pub const MAX_FAILED_SHARE: f64 = 0.1;

/// A scenario's actors.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Actors {
    /// Makers quoting on every venue the session trades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub makers: Option<MakerModel>,
    /// Traders that react to the bot's fills.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub counterparties: Vec<Counterparty>,
    /// A spot session: Pyth's SOL/USD price follows the scenario's path,
    /// written at each waypoint's slot, so the account is valued at the
    /// scenario's level.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub oracle: bool,
    /// With `oracle`, Pyth's own path instead of the scenario's: an oracle
    /// that trails or lags the level the makers quote around.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub oracle_path: Vec<crate::scenario::Waypoint>,
}

impl Actors {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.oracle_path.is_empty() && !self.oracle {
            return Err("actors: an oracle_path needs oracle".into());
        }
        if let Some(makers) = &self.makers {
            makers.validate()?;
        } else if !self.counterparties.is_empty() {
            return Err("actors: counterparties price off the makers' quotes; add makers".into());
        }
        for (i, counterparty) in self.counterparties.iter().enumerate() {
            counterparty
                .validate()
                .map_err(|e| format!("actors: counterparty {i}: {e}"))?;
        }
        Ok(())
    }
}

/// Where a step stands for the actors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moment {
    /// Slots into the session.
    pub at: u64,
    /// The scenario's level, moved by the flow the makers absorbed, in USD.
    pub reference_usd: f64,
}

/// What someone bought and sold on one venue in a step, in base units.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct VenueFlow {
    pub venue: String,
    pub bought: f64,
    pub sold: f64,
}

/// What traded in a step: the bot's fills, and each seat's, by seat number.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepFlow {
    pub bot: Vec<VenueFlow>,
    pub seats: Vec<VenueFlow>,
    /// The bot's fills as the maker, against its own resting orders (spot
    /// only), also counted in `bot`.
    pub bot_passive: Vec<VenueFlow>,
}

impl StepFlow {
    /// The bot's trading on `side`, over every venue.
    pub fn bot_traded(&self, side: Side) -> f64 {
        self.bot
            .iter()
            .map(|f| match side {
                Side::Buy => f.bought,
                Side::Sell => f.sold,
            })
            .sum()
    }

    /// The bot's trading on `side` on `venue`.
    pub fn bot_on(&self, venue: &str, side: Side) -> f64 {
        self.bot
            .iter()
            .filter(|f| f.venue == venue)
            .map(|f| match side {
                Side::Buy => f.bought,
                Side::Sell => f.sold,
            })
            .sum()
    }

    fn add(flows: &mut Vec<VenueFlow>, venue: &str, side: Side, base: f64) {
        let i = match flows.iter().position(|f| f.venue == venue) {
            Some(i) => i,
            None => {
                flows.push(VenueFlow {
                    venue: venue.into(),
                    ..VenueFlow::default()
                });
                flows.len() - 1
            }
        };
        match side {
            Side::Buy => flows[i].bought += base,
            Side::Sell => flows[i].sold += base,
        }
    }

    /// Note a fill of the bot's.
    pub fn add_bot(&mut self, venue: &str, side: Side, base: f64) {
        Self::add(&mut self.bot, venue, side, base);
    }

    /// Note a fill of the bot's resting order (also a fill of the bot's).
    pub fn add_bot_passive(&mut self, venue: &str, side: Side, base: f64) {
        Self::add(&mut self.bot_passive, venue, side, base);
    }
}

/// A participant the grader runs.
pub trait Actor: Send {
    /// The start of a step, before the takers trade.
    fn before_flow<'a>(&'a mut self, now: &'a Moment) -> BoxFuture<'a, ()>;
    /// After the step's blocks, before the bot's turn.
    fn after_blocks<'a>(&'a mut self, now: &'a Moment, flow: &'a StepFlow) -> BoxFuture<'a, ()>;
    /// What it did, into the session's report.
    fn report<'a>(&'a self, report: &'a mut ActorsReport) -> BoxFuture<'a, ()>;
}

/// Counts of an actor's actions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Actions {
    pub placed: usize,
    pub cancelled: usize,
    pub took: usize,
    /// OpenBook cranks sent.
    pub cranked: usize,
    pub failed: usize,
}

impl Actions {
    pub fn total(&self) -> usize {
        self.placed + self.cancelled + self.took + self.cranked
    }
}

/// What the actors did in a session.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ActorsReport {
    /// Every actor's actions, summed.
    pub actions: Actions,
    /// The reference at each step, in USD.
    pub reference_usd: Vec<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub makers: Vec<MakerReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub counterparties: Vec<CounterpartyReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tape: Option<TapeReport>,
    /// What a family's own actors report, by name.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

impl ActorsReport {
    /// More of the actors' actions failed than a session may lose.
    pub fn degraded(&self) -> bool {
        self.actions.failed as f64 > MAX_FAILED_SHARE * self.actions.total() as f64
    }
}

/// The reference the actors price off: the scenario's level, moved by the
/// flow the makers absorb. Each unit of net selling into the makers, in
/// [`MakerModel::impact_unit`]s, lowers it by `permanent_bps` for good and
/// by `transient_bps` more that decays by `transient_decay` a step.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Impact {
    pub permanent: f64,
    pub transient: f64,
}

/// Neither part of the impact moves the reference by more than this.
const MAX_IMPACT: f64 = 0.5;

impl Impact {
    /// One step: `sold` is the net base the makers bought from others.
    pub fn step(&mut self, model: &MakerModel, sold: f64, unit: f64) {
        self.transient *= 1.0 - model.transient_decay;
        if unit > 0.0 {
            self.permanent -= model.permanent_bps / 1e4 * sold / unit;
            self.transient -= model.transient_bps / 1e4 * sold / unit;
        }
        self.permanent = self.permanent.clamp(-MAX_IMPACT, MAX_IMPACT);
        self.transient = self.transient.clamp(-MAX_IMPACT, MAX_IMPACT);
    }

    pub fn multiplier(&self) -> f64 {
        1.0 + self.permanent + self.transient
    }
}

/// The flow that moves the reference in a step, as net base sold to the
/// market's liquidity: each impact seat's net buying at its factor, the
/// bot's own passive fills at `passive`, and with
/// [`ImpactSource::BotOnly`] the bot's aggressive fills at 1 instead of the
/// makers' seats (`makers`), whose flow then counts only if a family set it.
pub fn absorbed(
    seats: &[(usize, f64)],
    makers: &[usize],
    passive: f64,
    source: ImpactSource,
    flow: &StepFlow,
) -> f64 {
    let net = |flows: &[VenueFlow]| flows.iter().map(|f| f.bought - f.sold).sum::<f64>();
    let counted = |n: usize| source == ImpactSource::Absorbed || !makers.contains(&n);
    let held: f64 = seats
        .iter()
        .filter(|(n, _)| counted(*n))
        .filter_map(|&(n, factor)| flow.seats.get(n).map(|f| factor * (f.bought - f.sold)))
        .sum();
    let passive_net = net(&flow.bot_passive);
    let taken = match source {
        // What the bot took is already in the makers' fills.
        ImpactSource::Absorbed => 0.0,
        // Sold at the market is sold to its liquidity.
        ImpactSource::BotOnly => -(net(&flow.bot) - passive_net),
    };
    held + passive * passive_net + taken
}

/// A session's actors and the reference they share.
pub struct Cast {
    actors: Vec<Box<dyn Actor>>,
    /// The actors (by index) that go first at the start of each step.
    early: Vec<usize>,
    model: Option<MakerModel>,
    unit: f64,
    impact: Impact,
    /// The seats whose net buying moves the reference, each with the share
    /// of the usual impact it carries: the makers' at 1.
    impact_seats: Vec<(usize, f64)>,
    /// The makers' seats among them, until a family sets one explicitly.
    maker_seats: Vec<usize>,
    references: Vec<f64>,
    /// A seat on OpenBook, if any, which cranks its market after each
    /// step's blocks.
    cranker: Option<Arc<Seat>>,
    cranks: Actions,
    /// Each seat's position at the last step, where the flow is read from
    /// positions.
    positions: Vec<f64>,
    seats: Vec<Arc<Seat>>,
}

impl Cast {
    fn new(makers: Option<Makers>, mut actors: Vec<Box<dyn Actor>>, seats: Vec<Arc<Seat>>) -> Self {
        let model = makers.as_ref().map(|m| m.model().clone());
        let unit = makers.as_ref().map_or(0.0, Makers::impact_unit);
        let impact_seats = makers
            .as_ref()
            .map(Makers::seat_numbers)
            .unwrap_or_default()
            .into_iter()
            .map(|n| (n, 1.0))
            .collect::<Vec<_>>();
        let maker_seats = impact_seats.iter().map(|(n, _)| *n).collect();
        if let Some(makers) = makers {
            actors.insert(0, Box::new(makers));
        }
        Self {
            actors,
            early: Vec::new(),
            model,
            unit,
            impact: Impact::default(),
            impact_seats,
            maker_seats,
            references: Vec::new(),
            cranker: seats.iter().find(|s| s.venue == "openbook").cloned(),
            cranks: Actions::default(),
            positions: Vec::new(),
            seats,
        }
    }

    /// Onboard the seats `actors` need on `venues` and the seats `tape`
    /// sends from (setup only: `onboard` may use cheatcodes), and have the
    /// makers quote around `reference_usd`. Seats are numbered as
    /// onboarded: the makers' first, one per venue, then each
    /// counterparty's, then the tape's.
    pub async fn prepare<F, Fut>(
        actors: &Actors,
        venues: &[String],
        tape: &[TakerOrder],
        mut onboard: F,
        reference_usd: f64,
    ) -> Result<Self>
    where
        F: FnMut(String) -> Fut,
        Fut: Future<Output = Result<Seat>>,
    {
        let mut seats: Vec<Arc<Seat>> = Vec::new();
        let mut groups: Vec<Vec<(usize, Arc<Seat>)>> = Vec::new();
        let wanted = usize::from(actors.makers.is_some()) + actors.counterparties.len();
        for _ in 0..wanted {
            let mut group = Vec::new();
            for venue in venues {
                seats.push(Arc::new(onboard(venue.clone()).await?));
                group.push((seats.len() - 1, seats[seats.len() - 1].clone()));
            }
            groups.push(group);
        }
        let mut tape_seats = Vec::new();
        for (account, venue) in Tape::seats_for(tape, venues) {
            seats.push(Arc::new(onboard(venue.clone()).await?));
            tape_seats.push(((account, venue), seats[seats.len() - 1].clone()));
        }
        let mut groups = groups.into_iter();
        let makers = match &actors.makers {
            Some(model) => Some(
                Makers::prepare(
                    model.clone(),
                    groups.next().unwrap_or_default(),
                    reference_usd,
                )
                .await,
            ),
            None => None,
        };
        let half_spread = actors.makers.as_ref().map_or(0.0, |m| m.half_spread_bps);
        let mut others: Vec<Box<dyn Actor>> = actors
            .counterparties
            .iter()
            .zip(groups)
            .map(|(c, group)| Box::new(Ahead::new(c.clone(), group, half_spread)) as Box<dyn Actor>)
            .collect();
        if !tape.is_empty() {
            others.push(Box::new(Tape::new(tape.to_vec(), venues, tape_seats)));
        }
        Ok(Self::new(makers, others, seats))
    }

    /// Every seat, by seat number: a step's flow lists them in this order.
    pub fn seats(&self) -> &[Arc<Seat>] {
        &self.seats
    }

    /// Add a seat a family onboarded itself (at setup, like the others).
    /// Returns its seat number.
    pub fn add_seat(&mut self, seat: Seat) -> (usize, Arc<Seat>) {
        let seat = Arc::new(seat);
        self.seats.push(seat.clone());
        if self.cranker.is_none() && seat.venue == "openbook" {
            self.cranker = Some(seat.clone());
        }
        (self.seats.len() - 1, seat)
    }

    /// Let seat `seat`'s net buying move the reference at `factor` of the
    /// usual impact, as the makers' does at 1 (a seat's flow moves nothing
    /// unless set).
    pub fn set_seat_impact(&mut self, seat: usize, factor: f64) {
        self.impact_seats.retain(|(n, _)| *n != seat);
        self.impact_seats.push((seat, factor));
        self.maker_seats.retain(|n| *n != seat);
    }

    /// Add a family's own actor: it acts after the engine's at each point.
    pub fn add(&mut self, actor: Box<dyn Actor>) {
        self.actors.push(actor);
    }

    /// Add a family's own actor that acts before every other at the start
    /// of each step (before the tape's takers trade, say), and after the
    /// engine's after the blocks, as [`add`](Self::add)'s do.
    pub fn add_first(&mut self, actor: Box<dyn Actor>) {
        self.early.push(self.actors.len());
        self.actors.push(actor);
    }

    /// A step's flow on a market whose fills the session doesn't read one
    /// by one (a perp's): what the bot's position and each seat's moved by
    /// since the last step.
    pub async fn flow_from_positions(&mut self, venue: &str, bot_moved: f64) -> StepFlow {
        let mut flow = StepFlow::default();
        let side = |moved: f64| if moved > 0.0 { Side::Buy } else { Side::Sell };
        if bot_moved != 0.0 {
            flow.add_bot(venue, side(bot_moved), bot_moved.abs());
        }
        self.positions.resize(self.seats.len(), 0.0);
        for (i, seat) in self.seats.iter().enumerate() {
            let now = seat.position().await.unwrap_or(self.positions[i]);
            let moved = now - self.positions[i];
            self.positions[i] = now;
            let mut seat_flow = VenueFlow {
                venue: seat.venue.clone(),
                ..VenueFlow::default()
            };
            match side(moved) {
                Side::Buy => seat_flow.bought = moved.abs(),
                Side::Sell => seat_flow.sold = moved.abs(),
            }
            flow.seats.push(seat_flow);
        }
        flow
    }

    /// Crank OpenBook, so makers' fills, the bot's included, settle into
    /// their accounts.
    pub async fn crank(&mut self) {
        if let Some(seat) = &self.cranker {
            self.cranks.cranked += 1;
            if let Err(error) = seat.crank().await {
                tracing::warn!(%error, "an OpenBook crank failed");
                self.cranks.failed += 1;
            }
        }
    }

    /// The reference at `level_usd`.
    pub fn moment(&self, at: u64, level_usd: f64) -> Moment {
        Moment {
            at,
            reference_usd: level_usd * self.impact.multiplier(),
        }
    }

    pub async fn before_flow(&mut self, at: u64, level_usd: f64) {
        let now = self.moment(at, level_usd);
        for &i in &self.early {
            self.actors[i].before_flow(&now).await;
        }
        for (i, actor) in self.actors.iter_mut().enumerate() {
            if !self.early.contains(&i) {
                actor.before_flow(&now).await;
            }
        }
    }

    pub async fn after_blocks(&mut self, at: u64, level_usd: f64, flow: &StepFlow) {
        if let Some(model) = &self.model {
            self.impact.step(
                model,
                absorbed(
                    &self.impact_seats,
                    &self.maker_seats,
                    model.passive_impact,
                    model.impact_source,
                    flow,
                ),
                self.unit,
            );
        }
        let now = self.moment(at, level_usd);
        self.references.push(now.reference_usd);
        for actor in &mut self.actors {
            actor.after_blocks(&now, flow).await;
        }
    }

    pub async fn report(&self) -> ActorsReport {
        let mut report = ActorsReport {
            actions: self.cranks,
            reference_usd: self.references.clone(),
            ..ActorsReport::default()
        };
        for actor in &self.actors {
            actor.report(&mut report).await;
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selling_into_the_makers_lowers_the_reference_and_the_transient_part_decays() {
        let model = MakerModel {
            permanent_bps: 10.0,
            transient_bps: 20.0,
            transient_decay: 0.5,
            ..makers::tests::model()
        };
        let mut impact = Impact::default();
        // Two units sold: -20 bps for good, -40 bps that decays.
        impact.step(&model, 20.0, 10.0);
        assert!((impact.permanent + 0.0020).abs() < 1e-12);
        assert!((impact.transient + 0.0040).abs() < 1e-12);
        assert!((impact.multiplier() - 0.9940).abs() < 1e-12);
        impact.step(&model, 0.0, 10.0);
        assert!((impact.transient + 0.0020).abs() < 1e-12);
        // Buying it back moves it up again.
        impact.step(&model, -20.0, 10.0);
        assert!((impact.permanent).abs() < 1e-12);
        impact.step(&model, 1e9, 1.0);
        assert_eq!(impact.permanent, -MAX_IMPACT);
    }

    #[test]
    fn the_absorbed_flow_weighs_each_seat_and_the_bots_resting_orders() {
        let flow = |venue: &str, bought: f64, sold: f64| VenueFlow {
            venue: venue.into(),
            bought,
            sold,
        };
        let step = StepFlow {
            bot: vec![flow("phoenix", 1.0, 6.0)],
            seats: vec![
                flow("phoenix", 4.0, 0.0),
                flow("phoenix", 2.0, 0.0),
                flow("phoenix", 9.0, 0.0),
            ],
            bot_passive: vec![flow("phoenix", 1.0, 0.0)],
        };
        let all = ImpactSource::Absorbed;
        // Today's default: the makers' seat alone.
        assert_eq!(absorbed(&[(0, 1.0)], &[0], 0.0, all, &step), 4.0);
        // A wall's seat at 0.3, and the bot's resting bid at half.
        let both = [(0, 1.0), (1, 0.3)];
        assert!((absorbed(&both, &[0], 0.5, all, &step) - 5.1).abs() < 1e-12);
        // The bot's flow only: 6 sold at the market, its resting bid's 1 at
        // half, the wall's seat as set; the makers' fills (the tape's) not.
        let bot = ImpactSource::BotOnly;
        assert!((absorbed(&both, &[0], 0.5, bot, &step) - 7.1).abs() < 1e-12);
        assert_eq!(absorbed(&[(0, 1.0)], &[0], 0.0, bot, &step), 6.0);
        // A maker seat a family set explicitly counts again.
        assert_eq!(absorbed(&[(0, 1.0)], &[], 0.0, bot, &step), 10.0);
    }

    /// Notes when it acts, under its name.
    struct Noting(&'static str, Arc<std::sync::Mutex<Vec<String>>>);

    impl Actor for Noting {
        fn before_flow<'a>(&'a mut self, _now: &'a Moment) -> BoxFuture<'a, ()> {
            Box::pin(async move { self.1.lock().unwrap().push(format!("{} start", self.0)) })
        }
        fn after_blocks<'a>(&'a mut self, _: &'a Moment, _: &'a StepFlow) -> BoxFuture<'a, ()> {
            Box::pin(async move { self.1.lock().unwrap().push(format!("{} blocks", self.0)) })
        }
        fn report<'a>(&'a self, _: &'a mut ActorsReport) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn an_actor_added_first_goes_first_at_the_step_start() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let note = |name| Box::new(Noting(name, log.clone())) as Box<dyn Actor>;
        let mut cast = Cast::new(None, vec![note("tape")], Vec::new());
        cast.add(note("late"));
        cast.add_first(note("walls"));
        cast.before_flow(0, 100.0).await;
        cast.after_blocks(2, 100.0, &StepFlow::default()).await;
        assert_eq!(
            *log.lock().unwrap(),
            [
                "walls start",
                "tape start",
                "late start",
                "tape blocks",
                "late blocks",
                "walls blocks"
            ]
        );
    }

    #[test]
    fn counterparties_need_makers() {
        let ahead = ahead::tests::counterparty();
        let alone = Actors {
            makers: None,
            counterparties: vec![ahead.clone()],
            ..Actors::default()
        };
        assert!(alone.validate().unwrap_err().contains("makers"));
        let both = Actors {
            makers: Some(makers::tests::model()),
            counterparties: vec![ahead],
            ..Actors::default()
        };
        let mut lagging = both.clone();
        lagging.oracle_path = vec![crate::scenario::Waypoint::at(8, -1.0)];
        assert!(lagging.validate().unwrap_err().contains("needs oracle"));
        lagging.oracle = true;
        assert_eq!(lagging.validate(), Ok(()));
        assert_eq!(both.validate(), Ok(()));
        assert_eq!(
            Actors::default().validate(),
            Ok(()),
            "a tape needs no other actor"
        );
        let json = serde_json::to_string(&both).unwrap();
        assert_eq!(serde_json::from_str::<Actors>(&json).unwrap(), both);
    }

    #[test]
    fn a_session_whose_actors_failed_is_degraded() {
        let mut report = ActorsReport {
            actions: Actions {
                placed: 90,
                cancelled: 10,
                failed: 10,
                ..Actions::default()
            },
            ..ActorsReport::default()
        };
        assert!(!report.degraded());
        report.actions.failed = 11;
        assert!(report.degraded());
    }

    #[test]
    fn the_flow_sums_by_venue_and_side() {
        let mut flow = StepFlow::default();
        flow.add_bot("phoenix", Side::Sell, 1.0);
        flow.add_bot("openbook", Side::Sell, 2.0);
        flow.add_bot("phoenix", Side::Sell, 0.5);
        flow.add_bot("phoenix", Side::Buy, 0.25);
        assert_eq!(flow.bot_traded(Side::Sell), 3.5);
        assert_eq!(flow.bot_on("phoenix", Side::Sell), 1.5);
        assert_eq!(flow.bot_on("phoenix", Side::Buy), 0.25);
    }
}
