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
//! | `venues`       | the perp venue's funding, the spot markets, liquidity and data, the lending markets, the stake pools |
//! | `market_model` | the price path, the fair value, a closed period, the calibration record, the clock's jumps |
//! | `oracles`      | the prices venues read, driven and failed on purpose         |
//! | `actors`       | the competing maker, the taker flow, a pools' arbitrageur, the actor library |
//! | `faults`       | execution faults on the bot's sends and reads                |
//! | `tx`           | the transaction layer: fees and blocks, raw sends            |
//! | `mandate`      | what the bot may trade and its limits                        |
//! | `order`        | a quantity the bot has to trade by the end                   |
//! | `scoring`      | the reference, the bound, charges, net scoring, stop-outs    |
//!
//! A new capability adds a section (or a field to the one it belongs to)
//! with its own `validate`; the rules that tie sections together are listed
//! at the end of [`ScenarioV2::validate`], one function each.
//!
//! A file says which version it is in `schema`; without it, it is v1. Both
//! load into the same in-memory [`Scenario`], which the session still
//! consumes, and v1 converts to v2 and back without loss. `Scenario::save`
//! writes v2.

use std::collections::BTreeMap;

use mtx_core::{
    Side,
    mandate::{InstrumentRef, Kind, Mandate},
};
use serde::{Deserialize, Serialize};

use crate::{
    faults::Faults,
    lending::{Lending, Warp},
    making::{Competition, Making, TAKER_ACCOUNTS, TakerOrder},
    offhours::OffHours,
    orders::{CloseOut, ExecutionOrder, NetScoring, Participation},
    phoenix::SLOTS_BETWEEN_STEPS,
    scenario::{
        Funding, MAX_PREMIUM_PCT, MIN_LEVEL_PCT, RawSends, Scenario, StopOut, StopRule, VENUE,
        Waypoint, move_slots,
    },
    spot::{MAX_DEPTH_LEVELS, POOLS, Spot, TurnData, VENUES},
    stake::Stake,
    txlayer::TxLayer,
};

/// The schema version this module reads and writes.
pub const SCHEMA: u32 = 2;

/// What a lending scenario refuses beside its markets, in both versions.
const LENDING_ALONE: &str = "lending: a lending scenario moves prices through its oracles only, \
     and has no path, market making, tape, order or spot scoring";
/// What a stake-pool scenario refuses beside its pools, in both versions.
const STAKE_ALONE: &str = "stake: a stake-pool scenario has only its pools, wallet and epochs";

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
    /// A perp session's starting positions, in signed base units, by market
    /// symbol: the bot starts holding them. Absent, it starts flat.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub start_positions: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StartingWallet {
    /// Wrapped SOL, in SOL.
    pub base: f64,
    /// USDC.
    pub quote: f64,
}

/// Where the bot trades. Exactly one of `perp`, `spot`, `lending` and `stake`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Venues {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perp: Option<PerpVenue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot: Option<SpotVenues>,
    /// Kamino's lending markets: the reserves, the bot's wallet and opening
    /// position, and the borrowers and liquidators beside it (see
    /// [`crate::lending`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lending: Option<Lending>,
    /// Stake pools: the pools, the wallet's SOL and pool tokens, and the
    /// epoch boundaries the session crosses (see [`crate::stake`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stake: Option<Stake>,
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
    /// (`phoenix`, `openbook`) for SOL-USDC, pools (`raydium`, `meteora`)
    /// for SPCX-USDC.
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
    /// Opt-in: the market calibration the path was drawn from, and what was
    /// drawn (see [`crate::market::model`]). The path and fair value above
    /// are what sessions replay; this records where they came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<crate::market::MarketModelRecord>,
    /// Jumps of the session's clock, for interest over days. Lending
    /// sessions only (see [`crate::lending::Warp`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warps: Vec<Warp>,
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
    /// The market's other participants, from the actor library (see
    /// [`crate::actors::library`]). A perp session's only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<crate::actors::Library>,
    /// What the takers' limits are priced off: `level`, `book` or `fair`.
    /// Without it, the scenario's kind picks (the level for a market-making
    /// or closed-period flow, the book for a tape). `fair` needs the market
    /// model's `fair_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takers_pricing: Option<crate::actors::Pricing>,
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
    /// Opt-in: sends land as a seeded block builder orders them, against
    /// background demand for block space, with fees and Jito tips paid from
    /// the bot's wallet (see [`crate::txlayer`]). Absent, sends land on
    /// receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<TxLayer>,
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
    /// PnL the task's informed bot makes on this scenario: scoring v2's 1.
    /// Written by `mtx calibrate --informed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub informed_pnl_usd: Option<f64>,
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
    /// Perp markets, by symbol, whose residual position a broker takes at
    /// the close, at a haircut the PnL pays. An order's market has its own
    /// broker, the order's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub close_out: BTreeMap<String, CloseOut>,
    /// The risk desk: a breach of the limits it lists is priced, by
    /// closing the position through the book, instead of disqualifying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_out: Option<StopOut>,
}

impl Assets {
    /// A symbol, a wallet whose balances are numbers of at least 0, and
    /// starting positions that are numbers other than 0.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(wallet) = &self.wallet {
            for (name, amount) in [("base", wallet.base), ("quote", wallet.quote)] {
                if !(amount.is_finite() && amount >= 0.0) {
                    return Err(format!("spot: {name} must be a number of at least 0"));
                }
            }
        }
        for (symbol, position) in &self.start_positions {
            if !(position.is_finite() && *position != 0.0) {
                return Err(format!(
                    "start_positions: {symbol} must be a number other than 0, not {position}"
                ));
            }
        }
        Ok(())
    }
}

impl Venues {
    /// A spot venue list that is known, unique and not empty, its step
    /// data asking for something sensible, a perp venue's funding that
    /// settles, and lending markets and stake pools that check themselves.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(lending) = &self.lending {
            lending.validate()?;
        }
        if let Some(stake) = &self.stake {
            stake.validate()?;
        }
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
        if let Some(record) = &self.record {
            record
                .validate(duration_slots)
                .map_err(|reason| format!("market_model: {reason}"))?;
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
    /// session and well formed. The library checks itself.
    pub fn validate(&self, duration_slots: u64, closed_period: bool) -> Result<(), String> {
        if let Some(arbitrage) = &self.arbitrage {
            arbitrage.validate()?;
        }
        if let Some(library) = &self.library {
            library.validate()?;
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
        for close_out in self.close_out.values() {
            close_out.validate()?;
        }
        if let Some(net) = &self.net {
            net.validate()?;
        }
        if let Some(stop_out) = &self.stop_out {
            stop_out.validate()?;
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
            // A clock jump adds its slots to the session the feeds run over.
            let (_, warped) = crate::lending::gaps(&self.market_model.warps);
            oracles.validate(self.duration_slots + warped)?;
            self.validate_oracles(oracles)?;
        }
        if let Some(raw) = self.tx.as_ref().and_then(|tx| tx.raw_sends.as_ref()) {
            raw.programs()
                .map_err(|reason| format!("raw_sends: {reason}"))?;
            if self.venues.perp.is_none() {
                return Err("raw_sends: only perp sessions take raw sends yet".into());
            }
        }
        if let Some(layer) = self.tx.as_ref().and_then(|tx| tx.layer.as_ref()) {
            layer
                .validate(self.duration_slots.div_ceil(SLOTS_BETWEEN_STEPS))
                .map_err(|reason| format!("tx_layer: {reason}"))?;
            if self.venues.spot.is_some() {
                return Err("tx_layer: only perp sessions take the transaction layer yet".into());
            }
            // An actor hooked on the bot's sends acts as each arrives, as if
            // it ran next: under the layer a send only enters the pool, and
            // lands when a block takes it.
            let hooked = self
                .actors
                .library
                .as_ref()
                .map(|library| library.send_hooked())
                .unwrap_or_default();
            if !hooked.is_empty() {
                return Err(format!(
                    "tx_layer: {} acts on the bot's sends as they arrive (on_send), and the transaction layer pools them until a block takes them; the two can't run together until pool-aware actors (PR 17b)",
                    hooked.join(", ")
                ));
            }
        }
        self.venues_are_one_kind()?;
        self.flows_do_not_mix()?;
        self.library_is_for_perps()?;
        self.takers_pricing_is_valid()?;
        if self.venues.spot.is_some() {
            self.spot_is_alone()?;
        } else if self.venues.lending.is_some() {
            self.lending_is_alone()?;
        } else if self.venues.stake.is_some() {
            self.stake_is_alone()?;
        } else {
            self.spot_fields_are_for_spot()?;
        }
        if self.venues.lending.is_none() && !self.market_model.warps.is_empty() {
            return Err("warps: only lending sessions jump the clock".into());
        }
        self.assets.validate()?;
        self.venues.validate()?;
        self.spot_markets_trade_the_symbol()?;
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
        self.positions_fit_the_market()?;
        self.stop_out_has_its_limits()?;
        self.order_fits_the_wallet()
    }

    /// Exactly one venue kind: a session is a perp's, a spot's, a lending
    /// one's or a stake pool's.
    fn venues_are_one_kind(&self) -> Result<(), String> {
        let Venues {
            perp,
            spot,
            lending,
            stake,
        } = &self.venues;
        match [
            perp.is_some(),
            spot.is_some(),
            lending.is_some(),
            stake.is_some(),
        ]
            .iter()
            .filter(|kind| **kind)
            .count()
        {
            1 => Ok(()),
            _ => Err("venues: exactly one of perp, spot, lending and stake".into()),
        }
    }

    /// A stake-pool scenario has only its pools, wallet and epochs: no
    /// path, flow, oracles, faults, order or spot scoring. Its symbol is the
    /// first pool's, and each epoch boundary has a step after it.
    fn stake_is_alone(&self) -> Result<(), String> {
        let Some(stake) = &self.venues.stake else {
            return Ok(());
        };
        stake.validate()?;
        if !self.market_model.is_empty()
            || !self.actors.is_empty()
            || self.oracles.is_some()
            || self.faults.is_some()
            || self.assets.wallet.is_some()
            || self.order.is_some()
            || self.scoring.bound_usd.is_some()
            || self.scoring.participation.is_some()
            || self.scoring.net.is_some()
        {
            return Err(STAKE_ALONE.into());
        }
        if self.assets.symbol != stake.pools[0].symbol {
            return Err(format!(
                "stake: the symbol is the first pool's, {}",
                stake.pools[0].symbol
            ));
        }
        if self.duration_slots == 0 {
            return Err("stake: the session has no slots".into());
        }
        crate::epochs::validate(
            &stake.epochs,
            self.duration_slots.div_ceil(crate::session::SLOTS_PER_STEP),
        )
    }

    /// A lending scenario trades Kamino's reserves from the wallet its
    /// section gives it, and moves prices through its oracles only: no path,
    /// other flow, order or spot scoring. Clock jumps fit inside the session
    /// and leave execution faults out, which count steps on the unjumped
    /// clock.
    fn lending_is_alone(&self) -> Result<(), String> {
        let market = &self.market_model;
        if !market.path.is_empty()
            || market.fair_path.is_some()
            || market.closed_period.is_some()
            || market.record.is_some()
            || !self.actors.is_empty()
            || self.assets.wallet.is_some()
            || self.order.is_some()
            || self.scoring.bound_usd.is_some()
            || self.scoring.participation.is_some()
            || self.scoring.net.is_some()
        {
            return Err(LENDING_ALONE.into());
        }
        if self.duration_slots == 0 {
            return Err("lending: the session has no slots".into());
        }
        crate::lending::validate_warps(&market.warps, self.duration_slots)?;
        if !market.warps.is_empty() && self.faults.is_some() {
            return Err("warps: a session with clock jumps has no execution faults".into());
        }
        Ok(())
    }

    /// A spot scenario trades from a wallet, and has no path or market
    /// making. A tape trades the session's first venue; an arbitrageur, its
    /// pools.
    fn spot_is_alone(&self) -> Result<(), String> {
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
        if !self.assets.start_positions.is_empty() || !self.scoring.close_out.is_empty() {
            return Err("spot: start_positions and close_out are for perp scenarios".into());
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

    /// Each spot market's venue profile names the scenario's symbol: the
    /// books SOL-USDC, the pools SPCX-USDC.
    fn spot_markets_trade_the_symbol(&self) -> Result<(), String> {
        let symbol = &self.assets.symbol;
        for venue in self.venues.spot.iter().flat_map(|s| &s.markets) {
            let named = crate::venue::books::named_markets(venue).map_err(|e| e.to_string())?;
            if !named.iter().any(|m| m.symbol == symbol) {
                return Err(format!(
                    "spot: the scenario's symbol is {symbol}, and {venue} has no {symbol} market; its profile names {:?}",
                    named.iter().map(|m| m.symbol).collect::<Vec<_>>()
                ));
            }
        }
        Ok(())
    }

    /// A perp scenario has no wallet, charge, net scoring or arbitrageur.
    fn spot_fields_are_for_spot(&self) -> Result<(), String> {
        if self.actors.arbitrage.is_some() {
            return Err("arbitrage: an arbitrageur trades a spot scenario's pools".into());
        }
        if self.assets.wallet.is_some()
            || self.scoring.bound_usd.is_some()
            || self.scoring.participation.is_some()
            || self.scoring.net.is_some()
        {
            return Err("turn_data, participation and net are for spot scenarios".into());
        }
        Ok(())
    }

    /// A perp scenario's starting positions, close-outs and order are in
    /// its market, the one the session trades; a starting position is
    /// inside the mandate's range for it; and an order can be done without
    /// leaving that range. A start's leverage depends on the mark, which
    /// setup checks on chain.
    fn positions_fit_the_market(&self) -> Result<(), String> {
        if self.venues.perp.is_none() {
            return Ok(());
        }
        let market = self.perp_instrument();
        let symbols = self
            .assets
            .start_positions
            .keys()
            .map(|s| ("start_positions", s))
            .chain(self.scoring.close_out.keys().map(|s| ("close_out", s)));
        for (field, symbol) in symbols {
            if *symbol != market.symbol {
                return Err(format!(
                    "{field}: {symbol} is not the scenario's market, {}",
                    market.symbol
                ));
            }
        }
        let Some(allowance) = self.mandate.allowance(&market) else {
            return Ok(());
        };
        let min = allowance.min_position.unwrap_or(-allowance.max_position);
        let start = self.start_position();
        if start > allowance.max_position || start < min {
            return Err(format!(
                "start_positions: {start} {} is outside the mandate's range, {min} to {}",
                market.symbol, allowance.max_position
            ));
        }
        let Some(order) = &self.order else {
            return Ok(());
        };
        if order.instrument.as_ref().is_some_and(|i| *i != market) {
            return Err(format!("order: the scenario's market is {market}"));
        }
        if self.scoring.close_out.contains_key(&market.symbol) {
            return Err(format!(
                "close_out: {} has the order's broker; it can't have another",
                market.symbol
            ));
        }
        let end = match order.side {
            Side::Buy => start + order.quantity,
            Side::Sell => start - order.quantity,
        };
        if end > allowance.max_position || end < min {
            return Err(format!(
                "order: a {} of {} from {start} ends at {end}, outside the mandate's range, {min} to {}",
                order.side, order.quantity, allowance.max_position
            ));
        }
        Ok(())
    }

    /// The starting position in the scenario's market, in base units.
    fn start_position(&self) -> f64 {
        self.assets
            .start_positions
            .get(&self.assets.symbol)
            .copied()
            .unwrap_or(0.0)
    }

    /// The mandate covers exactly the scenario's markets: a session measures
    /// only those, so a wider universe would go unenforced. It has no term
    /// (the session sets it); a perp mandate has a drawdown limit.
    fn mandate_covers_the_market(&self) -> Result<(), String> {
        if let Some(lending) = &self.venues.lending {
            let mut wanted = lending.instruments();
            let mut universe: Vec<InstrumentRef> = self
                .mandate
                .universe
                .iter()
                .map(|a| a.instrument.clone())
                .collect();
            wanted.sort();
            universe.sort();
            if wanted != universe {
                return Err("mandate: the universe must be exactly the lending reserves".into());
            }
        } else if let Some(stake) = &self.venues.stake {
            let mut wanted = stake.pools.clone();
            let mut universe: Vec<InstrumentRef> = self
                .mandate
                .universe
                .iter()
                .map(|a| a.instrument.clone())
                .collect();
            wanted.sort();
            universe.sort();
            if wanted != universe {
                return Err("mandate: the universe must be exactly the stake pools".into());
            }
        } else if self.venues.spot.is_some() {
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
        if self.venues.perp.is_some() && self.mandate.limits.max_drawdown_usd.is_none() {
            return Err("mandate: a perp scenario's mandate has a drawdown limit".into());
        }
        Ok(())
    }

    /// A stop-out closes a perp position, on a limit the mandate sets.
    fn stop_out_has_its_limits(&self) -> Result<(), String> {
        let Some(stop_out) = &self.scoring.stop_out else {
            return Ok(());
        };
        if self.venues.perp.is_none() {
            return Err("stop_out: only perp sessions close positions yet".into());
        }
        if stop_out.covers(StopRule::Drawdown) && self.mandate.limits.max_drawdown_usd.is_none() {
            return Err(
                "stop_out: closes on a drawdown, but the mandate sets no drawdown limit".into(),
            );
        }
        Ok(())
    }

    /// What a scenario's oracle feeds may do beside the rest of it.
    fn validate_oracles(&self, oracles: &crate::oracles::Oracles) -> Result<(), String> {
        use crate::oracles::{Anchor, Source};
        let session: Vec<InstrumentRef> = if self.venues.spot.is_some() {
            self.venues.spot_instruments(&self.assets.symbol)
        } else {
            vec![self.perp_instrument()]
        };
        for feed in &oracles.feeds {
            if self.venues.lending.is_some() && !matches!(feed.source, Source::Scope(_)) {
                return Err(format!(
                    "oracles: {} isn't Scope's; Kamino's reserves read Scope only",
                    feed.name
                ));
            }
            if let Some(Anchor::Market { instrument, .. }) = &feed.anchor
                && !session.contains(instrument)
            {
                return Err(format!(
                    "oracles: {} is anchored to {instrument}, which isn't in the session",
                    feed.name
                ));
            }
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

    /// The actor library trades a perp's book: a spot, lending or stake session has none.
    fn library_is_for_perps(&self) -> Result<(), String> {
        if (self.venues.spot.is_some()
            || self.venues.lending.is_some()
            || self.venues.stake.is_some())
            && self.actors.library.is_some()
        {
            return Err("actors: the library runs in perp sessions".into());
        }
        Ok(())
    }

    /// The takers' pricing is a perp session's, and `fair` needs a fair
    /// value to price off.
    fn takers_pricing_is_valid(&self) -> Result<(), String> {
        let Some(pricing) = self.actors.takers_pricing else {
            return Ok(());
        };
        if self.venues.spot.is_some() {
            return Err("actors: takers_pricing is for perp sessions".into());
        }
        if pricing == crate::actors::Pricing::Fair && self.market_model.fair_path.is_none() {
            return Err("actors: takers_pricing fair needs market_model.fair_path".into());
        }
        Ok(())
    }

    /// A sell order can't be bigger than the wallet, and a spot wallet,
    /// which can't go short, has no reduce-only buy. A perp's reduce-only
    /// order can't be bigger than the position it reduces: a sell, the
    /// starting long; a buy, the starting short. A net-scored session has
    /// no order.
    fn order_fits_the_wallet(&self) -> Result<(), String> {
        if let (Some(order), Some(wallet)) = (&self.order, &self.assets.wallet) {
            if order.side == Side::Sell && wallet.base < order.quantity {
                return Err(format!(
                    "order: selling {} SOL from a wallet of {}",
                    order.quantity, wallet.base
                ));
            }
            if order.reduce_only && order.side == Side::Buy {
                return Err("order: a buy order can't be reduce-only on a spot wallet".into());
            }
        }
        if let Some(order) = &self.order
            && self.venues.perp.is_some()
            && order.reduce_only
        {
            let reducible = match order.side {
                Side::Sell => self.start_position(),
                Side::Buy => -self.start_position(),
            };
            if order.quantity > reducible {
                return Err(format!(
                    "order: a reduce-only {} of {} from a starting position of {}",
                    order.side,
                    order.quantity,
                    self.start_position()
                ));
            }
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
        Ok(self.into_v1())
    }

    /// The same facts in v1's layout, unchecked.
    fn into_v1(self) -> Scenario {
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
            record,
            warps,
        } = market_model;
        let Actors {
            competitor,
            takers,
            arbitrage,
            library,
            takers_pricing,
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
        Scenario {
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
            start_positions: assets.start_positions,
            close_out: scoring.close_out,
            reference_pnl_usd: scoring.reference_pnl_usd,
            informed_pnl_usd: scoring.informed_pnl_usd,
            fair_path,
            tx_layer: tx.as_ref().and_then(|tx| tx.layer.clone()),
            raw_sends: tx.and_then(|tx| tx.raw_sends),
            stop_out: scoring.stop_out,
            market_model: record,
            oracles,
            lending: venues.lending,
            warps,
            stake: venues.stake,
            actors: library,
            takers_pricing,
        }
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
            // Lending's v1 fields are its own section's; v2 has no place for
            // the others, so they are refused here as v2 refuses them.
            None if v1.lending.is_some() => {
                if v1.funding.is_some() || v1.turn_data.is_some() {
                    return Err(LENDING_ALONE.into());
                }
                (
                    Venues {
                        lending: v1.lending.clone(),
                        ..Venues::default()
                    },
                    None,
                )
            }
            // The stake section is its own; v2 has no place for the other
            // kinds' fields beside it, so they are refused here as v2
            // refuses them.
            None if v1.stake.is_some() => {
                if v1.funding.is_some() || v1.turn_data.is_some() {
                    return Err(STAKE_ALONE.into());
                }
                (
                    Venues {
                        stake: v1.stake.clone(),
                        ..Venues::default()
                    },
                    None,
                )
            }
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
                        lending: v1.lending.clone(),
                        stake: v1.stake.clone(),
                    },
                    Some(StartingWallet {
                        base: spot.base,
                        quote: spot.quote,
                    }),
                )
            }
            None => {
                if v1.turn_data.is_some() || v1.participation.is_some() || v1.net.is_some() {
                    return Err("turn_data, participation and net are for spot scenarios".into());
                }
                (
                    Venues {
                        perp: Some(PerpVenue {
                            funding: v1.funding,
                        }),
                        spot: None,
                        lending: None,
                        stake: None,
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
                start_positions: v1.start_positions.clone(),
            },
            venues,
            market_model: MarketModel {
                path: v1.path.clone(),
                fair_path: v1.fair_path.clone(),
                closed_period,
                record: v1.market_model.clone(),
                warps: v1.warps.clone(),
            },
            oracles: v1.oracles.clone(),
            actors: Actors {
                competitor,
                takers,
                arbitrage: v1.spot.as_ref().and_then(|s| s.arbitrage),
                library: v1.actors.clone(),
                takers_pricing: v1.takers_pricing,
            },
            faults: v1.faults.clone(),
            tx: (v1.raw_sends.is_some() || v1.tx_layer.is_some()).then(|| Tx {
                layer: v1.tx_layer.clone(),
                raw_sends: v1.raw_sends.clone(),
            }),
            mandate: v1.mandate.clone(),
            order: v1.order.clone(),
            scoring: Scoring {
                reference_pnl_usd: v1.reference_pnl_usd,
                informed_pnl_usd: v1.informed_pnl_usd,
                bound_usd: v1.spot.as_ref().and_then(|s| s.bound_usd),
                participation: v1.participation.clone(),
                net: v1.net.clone(),
                close_out: v1.close_out.clone(),
                stop_out: v1.stop_out.clone(),
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
            lending: None,
            stake: None,
        };
        s.assets = Assets {
            symbol: SYMBOL.into(),
            wallet: Some(StartingWallet {
                base: 10.0,
                quote: 0.0,
            }),
            start_positions: BTreeMap::new(),
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
        let path = format!(
            "{}/../../bench/checks/pools/smoke.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let v1 = parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        let as_v2 = v2(&v1);
        assert_eq!(as_v2.validate(), Ok(()));
        let listed = as_v2.venues.spot.as_ref().unwrap();
        assert_eq!(listed.liquidity.len(), 2);
        assert!(as_v2.actors.arbitrage.is_some());
        assert_eq!(as_v2.clone().into_scenario().unwrap(), v1);
        let text = serde_json::to_string_pretty(&as_v2).unwrap();
        assert_eq!(parse(&text).unwrap(), v1);
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
                .contains("phoenix has no SPCX-USDC market")
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
    fn takers_pricing_round_trips_and_fair_needs_a_fair_value() {
        use crate::actors::Pricing;
        // Absent, a scenario says nothing of it.
        assert!(
            !serde_json::to_string(&making())
                .unwrap()
                .contains("takers_pricing")
        );
        let mut s = making();
        s.actors.takers_pricing = Some(Pricing::Fair);
        assert!(s.validate().unwrap_err().contains("fair_path"));
        s.market_model.fair_path = Some(vec![Waypoint::at(10, 1.0)]);
        s.validate().unwrap();
        let again = parse(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(again.takers_pricing, Some(Pricing::Fair));
        let v1 = s.clone().into_scenario().unwrap();
        assert_eq!(v1.takers_pricing, Some(Pricing::Fair));
        assert_eq!(
            ScenarioV2::try_from(&v1).unwrap().actors.takers_pricing,
            Some(Pricing::Fair)
        );
        s.actors.takers_pricing = Some(Pricing::Book);
        s.validate().unwrap();
        let mut s = spot();
        s.actors.takers_pricing = Some(Pricing::Level);
        assert!(s.validate().unwrap_err().contains("perp sessions"));
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
                .contains("exactly one of perp, spot, lending and stake")
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
    fn a_perp_takes_a_start_position_an_order_and_close_outs_in_its_market() {
        let cap = |s: &ScenarioV2| s.mandate.universe[0].max_position;
        let order = |side, quantity, reduce_only| ExecutionOrder {
            instrument: None,
            side,
            quantity,
            haircut_bps: 30.0,
            reduce_only,
        };
        // Long half the cap, sold down by a reduce-only order, and the same
        // with the order's market named.
        let mut s = shock();
        let half = cap(&s) / 2.0;
        s.assets.start_positions.insert("SPCX".into(), half);
        s.order = Some(order(Side::Sell, half, true));
        assert_eq!(s.validate(), Ok(()));
        s.order.as_mut().unwrap().instrument = Some("phoenix:perp:SPCX".parse().unwrap());
        assert_eq!(s.validate(), Ok(()));
        // It round-trips through v1.
        let v1 = s.clone().into_scenario().unwrap();
        assert_eq!(v1.start_positions["SPCX"], half);
        assert_eq!(v2(&v1), s);
        assert!(v1.setup_slots() > ShockFamily::spcx().sample("s", 3).setup_slots());
        assert_eq!(v1.valuation(), "oracle");

        // A reduce-only order is no bigger than what it reduces: a buy, the
        // starting short.
        let mut over = s.clone();
        over.order = Some(order(Side::Sell, half + 1.0, true));
        assert!(over.validate().unwrap_err().contains("reduce-only"));
        let mut short = s.clone();
        short.assets.start_positions.insert("SPCX".into(), -half);
        short.order = Some(order(Side::Buy, half, true));
        assert_eq!(short.validate(), Ok(()));
        // An order ends inside the mandate's range.
        let mut past = s.clone();
        past.order = Some(order(Side::Buy, cap(&s), false));
        assert!(past.validate().unwrap_err().contains("outside the mandate"));
        // A start position is inside it, in the scenario's market, and not 0.
        let mut big = shock();
        big.assets
            .start_positions
            .insert("SPCX".into(), cap(&big) * 2.0);
        assert!(big.validate().unwrap_err().contains("outside the mandate"));
        let mut elsewhere = shock();
        elsewhere.assets.start_positions.insert("NVDA".into(), 1.0);
        assert!(
            elsewhere
                .validate()
                .unwrap_err()
                .contains("not the scenario's market")
        );
        let mut zero = shock();
        zero.assets.start_positions.insert("SPCX".into(), 0.0);
        assert!(zero.validate().unwrap_err().contains("other than 0"));
        let mut other = s.clone();
        other.order.as_mut().unwrap().instrument = Some("phoenix:perp:NVDA".parse().unwrap());
        assert!(other.validate().unwrap_err().contains("scenario's market"));

        // A close-out is in the session's market, and not the order's.
        let mut closed = shock();
        closed.assets.start_positions.insert("SPCX".into(), half);
        closed
            .scoring
            .close_out
            .insert("SPCX".into(), CloseOut { haircut_bps: 10.0 });
        assert_eq!(closed.validate(), Ok(()));
        assert_eq!(v2(&closed.clone().into_scenario().unwrap()), closed);
        let mut both = closed.clone();
        both.order = Some(order(Side::Sell, half, true));
        assert!(both.validate().unwrap_err().contains("order's broker"));
        let mut far = closed.clone();
        far.scoring
            .close_out
            .insert("QQQ".into(), CloseOut { haircut_bps: 10.0 });
        assert!(
            far.validate()
                .unwrap_err()
                .contains("not the scenario's market")
        );
        let mut negative = closed;
        negative
            .scoring
            .close_out
            .get_mut("SPCX")
            .unwrap()
            .haircut_bps = -1.0;
        assert!(negative.validate().unwrap_err().contains("haircut_bps"));
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
    fn stop_out_lives_in_scoring_and_round_trips_through_v1() {
        use crate::scenario::{CloseMode, StopOut};
        let mut s = shock();
        s.scoring.stop_out = Some(StopOut {
            on: vec![StopRule::Drawdown],
            close: CloseMode::Market,
            slice_base: Some(100.0),
        });
        assert_eq!(s.validate(), Ok(()));
        let text = serde_json::to_string(&s).unwrap();
        assert!(
            text.contains(r#""stop_out":{"on":["drawdown"],"close":"market","slice_base":100.0}"#)
        );
        let v1 = s.clone().into_scenario().unwrap();
        assert_eq!(v1.stop_out, s.scoring.stop_out);
        assert_eq!(ScenarioV2::try_from(&v1).unwrap(), s);

        let mut empty = s.clone();
        empty.scoring.stop_out.as_mut().unwrap().on.clear();
        assert!(empty.validate().unwrap_err().contains("no limit"));
        let mut twice = s.clone();
        twice.scoring.stop_out.as_mut().unwrap().on = vec![StopRule::Drawdown; 2];
        assert!(twice.validate().unwrap_err().contains("repeats"));
        let mut slice = s.clone();
        slice.scoring.stop_out.as_mut().unwrap().slice_base = Some(0.0);
        assert!(slice.validate().unwrap_err().contains("slice_base"));
        let mut spot_stop = spot();
        spot_stop.scoring.stop_out = s.scoring.stop_out.clone();
        assert!(spot_stop.validate().is_err());
    }

    #[test]
    fn raw_sends_live_in_the_tx_section_and_round_trip_through_v1() {
        let mut s = shock();
        s.tx = Some(Tx {
            layer: None,
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

    fn lending(check: &str) -> Scenario {
        let path = format!(
            "{}/../../bench/checks/kamino/{check}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        parse(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn stake() -> Scenario {
        let path = format!(
            "{}/../../bench/checks/stake-pool/smoke.json",
            env!("CARGO_MANIFEST_DIR")
        );
        parse(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn lending_scenarios_round_trip_through_v2_and_json() {
        for check in ["liquidation", "stale-oracle", "interest", "drain"] {
            let v1 = lending(check);
            let as_v2 = v2(&v1);
            assert_eq!(as_v2.validate(), Ok(()), "{check}");
            assert!(as_v2.venues.lending.is_some());
            let text = serde_json::to_string_pretty(&as_v2).unwrap();
            assert_eq!(parse(&text).unwrap(), v1, "{check}");
            assert_eq!(as_v2.into_scenario().unwrap(), v1, "{check}");
        }
        assert!(!v2(&lending("interest")).market_model.warps.is_empty());
    }

    #[test]
    fn a_lending_scenario_stands_alone_in_both_versions() {
        let refused = |s: &ScenarioV2, needle: &str| {
            let err = s.validate().unwrap_err();
            assert!(err.contains(needle), "{err}");
            assert_eq!(s.clone().into_v1().validate().unwrap_err(), err);
        };

        let mut s = v2(&lending("liquidation"));
        s.venues.spot = spot().venues.spot;
        s.assets.wallet = spot().assets.wallet;
        refused(&s, "exactly one of perp, spot and lending");

        let mut s = v2(&lending("liquidation"));
        s.market_model.path.push(Waypoint::at(4, -1.0));
        refused(&s, "moves prices through its oracles only");

        let mut s = v2(&lending("liquidation"));
        s.mandate.universe.pop();
        refused(&s, "exactly the lending reserves");

        let mut s = v2(&lending("interest"));
        s.market_model.warps[0].step = 10_000;
        refused(&s, "warps: step");

        let mut s = v2(&lending("liquidation"));
        s.tx = Some(Tx {
            raw_sends: Some(RawSends {
                programs: vec!["11111111111111111111111111111111".into()],
            }),
        });
        refused(&s, "only perp sessions take raw sends");

        let mut s = shock();
        s.market_model.warps = v2(&lending("interest")).market_model.warps;
        refused(&s, "only lending sessions jump the clock");
    }

    #[test]
    fn a_stake_pool_scenario_round_trips_through_v2_and_json() {
        let v1 = stake();
        let as_v2 = v2(&v1);
        assert_eq!(as_v2.validate(), Ok(()));
        assert!(as_v2.venues.stake.is_some());
        let text = serde_json::to_string_pretty(&as_v2).unwrap();
        assert_eq!(parse(&text).unwrap(), v1);
        assert_eq!(as_v2.into_scenario().unwrap(), v1);
    }

    #[test]
    fn a_stake_pool_scenario_stands_alone_in_both_versions() {
        let refused = |s: &ScenarioV2, needle: &str| {
            let err = s.validate().unwrap_err();
            assert!(err.contains(needle), "{err}");
            assert_eq!(s.clone().into_v1().validate().unwrap_err(), err);
        };

        let mut s = v2(&stake());
        s.venues.spot = spot().venues.spot;
        s.assets.wallet = spot().assets.wallet;
        refused(&s, "exactly one of perp, spot, lending and stake");

        let mut s = v2(&stake());
        s.market_model.path.push(Waypoint::at(4, -1.0));
        refused(&s, "only its pools, wallet and epochs");

        let mut s = v2(&stake());
        s.oracles = Some(crate::oracles::Oracles {
            seed: 1,
            feeds: Vec::new(),
        });
        refused(&s, "only its pools, wallet and epochs");

        let mut s = v2(&stake());
        s.mandate.universe.pop();
        refused(&s, "exactly the stake pools");

        let mut s = v2(&stake());
        s.assets.symbol = "MSOL".into();
        refused(&s, "the first pool's");

        let mut s = v2(&stake());
        s.venues.stake.as_mut().unwrap().epochs[0].step = 10_000;
        refused(&s, "epochs: step");

        let mut s = v2(&stake());
        s.tx = Some(Tx {
            raw_sends: Some(RawSends {
                programs: vec!["11111111111111111111111111111111".into()],
            }),
        });
        refused(&s, "only perp sessions take raw sends");
    }

    #[test]
    fn a_stake_pool_scenario_with_no_pools_gets_a_clean_error_in_both_versions() {
        let mut s = v2(&stake());
        let section = s.venues.stake.as_mut().unwrap();
        section.pools.clear();
        section.tokens.clear();
        let err = s.validate().unwrap_err();
        assert_eq!(err, "stake: no pools");
        assert_eq!(s.into_v1().validate().unwrap_err(), err);
    }

    #[test]
    fn the_transaction_layer_lives_in_the_tx_section_and_round_trips_through_v1() {
        let mut s = shock();
        s.tx = Some(Tx {
            layer: Some(crate::txlayer::tests::layer()),
            raw_sends: None,
        });
        s.validate().unwrap();
        let v1 = s.clone().into_scenario().unwrap();
        assert_eq!(v1.tx_layer, s.tx.as_ref().unwrap().layer);
        assert_eq!(ScenarioV2::try_from(&v1).unwrap(), s);
        let mut on_spot = spot();
        on_spot.tx = s.tx.clone();
        assert!(on_spot.validate().unwrap_err().starts_with("tx_layer:"));
        s.tx.as_mut().unwrap().layer.as_mut().unwrap().validators = 0;
        assert!(s.validate().unwrap_err().starts_with("tx_layer:"));
    }

    #[test]
    fn the_transaction_layer_refuses_actors_hooked_on_the_bots_sends() {
        let everyone = include_str!("../../../bench/checks/actors/everyone.json");
        let mut library = parse(everyone).unwrap().actors.unwrap();
        let mut s = shock();
        s.tx = Some(Tx {
            layer: Some(crate::txlayer::tests::layer()),
            raw_sends: None,
        });
        library.front_runners = vec![crate::actors::library::FrontRunner::default()];
        s.actors.library = Some(library.clone());
        let mut v1 = s.clone().into_scenario().unwrap();
        library.front_runners[0].on_send = true;
        s.actors.library = Some(library);
        let refused = s.validate().unwrap_err();
        assert!(
            refused.starts_with("tx_layer: actors.front_runners[0] acts on the bot's sends")
                && refused.contains("PR 17b"),
            "{refused}"
        );
        // A v1 scenario, the same.
        v1.actors.as_mut().unwrap().front_runners[0].on_send = true;
        assert_eq!(v1.validate().unwrap_err(), refused);
    }

    #[test]
    fn the_library_lives_in_the_actors_section_and_round_trips_through_v1() {
        let everyone = include_str!("../../../bench/checks/actors/everyone.json");
        let v1 = parse(everyone).unwrap();
        let library = v1.actors.clone().unwrap();
        let as_v2 = v2(&v1);
        assert_eq!(as_v2.actors.library.as_ref(), Some(&library));
        let text = serde_json::to_string_pretty(&as_v2).unwrap();
        assert_eq!(parse(&text).unwrap(), v1);
        assert_eq!(as_v2.clone().into_scenario().unwrap(), v1);

        let mut s = shock();
        s.actors.library = Some(library);
        s.validate().unwrap();
        let mut bad = s.clone();
        bad.actors.library.as_mut().unwrap().scale.trade_size = 0.0;
        assert!(bad.validate().is_err());
        let mut on_spot = spot();
        on_spot.actors.library = s.actors.library.clone();
        assert_eq!(
            on_spot.validate().unwrap_err(),
            "actors: the library runs in perp sessions"
        );
    }

    #[test]
    fn the_library_is_refused_in_a_lending_session_in_both_versions() {
        let everyone = include_str!("../../../bench/checks/actors/everyone.json");
        let library = parse(everyone).unwrap().actors.unwrap();
        let mut s = v2(&lending("liquidation"));
        s.validate().unwrap();
        s.actors.library = Some(library);
        let err = s.validate().unwrap_err();
        assert_eq!(err, "actors: the library runs in perp sessions");
        assert_eq!(s.into_v1().validate().unwrap_err(), err);
    }

    #[test]
    fn the_library_is_refused_in_a_stake_session_in_both_versions() {
        let everyone = include_str!("../../../bench/checks/actors/everyone.json");
        let library = parse(everyone).unwrap().actors.unwrap();
        let mut s = v2(&stake());
        s.validate().unwrap();
        s.actors.library = Some(library);
        let err = s.validate().unwrap_err();
        assert_eq!(err, "actors: the library runs in perp sessions");
        assert_eq!(s.into_v1().validate().unwrap_err(), err);
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
