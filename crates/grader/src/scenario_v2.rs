//! Scenario v2: a scenario file as independent sections.
//!
//! v1 is one flat [`Scenario`] whose kind-specific parts (making, offhours,
//! tape, spot, ...) are entangled: which of them may sit together is decided
//! by the code that branches on them. v2 groups the same facts by what they
//! describe, and each section checks only itself:
//!
//! | section        | holds                                                        |
//! |----------------|--------------------------------------------------------------|
//! | `assets`       | the traded symbol, a spot wallet's starting balances         |
//! | `venues`       | the perp venue's funding, the spot markets, liquidity, data  |
//! | `market_model` | the price path, the fair value, a closed period              |
//! | `oracles`      | the prices venues read, driven and failed on purpose         |
//! | `actors`       | the competing maker, the taker flow, a pools' arbitrageur    |
//! | `faults`       | execution faults on the bot's sends and reads                |
//! | `tx`           | the transaction layer: raw sends                             |
//! | `mandate`      | what the bot may trade and its limits                        |
//! | `order`        | a quantity the bot has to trade by the end                   |
//! | `scoring`      | the reference, the bound, charges and net scoring            |
//!
//! A new capability adds a section (or a field to the one it belongs to)
//! with its own `validate`; the rules that tie sections together are listed
//! at the end of [`ScenarioV2::validate`], one function each.
//!
//! A file says which version it is in `schema`; without it, it is v1. Both
//! load into the same in-memory [`Scenario`], which the session still
//! consumes, and v1 converts to v2 and back without loss. `Scenario::save`
//! writes v2.

use mtx_core::{
    Side,
    mandate::{InstrumentRef, Kind, Mandate},
};
use serde::{Deserialize, Serialize};

use crate::{
    faults::Faults,
    making::{Competition, Making, TAKER_ACCOUNTS, TakerOrder},
    offhours::OffHours,
    orders::{ExecutionOrder, NetScoring, Participation},
    phoenix::SLOTS_BETWEEN_STEPS,
    scenario::{
        Funding, MAX_PREMIUM_PCT, MIN_LEVEL_PCT, RawSends, Scenario, VENUE, Waypoint, move_slots,
    },
    spot::{MAX_DEPTH_LEVELS, POOLS, Spot, TurnData, VENUES},
};

/// The schema version this module reads and writes.
pub const SCHEMA: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScenarioV2 {
    /// Always [`SCHEMA`].
    pub schema: u32,
    pub name: String,
    pub description: String,
    /// Session length, counted from when the bot starts. The account is
    /// marked to market at this slot.
    pub duration_slots: u64,
    pub assets: Assets,
    pub venues: Venues,
    #[serde(default, skip_serializing_if = "MarketModel::is_empty")]
    pub market_model: MarketModel,
    /// The prices venues read (Phoenix's oracle, Pyth, Scope), driven along
    /// a path and failed on purpose (see [`crate::oracles`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracles: Option<crate::oracles::Oracles>,
    #[serde(default, skip_serializing_if = "Actors::is_empty")]
    pub actors: Actors,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faults: Option<Faults>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx: Option<Tx>,
    pub mandate: Mandate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ExecutionOrder>,
    #[serde(default, skip_serializing_if = "Scoring::is_empty")]
    pub scoring: Scoring,
}

/// What the session trades and holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Assets {
    /// The market's symbol: the perp's (`SPCX`) or the spot pair (`SOL-USDC`
    /// on the books, `SPCX-USDC` on the pools).
    pub symbol: String,
    /// A spot session's starting wallet. A perp session has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet: Option<StartingWallet>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StartingWallet {
    /// Wrapped SOL, in SOL.
    pub base: f64,
    /// USDC.
    pub quote: f64,
}

/// Where the bot trades. Exactly one of `perp` and `spot`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Venues {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perp: Option<PerpVenue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot: Option<SpotVenues>,
}

/// The perp venue (Phoenix) and what is set on it when the session starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PerpVenue {
    /// Phoenix's funding parameters for the session's market. Without it,
    /// the snapshot's own apply (hourly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SpotVenues {
    /// The venues whose market of the symbol the bot may trade: books
    /// (`phoenix`, `openbook`) for SOL-USDC, pools (`raydium`, `meteora`,
    /// `orca`) for SPCX-USDC.
    pub markets: Vec<String>,
    /// The pool venues among `markets` where the bot may provide liquidity:
    /// each adds its amm instrument to the universe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub liquidity: Vec<String>,
    /// What each step message carries beyond the clock. Without it, none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_data: Option<TurnData>,
}

/// The price the session replays, and what is known of its fair value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MarketModel {
    /// Marks relative to the session's starting mark. Empty in a spot session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<Waypoint>,
    /// Opt-in: the scenario's fair value, in the same terms as `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fair_path: Option<Vec<Waypoint>>,
    /// For a scenario where the underlying's own market is closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_period: Option<ClosedPeriod>,
}

/// The closed period before the underlying's market reopens, and Phoenix's
/// index through it (see [`crate::offhours`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClosedPeriod {
    pub reopen_slot: u64,
    pub band_pct: f64,
    pub half_spread_bps: f64,
    pub index: Vec<Waypoint>,
}

/// Who else trades: the maker competing with the bot, and the taker flow
/// the grader sends.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Actors {
    /// For a market-making scenario: the competing maker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub competitor: Option<Competitor>,
    /// The taker orders the grader sends from its own accounts, in slot
    /// order. In a market-making or closed-period scenario they are that
    /// scenario's flow; otherwise a tape that keeps the book price fresh.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub takers: Vec<TakerOrder>,
    /// For a spot scenario on pools: an arbitrageur that keeps each pool's
    /// price near the reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arbitrage: Option<crate::actors::ArbitrageSpec>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Competitor {
    /// The competing makers' half-spread around the mark, in basis points.
    pub competitor_bps: f64,
    /// How the grader's own maker competes.
    pub competition: Competition,
}

/// The transaction layer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tx {
    /// Opt-in: a strategy component may build transactions itself, for what
    /// its sheet can't say, and the grader signs them (see
    /// [`crate::raw`]). Absent, the session refuses every one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_sends: Option<RawSends>,
}

/// How the session is scored beyond the PnL itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Scoring {
    /// PnL the reference bot makes on this scenario; a score of 1 matches
    /// it. Written by `mtx calibrate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_pnl_usd: Option<f64>,
    /// A spot session's bound, which the reward divides the PnL by. Without
    /// it the reward is 0 and only `skill` against the reference is reported.
    /// A perp session's bound follows from its path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_usd: Option<f64>,
    /// A charge on the bot's share of a venue's traded volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participation: Option<Participation>,
    /// Score the PnL net of the starting holding's price drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<NetScoring>,
}

impl Assets {
    /// A symbol, and a wallet whose balances are numbers of at least 0.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(wallet) = &self.wallet {
            for (name, amount) in [("base", wallet.base), ("quote", wallet.quote)] {
                if !(amount.is_finite() && amount >= 0.0) {
                    return Err(format!("spot: {name} must be a number of at least 0"));
                }
            }
        }
        Ok(())
    }
}

impl Venues {
    /// A spot venue list that is known, unique and not empty, its step
    /// data asking for something sensible, and a perp venue's funding that
    /// settles.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(funding) = self.perp.as_ref().and_then(|p| p.funding.as_ref())
            && (funding.interval_seconds == 0
                || funding.period_seconds < funding.interval_seconds
                || funding.max_rate <= 0)
        {
            return Err(format!(
                "funding: needs an interval, a period at least as long, and a positive cap, not {funding:?}"
            ));
        }
        let Some(spot) = &self.spot else {
            return Ok(());
        };
        if spot.markets.is_empty() {
            return Err("spot: no venues".into());
        }
        for (i, venue) in spot.markets.iter().enumerate() {
            if !VENUES.contains(&venue.as_str()) {
                return Err(format!(
                    "spot: unknown venue {venue:?}; spot sessions trade on {VENUES:?}"
                ));
            }
            if spot.markets[..i].contains(venue) {
                return Err(format!("spot: {venue} is listed twice"));
            }
        }
        for (i, venue) in spot.liquidity.iter().enumerate() {
            if !POOLS.contains(&venue.as_str()) || !spot.markets.contains(venue) {
                return Err(format!(
                    "spot: liquidity on {venue:?}: only a session's pool venues ({POOLS:?}) take liquidity"
                ));
            }
            if spot.liquidity[..i].contains(venue) {
                return Err(format!("spot: liquidity on {venue} is listed twice"));
            }
        }
        if let Some(data) = &spot.turn_data {
            if !(data.tape || data.depth_levels > 0 || data.timestamps)
                || (data.parties && !data.tape)
            {
                return Err("turn_data: asks for nothing, or for parties without a tape".into());
            }
            if data.depth_levels > MAX_DEPTH_LEVELS {
                return Err(format!(
                    "turn_data: depth_levels {} is over {MAX_DEPTH_LEVELS}",
                    data.depth_levels
                ));
            }
        }
        Ok(())
    }

    /// The spot markets of `symbol`, then the amm instruments, as the
    /// mandate names them.
    fn spot_instruments(&self, symbol: &str) -> Vec<InstrumentRef> {
        let at = |venue: &String, kind| InstrumentRef {
            venue: venue.clone(),
            kind,
            symbol: symbol.into(),
        };
        self.spot
            .iter()
            .flat_map(|s| {
                s.markets
                    .iter()
                    .map(move |v| at(v, Kind::Spot))
                    .chain(s.liquidity.iter().map(move |v| at(v, Kind::Amm)))
            })
            .collect()
    }
}

impl MarketModel {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Waypoints in order, in range, and each move finished before the next
    /// starts; the fair value in order and inside the session; the closed
    /// period consistent with the path; the last move finished before the
    /// session ends. A closed period's taker orders (`takers`, the
    /// scenario's [`Actors::takers`]) are checked with it, as
    /// [`OffHours::validate`] does.
    pub fn validate(&self, duration_slots: u64, takers: &[TakerOrder]) -> Result<(), String> {
        let mut book = 0.0;
        let mut free_from = 0;
        // The move before the waypoint: from, to, and the slot it started.
        let mut before: Option<(f64, f64, u64)> = None;
        for (i, waypoint) in self.path.iter().enumerate() {
            if !(MIN_LEVEL_PCT..=100.0).contains(&waypoint.level_pct) {
                return Err(format!(
                    "waypoint {i}: level {}% is outside [{MIN_LEVEL_PCT}, 100]",
                    waypoint.level_pct
                ));
            }
            if waypoint.slot < free_from {
                let why = before.map_or(String::new(), |(from, to, at)| {
                    let parts = move_slots(from, to) / SLOTS_BETWEEN_STEPS;
                    format!(
                        ": the move before it, from {from}% to {to}% at slot {at}, lands in {parts} of Phoenix's 4% steps of {SLOTS_BETWEEN_STEPS} slots each"
                    )
                });
                return Err(format!(
                    "waypoint {i} starts at slot {} while the previous move runs until {free_from}{why}; start it at slot {free_from} or later, or make that move smaller",
                    waypoint.slot
                ));
            }
            if !(-MAX_PREMIUM_PCT..=MAX_PREMIUM_PCT).contains(&waypoint.premium_pct) {
                return Err(format!(
                    "waypoint {i}: premium {}% is outside [-{MAX_PREMIUM_PCT}, {MAX_PREMIUM_PCT}]",
                    waypoint.premium_pct
                ));
            }
            // A reopen lands at once, in one slot, however many of
            // Phoenix's steps it takes.
            free_from = match &self.closed_period {
                Some(closed) if waypoint.slot == closed.reopen_slot => {
                    waypoint.slot + SLOTS_BETWEEN_STEPS + 1
                }
                _ => waypoint.slot + move_slots(book, waypoint.book_pct()) + 1,
            };
            before = Some((book, waypoint.book_pct(), waypoint.slot));
            book = waypoint.book_pct();
        }
        if let Some(closed) = &self.closed_period {
            if self.path.iter().any(|w| w.premium_pct != 0.0) {
                return Err("an off-hours scenario's waypoints carry no premium".into());
            }
            closed
                .as_offhours(takers.to_vec())
                .validate(&self.path, duration_slots)
                .map_err(|reason| format!("offhours: {reason}"))?;
            if !self.path.iter().any(|w| w.slot == closed.reopen_slot) {
                return Err("offhours: the path has no waypoint at the reopen".into());
            }
        }
        if let Some(fair) = &self.fair_path {
            let mut previous = 0;
            for (i, waypoint) in fair.iter().enumerate() {
                if !(MIN_LEVEL_PCT..=100.0).contains(&waypoint.level_pct) {
                    return Err(format!(
                        "fair value waypoint {i}: level {}% is outside [{MIN_LEVEL_PCT}, 100]",
                        waypoint.level_pct
                    ));
                }
                if waypoint.slot < previous || waypoint.slot > duration_slots {
                    return Err(format!(
                        "fair value waypoint {i} at slot {} is out of order or past the session end",
                        waypoint.slot
                    ));
                }
                previous = waypoint.slot;
            }
        }
        if free_from > duration_slots {
            return Err(format!(
                "the last move runs until slot {free_from}, past the session end at {duration_slots}"
            ));
        }
        Ok(())
    }
}

impl ClosedPeriod {
    fn as_offhours(&self, takers: Vec<TakerOrder>) -> OffHours {
        OffHours {
            reopen_slot: self.reopen_slot,
            band_pct: self.band_pct,
            half_spread_bps: self.half_spread_bps,
            index: self.index.clone(),
            takers,
        }
    }
}

impl Actors {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// A competitor with a floor under its spread (and, with it, taker
    /// orders as [`Making::validate`] checks them); otherwise, unless the
    /// takers belong to a closed period, which checks them with its path
    /// (see [`MarketModel::validate`]), a tape in slot order, inside the
    /// session and well formed.
    pub fn validate(&self, duration_slots: u64, closed_period: bool) -> Result<(), String> {
        if let Some(arbitrage) = &self.arbitrage {
            arbitrage.validate()?;
        }
        if let Some(competitor) = &self.competitor {
            return Making {
                competitor_bps: competitor.competitor_bps,
                competition: competitor.competition,
                takers: self.takers.clone(),
            }
            .validate(duration_slots)
            .map_err(|reason| format!("making: {reason}"));
        }
        if closed_period {
            return Ok(());
        }
        let mut last = 0;
        for (i, order) in self.takers.iter().enumerate() {
            if order.slot < last || order.slot >= duration_slots {
                return Err(format!(
                    "tape order {i} at slot {} is out of order or past the session",
                    order.slot
                ));
            }
            if !(order.size.is_finite() && order.size > 0.0)
                || !(order.limit_bps.is_finite() && order.limit_bps > 0.0)
                || order.account >= TAKER_ACCOUNTS
            {
                return Err(format!("tape order {i} is malformed: {order:?}"));
            }
            last = order.slot;
        }
        Ok(())
    }
}

impl Scoring {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.bound_usd.is_some_and(|bound| !bound.is_finite()) {
            return Err("spot: bound_usd must be a number".into());
        }
        if let Some(participation) = &self.participation {
            participation.validate()?;
        }
        if let Some(net) = &self.net {
            net.validate()?;
        }
        Ok(())
    }
}

impl ScenarioV2 {
    /// Each section checks itself, then the rules that tie them together.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!(
                "schema: {} is not {SCHEMA}; a v1 file has no schema field",
                self.schema
            ));
        }
        self.mandate
            .validate()
            .map_err(|reason| format!("mandate: {reason}"))?;
        if let Some(oracles) = &self.oracles {
            oracles.validate(self.duration_slots)?;
            self.validate_oracles(oracles)?;
        }
        if let Some(raw) = self.tx.as_ref().and_then(|tx| tx.raw_sends.as_ref()) {
            raw.programs()
                .map_err(|reason| format!("raw_sends: {reason}"))?;
            if self.venues.spot.is_some() {
                return Err("raw_sends: only perp sessions take raw sends yet".into());
            }
        }
        self.venues_are_one_kind()?;
        self.flows_do_not_mix()?;
        if self.venues.spot.is_some() {
            self.spot_is_alone()?;
        } else {
            self.spot_fields_are_for_spot()?;
        }
        self.assets.validate()?;
        self.venues.validate()?;
        self.market_model
            .validate(self.duration_slots, &self.actors.takers)?;
        self.actors.validate(
            self.duration_slots,
            self.market_model.closed_period.is_some(),
        )?;
        if let Some(faults) = &self.faults {
            faults
                .validate(self.duration_slots.div_ceil(SLOTS_BETWEEN_STEPS))
                .map_err(|reason| format!("faults: {reason}"))?;
        }
        if let Some(order) = &self.order {
            order.validate()?;
        }
        self.scoring.validate()?;
        self.mandate_covers_the_market()?;
        self.order_fits_the_wallet()
    }

    /// Exactly one venue kind: a session is a perp's or a spot's.
    fn venues_are_one_kind(&self) -> Result<(), String> {
        match (&self.venues.perp, &self.venues.spot) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            _ => Err("venues: exactly one of perp and spot".into()),
        }
    }

    /// A spot scenario trades a pair its venues' profiles name from a
    /// wallet, and has no path or market making. A tape trades the session's
    /// first venue; an arbitrageur, its pools.
    fn spot_is_alone(&self) -> Result<(), String> {
        for venue in self.venues.spot.iter().flat_map(|s| &s.markets) {
            let named = crate::venue::books::named_markets(venue).map_err(|e| e.to_string())?;
            if !named.iter().any(|m| m.symbol == self.assets.symbol) {
                return Err(format!(
                    "spot: the symbol {} has no market on {venue}; its profile names {:?}",
                    self.assets.symbol,
                    named.iter().map(|m| m.symbol).collect::<Vec<_>>()
                ));
            }
        }
        if self.actors.arbitrage.is_some()
            && !self
                .venues
                .spot
                .iter()
                .flat_map(|s| &s.markets)
                .any(|v| POOLS.contains(&v.as_str()))
        {
            return Err("spot: an arbitrageur trades pools, and the session has none".into());
        }
        if self.assets.wallet.is_none() {
            return Err("spot: no starting wallet".into());
        }
        if self.duration_slots == 0 {
            return Err("spot: the session has no slots".into());
        }
        let market = &self.market_model;
        if !market.path.is_empty()
            || market.fair_path.is_some()
            || market.closed_period.is_some()
            || self.actors.competitor.is_some()
        {
            return Err(
                "spot: a spot scenario has no path, fair path, market making, off-hours or funding"
                    .into(),
            );
        }
        Ok(())
    }

    /// A perp scenario has no wallet, order, charge, net scoring or
    /// arbitrageur.
    fn spot_fields_are_for_spot(&self) -> Result<(), String> {
        if self.actors.arbitrage.is_some() {
            return Err("arbitrage: an arbitrageur trades a spot scenario's pools".into());
        }
        if self.assets.wallet.is_some()
            || self.order.is_some()
            || self.scoring.bound_usd.is_some()
            || self.scoring.participation.is_some()
            || self.scoring.net.is_some()
        {
            return Err("turn_data, order, participation and net are for spot scenarios".into());
        }
        Ok(())
    }

    /// The mandate covers exactly the scenario's markets: a session measures
    /// only those, so a wider universe would go unenforced. It has no term
    /// (the session sets it); a perp mandate has a drawdown limit.
    fn mandate_covers_the_market(&self) -> Result<(), String> {
        if self.venues.spot.is_some() {
            let mut wanted = self.venues.spot_instruments(&self.assets.symbol);
            let mut universe: Vec<InstrumentRef> = self
                .mandate
                .universe
                .iter()
                .map(|a| a.instrument.clone())
                .collect();
            wanted.sort();
            universe.sort();
            if wanted != universe {
                return Err(format!(
                    "mandate: the universe must be exactly the spot markets, {}",
                    wanted
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        } else {
            let market = self.perp_instrument();
            if self.mandate.universe.len() != 1 || self.mandate.allowance(&market).is_none() {
                return Err(format!(
                    "mandate: the universe must be exactly {market}, the scenario's market"
                ));
            }
        }
        if self.mandate.term.is_some() {
            return Err("mandate: a scenario's mandate has no term; the session sets it".into());
        }
        if self.venues.spot.is_none() && self.mandate.limits.max_drawdown_usd.is_none() {
            return Err("mandate: a perp scenario's mandate has a drawdown limit".into());
        }
        Ok(())
    }

    /// What a scenario's oracle feeds may do beside the rest of it.
    fn validate_oracles(&self, oracles: &crate::oracles::Oracles) -> Result<(), String> {
        use crate::oracles::Source;
        for feed in &oracles.feeds {
            match &feed.source {
                Source::Phoenix(phoenix) => {
                    if self.venues.spot.is_some() {
                        return Err("oracles: a Phoenix oracle is for perp scenarios".into());
                    }
                    // The feed's start price is the scenario market's mark.
                    if phoenix.symbol != self.assets.symbol {
                        return Err(format!(
                            "oracles: {} writes {}'s oracle, and the scenario's market is {}",
                            feed.name, phoenix.symbol, self.assets.symbol
                        ));
                    }
                    // The path and an off-hours reopen move this same oracle
                    // in steps sized from the level they know; a feed beside
                    // them would break those steps.
                    if !self.market_model.path.is_empty()
                        || self.market_model.closed_period.is_some()
                    {
                        return Err(format!(
                            "oracles: {} moves the oracle the scenario's own path moves; \
                             use one or the other",
                            feed.name
                        ));
                    }
                }
                // A spot session values the account at the chain's Pyth
                // SOL/USD. That is the grader's yardstick, not the bot's
                // market: a path on it is the market moving, but a failure
                // (stale, off price, delayed, wide) would change the score
                // without changing anything the bot did.
                Source::Pyth(pyth) if self.venues.spot.is_some() && !feed.failures.is_empty() => {
                    let valuation = mtx_kit::protocols::pyth::feed_account(
                        0,
                        &mtx_kit::protocols::pyth::SOL_USD_FEED_ID,
                    );
                    if pyth.address().ok() == Some(valuation) {
                        return Err(format!(
                            "oracles: {} is the feed a spot session values the account at; \
                             it can move but not fail",
                            feed.name
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn perp_instrument(&self) -> InstrumentRef {
        InstrumentRef {
            venue: VENUE.into(),
            kind: Kind::Perp,
            symbol: self.assets.symbol.clone(),
        }
    }

    /// A scenario's takers belong to one flow: a market-making scenario's, a
    /// closed period's, or a tape.
    fn flows_do_not_mix(&self) -> Result<(), String> {
        if self.actors.competitor.is_some() && self.market_model.closed_period.is_some() {
            return Err("a scenario has either making or offhours, not both".into());
        }
        Ok(())
    }

    /// A sell order can't be bigger than the wallet; a net-scored session
    /// has no order.
    fn order_fits_the_wallet(&self) -> Result<(), String> {
        if let (Some(order), Some(wallet)) = (&self.order, &self.assets.wallet)
            && order.side == Side::Sell
            && wallet.base < order.quantity
        {
            return Err(format!(
                "order: selling {} SOL from a wallet of {}",
                order.quantity, wallet.base
            ));
        }
        if self.scoring.net.is_some() && self.order.is_some() {
            return Err("net: a net-scored session has no order".into());
        }
        Ok(())
    }

    /// The in-memory scenario the session consumes. Checks the sections
    /// first, so what comes out is valid.
    pub fn into_scenario(self) -> Result<Scenario, String> {
        self.validate()?;
        let Self {
            name,
            description,
            duration_slots,
            assets,
            venues,
            market_model,
            oracles,
            actors,
            faults,
            tx,
            mandate,
            order,
            scoring,
            ..
        } = self;
        let MarketModel {
            path,
            fair_path,
            closed_period,
        } = market_model;
        let Actors {
            competitor,
            takers,
            arbitrage,
        } = actors;
        let (making, offhours, tape) = match (competitor, closed_period) {
            (Some(c), _) => (
                Some(Making {
                    competitor_bps: c.competitor_bps,
                    competition: c.competition,
                    takers,
                }),
                None,
                Vec::new(),
            ),
            (None, Some(closed)) => (None, Some(closed.as_offhours(takers)), Vec::new()),
            (None, None) => (None, None, takers),
        };
        let (spot, turn_data) = match (venues.spot, assets.wallet) {
            (Some(spot), Some(wallet)) => (
                Some(Spot {
                    venues: spot.markets,
                    liquidity: spot.liquidity,
                    arbitrage,
                    base: wallet.base,
                    quote: wallet.quote,
                    bound_usd: scoring.bound_usd,
                }),
                spot.turn_data,
            ),
            _ => (None, None),
        };
        Ok(Scenario {
            name,
            description,
            symbol: assets.symbol,
            duration_slots,
            mandate,
            path,
            making,
            offhours,
            funding: venues.perp.and_then(|p| p.funding),
            tape,
            spot,
            faults,
            turn_data,
            order,
            participation: scoring.participation,
            net: scoring.net,
            reference_pnl_usd: scoring.reference_pnl_usd,
            fair_path,
            raw_sends: tx.and_then(|tx| tx.raw_sends),
            oracles,
        })
    }
}

impl TryFrom<&Scenario> for ScenarioV2 {
    type Error = String;

    /// Regroup a v1 scenario. Fails only for what v2 can't say at all: the
    /// parts v1 let sit together that no kind of scenario uses.
    fn try_from(v1: &Scenario) -> Result<Self, String> {
        if v1.making.is_some() && v1.offhours.is_some() {
            return Err("a scenario has either making or offhours, not both".into());
        }
        if v1.making.is_some() && !v1.tape.is_empty() {
            return Err("a market-making scenario's takers are its making's, not a tape".into());
        }
        if v1.offhours.is_some() && !v1.tape.is_empty() {
            return Err("an off-hours scenario's takers are its offhours', not a tape".into());
        }
        let (venues, wallet) = match &v1.spot {
            Some(spot) => {
                if v1.making.is_some() || v1.offhours.is_some() || v1.funding.is_some() {
                    return Err("spot: a spot scenario has no path, fair path, market making, off-hours or funding".into());
                }
                (
                    Venues {
                        perp: None,
                        spot: Some(SpotVenues {
                            markets: spot.venues.clone(),
                            liquidity: spot.liquidity.clone(),
                            turn_data: v1.turn_data.clone(),
                        }),
                    },
                    Some(StartingWallet {
                        base: spot.base,
                        quote: spot.quote,
                    }),
                )
            }
            None => {
                if v1.turn_data.is_some()
                    || v1.order.is_some()
                    || v1.participation.is_some()
                    || v1.net.is_some()
                {
                    return Err(
                        "turn_data, order, participation and net are for spot scenarios".into(),
                    );
                }
                (
                    Venues {
                        perp: Some(PerpVenue {
                            funding: v1.funding,
                        }),
                        spot: None,
                    },
                    None,
                )
            }
        };
        let (competitor, closed_period, takers) = match (&v1.making, &v1.offhours) {
            (Some(making), _) => (
                Some(Competitor {
                    competitor_bps: making.competitor_bps,
                    competition: making.competition,
                }),
                None,
                making.takers.clone(),
            ),
            (None, Some(off)) => (
                None,
                Some(ClosedPeriod {
                    reopen_slot: off.reopen_slot,
                    band_pct: off.band_pct,
                    half_spread_bps: off.half_spread_bps,
                    index: off.index.clone(),
                }),
                off.takers.clone(),
            ),
            (None, None) => (None, None, v1.tape.clone()),
        };
        Ok(Self {
            schema: SCHEMA,
            name: v1.name.clone(),
            description: v1.description.clone(),
            duration_slots: v1.duration_slots,
            assets: Assets {
                symbol: v1.symbol.clone(),
                wallet,
            },
            venues,
            market_model: MarketModel {
                path: v1.path.clone(),
                fair_path: v1.fair_path.clone(),
                closed_period,
            },
            oracles: v1.oracles.clone(),
            actors: Actors {
                competitor,
                takers,
                arbitrage: v1.spot.as_ref().and_then(|s| s.arbitrage),
            },
            faults: v1.faults.clone(),
            tx: v1.raw_sends.clone().map(|raw_sends| Tx {
                raw_sends: Some(raw_sends),
            }),
            mandate: v1.mandate.clone(),
            order: v1.order.clone(),
            scoring: Scoring {
                reference_pnl_usd: v1.reference_pnl_usd,
                bound_usd: v1.spot.as_ref().and_then(|s| s.bound_usd),
                participation: v1.participation.clone(),
                net: v1.net.clone(),
            },
        })
    }
}

/// What a file says about its version, and nothing else.
#[derive(Deserialize)]
struct Sniff {
    #[serde(default)]
    schema: Option<serde_json::Value>,
}

/// A v1 file that states its version: `"schema": 1` is accepted, and the
/// rest is the flat v1 scenario. (A flattened struct can't deny unknown
/// fields itself, so what it leaves over is collected and refused here.)
#[derive(Deserialize)]
#[serde(try_from = "V1Fields")]
struct V1Stated(Scenario);

#[derive(Deserialize)]
struct V1Fields {
    #[allow(dead_code)]
    schema: u32,
    #[serde(flatten)]
    scenario: Scenario,
    #[serde(flatten)]
    left_over: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
}

impl TryFrom<V1Fields> for V1Stated {
    type Error = String;

    fn try_from(fields: V1Fields) -> Result<Self, String> {
        match fields.left_over.keys().next() {
            Some(field) => Err(format!("unknown field `{field}`")),
            None => Ok(Self(fields.scenario)),
        }
    }
}

/// Read a scenario file's text: v2 if it says `"schema": 2`, v1 if it has no
/// `schema` or says `"schema": 1`. Anything else is refused by name. The
/// text is parsed once, as the version it names, so errors point into the
/// file and a repeated key is an error; the version is read first and only
/// to choose the parser.
pub fn parse(contents: &str) -> Result<Scenario, ParseError> {
    let Sniff { schema } = serde_json::from_str(contents)?;
    match schema {
        None => Ok(serde_json::from_str(contents)?),
        Some(v) if v == 1 => Ok(serde_json::from_str::<V1Stated>(contents)?.0),
        Some(v) if v == 2 => serde_json::from_str::<ScenarioV2>(contents)?
            .into_scenario()
            .map_err(ParseError::Invalid),
        Some(other) => Err(ParseError::Schema(other)),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("schema {0} is not a scenario version this build reads (1 or 2)")]
    Schema(serde_json::Value),
    #[error("{0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        making::MakingFamily, scenario::ShockFamily, spot::SYMBOL, weekend::WeekendFamily,
    };

    fn v2(scenario: &Scenario) -> ScenarioV2 {
        ScenarioV2::try_from(scenario).unwrap()
    }

    fn shock() -> ScenarioV2 {
        v2(&ShockFamily::spcx().sample("s", 3))
    }

    fn making() -> ScenarioV2 {
        v2(&MakingFamily::spcx().sample("m", 3))
    }

    fn weekend() -> ScenarioV2 {
        v2(&WeekendFamily::spcx().sample("w", 3))
    }

    fn spot() -> ScenarioV2 {
        let mut s = shock();
        s.venues = Venues {
            perp: None,
            spot: Some(SpotVenues {
                markets: vec!["phoenix".into(), "openbook".into()],
                liquidity: Vec::new(),
                turn_data: None,
            }),
        };
        s.assets = Assets {
            symbol: SYMBOL.into(),
            wallet: Some(StartingWallet {
                base: 10.0,
                quote: 0.0,
            }),
        };
        s.market_model = MarketModel::default();
        s.mandate = Mandate::spot(&VENUES, SYMBOL, 10.0);
        s
    }

    #[test]
    fn every_kind_round_trips_through_v2_and_json() {
        for v1 in [
            ShockFamily::spcx().sample("s", 3),
            MakingFamily::spcx().sample("m", 3),
            WeekendFamily::spcx().sample("w", 3),
            ShockFamily::spcx().draw_marked("s", 4).0,
        ] {
            let as_v2 = v2(&v1);
            assert_eq!(as_v2.validate(), Ok(()), "{}", v1.name);
            let text = serde_json::to_string_pretty(&as_v2).unwrap();
            assert!(text.contains("\"schema\": 2"));
            assert_eq!(parse(&text).unwrap(), v1);
            assert_eq!(as_v2.clone().into_scenario().unwrap(), v1);
            let back: ScenarioV2 = serde_json::from_str(&text).unwrap();
            assert_eq!(back, as_v2);
        }
    }

    #[test]
    fn a_pools_scenario_keeps_its_liquidity_and_arbitrageur() {
        for check in ["smoke", "sell-only"] {
            let path = format!(
                "{}/../../bench/checks/pools/{check}.json",
                env!("CARGO_MANIFEST_DIR")
            );
            let v1 = parse(&std::fs::read_to_string(path).unwrap()).unwrap();
            let as_v2 = v2(&v1);
            assert_eq!(as_v2.validate(), Ok(()), "{check}");
            let listed = as_v2.venues.spot.as_ref().unwrap();
            assert_eq!(listed.liquidity.len(), 3, "{check}");
            assert!(as_v2.actors.arbitrage.is_some(), "{check}");
            assert_eq!(as_v2.clone().into_scenario().unwrap(), v1, "{check}");
            let text = serde_json::to_string_pretty(&as_v2).unwrap();
            assert_eq!(parse(&text).unwrap(), v1, "{check}");
        }
        let mut pools = v2(&parse(
            &std::fs::read_to_string(format!(
                "{}/../../bench/checks/pools/smoke.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap(),
        )
        .unwrap());
        pools.venues.spot.as_mut().unwrap().liquidity = vec!["phoenix".into()];
        assert!(
            pools
                .validate()
                .unwrap_err()
                .contains("only a session's pool venues")
        );
        let mut books = spot();
        books.actors.arbitrage = pools.actors.arbitrage;
        assert!(books.validate().unwrap_err().contains("arbitrageur"));
        books.assets.symbol = "SPCX-USDC".into();
        books.actors.arbitrage = None;
        assert!(
            books
                .validate()
                .unwrap_err()
                .contains("the symbol SPCX-USDC has no market on phoenix")
        );
    }

    #[test]
    fn a_file_without_a_schema_is_v1() {
        let v1 = ShockFamily::spcx().sample("s", 3);
        let text = serde_json::to_string(&v1).unwrap();
        assert_eq!(parse(&text).unwrap(), v1);
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["schema"] = 1.into();
        assert_eq!(parse(&value.to_string()).unwrap(), v1);
        value["schema"] = 3.into();
        assert!(
            parse(&value.to_string())
                .unwrap_err()
                .to_string()
                .contains("schema")
        );
        value["schema"] = "two".into();
        assert!(parse(&value.to_string()).is_err());
    }

    #[test]
    fn v1_parts_that_no_kind_uses_do_not_convert() {
        let mut both = MakingFamily::spcx().sample("m", 3);
        both.offhours = WeekendFamily::spcx().sample("w", 3).offhours;
        assert!(both.validate().unwrap_err().contains("either making"));
        let mut tape = MakingFamily::spcx().sample("m", 3);
        tape.tape = tape.making.as_ref().unwrap().takers.clone();
        assert!(tape.validate().unwrap_err().contains("not a tape"));
    }

    #[test]
    fn unknown_fields_and_sections_are_refused() {
        let mut value = serde_json::to_value(shock()).unwrap();
        value["market_model"]["nonsense"] = 1.into();
        assert!(parse(&value.to_string()).is_err());
        let mut value = serde_json::to_value(shock()).unwrap();
        value["nonsense"] = 1.into();
        assert!(parse(&value.to_string()).is_err());
    }

    #[test]
    fn each_section_reports_its_own_errors() {
        let mut s = shock();
        s.assets.wallet = Some(StartingWallet {
            base: 1.0,
            quote: 1.0,
        });
        assert!(s.validate().unwrap_err().contains("are for spot"));

        let mut s = spot();
        s.assets.wallet = Some(StartingWallet {
            base: -1.0,
            quote: 0.0,
        });
        assert!(s.assets.validate().unwrap_err().contains("base"));

        let mut s = shock();
        s.venues.perp = Some(PerpVenue {
            funding: Some(Funding {
                interval_seconds: 0,
                period_seconds: 10,
                max_rate: 1,
                mark: None,
            }),
        });
        assert!(s.venues.validate().unwrap_err().contains("funding"));

        let mut s = spot();
        let spot_venues = s.venues.spot.as_mut().unwrap();
        spot_venues.markets = vec!["phoenix".into(), "phoenix".into()];
        assert!(s.venues.validate().unwrap_err().contains("twice"));
        s.venues.spot.as_mut().unwrap().markets = vec![];
        assert!(s.venues.validate().unwrap_err().contains("no venues"));

        let mut s = shock();
        s.market_model.path.push(Waypoint::at(0, 5.0));
        assert!(
            s.market_model
                .validate(s.duration_slots, &s.actors.takers)
                .unwrap_err()
                .contains("previous move")
        );
        let mut s = shock();
        s.market_model.fair_path = Some(vec![Waypoint::at(s.duration_slots + 1, 1.0)]);
        assert!(
            s.market_model
                .validate(s.duration_slots, &s.actors.takers)
                .unwrap_err()
                .contains("fair value")
        );

        let mut s = making();
        s.actors.competitor.as_mut().unwrap().competitor_bps = 0.0;
        assert!(
            s.actors
                .validate(s.duration_slots, false)
                .unwrap_err()
                .contains("making: competitor_bps")
        );
        let mut s = making();
        s.actors.takers[0].account = TAKER_ACCOUNTS;
        assert!(
            s.actors
                .validate(s.duration_slots, false)
                .unwrap_err()
                .contains("making: taker order 0 uses account")
        );
        let mut s = shock();
        s.actors.takers = vec![TakerOrder {
            account: TAKER_ACCOUNTS,
            ..making().actors.takers[0]
        }];
        assert!(
            s.actors
                .validate(s.duration_slots, false)
                .unwrap_err()
                .contains("tape order 0 is malformed")
        );

        let mut s = shock();
        s.faults = Some(Faults {
            seed: 1,
            latency: None,
            congestion: None,
            blockhash_expiry: None,
            drop: Some(crate::faults::SendNumbers { sends: vec![0] }),
            lost_confirmation: None,
            rpc_unavailable: None,
        });
        assert!(s.validate().unwrap_err().contains("faults"));

        let mut s = shock();
        s.mandate.limits.max_drawdown_usd = None;
        assert!(s.validate().unwrap_err().contains("drawdown"));

        let mut s = shock();
        s.scoring.bound_usd = Some(f64::NAN);
        assert!(s.scoring.validate().is_err());
    }

    #[test]
    fn rules_across_sections_name_the_sections_they_join() {
        let mut s = shock();
        s.venues.spot = Some(SpotVenues {
            markets: vec!["phoenix".into()],
            liquidity: Vec::new(),
            turn_data: None,
        });
        assert!(
            s.validate()
                .unwrap_err()
                .contains("exactly one of perp and spot")
        );
        s.venues = Venues::default();
        assert!(s.validate().unwrap_err().contains("exactly one"));

        let mut s = weekend();
        s.actors.competitor = making().actors.competitor;
        assert!(s.validate().unwrap_err().contains("either making"));

        let mut s = spot();
        s.market_model.path.push(Waypoint::at(4, -1.0));
        assert!(s.validate().unwrap_err().contains("no path"));
        let mut s = spot();
        s.assets.symbol = "SOL".into();
        assert!(s.validate().unwrap_err().contains("symbol"));
        let mut s = spot();
        s.assets.wallet = None;
        assert!(s.validate().unwrap_err().contains("wallet"));

        let mut s = shock();
        s.assets.symbol = "SOL".into();
        assert!(s.validate().unwrap_err().contains("exactly"));

        let mut s = shock();
        s.schema = 1;
        assert!(s.validate().unwrap_err().contains("schema"));
    }

    #[test]
    fn an_empty_tx_section_takes_nothing_else() {
        let mut s = shock();
        s.tx = Some(Tx::default());
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"tx\":{}"));
        assert_eq!(serde_json::from_str::<ScenarioV2>(&text).unwrap(), s);
        assert!(
            serde_json::from_str::<ScenarioV2>(&text.replace("\"tx\":{}", "\"tx\":{\"x\":1}"))
                .is_err()
        );
    }

    #[test]
    fn raw_sends_live_in_the_tx_section_and_round_trip_through_v1() {
        let mut s = shock();
        s.tx = Some(Tx {
            raw_sends: Some(RawSends {
                programs: vec!["11111111111111111111111111111111".into()],
            }),
        });
        s.validate().unwrap();
        let v1 = s.clone().into_scenario().unwrap();
        assert_eq!(v1.raw_sends, s.tx.as_ref().unwrap().raw_sends);
        assert_eq!(ScenarioV2::try_from(&v1).unwrap(), s);
        s.tx.as_mut()
            .unwrap()
            .raw_sends
            .as_mut()
            .unwrap()
            .programs
            .clear();
        assert!(s.validate().unwrap_err().starts_with("raw_sends:"));
    }

    /// What main said for the same input, word for word.
    #[test]
    fn v1_inputs_keep_their_error_texts() {
        let err = |scenario: &Scenario| scenario.validate().unwrap_err();

        let mut m = MakingFamily::spcx().sample("m", 3);
        m.making.as_mut().unwrap().takers[1].slot = 0;
        m.making.as_mut().unwrap().takers[0].slot = 5;
        assert_eq!(err(&m), "making: taker order 1 is out of slot order");
        let mut m = MakingFamily::spcx().sample("m", 3);
        m.making.as_mut().unwrap().takers[0].size = 0.0;
        assert_eq!(err(&m), "making: taker order 0 has size 0");
        let mut m = MakingFamily::spcx().sample("m", 3);
        m.making.as_mut().unwrap().takers[0].account = 99;
        assert_eq!(err(&m), "making: taker order 0 uses account 99, of 8");
        let mut m = MakingFamily::spcx().sample("m", 3);
        m.making.as_mut().unwrap().competitor_bps = 0.0;
        assert_eq!(err(&m), "making: competitor_bps 0 must be positive");

        let mut w = WeekendFamily::spcx().sample("w", 3);
        let first = w.offhours.as_ref().unwrap().takers[0];
        w.offhours
            .as_mut()
            .unwrap()
            .takers
            .push(TakerOrder { slot: 0, ..first });
        let end = w.offhours.as_ref().unwrap().takers.len() - 1;
        assert_eq!(
            err(&w),
            format!("offhours: taker order {end} is out of order or past the session")
        );

        let mut t = ShockFamily::spcx().sample("s", 3);
        t.tape = vec![TakerOrder {
            slot: t.duration_slots,
            ..making().actors.takers[0]
        }];
        assert!(err(&t).starts_with("tape order 0 at slot"));
    }

    #[test]
    fn save_writes_v1_and_save_v2_writes_v2() {
        let dir = std::env::temp_dir().join(format!("mtx-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut v1 = MakingFamily::spcx().sample("m", 3);
        v1.reference_pnl_usd = Some(12.5);
        let path = dir.join("v1.json");
        v1.save(&path).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // The layout CI's jq reads.
        assert!(json.get("schema").is_none());
        assert_eq!(json["reference_pnl_usd"], 12.5);
        assert!(json["path"].as_array().is_some_and(|p| !p.is_empty()));
        assert_eq!(Scenario::load(&path).unwrap(), v1);

        let path = dir.join("v2.json");
        v1.save_v2(&path).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["schema"], 2);
        assert_eq!(Scenario::load(&path).unwrap(), v1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parsing_reads_the_text_once_as_the_version_it_names() {
        let v1 = ShockFamily::spcx().sample("s", 3);
        let text = serde_json::to_string_pretty(&v1).unwrap();

        // A repeated key is an error, in v1 and in v2.
        let repeated = text.replacen("\"name\"", "\"name\": \"x\",\n  \"name\"", 1);
        assert!(
            parse(&repeated)
                .unwrap_err()
                .to_string()
                .contains("duplicate field")
        );
        let text2 = serde_json::to_string_pretty(&v2(&v1)).unwrap();
        let repeated = text2.replacen("\"name\"", "\"name\": \"x\",\n  \"name\"", 1);
        assert!(
            parse(&repeated)
                .unwrap_err()
                .to_string()
                .contains("duplicate field")
        );

        // A syntax error points into the file.
        let broken = text.replacen("\"symbol\"", "\"symbol\" 1", 1);
        let message = parse(&broken).unwrap_err().to_string();
        assert!(
            message.contains("line ") && message.contains("column "),
            "{message}"
        );
        let unknown = text.replacen("\"symbol\"", "\"nonsense\": 1,\n  \"symbol\"", 1);
        assert!(parse(&unknown).unwrap_err().to_string().contains("line "));

        // `"schema": 1` is v1 and is accepted; the v1 field set still holds.
        let stated = text.replacen('{', "{\n  \"schema\": 1,", 1);
        assert_eq!(parse(&stated).unwrap(), v1);
        let stated_unknown = stated.replacen("\"symbol\"", "\"nonsense\": 1,\n  \"symbol\"", 1);
        assert!(parse(&stated_unknown).is_err());
    }
}
