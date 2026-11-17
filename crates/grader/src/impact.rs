//! Price impact: a book that stays where trades leave it.
//!
//! Without it a perp session's book is centred on the scenario's path, and
//! the path is a schedule: the snapshot's makers re-centre on every planned
//! move at their full depth. A bot that walks the book up pays for the
//! depth it takes, and then finds the book back where it was: nothing it
//! does moves the price it trades at next.
//!
//! With a scenario's `impact`, the path is played by a Surfpool scenario
//! script instead of a schedule of overrides. Each step, on the surfnet, the
//! script asks Phoenix what a market order of a set size (the `probe`)
//! would fill for, each side. Where a buy fills further from the makers'
//! centre than it did on their untouched quotes, trades walked the asks up
//! by that much (and the same for a sale and the bids, down). The script
//! then centres the makers on the path's book level plus what the walking
//! left:
//!
//! - at once, the whole distance walked: the makers do not come back to
//!   refill what was taken, so the next trade starts where the last ended;
//! - of that, `permanent_share` for good, and the rest for a while: it
//!   halves every `half_life_slots`, the makers leaning back.
//!
//! The impact is never more than the trade itself moved the book, so a
//! round trip pays the spread and no more: buying up `d` ticks and selling
//! straight back sells into bids `d` higher, down `d`. A model that moved
//! the book by more than the trade walked would pay a bot to push it.
//!
//! A scenario can make the book harder to move over stretches of that
//! distance, its `resistance`: between 20 and 40 basis points above its
//! level, say, the book moves a quarter as far as trades walk it, and the
//! makers put back the depth the book did not keep. A zone is measured from
//! the path's level, not at a price, and it works the same both ways, in
//! and out: [`Curve`] says why anything else would pay a bot to push.
//!
//! Every taker's trades count, the bot's and the scenario's takers' alike:
//! the script reads the book, not an account. It reads it at a size, not at
//! the touch: an odd lot resting at the best bid moves the best bid and
//! hides a walk behind it, where what the probe fills for hardly moves.
//!
//! The script works in pairs of slots, as a session's steps are. On the
//! first slot of a pair it reads what trades did since the last pair, and
//! moves the makers; on the second it reads the makers' quotes as that move
//! left them, which is what the next pair's trades are measured against.
//! Phoenix's makers do not quote the same distance from their centre after
//! every move (one that has bought its fill stops bidding), so the quotes
//! are read again after each. A session's trades all land between pairs: the
//! takers, the actors and the bot trade between a step's blocks and the
//! next step's. A trade that landed inside a pair, after a move, would be
//! read as the makers' own quotes and leave no impact, which is why a
//! scenario with the transaction layer (it lands the bot's sends on any
//! slot) takes no impact yet.
//!
//! Only the book moves: the oracles stay on the scenario's index, as they do
//! under a premium, and the session values the account at the scenario's
//! own book level ([`crate::scenario::Scenario::valuation`]), not at the
//! price trades pushed the book to. The impact is state the script writes,
//! not orders: the fills a chart counts as volume are still the bot's and
//! the takers', which are transactions.
//!
//! The script publishes the impact in an account of the grader's choosing,
//! and the session reads it at every step ([`ImpactReading`]).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solana_pubkey::Pubkey;

use crate::chain::Override;

fn half() -> f64 {
    0.5
}

fn default_max_bps() -> f64 {
    150.0
}

fn default_probe() -> f64 {
    10.0
}

/// The most the makers' centre moves in one slot, as a share of where it
/// is: under the 4% steps a planned move is cut into, so Phoenix takes the
/// oracle reports that go with it. What is left lands on the next slots.
pub const MAX_STEP: f64 = 0.038;
/// The largest `max_bps`: with a waypoint's premium (3% at most), the book
/// stays inside the 5% Phoenix lets it sit from the oracles.
pub const MAX_IMPACT_BPS: f64 = 190.0;
/// The most zones a scenario's `resistance` holds. A session has a handful
/// of levels, and the script walks every zone on every slot.
pub const RESISTANCE_ZONES_MAX: usize = 8;
/// The largest `strength`. Past it a zone is a wall, and `max_bps` is one.
pub const RESISTANCE_STRENGTH_MAX: f64 = 50.0;
/// Under this many ticks, what is left of the transient part is dropped:
/// it would otherwise decay forever without moving the book a tick.
const DUST_TICKS: f64 = 0.5;

/// A scenario's impact model (see the module).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Impact {
    /// The share of the distance trades walked that stays for good, from 0
    /// to 1. The rest fades.
    #[serde(default = "half")]
    pub permanent_share: f64,
    /// Slots the fading part takes to halve.
    pub half_life_slots: f64,
    /// The most the impact moves the book either way, in basis points of
    /// the book's level.
    #[serde(default = "default_max_bps")]
    pub max_bps: f64,
    /// The size, in base units, the script prices the book at: where the
    /// book is, each side, is what a market order of this size would fill
    /// for on average. Larger than the odd lots that rest at the touch, and
    /// small beside the makers' depth.
    #[serde(default = "default_probe")]
    pub probe: f64,
    /// Stretches of the book's distance from its level where it is harder
    /// to move ([`Zone`]). None when absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resistance: Vec<Zone>,
}

/// A stretch where the book resists: while it sits between `from_bps` and
/// `to_bps` from the path's level (negative below it), a trade moves it
/// `1 / strength` as far as the trade walked, and the makers put the rest of
/// the depth back. Basis points of the session's starting mark.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Zone {
    pub from_bps: f64,
    pub to_bps: f64,
    /// Above 1: how many ticks of walking move the book one tick.
    pub strength: f64,
}

impl Impact {
    pub fn validate(&self) -> Result<(), String> {
        if !(0.0..=1.0).contains(&self.permanent_share) {
            return Err(format!(
                "impact: permanent_share is from 0 to 1, not {}",
                self.permanent_share
            ));
        }
        if !self.half_life_slots.is_finite() || self.half_life_slots < 1.0 {
            return Err(format!(
                "impact: half_life_slots is 1 or more, not {}",
                self.half_life_slots
            ));
        }
        if !(1.0..=MAX_IMPACT_BPS).contains(&self.max_bps) {
            return Err(format!(
                "impact: max_bps is between 1 and {MAX_IMPACT_BPS}, not {}",
                self.max_bps
            ));
        }
        if !self.probe.is_finite() || self.probe <= 0.0 {
            return Err(format!(
                "impact: probe is a size above 0, in base units, not {}",
                self.probe
            ));
        }
        self.validate_resistance()
    }

    fn validate_resistance(&self) -> Result<(), String> {
        if self.resistance.len() > RESISTANCE_ZONES_MAX {
            return Err(format!(
                "impact: resistance holds {RESISTANCE_ZONES_MAX} zones at most, not {}",
                self.resistance.len()
            ));
        }
        for zone in &self.resistance {
            let Zone {
                from_bps,
                to_bps,
                strength,
            } = *zone;
            if !(from_bps.is_finite() && to_bps.is_finite() && strength.is_finite()) {
                return Err(format!(
                    "impact: resistance: from_bps, to_bps and strength are numbers, not {from_bps}, {to_bps} and {strength}"
                ));
            }
            if from_bps >= to_bps {
                return Err(format!(
                    "impact: resistance: from_bps is under to_bps, not {from_bps} and {to_bps}"
                ));
            }
            if from_bps < -self.max_bps || to_bps > self.max_bps {
                return Err(format!(
                    "impact: resistance: the zone from {from_bps} to {to_bps} reaches past max_bps ({}), where the book never is",
                    self.max_bps
                ));
            }
            if strength <= 1.0 || strength > RESISTANCE_STRENGTH_MAX {
                return Err(format!(
                    "impact: resistance: strength is above 1 and {RESISTANCE_STRENGTH_MAX} at most, not {strength}"
                ));
            }
        }
        let mut zones = self.resistance.clone();
        zones.sort_by(|a, b| a.from_bps.total_cmp(&b.from_bps));
        for pair in zones.windows(2) {
            if pair[1].from_bps < pair[0].to_bps {
                return Err(format!(
                    "impact: resistance: the zones from {} and from {} overlap",
                    pair[0].from_bps, pair[1].from_bps
                ));
            }
        }
        Ok(())
    }

    /// The scenario's zones in ticks, for a session that starts at
    /// `start_mark_ticks`. The zones are valid ([`Self::validate`]).
    pub fn curve(&self, start_mark_ticks: u64) -> Curve {
        let ticks = |bps: f64| start_mark_ticks as f64 * bps / 10_000.0;
        let (mut above, mut below) = (Vec::new(), Vec::new());
        for zone in &self.resistance {
            // A zone that holds the level is two, one each side.
            if zone.to_bps > 0.0 {
                above.push(Stretch {
                    from: ticks(zone.from_bps.max(0.0)),
                    to: ticks(zone.to_bps),
                    strength: zone.strength,
                });
            }
            if zone.from_bps < 0.0 {
                below.push(Stretch {
                    from: ticks((-zone.to_bps).max(0.0)),
                    to: ticks(-zone.from_bps),
                    strength: zone.strength,
                });
            }
        }
        for side in [&mut above, &mut below] {
            side.sort_by(|a, b| a.from.total_cmp(&b.from));
            assert!(
                side.iter().all(|s| 0.0 <= s.from && s.from < s.to),
                "a zone that is no stretch: {side:?}"
            );
            assert!(
                side.windows(2).all(|pair| pair[0].to <= pair[1].from),
                "zones that overlap: {side:?}"
            );
        }
        Curve { above, below }
    }

    /// What the fading part keeps of itself each slot.
    fn decay(&self) -> f64 {
        0.5f64.powf(1.0 / self.half_life_slots)
    }
}

/// One side's zone, in ticks from the level: `0 <= from < to`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stretch {
    from: f64,
    to: f64,
    strength: f64,
}

/// How far the book sits from its level for what trades walked: tick for
/// tick outside the zones, `1 / strength` of a tick inside one.
///
/// The curve is one function of the distance walked, the same at every slot
/// and whichever way the book is going. That is what keeps a round trip from
/// paying. Write `x` for the walk that stands (the permanent part and what
/// is left of the fading one) and `f(x)` for the book's distance: for any
/// `f` that never falls, buying and selling back the same size costs at
/// least nothing, however it is cut up and whatever waits are put between.
/// A zone that resisted a push one way only, gave way once crossed, or sat
/// at a price the path could carry away from the book, is an `f` that
/// changes under the trader: buy where the book does not move, sell where
/// it does, and the difference is profit no market paid for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Curve {
    /// Nearest the level first, none overlapping.
    above: Vec<Stretch>,
    below: Vec<Stretch>,
}

/// The book's distance on one side, for `walked >= 0` ticks on that side.
fn squeeze(walked: f64, zones: &[Stretch]) -> f64 {
    assert!(walked >= 0.0, "a side's walk is its size: {walked}");
    let (mut left, mut at) = (walked, 0.0);
    for zone in zones {
        let free = zone.from - at;
        if left <= free {
            return at + left;
        }
        left -= free;
        let held = (zone.to - zone.from) * zone.strength;
        if left <= held {
            return zone.from + left / zone.strength;
        }
        left -= held;
        at = zone.to;
    }
    at + left
}

/// The walk that puts the book `distance >= 0` ticks out on one side: what
/// [`squeeze`] undoes.
fn stretch(distance: f64, zones: &[Stretch]) -> f64 {
    assert!(distance >= 0.0, "a side's distance is its size: {distance}");
    let mut walked = distance;
    for zone in zones {
        let inside = distance.min(zone.to) - zone.from;
        if inside <= 0.0 {
            break;
        }
        walked += inside * (zone.strength - 1.0);
    }
    walked
}

impl Curve {
    /// How far the book sits from its level, in ticks, once `walked` ticks
    /// stand (negative below the level).
    pub fn offset(&self, walked: f64) -> f64 {
        if walked >= 0.0 {
            squeeze(walked, &self.above)
        } else {
            -squeeze(-walked, &self.below)
        }
    }

    fn lua(zones: &[Stretch]) -> String {
        zones
            .iter()
            .map(|zone| {
                format!(
                    "{{{},{},{}}}",
                    lua_number(zone.from),
                    lua_number(zone.to),
                    lua_number(zone.strength)
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// The impact as it stands: the model the script runs, in Rust, for tests
/// and for anyone reasoning about a session. The two parts are ticks trades
/// walked; where that puts the book is the [`Curve`]'s to say.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ImpactState {
    pub permanent_ticks: f64,
    pub transient_ticks: f64,
}

impl ImpactState {
    /// One slot at a book level of `book` ticks: the fading part decays,
    /// then `walked` ticks (positive for asks walked up, negative for bids
    /// walked down) add to both parts, and the two stay inside the limit.
    /// Returns where the makers want to be centred.
    pub fn step(&mut self, impact: &Impact, curve: &Curve, book: u64, walked: i64) -> u64 {
        self.transient_ticks *= impact.decay();
        if self.transient_ticks.abs() < DUST_TICKS {
            self.transient_ticks = 0.0;
        }
        let walked = walked as f64;
        self.permanent_ticks += impact.permanent_share * walked;
        self.transient_ticks += (1.0 - impact.permanent_share) * walked;
        // Past the limit, the excess is dropped, the fading part first:
        // impact that never reached the book has nothing to take back. The
        // limit is on the book: the walk that reaches it is longer by what
        // the zones on the way hold.
        let limit = book as f64 * impact.max_bps / 10_000.0;
        let (up, down) = (stretch(limit, &curve.above), stretch(limit, &curve.below));
        let total = self.total_ticks();
        let excess = total - total.clamp(-down, up);
        if excess != 0.0 {
            let from_transient = if excess > 0.0 {
                excess.min(self.transient_ticks.max(0.0))
            } else {
                excess.max(self.transient_ticks.min(0.0))
            };
            self.transient_ticks -= from_transient;
            self.permanent_ticks -= excess - from_transient;
        }
        let offset = curve.offset(self.total_ticks());
        assert!(offset.abs() <= limit + 1e-6, "the book is past its limit");
        (book as f64 + offset + 0.5).floor() as u64
    }

    pub fn total_ticks(&self) -> f64 {
        self.permanent_ticks + self.transient_ticks
    }
}

/// What the script published last, in basis points of the book's level:
/// the account's data is three little-endian doubles.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ImpactReading {
    /// Where the book settles once the fading part is gone.
    pub permanent_bps: f64,
    /// What is still to fade.
    pub transient_bps: f64,
    /// How far the makers are centred from the book's level. The two parts'
    /// sum, once a large move has landed (see [`MAX_STEP`]).
    pub total_bps: f64,
}

impl ImpactReading {
    pub fn from_account_data(data: &[u8]) -> Option<Self> {
        let double = |at: usize| {
            data.get(at..at + 8)
                .and_then(|bytes| bytes.try_into().ok())
                .map(f64::from_le_bytes)
                .filter(|v| v.is_finite())
        };
        Some(Self {
            permanent_bps: double(0)?,
            transient_bps: double(8)?,
            total_bps: double(16)?,
        })
    }
}

const MARKET_MOVE: &str = "phoenix-market-move";
const ORACLE_PRICE: &str = "phoenix-oracle-price";
const UNCROSS: &str = "phoenix-market-uncross";

/// The path's moves as the script replays them: `(slot, kind, ticks)`, where
/// kind 1 moves the book and the oracles and kind 2 the oracles alone. The
/// uncross after a move is the script's own: it uncrosses after every move
/// it makes.
fn plan(overrides: &[Override]) -> Vec<(u64, u8, u64)> {
    let mut steps: Vec<(u64, u8, u64)> = overrides
        .iter()
        .filter_map(|o| {
            let kind = match o.template_id {
                MARKET_MOVE => 1,
                ORACLE_PRICE => 2,
                _ => return None,
            };
            let ticks = o.values["target_ticks"].as_str()?.parse().ok()?;
            Some((o.slot, kind, ticks))
        })
        .collect();
    // Stable: within a slot, the order the path gave them.
    steps.sort_by_key(|(slot, ..)| *slot);
    steps
}

/// What a session gives the script.
pub struct ScriptSetup<'a> {
    pub symbol: &'a str,
    /// A trader account for the script's quotes: Phoenix's quote reads one,
    /// and trades nothing in it.
    pub trader: Pubkey,
    /// The probe: the size, in base lots, whose fill price is where the
    /// book is ([`Impact::probe`]).
    pub probe_lots: u64,
    pub start_mark_ticks: u64,
    /// The scenario's path, as the overrides a session without impact
    /// would have registered.
    pub overrides: &'a [Override],
    /// Where the script publishes the impact.
    pub state: Pubkey,
}

/// A Lua number that reads back as the same double.
fn lua_number(value: f64) -> String {
    format!("{value:?}")
}

impl Impact {
    /// The script's Lua source.
    pub fn source(&self, setup: &ScriptSetup) -> String {
        let curve = self.curve(setup.start_mark_ticks);
        let steps = plan(setup.overrides)
            .iter()
            .map(|(slot, kind, ticks)| format!("{{{slot},{kind},{ticks}}}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"-- mtx: the scenario's path, and the impact trades leave on the book.
local SYMBOL, STATE, TRADER = "{symbol}", "{state}", "{trader}"
local PROBE_LOTS = {probe_lots}
local SHARE, DECAY, MAX_BPS = {share}, {decay}, {max_bps}
local MAX_STEP, DUST = {max_step}, {dust}
-- Where the book is harder to move, each side of its level, nearest first:
-- {{from, to, strength}}, in ticks from the level.
local ABOVE, BELOW = {{{above}}}, {{{below}}}
-- The path: {{slot, kind, ticks}}; kind 1 moves the book and the oracles, 2 the oracles.
local PLAN = {{{steps}}}
local cursor, book, index = 1, {start}, {start}
-- Where the makers are centred, and how far their quotes sat from it when
-- last read untouched, right after a move.
local centre, base_bid, base_ask = nil, nil, nil
-- Whether the makers moved since their quotes were read.
local fresh = false
-- What of the touch's distance is already counted, since the last move.
local counted_up, counted_down = 0, 0
local permanent, transient = 0.0, 0.0
local calls, published = 0, nil

-- Where the book is, each side, in ticks: what a market order of the probe
-- size would fill for on average, not the best bid and ask. A sliver of
-- liquidity at the touch moves the touch and hardly moves this. A book too
-- thin to fill the probe is not read. A global, so a test can replace it.
function read_touch(ctx)
  local sell = ctx.phoenix.quote(SYMBOL, "sell", PROBE_LOTS, TRADER)
  local buy = ctx.phoenix.quote(SYMBOL, "buy", PROBE_LOTS, TRADER)
  if not (sell.filled and buy.filled) then error("the book can't fill the probe") end
  return sell.avg_ticks, buy.avg_ticks
end

-- How far the book sits on one side of its level once `walked` ticks stand
-- there: tick for tick outside the zones, a zone's share of a tick inside.
local function squeeze(walked, zones)
  local at = 0
  for i = 1, #zones do
    local zone = zones[i]
    local free = zone[1] - at
    if walked <= free then return at + walked end
    walked = walked - free
    local held = (zone[2] - zone[1]) * zone[3]
    if walked <= held then return zone[1] + walked / zone[3] end
    walked, at = walked - held, zone[2]
  end
  return at + walked
end

-- The walk that puts the book `distance` ticks out on one side.
local function stretch(distance, zones)
  local walked = distance
  for i = 1, #zones do
    local zone = zones[i]
    local inside = math.min(distance, zone[2]) - zone[1]
    if inside <= 0 then break end
    walked = walked + inside * (zone[3] - 1)
  end
  return walked
end

local function offset(walked)
  if walked >= 0 then return squeeze(walked, ABOVE) end
  return -squeeze(-walked, BELOW)
end

local function publish(ctx)
  local bps = 10000 / book
  -- Where the book settles once the fading part is gone, and what is to fade.
  local settled = offset(permanent)
  local fading = offset(permanent + transient) - settled
  local state = string.pack("<d<d<d", settled * bps, fading * bps, (centre - book) * bps)
  if state ~= published then
    published = state
    ctx.set_account(STATE, {{ lamports = 1000000, data = state }})
  end
end

function on_slot(ctx)
  calls = calls + 1
  transient = transient * DECAY
  if math.abs(transient) < DUST then transient = 0.0 end

  -- The second slot of a pair: the makers' quotes, as the move left them.
  if calls % 2 == 0 then
    if fresh then
      local ok, bid, ask = pcall(read_touch, ctx)
      if ok and bid > 0 and ask > bid then
        base_bid, base_ask, fresh = centre - bid, ask - centre, false
      end
    end
    publish(ctx)
    return
  end

  -- The first slot of a pair: the path's moves for the pair, both slots'.
  local moved = centre == nil
  while cursor <= #PLAN and PLAN[cursor][1] <= ctx.elapsed + 1 do
    local step = PLAN[cursor]
    book = step[2] == 1 and step[3] or book
    index = step[3]
    moved = true
    cursor = cursor + 1
  end

  -- How far trades walked the book since the last pair: a touch further
  -- from the centre than the makers' own quotes is depth taken.
  local walked = 0
  -- The limit is on the book: the walk that reaches it is longer by what
  -- the zones on the way hold.
  local limit = book * MAX_BPS / 10000
  local reach_up, reach_down = stretch(limit, ABOVE), stretch(limit, BELOW)
  if centre and base_bid and not fresh then
    local ok, bid, ask = pcall(read_touch, ctx)
    if ok and bid > 0 and ask > bid then
      local up = math.max(0, (ask - centre) - base_ask)
      local down = math.max(0, (centre - bid) - base_bid)
      -- A touch further than any impact could put it is a book this
      -- script lost track of, not trades: centre it again and go on.
      local reach = reach_up + reach_down + 20
      if up > reach or down > reach then
        moved = true
      else
        walked = (up - counted_up) - (down - counted_down)
        counted_up, counted_down = up, down
      end
    end
  end

  local before = offset(permanent + transient)
  permanent = permanent + SHARE * walked
  transient = transient + (1 - SHARE) * walked
  -- A zone kept the book from following the walk: the makers are centred
  -- again, on the tick they were on if need be, and put back the depth the
  -- book did not keep.
  if math.abs(walked - (offset(permanent + transient) - before)) >= 1 then moved = true end
  -- Past the limit the excess is dropped, the fading part first.
  local total = permanent + transient
  local excess = total - math.max(-reach_down, math.min(reach_up, total))
  if excess ~= 0 then
    local from_transient
    if excess > 0 then
      from_transient = math.min(excess, math.max(transient, 0))
    else
      from_transient = math.max(excess, math.min(transient, 0))
    end
    transient = transient - from_transient
    permanent = permanent - (excess - from_transient)
  end

  local to = math.floor(book + offset(permanent + transient) + 0.5)
  -- A slot moves the makers by no more than Phoenix takes at once.
  if centre then
    local step = math.max(1, math.floor(centre * MAX_STEP))
    to = math.max(centre - step, math.min(centre + step, to))
  end
  if moved or to ~= centre then
    centre, counted_up, counted_down, fresh = to, 0, 0, true
    ctx.override {{
      template = "{market_move}",
      values = {{ symbol = SYMBOL, target_ticks = tostring(to) }},
      label = "book",
    }}
    -- A move takes the oracles with the book: they go back to the index.
    if index ~= to then
      ctx.override {{
        template = "{oracle_price}",
        values = {{ symbol = SYMBOL, target_ticks = tostring(index) }},
        label = "index",
      }}
    end
    ctx.override {{ template = "{uncross}", values = {{ symbol = SYMBOL }}, label = "uncross" }}
  end
  publish(ctx)
end
"#,
            symbol = setup.symbol,
            state = setup.state,
            trader = setup.trader,
            probe_lots = setup.probe_lots,
            share = lua_number(self.permanent_share),
            decay = lua_number(self.decay()),
            max_bps = lua_number(self.max_bps),
            max_step = lua_number(MAX_STEP),
            dust = lua_number(DUST_TICKS),
            above = Curve::lua(&curve.above),
            below = Curve::lua(&curve.below),
            start = setup.start_mark_ticks,
            market_move = MARKET_MOVE,
            oracle_price = ORACLE_PRICE,
            uncross = UNCROSS,
        )
    }

    /// The script as `surfnet_registerScenario` takes it.
    pub fn script(&self, setup: &ScriptSetup) -> Value {
        json!({
            "id": "mtx-impact",
            "name": "impact",
            "source": self.source(setup),
        })
    }
}

#[cfg(test)]
mod tests {
    use surfpool_core::{
        scenarios::script::{Action, ScenarioScripts},
        surfnet::svm::SurfnetSvm,
    };
    use surfpool_types::ScenarioScript;

    use super::*;
    use crate::phoenix::{market_move, oracle_move};

    const IMPACT: Impact = Impact {
        permanent_share: 0.5,
        half_life_slots: 8.0,
        max_bps: 150.0,
        probe: 10.0,
        resistance: Vec::new(),
    };
    /// No zones: the book follows the walk tick for tick.
    const FLAT: Curve = Curve {
        above: Vec::new(),
        below: Vec::new(),
    };
    const START: u64 = 143_050;
    /// The lab's makers: their untouched quotes, in ticks from their centre.
    const HALF_BID: u64 = 7;
    const HALF_ASK: u64 = 9;

    /// The script on a bare surfnet, with a book the lab keeps: the touch
    /// is two accounts' lamports, which the script reads in place of
    /// Phoenix's view, and a move the script queues centres it again, as
    /// Phoenix's makers do.
    struct Lab {
        scripts: ScenarioScripts,
        svm: SurfnetSvm,
        touch: [Pubkey; 2],
        state: Pubkey,
        slot: u64,
        /// The makers' centre and the oracles, as the script's moves left them.
        centre: u64,
        index: u64,
        bid: u64,
        ask: u64,
        /// How far the makers quote from their centre, each side.
        half_bid: u64,
        half_ask: u64,
        /// Makers that took a trade's other side quote that side further
        /// out afterwards, by this share of what was walked: Phoenix's do,
        /// once they hold their fill.
        withdraw: f64,
        /// Every move the script made: (slot, ticks).
        moves: Vec<(u64, u64)>,
    }

    impl Lab {
        fn new(impact: &Impact, overrides: &[Override]) -> Self {
            let touch = [Pubkey::new_unique(), Pubkey::new_unique()];
            let state = Pubkey::new_unique();
            let setup = ScriptSetup {
                symbol: "SPCX",
                trader: Pubkey::new_unique(),
                probe_lots: 1_000,
                start_mark_ticks: START,
                overrides,
                state,
            };
            let source = format!(
                "{}\nfunction read_touch(ctx)\n  return ctx.account(\"{}\").lamports, ctx.account(\"{}\").lamports\nend\n",
                impact.source(&setup),
                touch[0],
                touch[1]
            );
            let mut scripts = ScenarioScripts::default();
            scripts
                .register(
                    ScenarioScript {
                        id: "impact".into(),
                        name: "impact".into(),
                        source,
                        seed: None,
                        instruction_limit: None,
                        active_from: None,
                        active_until: None,
                        every: None,
                    },
                    0,
                )
                .expect("the script loads");
            let mut lab = Self {
                scripts,
                svm: SurfnetSvm::default().0,
                touch,
                state,
                slot: 0,
                // Before the script's first move the book is wherever setup
                // left it: off the start, and already walked.
                centre: START - 40,
                index: START - 40,
                bid: START - 40 - HALF_BID,
                ask: START - 40 + HALF_ASK + 11,
                half_bid: HALF_BID,
                half_ask: HALF_ASK,
                withdraw: 0.0,
                moves: Vec::new(),
            };
            lab.write_touch();
            lab
        }

        fn write_touch(&mut self) {
            for (pubkey, lamports) in [(self.touch[0], self.bid), (self.touch[1], self.ask)] {
                self.svm
                    .set_account(
                        &pubkey,
                        solana_account::Account {
                            lamports,
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
        }

        /// A taker buys through `ticks` of asks (or sells through that many
        /// bids, when negative).
        fn take(&mut self, ticks: i64) {
            let withdrawn = (ticks.unsigned_abs() as f64 * self.withdraw).round() as u64;
            if ticks > 0 {
                self.ask += ticks as u64;
                self.half_ask += withdrawn;
            } else {
                self.bid -= ticks.unsigned_abs();
                self.half_bid += withdrawn;
            }
            self.write_touch();
        }

        /// One slot: the script runs, and what it queued lands.
        fn step(&mut self) -> Vec<(String, Option<u64>)> {
            self.slot += 1;
            let run = self.scripts.run_at(0, &self.svm, self.slot).expect("due");
            assert_eq!(run.error, None);
            let mut queued = Vec::new();
            for action in run.actions {
                match action {
                    Action::SetAccount(pubkey, account) => {
                        self.svm.set_account(&pubkey, account).unwrap()
                    }
                    Action::Override(o) => {
                        let ticks = o
                            .values
                            .get("target_ticks")
                            .and_then(|v| v.as_str())
                            .and_then(|v| v.parse().ok());
                        match o.template_id.as_str() {
                            MARKET_MOVE => {
                                let to: u64 = ticks.unwrap();
                                assert!(
                                    to.abs_diff(self.centre) as f64 <= self.centre as f64 * 0.04,
                                    "a move Phoenix would refuse: {} to {to}",
                                    self.centre
                                );
                                (self.centre, self.index) = (to, to);
                                (self.bid, self.ask) = (to - self.half_bid, to + self.half_ask);
                                self.moves.push((self.slot, to));
                            }
                            ORACLE_PRICE => self.index = ticks.unwrap(),
                            _ => {}
                        }
                        queued.push((o.template_id.clone(), ticks));
                    }
                }
            }
            self.write_touch();
            queued
        }

        /// A session's step: two slots.
        fn pair(&mut self) {
            self.step();
            self.step();
        }

        fn reading(&self) -> ImpactReading {
            let account = self.svm.get_account(&self.state).unwrap().unwrap();
            ImpactReading::from_account_data(&account.data).unwrap()
        }
    }

    fn path(waypoints: &[(u64, f64)]) -> Vec<Override> {
        let map = Pubkey::new_unique();
        let mut from = START;
        let mut overrides = Vec::new();
        for (slot, pct) in waypoints {
            let to = (START as f64 * (1.0 + pct / 100.0)).round() as u64;
            overrides.extend(market_move(map, "SPCX", from, to, *slot).0);
            from = to;
        }
        overrides
    }

    #[test]
    fn with_no_trades_the_path_plays_in_the_steps_the_schedule_would() {
        // A 3% fall, then back in three of Phoenix's steps (a 9% rise).
        let overrides = path(&[(4, -3.0), (10, 6.0)]);
        let scheduled = plan(&overrides);
        assert_eq!(
            scheduled.iter().map(|(slot, ..)| *slot).collect::<Vec<_>>(),
            [4, 10, 12, 14],
            "one step down, three up"
        );
        let mut lab = Lab::new(&IMPACT, &overrides);
        for _ in 0..20 {
            for (template, _) in lab.step() {
                // No impact and no premium: the oracles are where each
                // move left them.
                assert_ne!(template, ORACLE_PRICE);
            }
        }
        // The first call centres the makers on the start, whatever setup
        // left. Then the path: each move on the first slot of the session
        // step the schedule lands it in, so a step ends on the same book.
        let mut expected = vec![(1, START)];
        expected.extend(scheduled.iter().map(|(slot, _, ticks)| (slot - 1, *ticks)));
        assert_eq!(lab.moves, expected);
        assert_eq!(
            lab.reading(),
            ImpactReading {
                permanent_bps: 0.0,
                transient_bps: 0.0,
                total_bps: 0.0
            }
        );
    }

    #[test]
    fn the_book_stays_where_a_trade_left_it_and_part_of_it_fades() {
        let mut lab = Lab::new(&IMPACT, &[]);
        let mut model = ImpactState::default();
        lab.pair();
        model.step(&IMPACT, &FLAT, START, 0);
        model.step(&IMPACT, &FLAT, START, 0);
        // A buy through 40 ticks of asks.
        lab.take(40);
        let queued = lab.step();
        assert_eq!(model.step(&IMPACT, &FLAT, START, 40), START + 40);
        // The makers are centred 40 up: the ask is where the buy ended, the
        // oracles back on the index.
        assert_eq!(
            queued,
            vec![
                (MARKET_MOVE.to_string(), Some(START + 40)),
                (ORACLE_PRICE.to_string(), Some(START)),
                (UNCROSS.to_string(), None),
            ]
        );
        assert_eq!(lab.ask, START + 40 + HALF_ASK);
        assert_eq!(lab.index, START);

        // Nothing trades: half of it fades, a half-life at a time. The
        // makers move on the first slot of each pair.
        for _ in 0..48 {
            lab.step();
            let to = model.step(&IMPACT, &FLAT, START, 0);
            if lab.slot % 2 == 1 {
                assert_eq!(lab.centre, to, "slot {}", lab.slot);
            }
            if lab.slot == 11 {
                assert_eq!(to, START + 30, "a half-life after the buy");
            }
        }
        assert_eq!(lab.centre, START + 20, "the permanent half");
        let reading = lab.reading();
        let bps = 10_000.0 / START as f64;
        assert!((reading.permanent_bps - 20.0 * bps).abs() < 1e-9);
        assert_eq!(reading.transient_bps, 0.0);
        assert!((reading.total_bps - 20.0 * bps).abs() < 1e-9);
    }

    /// The property the model is built for: a trade moves the book by what
    /// it walked and no more, so trading in and straight back out sells
    /// into bids no better than the asks it bought, whatever the sizes.
    #[test]
    fn a_round_trip_never_sells_above_where_it_bought() {
        for (share, withdraw, clips) in [
            (0.5, 0.0, vec![40i64, -40]),
            (1.0, 0.0, vec![40, -40]),
            (0.0, 0.0, vec![40, -40]),
            (0.5, 0.0, vec![10, 10, 10, 10, -40]),
            (0.5, 0.0, vec![40, -10, -10, -10, -10]),
            (1.0, 0.0, vec![25, 3, -7, -21]),
            (1.0, 0.25, vec![40, -40]),
            (0.5, 0.25, vec![10, 10, 10, 10, -40]),
        ] {
            let impact = Impact {
                permanent_share: share,
                ..IMPACT
            };
            let mut lab = Lab::new(&impact, &[]);
            lab.withdraw = withdraw;
            lab.pair();
            let (mut paid, mut received) = (0u64, 0u64);
            for clip in &clips {
                let ticks = clip.unsigned_abs();
                if *clip > 0 {
                    paid += walk(lab.ask, ticks, true);
                } else {
                    received += walk(lab.bid, ticks, false);
                }
                lab.take(*clip);
                lab.pair();
            }
            assert!(
                received < paid,
                "share {share}, clips {clips:?}: sold for {received}, bought for {paid}"
            );
        }
    }

    /// Phoenix's makers quote further out on the side they were hit on,
    /// and stay there after they are centred again. Those quotes are the
    /// makers', not trades: the impact stops at what was walked.
    #[test]
    fn makers_that_quote_further_out_after_a_trade_are_not_more_trades() {
        let mut lab = Lab::new(&IMPACT, &[]);
        lab.withdraw = 0.2;
        let mut model = ImpactState::default();
        lab.pair();
        model.step(&IMPACT, &FLAT, START, 0);
        model.step(&IMPACT, &FLAT, START, 0);
        lab.take(-300);
        lab.step();
        let mut to = model.step(&IMPACT, &FLAT, START, -300);
        assert_eq!(lab.centre, to);
        assert_eq!(lab.half_bid, HALF_BID + 60, "the bids sit 60 ticks further");
        for _ in 0..80 {
            lab.step();
            to = model.step(&IMPACT, &FLAT, START, 0);
        }
        // Half of the 300 ticks walked, and not a tick more.
        assert_eq!(to, START - 150);
        assert_eq!(lab.centre, to);
    }

    #[test]
    fn what_is_past_the_limit_is_dropped_and_large_moves_land_in_steps() {
        let tight = Impact {
            max_bps: 20.0,
            ..IMPACT
        };
        // 20 bps of the start is 286 ticks.
        let limit = (START as f64 * 20.0 / 10_000.0).floor() as u64;
        let mut lab = Lab::new(&tight, &[]);
        lab.pair();
        lab.take(400);
        lab.pair();
        assert_eq!(lab.centre, START + limit);
        // The excess is gone: nothing more comes of it later.
        lab.take(300);
        lab.pair();
        assert_eq!(lab.centre, START + limit);
        // And selling it back counts from the limit, at once.
        lab.take(-200);
        lab.pair();
        assert_eq!(lab.centre, START + limit - 200);

        // A walk past what Phoenix moves in a slot lands over two pairs.
        let wide = Impact {
            max_bps: 190.0,
            permanent_share: 1.0,
            ..IMPACT
        };
        let overrides = path(&[(4, 3.8)]);
        let mut lab = Lab::new(&wide, &overrides);
        lab.pair();
        // The walk and the planned move reach the script in the same slot.
        lab.take(2_700);
        let to = (START as f64 * 1.038).round() as u64 + 2_700;
        lab.pair();
        assert!(
            lab.centre < to,
            "the move and the walk are more than a step"
        );
        lab.pair();
        assert_eq!(lab.centre, to);
    }

    /// A zone given in ticks from the level, as the session's curve has it.
    fn zone(from_ticks: f64, to_ticks: f64, strength: f64) -> Zone {
        let bps = 10_000.0 / START as f64;
        Zone {
            from_bps: from_ticks * bps,
            to_bps: to_ticks * bps,
            strength,
        }
    }

    fn resisting(zones: &[Zone]) -> Impact {
        let impact = Impact {
            resistance: zones.to_vec(),
            ..IMPACT
        };
        assert_eq!(impact.validate(), Ok(()));
        impact
    }

    /// A seeded sequence: the next number under `below`.
    fn draw(seed: &mut u64, below: u64) -> u64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (*seed >> 33) % below
    }

    #[test]
    fn a_zone_moves_the_book_less_than_the_walk_and_gives_the_depth_back() {
        // From 20 to 40 ticks above the level, four ticks walked move one.
        let impact = resisting(&[zone(20.0, 40.0, 4.0)]);
        let curve = impact.curve(START);
        let mut lab = Lab::new(&impact, &[]);
        let mut model = ImpactState::default();
        lab.pair();
        model.step(&impact, &curve, START, 0);
        model.step(&impact, &curve, START, 0);

        // A buy through 40 ticks of asks: 20 to the zone, and 20 inside it
        // that move the book 5.
        lab.take(40);
        assert_eq!(lab.ask, START + HALF_ASK + 40);
        let queued = lab.step();
        assert_eq!(model.step(&impact, &curve, START, 40), START + 25);
        assert_eq!(queued[0], (MARKET_MOVE.to_string(), Some(START + 25)));
        // The asks the book did not keep are back: 15 ticks under where the
        // buy ended.
        assert_eq!(lab.ask, START + HALF_ASK + 25);
        lab.step();
        model.step(&impact, &curve, START, 0);
        let bps = 10_000.0 / START as f64;
        let reading = lab.reading();
        assert!((reading.permanent_bps - 20.0 * bps).abs() < 1e-9);
        assert!(
            (reading.permanent_bps + reading.transient_bps - reading.total_bps).abs() < 0.5 * bps,
            "the parts are the book's, not the walk's: {reading:?}"
        );

        // The fading half leaves the zone as it goes: the permanent half is
        // the 20 ticks under it.
        for _ in 0..80 {
            lab.step();
            let to = model.step(&impact, &curve, START, 0);
            if lab.slot % 2 == 1 {
                assert_eq!(lab.centre, to, "slot {}", lab.slot);
            }
        }
        assert_eq!(lab.centre, START + 20);
        assert!((lab.reading().total_bps - 20.0 * bps).abs() < 1e-9);

        // Selling it back walks a side with no zone: tick for tick.
        lab.take(-40);
        lab.pair();
        assert_eq!(lab.centre, START - 20);
    }

    /// Inside a strong zone a small trade does not move the book a tick. The
    /// makers are centred where they were all the same: the depth is back.
    #[test]
    fn a_walk_a_zone_holds_whole_still_brings_the_depth_back() {
        let impact = resisting(&[zone(0.0, 30.0, 10.0)]);
        let mut lab = Lab::new(&impact, &[]);
        lab.pair();
        lab.take(3);
        let queued = lab.step();
        assert_eq!(queued[0], (MARKET_MOVE.to_string(), Some(START)));
        assert_eq!(lab.ask, START + HALF_ASK);
        lab.step();
        // And nothing more comes of it: the makers are left alone.
        let moves = lab.moves.len();
        for _ in 0..6 {
            lab.pair();
        }
        assert_eq!(lab.moves.len(), moves);

        // A walk under a tick of difference is left on the book, as a walk
        // the book follows is.
        lab.take(1);
        assert!(lab.step().is_empty());
        assert_eq!(lab.ask, START + HALF_ASK + 1);
    }

    #[test]
    fn the_script_and_the_model_agree_through_zones_on_both_sides() {
        // Zones above and below, and one that holds the level.
        let impact = resisting(&[
            zone(-90.0, -40.0, 6.0),
            zone(-12.0, 15.0, 3.0),
            zone(30.0, 80.0, 10.0),
        ]);
        let curve = impact.curve(START);
        let mut lab = Lab::new(&impact, &[]);
        let mut model = ImpactState::default();
        lab.pair();
        model.step(&impact, &curve, START, 0);
        model.step(&impact, &curve, START, 0);
        let mut seed = 7;
        let (mut low, mut high) = (START, START);
        for pair in 0..400 {
            // A run of buying, a longer one of selling, and buying again: the
            // book crosses every zone, both ways.
            let lean = if (80..240).contains(&pair) { 60 } else { 20 };
            let clip = draw(&mut seed, 81) as i64 - lean;
            lab.take(clip);
            lab.step();
            let to = model.step(&impact, &curve, START, clip);
            assert_eq!(lab.centre, to, "slot {}", lab.slot);
            lab.step();
            model.step(&impact, &curve, START, 0);
            (low, high) = (low.min(to), high.max(to));
        }
        assert!(
            low < START - 90 && high > START + 80,
            "the book went through every zone: {low} to {high}"
        );
    }

    /// A round trip, seeded: a model with zones both sides of the level,
    /// and a position opened in up to four clips and closed in up to four,
    /// long or short.
    fn trip(seed: &mut u64, size_max: u64) -> (Impact, Vec<i64>) {
        let impact = Impact {
            permanent_share: [0.0, 0.5, 1.0][draw(seed, 3) as usize],
            half_life_slots: [1.0, 8.0, 60.0][draw(seed, 3) as usize],
            max_bps: [3.0, 150.0][draw(seed, 2) as usize],
            ..resisting(&[
                zone(-40.0, -25.0, 6.0),
                zone(-12.0, 15.0, 3.0),
                zone(30.0, 42.0, 10.0),
            ])
        };
        assert_eq!(impact.validate(), Ok(()));
        let size = 1 + draw(seed, size_max);
        let side = if draw(seed, 2) == 0 { 1 } else { -1 };
        let mut clips = Vec::new();
        for leg in [side, -side] {
            let mut left = size;
            for _ in 0..draw(seed, 4) {
                let clip = draw(seed, left + 1);
                clips.push(leg * clip as i64);
                left -= clip;
            }
            clips.push(leg * left as i64);
        }
        assert_eq!(clips.iter().sum::<i64>(), 0);
        (impact, clips)
    }

    /// What `ticks` levels cost a taker, a unit a level, from `touch` on:
    /// up the asks, or down the bids.
    fn walk(touch: u64, ticks: u64, up: bool) -> u64 {
        let spread = ticks * ticks.saturating_sub(1) / 2;
        if up {
            ticks * touch + spread
        } else {
            ticks * touch - spread
        }
    }

    /// The round trip's property holds through zones: the curve is the same
    /// whichever way the book goes, so no cutting of a trade and no wait
    /// sells above where it bought. The model, over many trips: sizes from
    /// inside one zone to past the limit.
    #[test]
    fn round_trips_through_zones_never_sell_above_where_they_bought() {
        let mut seed = 11;
        for number in 0..20_000 {
            let (impact, clips) = trip(&mut seed, 400);
            let curve = impact.curve(START);
            let mut model = ImpactState::default();
            let mut centre = START;
            let (mut paid, mut received) = (0u64, 0u64);
            for clip in &clips {
                let ticks = clip.unsigned_abs();
                if *clip > 0 {
                    paid += walk(centre + HALF_ASK, ticks, true);
                } else {
                    received += walk(centre - HALF_BID, ticks, false);
                }
                centre = model.step(&impact, &curve, START, *clip);
                for _ in 0..draw(&mut seed, 40) {
                    centre = model.step(&impact, &curve, START, 0);
                }
            }
            assert!(
                received < paid,
                "trip {number}, {impact:?}, clips {clips:?}: sold for {received}, bought for {paid}"
            );
        }
    }

    /// And the script, on a book: what a zone leaves walked and what it
    /// puts back are the book's, not the model's.
    #[test]
    fn the_script_s_round_trips_through_zones_never_sell_above_where_they_bought() {
        let mut seed = 13;
        for number in 0..40 {
            let (impact, clips) = trip(&mut seed, 150);
            let mut lab = Lab::new(&impact, &[]);
            lab.pair();
            let (mut paid, mut received) = (0u64, 0u64);
            for clip in &clips {
                let ticks = clip.unsigned_abs();
                if *clip > 0 {
                    paid += walk(lab.ask, ticks, true);
                } else {
                    received += walk(lab.bid, ticks, false);
                }
                lab.take(*clip);
                for _ in 0..=draw(&mut seed, 12) {
                    lab.pair();
                }
            }
            assert!(
                received < paid,
                "trip {number}, {impact:?}, clips {clips:?}: sold for {received}, bought for {paid}"
            );
        }
    }

    #[test]
    fn the_limit_is_on_the_book_whatever_the_zones_hold() {
        // 20 bps of the start is 286 ticks, and a zone on the way holds 200
        // ticks of walking in 100 of the book. Nothing fades, so the walk
        // that stands is the walk.
        let tight = Impact {
            max_bps: 20.0,
            permanent_share: 1.0,
            ..resisting(&[zone(100.0, 200.0, 3.0)])
        };
        let limit = START as f64 * 20.0 / 10_000.0;
        let curve = tight.curve(START);
        assert!((stretch(limit, &curve.above) - (limit + 200.0)).abs() < 1e-9);
        let mut lab = Lab::new(&tight, &[]);
        lab.pair();
        // 400 ticks walked reach the zone's far side, short of the limit.
        lab.take(400);
        lab.pair();
        assert_eq!(lab.centre, START + 200);
        // 300 more would pass it: the book stops on the limit.
        lab.take(300);
        lab.pair();
        assert_eq!(lab.centre, START + 286);
        // Selling counts from the limit, at once: 86 ticks down to the zone,
        // then three walked for each one moved.
        lab.take(-200);
        lab.pair();
        assert_eq!(lab.centre, START + 162);
    }

    #[test]
    fn a_curve_never_falls_and_never_moves_the_book_more_than_the_walk() {
        assert_eq!(FLAT.offset(37.5), 37.5);
        assert_eq!(FLAT.offset(-37.5), -37.5);
        let impact = resisting(&[
            zone(-90.0, -40.0, 6.0),
            zone(-12.0, 15.0, 3.0),
            zone(15.0, 20.0, 50.0),
            zone(30.0, 80.0, 10.0),
        ]);
        let curve = impact.curve(START);
        // The zone that holds the level is one each side, nearest first.
        assert_eq!(curve.above.len(), 3);
        assert_eq!(curve.below.len(), 2);
        assert!((curve.below[0].to - 12.0).abs() < 1e-9);
        assert_eq!(curve.offset(0.0), 0.0);
        let mut last = curve.offset(-400.0);
        for step in -1_599..=1_600 {
            let walked = step as f64 / 4.0;
            let offset = curve.offset(walked);
            assert!(
                (0.0..=0.25 + 1e-9).contains(&(offset - last)),
                "at {walked}: {last} to {offset}"
            );
            last = offset;
            // The walk that reaches a distance is the one that left it.
            let zones = if walked >= 0.0 {
                &curve.above
            } else {
                &curve.below
            };
            assert!((stretch(offset.abs(), zones) - walked.abs()).abs() < 1e-6);
        }
        // Past the last zone the book follows again, short of the walk by
        // what the zones held: 15 at 1/3, 5 at 1/50, 50 at 1/10.
        let held = 15.0 * 2.0 + 5.0 * 49.0 + 50.0 * 9.0;
        assert!((curve.offset(1_000.0) - (1_000.0 - held)).abs() < 1e-6);
    }

    #[test]
    fn impact_rides_on_a_planned_move_and_a_premium_keeps_the_index() {
        let map = Pubkey::new_unique();
        // A move to +2% whose book holds a 1% premium over the index.
        let book = (START as f64 * 1.03).round() as u64;
        let index = (START as f64 * 1.02).round() as u64;
        let (mut overrides, at) = market_move(map, "SPCX", START, book, 4);
        let uncross = overrides.pop().unwrap();
        overrides.push(oracle_move(map, "SPCX", index, at - 2));
        overrides.push(uncross);

        let mut lab = Lab::new(&IMPACT, &overrides);
        let mut model = ImpactState::default();
        lab.pair();
        model.step(&IMPACT, &FLAT, START, 0);
        model.step(&IMPACT, &FLAT, START, 0);
        lab.take(-60);
        // The move lands on top of what the sale left.
        let queued = lab.step();
        let to = model.step(&IMPACT, &FLAT, book, -60);
        assert_eq!(
            queued,
            vec![
                (MARKET_MOVE.to_string(), Some(to)),
                (ORACLE_PRICE.to_string(), Some(index)),
                (UNCROSS.to_string(), None),
            ]
        );
        assert_eq!(to, book - 60);
        assert_eq!(lab.index, index);
    }

    #[test]
    fn quotes_inside_the_makers_and_a_book_it_cannot_read_are_not_trades() {
        let mut lab = Lab::new(&IMPACT, &[]);
        lab.pair();
        // A resting order inside the makers' quotes is the touch now, and
        // then it is canceled.
        lab.bid += 3;
        lab.ask -= 4;
        lab.write_touch();
        assert!(lab.step().is_empty());
        lab.step();
        lab.bid -= 3;
        lab.ask += 4;
        lab.write_touch();
        assert!(lab.step().is_empty());
        assert_eq!(lab.reading().total_bps, 0.0);

        // On a surfnet without Phoenix the read fails every slot: the path
        // still plays.
        let overrides = path(&[(4, -2.0)]);
        let setup = ScriptSetup {
            symbol: "SPCX",
            trader: Pubkey::new_unique(),
            probe_lots: 1_000,
            start_mark_ticks: START,
            overrides: &overrides,
            state: Pubkey::new_unique(),
        };
        let mut scripts = ScenarioScripts::default();
        let script: ScenarioScript = serde_json::from_value(IMPACT.script(&setup)).unwrap();
        scripts.register(script, 0).unwrap();
        let svm = SurfnetSvm::default().0;
        let mut moved = Vec::new();
        for slot in 1..=6 {
            let run = scripts.run_at(0, &svm, slot).unwrap();
            assert_eq!(run.error, None);
            moved.extend(run.actions.iter().filter_map(|action| match action {
                Action::Override(o) if o.template_id == MARKET_MOVE => {
                    o.values["target_ticks"].as_str().map(str::to_string)
                }
                _ => None,
            }));
        }
        let fallen = (START as f64 * 0.98).round() as u64;
        assert_eq!(moved, vec![START.to_string(), fallen.to_string()]);
    }

    #[test]
    fn a_book_the_script_lost_track_of_is_centred_again() {
        let mut lab = Lab::new(&IMPACT, &[]);
        lab.pair();
        // A move that never landed: the book is 5% from where the script
        // put it, further than any trade's impact.
        lab.ask += START / 20;
        lab.write_touch();
        let queued = lab.step();
        assert_eq!(queued[0], (MARKET_MOVE.to_string(), Some(START)));
        assert_eq!(lab.reading().total_bps, 0.0);
    }

    #[test]
    fn validation_names_what_is_wrong() {
        assert_eq!(IMPACT.validate(), Ok(()));
        for (impact, message) in [
            (
                Impact {
                    permanent_share: 1.5,
                    ..IMPACT
                },
                "permanent_share",
            ),
            (
                Impact {
                    permanent_share: f64::NAN,
                    ..IMPACT
                },
                "permanent_share",
            ),
            (
                Impact {
                    half_life_slots: 0.5,
                    ..IMPACT
                },
                "half_life_slots",
            ),
            (
                Impact {
                    max_bps: 400.0,
                    ..IMPACT
                },
                "max_bps",
            ),
            (
                Impact {
                    probe: 0.0,
                    ..IMPACT
                },
                "probe",
            ),
        ] {
            let error = impact.validate().unwrap_err();
            assert!(error.contains(message), "{error}");
        }
        for (zones, message) in [
            (vec![(30.0, 5.0, 4.0)], "from_bps is under to_bps"),
            (vec![(5.0, 5.0, 4.0)], "from_bps is under to_bps"),
            (vec![(f64::NAN, 5.0, 4.0)], "are numbers"),
            (vec![(5.0, 30.0, 1.0)], "strength"),
            (vec![(5.0, 30.0, 0.5)], "strength"),
            (vec![(5.0, 30.0, 51.0)], "strength"),
            (vec![(5.0, 30.0, f64::INFINITY)], "are numbers"),
            (vec![(5.0, 151.0, 4.0)], "past max_bps"),
            (vec![(-151.0, 5.0, 4.0)], "past max_bps"),
            (vec![(20.0, 40.0, 4.0), (5.0, 21.0, 2.0)], "overlap"),
            (
                vec![(1.0, 2.0, 4.0); RESISTANCE_ZONES_MAX + 1],
                "zones at most",
            ),
        ] {
            let impact = Impact {
                resistance: zones
                    .iter()
                    .map(|&(from_bps, to_bps, strength)| Zone {
                        from_bps,
                        to_bps,
                        strength,
                    })
                    .collect(),
                ..IMPACT
            };
            let error = impact.validate().unwrap_err();
            assert!(error.starts_with("impact: resistance"), "{error}");
            assert!(error.contains(message), "{error}");
        }
        // Zones that touch, in any order, and one that holds the level.
        let zones: Impact = serde_json::from_str(
            r#"{"half_life_slots":8,"resistance":[
                {"from_bps":30,"to_bps":150,"strength":50},
                {"from_bps":-150,"to_bps":30,"strength":1.5}]}"#,
        )
        .unwrap();
        assert_eq!(zones.validate(), Ok(()));
        assert_eq!(zones.resistance.len(), 2);
        assert!(
            serde_json::from_str::<Impact>(
                r#"{"half_life_slots":8,"resistance":[{"from_bps":5,"to_bps":30,"strength":4,"breaks":1}]}"#
            )
            .is_err()
        );
        // The defaults: half of it permanent, a 150 bp cap and no zones.
        let parsed: Impact = serde_json::from_str(r#"{"half_life_slots":8}"#).unwrap();
        assert_eq!(parsed, IMPACT);
        assert!(serde_json::from_str::<Impact>(r#"{"half_life_slots":8,"k":2}"#).is_err());
    }

    #[test]
    fn a_reading_is_three_doubles() {
        let mut data = Vec::new();
        for v in [1.5f64, -2.25, 0.75] {
            data.extend(v.to_le_bytes());
        }
        assert_eq!(
            ImpactReading::from_account_data(&data),
            Some(ImpactReading {
                permanent_bps: 1.5,
                transient_bps: -2.25,
                total_bps: 0.75
            })
        );
        assert_eq!(ImpactReading::from_account_data(&data[..20]), None);
        assert_eq!(ImpactReading::from_account_data(&[]), None);
    }
}
