//! Market scenarios: the price path a trading session replays.
//!
//! A scenario is a list of waypoints, each a mark price relative to the mark
//! at the start of the session. It is independent of any one snapshot: the
//! same file replays on any snapshot of the same market. Waypoints compile to
//! Surfpool overrides that walk Phoenix's oracle and book to each level in
//! steps Phoenix accepts.

use std::path::Path;

use mtx_core::mandate::{InstrumentRef, Kind, Mandate};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use crate::{
    Error, Result,
    chain::Override,
    error::io_error,
    making::{Making, level_at},
    offhours::OffHours,
    phoenix::{SLOTS_BETWEEN_STEPS, market_move, oracle_move},
    spot::Spot,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    pub symbol: String,
    /// Session length, counted from when the bot starts. The account is
    /// marked to market at this slot.
    pub duration_slots: u64,
    /// What the bot may trade and the limits it must stay within: the
    /// scenario's market, with a position cap and a drawdown limit (see
    /// `specs/mandate.md`). Without a cap, PnL in dollars rewards size as
    /// much as skill: a bot betting several times the reference's size beats
    /// it on any profitable family. The reference trades up to the cap, so
    /// the reward compares strategies at equal size. The drawdown limit is a
    /// prop desk's loss limit: PnL alone pays a bot that goes deep underwater
    /// and gets lucky; the limit makes surviving the path part of the score.
    pub mandate: Mandate,
    pub path: Vec<Waypoint>,
    /// For a market-making scenario: the competing makers and the taker
    /// flow the grader sends. Without it, nobody trades but the bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub making: Option<Making>,
    /// For a scenario where the underlying's own market is closed: the
    /// closed period before it reopens, Phoenix's index through it, and the
    /// taker flow the grader sends (see [`crate::offhours`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offhours: Option<OffHours>,
    /// Phoenix's funding parameters for the session's market, set when the
    /// session starts. Without it, the snapshot's own apply (hourly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// Outside market making: taker orders the grader sends from its own
    /// accounts, priced off the book, as anyone trading the market would.
    /// They keep Phoenix's book price, and with it a book-priced mark, fresh.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tape: Vec<crate::making::TakerOrder>,
    /// For a spot session: the spot markets the bot trades and what its
    /// wallet starts with (see [`crate::spot`]). The mandate's universe is
    /// those markets and `symbol` their pair, such as `SOL-USDC`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot: Option<Spot>,
    /// Execution faults on the bot's sends and reads, planned from the
    /// faults' own seed (see [`crate::faults`]). Without them the guard
    /// forwards at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faults: Option<crate::faults::Faults>,
    /// For a spot session: what each step message carries beyond the clock
    /// (see [`crate::spot::TurnData`]). Without it, none of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_data: Option<crate::spot::TurnData>,
    /// For a spot session: a quantity the bot has to trade by the end, and
    /// what a broker would charge to do it for the bot (see
    /// [`crate::orders::ExecutionOrder`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<crate::orders::ExecutionOrder>,
    /// For a spot session: a charge on the bot's share of a venue's traded
    /// volume (see [`crate::orders::Participation`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participation: Option<crate::orders::Participation>,
    /// For a spot session where the bot starts with inventory and trades
    /// both ways: score the PnL net of the starting holding's price drift,
    /// with optional priced charges (see [`crate::orders::NetScoring`]).
    /// Not with an `order`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<crate::orders::NetScoring>,
    /// PnL the reference bot makes on this scenario; a score of 1 matches it.
    /// Written by `mtx calibrate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_pnl_usd: Option<f64>,
    /// Opt-in: the scenario's fair value, as waypoints in the same terms as
    /// [`Scenario::path`]. The session records it at every step, and the
    /// grade reports each session's PnL split by
    /// [`mtx_kit::core::MarkoutMeter`]
    /// (markout, luck and a residual). Absent, sessions, grades and rewards
    /// are exactly what they are without it.
    ///
    /// The fair value is what the scenario's author plans, never a forecast
    /// of the walk's future: a bot has no edge in knowing it only by being
    /// told. It is not shown to the bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fair_path: Option<Vec<Waypoint>>,
    /// Opt-in: a strategy component may build transactions itself, for
    /// what its sheet can't say, and the grader signs them (`send`, see
    /// [`crate::raw`]). Absent, the session refuses every one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_sends: Option<RawSends>,
}

/// What a scenario's raw sends may do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RawSends {
    /// The programs, base58, a raw transaction may run, at the top level or
    /// through another program. The compute budget program is always
    /// allowed: it only sets the transaction's own limits and fee.
    pub programs: Vec<String>,
}

/// The programs a raw send may be allowed to run: those whose effects the
/// grader's audit of a raw send measures (Phoenix's main trader account, the
/// wallet's tokens), and the system, associated token and compute budget
/// programs. A venue's program joins when its venue adds its own audit;
/// until then a position it holds would go unmeasured.
pub const RAW_SEND_PROGRAMS: [(&str, &str); 5] = [
    ("Phoenix", "EtrnLzgbS7nMMy5fbD42kXiUzGg8XQzJ972Xtk1cjWih"),
    ("system", "11111111111111111111111111111111"),
    ("token", "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
    (
        "associated token",
        "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
    ),
    (
        "compute budget",
        "ComputeBudget111111111111111111111111111111",
    ),
];

impl RawSends {
    /// The programs, parsed and in order.
    pub fn programs(&self) -> std::result::Result<Vec<Pubkey>, String> {
        let mut programs = Vec::with_capacity(self.programs.len());
        for program in &self.programs {
            let key = program
                .parse::<Pubkey>()
                .map_err(|_| format!("{program} is not a base58 address"))?;
            if !RAW_SEND_PROGRAMS.iter().any(|(_, p)| p == program) {
                let audited: Vec<&str> = RAW_SEND_PROGRAMS.iter().map(|(name, _)| *name).collect();
                return Err(format!(
                    "{program}: raw sends can't run it yet, since the audit can't measure what it holds; only the {} programs",
                    audited.join(", ")
                ));
            }
            if programs.contains(&key) {
                return Err(format!("{program} is listed twice"));
            }
            programs.push(key);
        }
        if programs.is_empty() {
            return Err("no programs: leave raw_sends out to turn raw sends off".into());
        }
        Ok(programs)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Waypoint {
    /// Slot, relative to the session start, at which the move begins.
    pub slot: u64,
    /// Target mark relative to the session's starting mark, in percent:
    /// `-8.0` is 8% below it, `0.0` is back to where it started.
    pub level_pct: f64,
    /// The order book's premium over the level, in percent of the level:
    /// the oracle (Phoenix's index) goes to the level, and the makers
    /// re-centre their quotes `premium_pct` above it (below, when negative).
    /// Zero moves both to the level, as every waypoint did before premiums.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub premium_pct: f64,
}

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

impl Waypoint {
    /// A waypoint that moves the oracle and the book together.
    pub fn at(slot: u64, level_pct: f64) -> Self {
        Self {
            slot,
            level_pct,
            premium_pct: 0.0,
        }
    }

    /// Where the book's makers centre, relative to the session's starting
    /// mark, in percent.
    pub fn book_pct(&self) -> f64 {
        if self.premium_pct == 0.0 {
            return self.level_pct;
        }
        ((1.0 + self.level_pct / 100.0) * (1.0 + self.premium_pct / 100.0) - 1.0) * 100.0
    }
}

/// Phoenix's funding parameters for one market, as the
/// `phoenix-market-funding` template sets them: funding accrues on the gap
/// between Phoenix's mark and its index, and settles every interval at that
/// gap's average over the interval times `interval / period`, capped at
/// `max_rate` per interval.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Funding {
    pub interval_seconds: u64,
    pub period_seconds: u64,
    /// The cap on one interval's funding, in quote lots per base lot (the
    /// template's unit).
    pub max_rate: i64,
    /// How Phoenix prices the market's mark in the session, written at
    /// setup; without it, the snapshot's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<crate::phoenix::MarkConfig>,
}

/// Lowest level a waypoint may target. Past this, Phoenix's risk checks and
/// the market's resting liquidity stop behaving like a real market.
const MIN_LEVEL_PCT: f64 = -40.0;

/// Market time per slot in a session: the surfnet's clock, which Phoenix's
/// funding reads.
pub const SLOT_SECONDS: f64 = 0.25;

/// The venue every scenario trades on.
pub const VENUE: &str = "phoenix";

impl Scenario {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(io_error(path))?;
        let scenario: Self = serde_json::from_str(&contents)?;
        scenario
            .validate()
            .map_err(|reason| Error::InvalidScenario {
                path: path.to_path_buf(),
                reason,
            })?;
        Ok(scenario)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let contents = serde_json::to_string_pretty(self)? + "\n";
        std::fs::write(path, contents).map_err(io_error(path))
    }

    /// The market the scenario moves, as the mandate names it.
    pub fn instrument(&self) -> InstrumentRef {
        InstrumentRef {
            venue: VENUE.into(),
            kind: Kind::Perp,
            symbol: self.symbol.clone(),
        }
    }

    /// The mandate's position cap on the scenario's market. In a spot
    /// session, the largest of the spot markets' caps: they all bound the
    /// same base asset.
    pub fn max_position(&self) -> f64 {
        if self.spot.is_some() {
            return self
                .mandate
                .universe
                .iter()
                .map(|a| a.max_position)
                .fold(0.0, f64::max);
        }
        self.mandate
            .allowance(&self.instrument())
            .map_or(0.0, |a| a.max_position)
    }

    /// The mandate must be valid and cover exactly the scenario's market: a
    /// session measures only that one, so a wider universe would go
    /// unenforced. Waypoints must be in order, each move must finish before
    /// the next one starts, and the last must finish before the session ends.
    pub fn validate(&self) -> std::result::Result<(), String> {
        self.mandate
            .validate()
            .map_err(|reason| format!("mandate: {reason}"))?;
        if let Some(raw) = &self.raw_sends {
            raw.programs()
                .map_err(|reason| format!("raw_sends: {reason}"))?;
            if self.spot.is_some() {
                return Err("raw_sends: only perp sessions take raw sends yet".into());
            }
        }
        if let Some(spot) = &self.spot {
            if let Some(faults) = &self.faults {
                faults
                    .validate(self.duration_slots.div_ceil(SLOTS_BETWEEN_STEPS))
                    .map_err(|reason| format!("faults: {reason}"))?;
            }
            return spot.validate(self);
        }
        if self.turn_data.is_some()
            || self.order.is_some()
            || self.participation.is_some()
            || self.net.is_some()
        {
            return Err("turn_data, order, participation and net are for spot scenarios".into());
        }
        if self.mandate.universe.len() != 1 || self.mandate.allowance(&self.instrument()).is_none()
        {
            return Err(format!(
                "mandate: the universe must be exactly {}, the scenario's market",
                self.instrument()
            ));
        }
        if self.mandate.term.is_some() {
            return Err("mandate: a scenario's mandate has no term; the session sets it".into());
        }
        if self.mandate.limits.max_drawdown_usd.is_none() {
            return Err("mandate: a perp scenario's mandate has a drawdown limit".into());
        }
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
            // An off-hours reopen lands at once, in one slot, however many
            // of Phoenix's steps it takes.
            free_from = match &self.offhours {
                Some(off) if waypoint.slot == off.reopen_slot => {
                    waypoint.slot + SLOTS_BETWEEN_STEPS + 1
                }
                _ => waypoint.slot + move_slots(book, waypoint.book_pct()) + 1,
            };
            before = Some((book, waypoint.book_pct(), waypoint.slot));
            book = waypoint.book_pct();
        }
        if let Some(funding) = &self.funding
            && (funding.interval_seconds == 0
                || funding.period_seconds < funding.interval_seconds
                || funding.max_rate <= 0)
        {
            return Err(format!(
                "funding: needs an interval, a period at least as long, and a positive cap, not {funding:?}"
            ));
        }
        if let Some(off) = &self.offhours {
            if self.making.is_some() {
                return Err("a scenario has either making or offhours, not both".into());
            }
            if !self.tape.is_empty() {
                return Err("an off-hours scenario's takers are its offhours', not a tape".into());
            }
            if self.path.iter().any(|w| w.premium_pct != 0.0) {
                return Err("an off-hours scenario's waypoints carry no premium".into());
            }
            off.validate(&self.path, self.duration_slots)
                .map_err(|reason| format!("offhours: {reason}"))?;
            if !self.path.iter().any(|w| w.slot == off.reopen_slot) {
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
                if waypoint.slot < previous || waypoint.slot > self.duration_slots {
                    return Err(format!(
                        "fair value waypoint {i} at slot {} is out of order or past the session end",
                        waypoint.slot
                    ));
                }
                previous = waypoint.slot;
            }
        }
        if let Some(making) = &self.making {
            making
                .validate(self.duration_slots)
                .map_err(|reason| format!("making: {reason}"))?;
            if !self.tape.is_empty() {
                return Err(
                    "a market-making scenario's takers are its making's, not a tape".into(),
                );
            }
        }
        if let Some(faults) = &self.faults {
            faults
                .validate(self.duration_slots.div_ceil(SLOTS_BETWEEN_STEPS))
                .map_err(|reason| format!("faults: {reason}"))?;
        }
        let mut last = 0;
        for (i, order) in self.tape.iter().enumerate() {
            if order.slot < last || order.slot >= self.duration_slots {
                return Err(format!(
                    "tape order {i} at slot {} is out of order or past the session",
                    order.slot
                ));
            }
            if !(order.size.is_finite() && order.size > 0.0)
                || !(order.limit_bps.is_finite() && order.limit_bps > 0.0)
                || order.account >= crate::making::TAKER_ACCOUNTS
            {
                return Err(format!("tape order {i} is malformed: {order:?}"));
            }
            last = order.slot;
        }
        if free_from > self.duration_slots {
            return Err(format!(
                "the last move runs until slot {free_from}, past the session end at {}",
                self.duration_slots
            ));
        }
        Ok(())
    }

    /// The scenario's fair value in USD at `slot`, counted from the session
    /// start, if it reports one (see [`Scenario::fair_path`]). Before the
    /// first waypoint it is the starting mark.
    pub fn fair_value_usd(&self, start_mark_usd: f64, slot: u64) -> Option<f64> {
        let path = self.fair_path.as_ref()?;
        Some(start_mark_usd * (1.0 + level_at(path, slot) / 100.0))
    }

    /// Total variation of the path, in percent: the sum of every move's size,
    /// from the starting mark through each waypoint.
    /// With premiums, the path is the book's: the price a bot trades at.
    pub fn total_variation_pct(&self) -> f64 {
        let mut level = 0.0;
        let mut total = 0.0;
        for waypoint in &self.path {
            total += (waypoint.book_pct() - level).abs();
            level = waypoint.book_pct();
        }
        total
    }

    /// The most a bot could make on this scenario with perfect hindsight:
    /// holding the full position cap the right way through every move, at no
    /// cost. No bot reaches it (fills cost fees and slippage, and the path is
    /// unknown), so a score against it never saturates.
    pub fn hindsight_bound_usd(&self, start_mark_usd: f64) -> f64 {
        self.max_position() * start_mark_usd * self.total_variation_pct() / 100.0
    }

    /// What the reward divides PnL by: the hindsight bound, plus, for a
    /// market-making scenario, the spread bound (see
    /// [`Making::spread_bound_usd`]). A maker can earn both: the spread on
    /// every taker order, and the moves it trades or leans into, so a bound
    /// on the spread alone is one a bot can pass.
    pub fn bound_usd(&self, start_mark_usd: f64) -> f64 {
        if let Some(spot) = &self.spot {
            return spot.bound_usd.unwrap_or(0.0);
        }
        let spread = self.making.as_ref().map_or(0.0, |making| {
            making.spread_bound_usd(&self.path, start_mark_usd)
        });
        spread + self.hindsight_bound_usd(start_mark_usd) + self.funding_bound_usd(start_mark_usd)
    }

    /// The most funding the cap could collect: held against every premium
    /// for as long as the path keeps it open, each at most the funding cap,
    /// at Phoenix's rate of the premium times the time over the funding
    /// period.
    pub fn funding_bound_usd(&self, start_mark_usd: f64) -> f64 {
        let Some(funding) = &self.funding else {
            return 0.0;
        };
        let cap_pct = funding.max_rate as f64 / 1_000.0 / start_mark_usd
            * 100.0
            * funding.period_seconds as f64
            / funding.interval_seconds as f64;
        let ends = self
            .path
            .iter()
            .skip(1)
            .map(|w| w.slot)
            .chain(std::iter::once(self.duration_slots));
        let premium_seconds: f64 = self
            .path
            .iter()
            .zip(ends)
            .map(|(w, end)| {
                w.premium_pct.abs().min(cap_pct) * end.saturating_sub(w.slot) as f64 * SLOT_SECONDS
            })
            .sum();
        self.max_position() * start_mark_usd * premium_seconds
            / 100.0
            / funding.period_seconds as f64
    }

    /// Surfpool overrides for the whole path, with slots relative to when
    /// they are registered, given the mark at that moment.
    pub fn overrides(&self, perp_asset_map: Pubkey, start_mark_ticks: u64) -> Vec<Override> {
        let ticks_at =
            |level_pct: f64| (start_mark_ticks as f64 * (1.0 + level_pct / 100.0)).round() as u64;
        if let Some(off) = &self.offhours {
            return self.offhours_overrides(off, perp_asset_map, start_mark_ticks);
        }
        let mut from = start_mark_ticks;
        let mut overrides = Vec::new();
        for waypoint in &self.path {
            let to = ticks_at(waypoint.book_pct());
            let (mut moves, at) =
                market_move(perp_asset_map, &self.symbol, from, to, waypoint.slot);
            // The makers stay at the book's level; the oracles go back to
            // the index in the slot of the last step, before the uncross.
            if waypoint.premium_pct != 0.0 {
                let uncross = moves.pop().expect("a move ends with its uncross");
                moves.push(oracle_move(
                    perp_asset_map,
                    &self.symbol,
                    ticks_at(waypoint.level_pct),
                    at - SLOTS_BETWEEN_STEPS,
                ));
                moves.push(uncross);
            }
            overrides.extend(moves);
            from = to;
        }
        overrides
    }
}

/// The largest premium a waypoint may give the book, in percent: Phoenix
/// refuses oracle reports more than 5% from its median, and a move steps the
/// oracles from the index to the book.
const MAX_PREMIUM_PCT: f64 = 3.0;

/// Set the market's funding parameters, `slot` slots after the override is
/// registered. The session plays it in setup, before the bot starts (see
/// [`crate::session`]).
pub fn funding_override(
    perp_asset_map: Pubkey,
    symbol: &str,
    funding: &Funding,
    slot: u64,
) -> Override {
    Override {
        template_id: "phoenix-market-funding",
        account: perp_asset_map,
        values: serde_json::json!({
            "symbol": symbol,
            "fundingIntervalSeconds": funding.interval_seconds.to_string(),
            "fundingPeriodSeconds": funding.period_seconds.to_string(),
            "maxFundingRate": funding.max_rate.to_string(),
        }),
        slot,
        label: "funding".into(),
    }
}

impl Scenario {
    /// The price an open position is valued at, at the end and for the
    /// drawdown: `mark` (Phoenix's), `oracle` (the scenario's level, for
    /// market making, where a bot's quotes move the mark) or `book` (the
    /// scenario's book level, where the book trades at a premium: what the
    /// position would close at, which Phoenix's mark, a blend of the book
    /// and the index, is not).
    pub fn valuation(&self) -> &'static str {
        if self.making.is_some() {
            "oracle"
        } else if self.path.iter().any(|w| w.premium_pct != 0.0) || self.funding.is_some() {
            "book"
        } else {
            "mark"
        }
    }

    /// Slots the session's setup gets before the bot's first slot: setup
    /// must end inside them for the session to replay (state that depends
    /// on the slot, Phoenix's funding and the oracle's age, starts from the
    /// same slot every time). Onboarding takers takes longer, and longer
    /// still under load.
    pub fn setup_slots(&self) -> u64 {
        if !self.tape.is_empty() {
            TAPE_SETUP_SLOTS
        } else if self.offhours.is_some() {
            OFFHOURS_SETUP_SLOTS
        } else {
            SETUP_SLOTS
        }
    }

    /// An off-hours path: through the closed period, each move of the perp
    /// lands as Phoenix moves a market (oracle reports and makers), and in
    /// the same slot the oracle reports go back to the closed period's
    /// index, which only the reports carry: the makers stay on the perp's
    /// price. Between moves the index updates on its own. At the reopen the
    /// market moves from the index to the open, oracle and makers together,
    /// as on any weekday.
    fn offhours_overrides(
        &self,
        off: &OffHours,
        perp_asset_map: Pubkey,
        start_mark_ticks: u64,
    ) -> Vec<Override> {
        let ticks_at =
            |level_pct: f64| (start_mark_ticks as f64 * (1.0 + level_pct / 100.0)).round() as u64;
        // (slot, order within the slot, override): a move, then the index,
        // then the uncross.
        let mut timed: Vec<(u64, u8, Override)> = Vec::new();
        let mut from = start_mark_ticks;
        let mut moved_at = std::collections::BTreeSet::new();
        for waypoint in &self.path {
            let to = ticks_at(waypoint.level_pct);
            if waypoint.slot < off.reopen_slot {
                let (moves, _) = market_move(perp_asset_map, &self.symbol, from, to, waypoint.slot);
                for o in moves {
                    let order = if o.template_id == "phoenix-market-uncross" {
                        2
                    } else {
                        0
                    };
                    timed.push((o.slot, order, o));
                }
                let index = ticks_at(off.index_at(&self.path, waypoint.slot));
                timed.push((
                    waypoint.slot,
                    1,
                    oracle_move(perp_asset_map, &self.symbol, index, waypoint.slot),
                ));
                moved_at.insert(waypoint.slot);
            } else {
                // The reopen: from wherever the index is, to the open, in
                // Phoenix's steps but all in one slot: the index snaps, as
                // Phoenix's does, and nobody trades between the steps.
                let atomic = waypoint.slot == off.reopen_slot;
                if atomic {
                    from = ticks_at(off.index_at(&self.path, off.reopen_slot - 1));
                }
                let (moves, _) = market_move(perp_asset_map, &self.symbol, from, to, waypoint.slot);
                for mut o in moves {
                    let order = if o.template_id == "phoenix-market-uncross" {
                        2
                    } else {
                        0
                    };
                    if atomic {
                        o.slot = waypoint.slot;
                    }
                    timed.push((o.slot, order, o));
                }
            }
            from = to;
        }
        for w in &off.index {
            if !moved_at.contains(&w.slot) {
                timed.push((
                    w.slot,
                    1,
                    oracle_move(perp_asset_map, &self.symbol, ticks_at(w.level_pct), w.slot),
                ));
            }
        }
        timed.sort_by_key(|(slot, order, _)| (*slot, *order));
        timed.into_iter().map(|(_, _, o)| o).collect()
    }
}

/// Setup slots for most sessions (see [`Scenario::setup_slots`]).
pub const SETUP_SLOTS: u64 = 60;
/// An off-hours session onboards eight takers before the bot: under load (a
/// verifier grades the reference four sessions at a time) that runs past
/// 60 slots.
pub const OFFHOURS_SETUP_SLOTS: u64 = 240;
/// A session that also onboards a taker tape.
pub const TAPE_SETUP_SLOTS: u64 = 600;

/// The largest part of an at-once move, in percent of the level it starts
/// from: under Phoenix's 4% step (`MAX_MOVE_STEP_PCT`), so each part compiles
/// to exactly one step.
const JUMP_PART_PCT: f64 = 3.8;

/// Slots a move between two levels spans, uncross included. Mirrors
/// [`market_move`]'s stepping.
pub(crate) fn move_slots(from_pct: f64, to_pct: f64) -> u64 {
    let ratio = (1.0 + to_pct / 100.0) / (1.0 + from_pct / 100.0);
    let steps = (((ratio - 1.0).abs() * 100.0 / 4.0).ceil() as u64).max(1);
    steps * SLOTS_BETWEEN_STEPS
}

/// Which way the shock goes. Half the family's scenarios crash, half
/// squeeze, so a blind bet at the open has no edge: a family where every
/// scenario moved the same way would pay a bot for knowing the family, not
/// for reading the market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The market drops: forced selling.
    Crash,
    /// The market spikes: forced buying.
    Squeeze,
}

impl Direction {
    fn sign(self) -> f64 {
        match self {
            Self::Crash => -1.0,
            Self::Squeeze => 1.0,
        }
    }
}

/// What the market does once the shock has held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Aftermath {
    /// It reverts, giving back part or all of the shock.
    Revert,
    /// It barely moves back: trading the reversion doesn't pay.
    Stall,
    /// It keeps going the shock's way: fading the shock loses.
    Extend,
}

/// Parameters for a family of shock scenarios: a crash or a squeeze in one
/// or more legs, then its aftermath.
///
/// The path is built so that its shape gives nothing away beyond what a
/// trader would see in a real market, and so that what it does give away
/// doesn't pay for certain (the exploit checks in `bench/checks/exploits`
/// hold it to that):
/// - Leg sizes are log-uniform over a wide range, and most legs land at
///   once, so a move's first print says little about how far it goes and
///   leaves little to ride. The rest ramp in unequal steps.
/// - Quiet stretches carry fizzles: moves that start like a leg and give
///   most or all of it back, so not every move away is a shock, and fading
///   one carries risk: how much comes back is drawn, not fixed.
/// - Each leg is drawn independently and another follows with fixed odds, so
///   a leg's size says nothing about whether another is coming; pauses and
///   the hold before the aftermath share one range.
/// - The session's length is drawn before the path and independently of it.
///   The path is cut where the session ends, so the end slot dates nothing:
///   a session may end mid-shock.
/// - Between moves the mark wobbles, so quiet stretches aren't flat lines.
#[derive(Debug, Clone)]
pub struct ShockFamily {
    pub symbol: String,
    /// Range of the session's length, in slots. Drawn before the path.
    pub duration: (u64, u64),
    /// Range of the slot the shock starts at.
    pub shock_slot: (u64, u64),
    /// Range of each leg's size, in percent of the starting mark. Sizes are
    /// log-uniform over it: small legs are as common as large ones.
    pub leg_pct: (f64, f64),
    /// Odds a leg or the aftermath lands at once, as one move; the rest ramp.
    pub gap_odds: f64,
    /// Odds that another leg follows a leg, up to `max_legs`.
    pub extra_leg_odds: f64,
    pub max_legs: u32,
    /// Range of how many slots the market holds still after a leg, before
    /// the next leg or the aftermath.
    pub pause_slots: (u64, u64),
    /// Odds of a crash; a squeeze takes the rest.
    pub crash_odds: f64,
    /// Odds of a reversion and of a stall; an extension takes the rest.
    pub revert_odds: f64,
    pub stall_odds: f64,
    /// Range of how much of the shock a reversion gives back, 0 to 1.
    pub revert_recovery: (f64, f64),
    /// Range of how much of the shock a stall gives back, 0 to 1.
    pub stall_recovery: (f64, f64),
    /// Range of how much further an extension goes, in percent of the starting mark.
    pub extend_pct: (f64, f64),
    /// Largest single step of a ramp, in percent of the starting mark. Phoenix
    /// moves at most 4% of the current price per step; 3% of the start stays
    /// under that down to a 25% drop.
    pub max_step_pct: f64,
    /// Range of slots between a ramp's steps.
    pub step_gap: (u64, u64),
    /// Odds a quiet stretch carries a fizzle, its size in percent (either
    /// way), how many slots it holds, and the share of the move it then
    /// gives back. In 0.10.0 fizzles gave back all of it, to the exact level
    /// they left, so fading one once it landed was nearly riskless, and it
    /// was the best bots' largest edge.
    pub fizzle_odds: f64,
    pub fizzle_pct: (f64, f64),
    pub fizzle_hold: (u64, u64),
    pub fizzle_giveback: (f64, f64),
    /// Largest change of the wobble per tick, in percent of the starting mark.
    pub noise_pct: f64,
    /// Range of slots between wobble ticks.
    pub noise_gap: (u64, u64),
    /// The position cap, in base units, every scenario in the family carries.
    pub max_position: f64,
    /// The drawdown limit, in USD, every scenario in the family carries.
    pub max_drawdown_usd: f64,
}

/// What a draw from a [`ShockFamily`] is, beneath its path: the regime a
/// session is, which the ladder's regime oracle is told and no bot is.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    pub direction: Direction,
    pub aftermath: Aftermath,
    pub legs: u32,
    /// Size of the first leg, in percent of the starting mark.
    pub first_leg_pct: f64,
    /// Level of the shock's extreme, before the aftermath, in percent.
    pub extreme_pct: f64,
    /// Level the aftermath ends at, in percent.
    pub end_pct: f64,
    /// Slots from the last waypoint to the session's end.
    pub tail_slots: u64,
    /// Whether the session ended before the aftermath had landed.
    pub cut: bool,
}

impl ShockFamily {
    pub fn spcx() -> Self {
        Self {
            symbol: "SPCX".into(),
            duration: (320, 720),
            shock_slot: (40, 200),
            leg_pct: (0.6, 8.0),
            gap_odds: 0.7,
            extra_leg_odds: 0.35,
            max_legs: 3,
            pause_slots: (30, 140),
            crash_odds: 0.5,
            revert_odds: 0.65,
            stall_odds: 0.2,
            revert_recovery: (0.5, 1.0),
            stall_recovery: (0.0, 0.2),
            extend_pct: (1.0, 5.0),
            max_step_pct: 3.0,
            step_gap: (3, 6),
            fizzle_odds: 0.5,
            fizzle_pct: (0.4, 2.0),
            fizzle_hold: (6, 40),
            fizzle_giveback: (0.4, 1.0),
            noise_pct: 0.12,
            noise_gap: (4, 12),
            // ~1x the 10,000 USDC of collateral at the snapshot's ~$143 mark.
            max_position: 70.0,
            // 10% of the collateral: the reference's worst path (both
            // tranches on, then stopped out at 13.5%) stays inside it; a
            // blind full-size bet caught by a deep shock breaks it.
            max_drawdown_usd: 1_000.0,
        }
    }

    /// A scenario drawn from the family. The same seed always yields the same
    /// scenario.
    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    /// A scenario and the shape it was drawn from.
    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        self.draw_with(name, seed, false)
    }

    /// [`Self::draw`], with the scenario reporting a fair value (see
    /// [`Scenario::fair_path`]): the level the shock settles at, from the slot
    /// it starts. The draw is otherwise the same, so the scenario's path and
    /// shape are those of `draw` for the same seed. Wobble, fizzles and the
    /// overshoot of a reversion move the mark, not the fair value, so a fill
    /// is judged by where the market ends up rather than by where it sits
    /// when the bot trades.
    pub fn draw_marked(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        self.draw_with(name, seed, true)
    }

    fn draw_with(&self, name: &str, seed: u64, marked: bool) -> (Scenario, Shape) {
        let mut rng = SplitMix64(seed);
        // Direction and aftermath come first, so `kind` can read them alone.
        let direction = self.pick_direction(rng.next_f64());
        let aftermath = self.pick_aftermath(rng.next_f64());
        let sign = direction.sign();
        // The session's length comes before the path, so the path can't
        // date it.
        let duration_slots = rng.range_u64(self.duration);
        let shock_slot = rng.range_u64(self.shock_slot);
        let mut legs = 1;
        while legs < self.max_legs && rng.next_f64() < self.extra_leg_odds {
            legs += 1;
        }

        let mut path = PathBuilder::new(self, rng);
        path.quiet(0.0, shock_slot);
        let mut base = 0.0;
        let mut first_leg_pct = 0.0;
        for leg in 0..legs {
            let size = path.rng.log_range_f64(self.leg_pct);
            if leg == 0 {
                first_leg_pct = size;
            }
            base = round(base + sign * size);
            path.travel(base);
            let pause = path.rng.range_u64(self.pause_slots);
            path.quiet(base, path.slot + pause);
        }
        let extreme = base;
        let end = match aftermath {
            Aftermath::Revert => round(extreme * (1.0 - path.rng.range_f64(self.revert_recovery))),
            Aftermath::Stall => round(extreme * (1.0 - path.rng.range_f64(self.stall_recovery))),
            Aftermath::Extend => round(extreme + sign * path.rng.range_f64(self.extend_pct)),
        };
        path.travel(end);
        let aftermath_landed = path.slot;
        path.quiet(end, duration_slots);
        let cut = aftermath_landed > duration_slots;
        path.cut(duration_slots);
        let tail = duration_slots - path.waypoints.last().map_or(0, |w| w.slot);

        let depth = extreme.abs();
        let legs_text = if legs == 1 {
            "at once".to_string()
        } else {
            format!("in {legs} legs")
        };
        let (moves, then) = match (direction, aftermath) {
            (Direction::Crash, Aftermath::Revert) => ("drops", "rebounds"),
            (Direction::Crash, Aftermath::Stall) => ("drops", "stalls"),
            (Direction::Crash, Aftermath::Extend) => ("drops", "slides further"),
            (Direction::Squeeze, Aftermath::Revert) => ("spikes", "fades"),
            (Direction::Squeeze, Aftermath::Stall) => ("spikes", "stalls"),
            (Direction::Squeeze, Aftermath::Extend) => ("spikes", "keeps running"),
        };
        let scenario = Scenario {
            name: name.to_string(),
            description: if cut {
                format!(
                    "{} {moves} {depth:.1}% {legs_text} from around slot {shock_slot}; the session ends before it {then}",
                    self.symbol
                )
            } else {
                format!(
                    "{} {moves} {depth:.1}% {legs_text} from around slot {shock_slot}, then {then} to {end:+.2}% of its starting mark",
                    self.symbol
                )
            },
            symbol: self.symbol.clone(),
            duration_slots,
            mandate: Mandate::perp(
                VENUE,
                &self.symbol,
                self.max_position,
                self.max_drawdown_usd,
            ),
            path: path.waypoints,
            making: None,
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            turn_data: None,
            order: None,
            participation: None,
            net: None,
            reference_pnl_usd: None,
            fair_path: marked.then(|| {
                vec![Waypoint {
                    slot: shock_slot,
                    level_pct: end,
                    premium_pct: 0.0,
                }]
            }),
            raw_sends: None,
        };
        let shape = Shape {
            direction,
            aftermath,
            legs,
            first_leg_pct,
            extreme_pct: extreme,
            end_pct: end,
            tail_slots: tail,
            cut,
        };
        (scenario, shape)
    }

    /// `count` scenarios named `<prefix>-<n>`, stratified: each direction and
    /// aftermath appears in proportion to its odds (largest remainder), so a
    /// small set isn't skewed by luck of the draw. Seeds derive from `seed`;
    /// scenarios keep their draw order.
    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// [`Self::sample_set`], with each scenario's [`Shape`].
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let mut cells: Vec<((Direction, Aftermath), f64)> = Vec::new();
        for (direction, p_dir) in [
            (Direction::Crash, self.crash_odds),
            (Direction::Squeeze, 1.0 - self.crash_odds),
        ] {
            for (aftermath, p_after) in [
                (Aftermath::Revert, self.revert_odds),
                (Aftermath::Stall, self.stall_odds),
                (Aftermath::Extend, 1.0 - self.revert_odds - self.stall_odds),
            ] {
                cells.push(((direction, aftermath), p_dir * p_after));
            }
        }
        let mut quotas: Vec<((Direction, Aftermath), usize, f64)> = cells
            .iter()
            .map(|&(cell, p)| {
                let exact = p * count as f64;
                (cell, exact.floor() as usize, exact - exact.floor())
            })
            .collect();
        let short = count - quotas.iter().map(|q| q.1).sum::<usize>();
        let mut by_remainder: Vec<usize> = (0..quotas.len()).collect();
        // Stable: ties go to the earlier cell.
        by_remainder.sort_by(|&a, &b| quotas[b].2.total_cmp(&quotas[a].2));
        for &i in by_remainder.iter().take(short) {
            quotas[i].1 += 1;
        }

        let mut scenarios = Vec::with_capacity(count);
        let mut draw = 0u64;
        while scenarios.len() < count {
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let cell = self.kind(draw_seed);
            let quota = quotas.iter_mut().find(|q| q.0 == cell).expect("every cell");
            if quota.1 == 0 {
                continue;
            }
            quota.1 -= 1;
            let name = format!("{prefix}-{}", scenarios.len() + 1);
            scenarios.push(self.draw(&name, draw_seed));
        }
        scenarios
    }

    /// The direction and aftermath a seed draws, without building the
    /// scenario: the first two draws of [`Self::draw`].
    pub fn kind(&self, seed: u64) -> (Direction, Aftermath) {
        let mut rng = SplitMix64(seed);
        let direction = self.pick_direction(rng.next_f64());
        (direction, self.pick_aftermath(rng.next_f64()))
    }

    fn pick_aftermath(&self, draw: f64) -> Aftermath {
        if draw < self.revert_odds {
            Aftermath::Revert
        } else if draw < self.revert_odds + self.stall_odds {
            Aftermath::Stall
        } else {
            Aftermath::Extend
        }
    }

    fn pick_direction(&self, draw: f64) -> Direction {
        if draw < self.crash_odds {
            Direction::Crash
        } else {
            Direction::Squeeze
        }
    }
}

fn round(level_pct: f64) -> f64 {
    (level_pct * 1000.0).round() / 1000.0
}

/// Builds a family's waypoints in slot order. Each waypoint is one step
/// Phoenix accepts in a single move, so the path compiles to exactly the
/// steps drawn here.
struct PathBuilder<'a> {
    family: &'a ShockFamily,
    rng: SplitMix64,
    waypoints: Vec<Waypoint>,
    /// Current level, wobble included.
    level: f64,
    /// First slot free for the next waypoint.
    slot: u64,
}

impl<'a> PathBuilder<'a> {
    fn new(family: &'a ShockFamily, rng: SplitMix64) -> Self {
        Self {
            family,
            rng,
            waypoints: Vec::new(),
            level: 0.0,
            slot: 0,
        }
    }

    fn push(&mut self, level_pct: f64, gap: u64) {
        self.waypoints.push(Waypoint::at(self.slot, level_pct));
        self.level = level_pct;
        self.slot += gap;
    }

    /// Go to `to`: at once with the family's gap odds, else in a ramp.
    fn travel(&mut self, to: f64) {
        if self.rng.next_f64() < self.family.gap_odds {
            self.jump(to);
        } else {
            self.ramp(to);
        }
    }

    /// Go to `to` at once: as fast as Phoenix moves its price. A move bigger
    /// than one of Phoenix's steps lands in parts on consecutive steps, of
    /// unequal random sizes, so the first part says nothing exact about the
    /// rest. Equal parts did: in 0.10.0, a first step of a given size told
    /// how many more were coming, and the best bots rode them.
    fn jump(&mut self, to: f64) {
        let total = to - self.level;
        let fits = |from: f64, to: f64| {
            ((1.0 + to / 100.0) / (1.0 + from / 100.0) - 1.0).abs() * 100.0 <= JUMP_PART_PCT
        };
        if fits(self.level, to) {
            let slots = move_slots(self.level, to) + 1;
            self.push(to, slots);
            return;
        }
        // Fewest parts that can fit, in ratio terms (a part is a ratio of
        // the level it starts from), sometimes one more.
        let ratio = ((1.0 + to / 100.0) / (1.0 + self.level / 100.0)).ln().abs();
        let min_parts = (ratio / (1.0 + JUMP_PART_PCT / 100.0).ln()).ceil().max(2.0) as u64;
        let mut parts = min_parts + self.rng.range_u64((0, 1));
        let from = self.level;
        // Random weights, redrawn until every part is one step Phoenix takes;
        // a part more if the draws keep missing.
        let mut tries = 0;
        let levels = loop {
            tries += 1;
            if tries % 64 == 0 {
                parts += 1;
            }
            let w: Vec<f64> = (0..parts).map(|_| 0.35 + self.rng.next_f64()).collect();
            let sum: f64 = w.iter().sum();
            let mut done = 0.0;
            let levels: Vec<f64> = w
                .iter()
                .enumerate()
                .map(|(i, x)| {
                    done += x / sum;
                    if i + 1 == w.len() {
                        to
                    } else {
                        round(from + total * done)
                    }
                })
                .collect();
            let mut prev = from;
            if levels.iter().all(|&l| {
                let ok = fits(prev, l);
                prev = l;
                ok
            }) {
                break levels;
            }
        };
        for level in levels {
            let slots = move_slots(self.level, level) + 1;
            self.push(level, slots);
        }
    }

    /// Walk to `to` in unequal steps at uneven intervals.
    fn ramp(&mut self, to: f64) {
        let total = to - self.level;
        // Enough steps that random weights rarely need redrawing.
        let min_steps = (total.abs() / (0.6 * self.family.max_step_pct))
            .ceil()
            .max(1.0) as u64;
        let steps = min_steps + self.rng.range_u64((0, 2));
        // Random weights, redrawn until no step is too large.
        let weights = loop {
            let w: Vec<f64> = (0..steps).map(|_| 0.25 + self.rng.next_f64()).collect();
            let sum: f64 = w.iter().sum();
            let w: Vec<f64> = w.iter().map(|x| x / sum).collect();
            if w.iter()
                .all(|x| (x * total).abs() <= self.family.max_step_pct)
            {
                break w;
            }
        };
        let from = self.level;
        let mut done = 0.0;
        for (i, weight) in weights.iter().enumerate() {
            done += weight;
            let level = if i + 1 == weights.len() {
                to
            } else {
                round(from + total * done)
            };
            let gap = self.rng.range_u64(self.family.step_gap);
            self.push(level, gap);
        }
    }

    /// Wobble around `base` until `until`, leaving room for a move there.
    /// With the family's fizzle odds, somewhere in the stretch the market
    /// moves away as a leg would, holds, and gives most or all of it back;
    /// the rest of the stretch wobbles around where it settled.
    fn quiet(&mut self, base: f64, until: u64) {
        let family = self.family;
        let fizzle = if self.rng.next_f64() < family.fizzle_odds {
            let sign = if self.rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
            let size = sign * self.rng.range_f64(family.fizzle_pct);
            let hold = self.rng.range_u64(family.fizzle_hold);
            // Room for the move out, the hold and the move back.
            let needs = 2 * move_slots(0.0, family.fizzle_pct.1) + hold + 12;
            let room = until.saturating_sub(self.slot);
            (room > needs + 8).then(|| {
                let at = self.slot + 4 + self.rng.range_u64((0, room - needs - 8));
                (at, size, hold)
            })
        } else {
            None
        };
        let mut settle = base;
        if let Some((at, size, hold)) = fizzle {
            let giveback = self.rng.range_f64(family.fizzle_giveback);
            self.wobble(base, at);
            let away = round(base + size);
            self.travel(away);
            self.wobble(away, self.slot + hold);
            settle = round(away - size * giveback);
            self.travel(settle);
        }
        self.wobble(settle, until);
    }

    fn wobble(&mut self, base: f64, until: u64) {
        let mut offset = self.level - base;
        loop {
            let gap = self.rng.range_u64(self.family.noise_gap);
            if self.slot + gap + 3 > until {
                break;
            }
            self.slot += gap;
            let step = self.family.noise_pct * (2.0 * self.rng.next_f64() - 1.0);
            offset = 0.5 * offset + step;
            self.push(round(base + offset), 0);
        }
        self.slot = self.slot.max(until);
    }

    /// End the path at `duration`: drop every waypoint whose move wouldn't
    /// finish by then.
    fn cut(&mut self, duration: u64) {
        let mut level = 0.0;
        let mut keep = 0;
        for (i, w) in self.waypoints.iter().enumerate() {
            if w.slot + move_slots(level, w.level_pct) + 1 > duration {
                break;
            }
            level = w.level_pct;
            keep = i + 1;
        }
        self.waypoints.truncate(keep);
    }
}

/// Small, dependency-free, and stable across platforms and releases, which a
/// generator for checked-in task data needs more than statistical quality.
pub(crate) struct SplitMix64(pub(crate) u64);

impl SplitMix64 {
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub(crate) fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub(crate) fn range_f64(&mut self, (lo, hi): (f64, f64)) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }

    /// Log-uniform over `[lo, hi]`, both positive.
    pub(crate) fn log_range_f64(&mut self, (lo, hi): (f64, f64)) -> f64 {
        (lo.ln() + (hi.ln() - lo.ln()) * self.next_f64()).exp()
    }

    pub(crate) fn range_u64(&mut self, (lo, hi): (u64, u64)) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }

    /// A standard normal draw (Box-Muller).
    pub(crate) fn normal(&mut self) -> f64 {
        let u = self.next_f64().max(f64::MIN_POSITIVE);
        let v = self.next_f64();
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_are_stratified_by_direction_and_aftermath() {
        let family = ShockFamily::spcx();
        let set = family.sample_set("h", 12, 77);
        assert_eq!(set.len(), 12);
        let n = |a: &str, b: &str| {
            set.iter()
                .filter(|s| s.description.contains(a) && s.description.contains(b))
                .count()
        };
        assert_eq!(
            [
                n("drops", "rebounds"),
                n("drops", "stalls"),
                n("drops", "slides")
            ],
            [4, 1, 1]
        );
        assert_eq!(
            [
                n("spikes", "fades"),
                n("spikes", "stalls"),
                n("spikes", "keeps running")
            ],
            [4, 1, 1]
        );
        assert_eq!(set, family.sample_set("h", 12, 77));
    }

    #[test]
    fn the_family_draws_every_kind_at_roughly_its_odds() {
        let family = ShockFamily::spcx();
        let draws: Vec<(Direction, Aftermath)> = (0..4_000).map(|seed| family.kind(seed)).collect();
        let share = |f: &dyn Fn(&(Direction, Aftermath)) -> bool| {
            draws.iter().filter(|d| f(d)).count() as f64 / draws.len() as f64
        };
        assert!((share(&|d| d.0 == Direction::Crash) - 0.5).abs() < 0.03);
        assert!((share(&|d| d.1 == Aftermath::Revert) - 0.65).abs() < 0.03);
        assert!((share(&|d| d.1 == Aftermath::Stall) - 0.2).abs() < 0.03);
        assert!((share(&|d| d.1 == Aftermath::Extend) - 0.15).abs() < 0.03);
    }

    #[test]
    fn shocks_go_their_way_and_aftermaths_follow() {
        let family = ShockFamily::spcx();
        // A fizzle can take the mark past the shock's levels, and back.
        let slack = family.fizzle_pct.1 + 2.0 * family.noise_pct;
        for seed in 0..200 {
            let (scenario, shape) = family.draw("s", seed);
            assert_eq!((shape.direction, shape.aftermath), family.kind(seed));
            let sign = shape.direction.sign();
            let extreme = sign * shape.extreme_pct;
            let end = sign * shape.end_pct;
            assert!(extreme >= family.leg_pct.0 - 0.01, "seed {seed}: {shape:?}");
            match shape.aftermath {
                Aftermath::Extend => assert!(end > extreme, "seed {seed}"),
                _ => assert!(end <= extreme && end >= 0.0, "seed {seed}"),
            }
            if shape.cut {
                continue;
            }
            let levels: Vec<f64> = scenario.path.iter().map(|w| w.level_pct * sign).collect();
            let peak = levels.iter().cloned().fold(f64::MIN, f64::max);
            assert!(peak < extreme.max(end) + slack, "seed {seed}");
            assert!((levels.last().unwrap() - end).abs() < slack, "seed {seed}");
        }
    }

    /// Most moves land at once, so a move's first print leaves little to
    /// ride; the rest ramp in unequal steps, none larger than Phoenix takes.
    #[test]
    fn most_moves_land_at_once() {
        let family = ShockFamily::spcx();
        let (mut at_once, mut uneven, mut ramps) = (0, 0, 0);
        for seed in 0..400 {
            let mut path = PathBuilder::new(&family, SplitMix64(seed));
            // Within one of Phoenix's steps: at once is a single step.
            path.travel(3.5);
            let steps: Vec<f64> = std::iter::once(0.0)
                .chain(path.waypoints.iter().map(|w| w.level_pct))
                .collect::<Vec<_>>()
                .windows(2)
                .map(|w| w[1] - w[0])
                .collect();
            if steps.len() == 1 {
                at_once += 1;
                continue;
            }
            ramps += 1;
            assert!(
                steps.iter().all(|d| *d <= family.max_step_pct + 1e-9),
                "seed {seed}"
            );
            let (lo, hi) = steps
                .iter()
                .fold((f64::MAX, f64::MIN), |(lo, hi), d| (lo.min(*d), hi.max(*d)));
            if hi - lo > 0.2 {
                uneven += 1;
            }
        }
        let share = at_once as f64 / 400.0;
        assert!(
            (share - family.gap_odds).abs() < 0.07,
            "{share} land at once"
        );
        assert!(
            uneven * 4 > ramps * 3,
            "only {uneven} of {ramps} ramps are uneven"
        );
    }

    /// A move bigger than one of Phoenix's steps lands at once in parts that
    /// Phoenix takes in one step each, of unequal sizes, so the first part
    /// doesn't say how many follow.
    #[test]
    fn big_moves_land_in_unequal_parts() {
        let family = ShockFamily::spcx();
        let mut first_parts = std::collections::BTreeSet::new();
        let mut part_counts = std::collections::BTreeSet::new();
        for seed in 0..300 {
            for total in [-8.0, 6.0, -5.0] {
                let mut path = PathBuilder::new(&family, SplitMix64(seed));
                path.jump(total);
                let levels: Vec<f64> = std::iter::once(0.0)
                    .chain(path.waypoints.iter().map(|w| w.level_pct))
                    .collect();
                assert_eq!(
                    *levels.last().unwrap(),
                    total,
                    "seed {seed}: lands on the target"
                );
                for w in levels.windows(2) {
                    let step = ((1.0 + w[1] / 100.0) / (1.0 + w[0] / 100.0) - 1.0).abs() * 100.0;
                    assert!(step <= JUMP_PART_PCT + 1e-9, "seed {seed}: a {step}% part");
                }
                let parts: Vec<f64> = levels.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
                first_parts.insert((parts[0] * 10.0).round() as i64);
                part_counts.insert((total as i64, parts.len()));
                let (lo, hi) = parts
                    .iter()
                    .fold((f64::MAX, f64::MIN), |(lo, hi), d| (lo.min(*d), hi.max(*d)));
                assert!(
                    parts.len() < 2 || hi - lo > 1e-6,
                    "seed {seed}: equal parts {parts:?}"
                );
            }
        }
        // First parts spread over many sizes, and a move's size doesn't fix
        // its number of parts.
        assert!(first_parts.len() > 15, "{first_parts:?}");
        assert!(part_counts.iter().filter(|(t, _)| *t == -8).count() > 1);
    }

    /// Quiet stretches carry fizzles: a move away that comes most or all of
    /// the way back, by a share that varies.
    #[test]
    fn fizzles_come_most_of_the_way_back() {
        let family = ShockFamily::spcx();
        let mut fizzled = 0;
        let mut kept = Vec::new();
        for seed in 0..200 {
            let mut path = PathBuilder::new(&family, SplitMix64(seed));
            path.quiet(0.0, 300);
            let away = path
                .waypoints
                .iter()
                .any(|w| w.level_pct.abs() >= family.fizzle_pct.0 - 1e-9);
            if away {
                fizzled += 1;
                // It stays near its size, and settles back most of the way.
                assert!(
                    path.waypoints
                        .iter()
                        .all(|w| w.level_pct.abs() <= family.fizzle_pct.1 + 1.0),
                    "seed {seed}"
                );
                let peak = path
                    .waypoints
                    .iter()
                    .map(|w| w.level_pct.abs())
                    .fold(0.0, f64::max);
                let left = path.waypoints.last().unwrap().level_pct.abs();
                assert!(
                    left < (1.0 - family.fizzle_giveback.0) * peak + 2.0 * family.noise_pct,
                    "seed {seed}"
                );
                kept.push(left / peak);
            }
        }
        // How much stays away varies: no exact level to fade back to.
        kept.sort_by(f64::total_cmp);
        assert!(
            kept[kept.len() / 10] < 0.1 && kept[kept.len() * 9 / 10] > 0.3,
            "{kept:?}"
        );
        let share = fizzled as f64 / 200.0;
        assert!(
            (share - family.fizzle_odds).abs() < 0.1,
            "{share} of stretches fizzle"
        );
    }

    /// The session's length is drawn before the path: how long a session
    /// runs says nothing about its shock, and some end before the aftermath.
    #[test]
    fn the_end_dates_nothing() {
        let family = ShockFamily::spcx();
        let draws: Vec<(Scenario, Shape)> = (0..600).map(|seed| family.draw("s", seed)).collect();
        let mean = |legs: u32| {
            let d: Vec<f64> = draws
                .iter()
                .filter(|(_, s)| s.legs == legs)
                .map(|(sc, _)| sc.duration_slots as f64)
                .collect();
            d.iter().sum::<f64>() / d.len() as f64
        };
        assert!(
            (mean(1) - mean(3)).abs() / mean(1) < 0.08,
            "{} vs {}",
            mean(1),
            mean(3)
        );
        let cut = draws.iter().filter(|(_, s)| s.cut).count();
        assert!(cut > 0 && cut < 120, "{cut} of 600 sessions end mid-shock");
    }

    /// A shock's legs are drawn independently: how far the first leg went
    /// says nothing about whether another follows.
    #[test]
    fn a_legs_size_says_nothing_about_the_next() {
        let family = ShockFamily::spcx();
        let mid = (family.leg_pct.0 + family.leg_pct.1) / 2.0;
        let (mut small, mut small_more, mut large, mut large_more) = (0, 0, 0, 0);
        for seed in 0..4_000 {
            let (_, shape) = family.draw("s", seed);
            let first = shape.first_leg_pct;
            if first < mid {
                small += 1;
                small_more += (shape.legs > 1) as u32;
            } else {
                large += 1;
                large_more += (shape.legs > 1) as u32;
            }
        }
        let share = |more: u32, n: u32| more as f64 / n as f64;
        assert!((share(small_more, small) - share(large_more, large)).abs() < 0.05);
    }

    /// Across the family, a position taken blind at the open and held
    /// through the shock gains on half the draws and loses on the other half.
    #[test]
    fn a_blind_bet_at_the_open_has_no_edge() {
        let family = ShockFamily::spcx();
        let set = family.sample_set("s", 12, 5);
        let short_to_extreme: f64 = set
            .iter()
            .map(|s| {
                -s.path
                    .iter()
                    .map(|w| w.level_pct)
                    .fold(0.0_f64, |acc, l| if l.abs() > acc.abs() { l } else { acc })
            })
            .sum();
        assert!(
            short_to_extreme.abs() < 15.0,
            "a short at the open nets {short_to_extreme:.1}%"
        );
    }

    #[test]
    fn sampled_scenarios_are_valid_and_deterministic() {
        let family = ShockFamily::spcx();
        for seed in 0..200 {
            let scenario = family.sample("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}: {scenario:?}");
            assert_eq!(scenario, family.sample("s", seed));
        }
    }

    #[test]
    fn a_marked_draw_is_the_same_scenario_with_a_fair_value() {
        let family = ShockFamily::spcx();
        for seed in 0..60 {
            let (plain, shape) = family.draw("s", seed);
            let (marked, marked_shape) = family.draw_marked("s", seed);
            assert_eq!(shape, marked_shape);
            assert!(plain.fair_path.is_none());
            marked.validate().unwrap();
            assert_eq!(
                Scenario {
                    fair_path: None,
                    raw_sends: None,
                    ..marked.clone()
                },
                plain
            );
            // The fair value is the settled level, from the shock's start.
            let first = marked.fair_path.as_ref().unwrap()[0];
            assert_eq!(first.level_pct, shape.end_pct);
            assert_eq!(marked.fair_value_usd(100.0, 0), Some(100.0));
            assert_eq!(
                marked.fair_value_usd(100.0, marked.duration_slots),
                Some(100.0 * (1.0 + shape.end_pct / 100.0))
            );
            assert_eq!(plain.fair_value_usd(100.0, 50), None);
        }
    }

    #[test]
    fn a_scenario_file_without_a_fair_value_is_unchanged() {
        let scenario = ShockFamily::spcx().sample("s", 9);
        let json = serde_json::to_string_pretty(&scenario).unwrap();
        assert!(!json.contains("fair_path"));
        let back: Scenario = serde_json::from_str(&json).unwrap();
        assert_eq!(back, scenario);
        let (marked, _) = ShockFamily::spcx().draw_marked("s", 9);
        let back: Scenario =
            serde_json::from_str(&serde_json::to_string_pretty(&marked).unwrap()).unwrap();
        assert_eq!(back, marked);
    }

    #[test]
    fn fair_value_waypoints_must_be_in_order_and_inside_the_session() {
        let (mut scenario, _) = ShockFamily::spcx().draw_marked("s", 1);
        let late = scenario.duration_slots + 1;
        scenario.fair_path = Some(vec![Waypoint {
            slot: late,
            level_pct: 1.0,
            premium_pct: 0.0,
        }]);
        assert!(scenario.validate().unwrap_err().contains("fair value"));
        scenario.fair_path = Some(vec![
            Waypoint {
                slot: 50,
                level_pct: 1.0,
                premium_pct: 0.0,
            },
            Waypoint {
                slot: 40,
                level_pct: 2.0,
                premium_pct: 0.0,
            },
        ]);
        assert!(scenario.validate().unwrap_err().contains("fair value"));
    }

    #[test]
    fn overlapping_moves_are_rejected() {
        let scenario = Scenario {
            name: "overlap".into(),
            description: String::new(),
            symbol: "SPCX".into(),
            duration_slots: 400,
            mandate: Mandate::perp(VENUE, "SPCX", 70.0, 1_000.0),
            path: vec![Waypoint::at(10, -12.0), Waypoint::at(12, 0.0)],
            making: None,
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            turn_data: None,
            order: None,
            participation: None,
            net: None,
            reference_pnl_usd: None,
            fair_path: None,
            raw_sends: None,
        };
        assert!(scenario.validate().unwrap_err().contains("previous move"));

        let elsewhere = Scenario {
            mandate: Mandate::perp(VENUE, "SOL", 70.0, 1_000.0),
            path: vec![],
            ..scenario.clone()
        };
        assert!(
            elsewhere
                .validate()
                .unwrap_err()
                .contains("phoenix:perp:SPCX")
        );

        let mut wide = Scenario {
            path: vec![],
            ..scenario
        };
        wide.mandate
            .universe
            .extend(Mandate::perp(VENUE, "SOL", 1.0, 1.0).universe);
        assert!(wide.validate().unwrap_err().contains("exactly"));
    }

    #[test]
    fn overrides_walk_to_each_level_and_uncross() {
        let scenario = ShockFamily::spcx().sample("s", 7);
        let overrides = scenario.overrides(Pubkey::new_unique(), 10_000);
        let last_move = overrides
            .iter()
            .rev()
            .find(|o| o.template_id == "phoenix-market-move")
            .unwrap();
        let expected =
            (10_000.0 * (1.0 + scenario.path.last().unwrap().level_pct / 100.0)).round() as u64;
        assert_eq!(last_move.values["target_ticks"], expected.to_string());
        assert_eq!(
            overrides.last().unwrap().template_id,
            "phoenix-market-uncross"
        );
    }

    /// A scenario with one move and none of the opt-in hooks.
    fn plain() -> Scenario {
        Scenario {
            name: "plain".into(),
            description: String::new(),
            symbol: "SPCX".into(),
            duration_slots: 400,
            mandate: Mandate::perp(VENUE, "SPCX", 70.0, 1_000.0),
            path: vec![Waypoint::at(10, -2.0)],
            making: None,
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            turn_data: None,
            order: None,
            participation: None,
            net: None,
            reference_pnl_usd: None,
            fair_path: None,
            raw_sends: None,
        }
    }

    #[test]
    fn a_scenario_without_premiums_or_a_tape_is_as_before() {
        let plain = plain();
        assert!(plain.validate().is_ok());
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("tape").is_none());
        assert!(json["path"][0].get("premium_pct").is_none());
        assert_eq!(plain.valuation(), "mark");
        assert_eq!(plain.setup_slots(), SETUP_SLOTS);
        assert_eq!(plain.path[0].book_pct(), plain.path[0].level_pct);
    }

    #[test]
    fn a_premium_moves_the_book_and_returns_the_oracle_to_the_index() {
        let waypoint = Waypoint {
            premium_pct: 1.0,
            ..Waypoint::at(10, -2.0)
        };
        assert!((waypoint.book_pct() - -1.02).abs() < 1e-9);
        let premium = Scenario {
            path: vec![waypoint],
            ..plain()
        };
        assert!(premium.validate().is_ok());
        assert_eq!(premium.valuation(), "book");
        let overrides = premium.overrides(Pubkey::new_unique(), 10_000);
        let [.., index, uncross] = overrides.as_slice() else {
            panic!("a move and its uncross");
        };
        assert_eq!(index.template_id, "phoenix-oracle-price");
        assert_eq!(index.values["target_ticks"], "9800");
        assert_eq!(uncross.template_id, "phoenix-market-uncross");

        let too_far = Scenario {
            path: vec![Waypoint {
                premium_pct: 3.5,
                ..waypoint
            }],
            ..plain()
        };
        assert!(too_far.validate().unwrap_err().contains("premium"));
    }

    #[test]
    fn a_tape_is_checked_and_set_up_longer() {
        let order = |slot| crate::making::TakerOrder {
            slot,
            side: crate::making::TakerSide::Buy,
            size: 1.0,
            limit_bps: 20.0,
            account: 0,
        };
        let tape = Scenario {
            tape: vec![order(5), order(9)],
            ..plain()
        };
        assert!(tape.validate().is_ok());
        assert_eq!(tape.setup_slots(), TAPE_SETUP_SLOTS);
        let unordered = Scenario {
            tape: vec![order(9), order(5)],
            ..plain()
        };
        assert!(unordered.validate().unwrap_err().contains("tape order 1"));
        let late = Scenario {
            tape: vec![order(400)],
            ..plain()
        };
        assert!(late.validate().is_err());
    }

    #[test]
    fn funding_is_opt_in_and_checked() {
        assert_eq!(plain().funding_bound_usd(100.0), 0.0);
        let funding = Funding {
            interval_seconds: 4,
            period_seconds: 20,
            max_rate: 1_000,
            mark: None,
        };
        let funded = Scenario {
            funding: Some(funding),
            path: vec![Waypoint {
                premium_pct: 1.0,
                ..Waypoint::at(10, 0.0)
            }],
            ..plain()
        };
        assert!(funded.validate().is_ok());
        assert_eq!(funded.valuation(), "book");
        assert!(funded.funding_bound_usd(100.0) > 0.0);
        let json = serde_json::to_value(&funded).unwrap();
        assert_eq!(json["funding"]["interval_seconds"], 4);
        assert!(json["funding"].get("mark").is_none());

        let no_interval = Scenario {
            funding: Some(Funding {
                interval_seconds: 0,
                ..funding
            }),
            ..plain()
        };
        assert!(no_interval.validate().unwrap_err().contains("funding"));
        let short_period = Scenario {
            funding: Some(Funding {
                period_seconds: 2,
                ..funding
            }),
            ..plain()
        };
        assert!(short_period.validate().is_err());
    }

    #[test]
    fn off_hours_move_the_index_apart_and_reopen_at_once() {
        let off = OffHours {
            reopen_slot: 200,
            band_pct: 5.0,
            half_spread_bps: 10.0,
            index: vec![Waypoint::at(20, 1.0)],
            takers: Vec::new(),
        };
        let weekend = Scenario {
            path: vec![Waypoint::at(20, 1.5), Waypoint::at(200, 3.0)],
            offhours: Some(off.clone()),
            ..plain()
        };
        assert!(weekend.validate().is_ok());
        assert_eq!(weekend.setup_slots(), OFFHOURS_SETUP_SLOTS);
        assert_eq!(plain().setup_slots(), SETUP_SLOTS);
        let overrides = weekend.overrides(Pubkey::new_unique(), 10_000);
        let index = overrides
            .iter()
            .find(|o| o.template_id == "phoenix-oracle-price")
            .unwrap();
        assert_eq!(
            (index.slot, index.values["target_ticks"].as_str()),
            (20, Some("10100"))
        );
        assert!(
            overrides
                .iter()
                .filter(|o| o.slot >= 200)
                .all(|o| o.slot == 200),
            "the reopen lands in one slot"
        );

        let outside = Scenario {
            path: vec![Waypoint::at(20, 6.0), Waypoint::at(200, 3.0)],
            ..weekend.clone()
        };
        assert!(outside.validate().unwrap_err().contains("band"));
        let no_reopen = Scenario {
            path: vec![Waypoint::at(20, 1.5)],
            ..weekend.clone()
        };
        assert!(no_reopen.validate().unwrap_err().contains("reopen"));
        let also_making = Scenario {
            making: crate::making::MakingFamily::spcx().sample("m", 1).making,
            ..weekend
        };
        assert!(also_making.validate().is_err());
    }
}
