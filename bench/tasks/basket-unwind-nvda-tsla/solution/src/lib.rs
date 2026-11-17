//! Reference: unwind the desk's NVDA and TSLA longs by the close, spending
//! the shared execution capacity on the name where waiting costs most.
//!
//! The desk starts long both names (`MTX_START_POSITIONS`), may only sell,
//! and a broker takes what is left at the close less a haircut per market
//! (`MTX_CLOSE_OUT`). Its fills may trade at most `max_usd` of notional in
//! any `window_slots` (`MTX_TRADED_CAP`), both markets together.
//!
//! - **Capacity.** It counts what it sold, step by step, at the oracle,
//!   and keeps what it sends and rests under nine tenths of the cap less
//!   what the last window already traded.
//! - **The tape.** Each step it reads every order sent to each market
//!   (`getSignaturesForAddress` on the order book, then `getTransaction`):
//!   who took, which way and how much. A wallet that takes one way four
//!   times or more within 100 slots is a burst.
//! - **A sell burst** puts that name on watch: it sells up to three tenths
//!   of it at the maker's bid, never through the backstop. Once the oracle
//!   has fallen 15 bp from where the watch began and the market's net flow
//!   still sells, the fall is confirmed, and the name takes the capacity
//!   first until it is gone.
//! - **A buy burst** holds the name: it sells nothing there until the rise
//!   has landed (0.3% over its low and no new high for 80 slots). A watch
//!   whose price never moved 15 bp its way within 400 slots was a fake-out.
//! - **Otherwise** it works each name passively: an ask a tick inside the
//!   maker's, and behind a back-loaded floor it crosses, but only while the
//!   bid beats the broker's price after the fee. What costs more than the
//!   broker charges is left to the broker.
//!
//! It doesn't know whose trades are informed, and tells a fake-out from a
//! real burst only by the price and flow that follow.
//!
//! The same machinery plays the ladder's rungs ([`Mode`]): the informed bot
//! (told each market's informed wallet), the per-market reader that splits
//! the capacity pro rata instead of allocating it, and the rungs that read
//! nothing (dump, TWAP each, pro rata, bigger first).

use std::collections::{HashSet, VecDeque};

use mtx_kit::{
    log,
    prelude::*,
    rpc, rpc_account,
    venue::{
        PhoenixVenue,
        phoenix_rise::{
            accounts::perp_asset_map::PerpAssetMap,
            ix::{PhoenixInstruction, order_packet::OrderPacket},
            types::exchange::ExchangeMarketConfig,
        },
        ticks_to_usd,
    },
};
use serde_json::{Value, json};

/// The share of the cap it plans to, and the window it counts over, a
/// little longer than the cap's: fills are seen a step after they land.
const CAP_USE: f64 = 0.9;
const CAP_SLACK_SLOTS: u64 = 4;
/// A wallet's burst: this many takes one way within the window, nearly all
/// of what it traded.
const BURST_TAKES: usize = 4;
const BURST_SLOTS: u64 = 100;
const BURST_ONE_WAY: f64 = 0.8;
/// The fall that confirms a sell burst, and how long a watch lasts without
/// the price moving its way.
const CONFIRM_BPS: f64 = 15.0;
const FLOW_SLOTS: u64 = 120;
const WATCH_SLOTS: u64 = 400;
/// The share of a name it sells on watch, before a fall is confirmed.
const WATCH_SHARE: f64 = 0.3;
/// A rise has landed this far over its low, with no new high for a while.
const LANDED: f64 = 0.003;
const SETTLE_SLOTS: u64 = 80;
/// The taker fee, and what it gives up crossing a calm name.
const TAKER_FEE_BPS: f64 = 3.5;
/// A slice and a resting ask, in USD; what an urgent name sends a step.
const SLICE_USD: f64 = 160.0;
const URGENT_USD: f64 = 500.0;
/// How far an urgent sale may sweep, and a watch sale, in bp under the
/// oracle past the maker's half-spread.
const SWEEP_BPS: f64 = 45.0;
const WATCH_SLACK_BPS: f64 = 6.0;
/// From here on, in share of the session, it sells what beats the broker
/// whatever the reads say.
const LATE: f64 = 0.9;

/// Which rung the machinery plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reference,
    /// Told each market's informed wallet (`MTX_REGIME`).
    Informed,
    /// Sells both names as fast as the cap lets it, pro rata.
    Dump,
    /// Crosses equal slices of each name over the session.
    TwapEach,
    /// Rests asks in both names and crosses behind a straight schedule.
    ProRata,
    /// The bigger notional over the first half, the other over the second.
    BiggerFirst,
}

impl Mode {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "reference" => Self::Reference,
            "informed" => Self::Informed,
            "dump" => Self::Dump,
            "twap-each" => Self::TwapEach,
            "pro-rata" => Self::ProRata,
            "bigger-first" => Self::BiggerFirst,
            _ => return None,
        })
    }

    fn reads(self) -> bool {
        matches!(self, Self::Reference | Self::Informed)
    }
}

/// What it knows of a name's flow: nothing, a sell burst on watch, a fall
/// it is sure of, or a rise it holds for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Read {
    #[default]
    Calm,
    Watch,
    Falling,
    Rising,
}

/// One market of the basket.
struct Leg {
    id: String,
    symbol: String,
    market: ExchangeMarketConfig,
    orderbook: String,
    start: f64,
    /// The oracle at the first step, which weighs the start's notional.
    arrival: f64,
    haircut_bps: f64,
    tick: f64,
    lot: f64,
    /// The told informed wallet, for the informed bot.
    informed: Option<String>,
    /// The maker's half-spread at the first look, in bp.
    base_bps: Option<f64>,
    held: f64,
    read: Read,
    watch_from: u64,
    watch_oracle: f64,
    watch_sold: f64,
    high: f64,
    high_at: u64,
    low: f64,
    /// The share of the session from which a calm floor counts.
    from: f64,
    takes: VecDeque<(u64, String, f64)>,
    seen: HashSet<String>,
}

impl Leg {
    fn sold(&self) -> f64 {
        self.start - self.held
    }

    /// A burst in the window: its sign, and whether the told wallet sent it.
    fn burst(&self, now: u64) -> Option<(f64, bool)> {
        let mut wallets: Vec<&str> = self
            .takes
            .iter()
            .filter(|(at, ..)| at + BURST_SLOTS > now)
            .map(|(_, w, _)| w.as_str())
            .collect();
        wallets.sort_unstable();
        wallets.dedup();
        for wallet in wallets {
            let mine: Vec<f64> = self
                .takes
                .iter()
                .filter(|(at, w, _)| at + BURST_SLOTS > now && w == wallet)
                .map(|(.., s)| *s)
                .collect();
            let net: f64 = mine.iter().sum();
            let traded: f64 = mine.iter().map(|s| s.abs()).sum();
            let same = mine.iter().filter(|s| s.signum() == net.signum()).count();
            if same >= BURST_TAKES && net.abs() >= BURST_ONE_WAY * traded {
                let told = self.informed.as_deref() == Some(wallet);
                return Some((net.signum(), told));
            }
        }
        None
    }

    fn net_flow(&self, now: u64) -> f64 {
        self.takes
            .iter()
            .filter(|(at, ..)| at + FLOW_SLOTS > now)
            .map(|(.., s)| *s)
            .sum()
    }
}

/// What one step sees of a leg.
#[derive(Debug, Clone, Copy, Default)]
struct View {
    oracle: f64,
    bid: Option<f64>,
}

/// The strategy.
pub struct Unwind {
    mode: Mode,
    /// Spend the capacity on the most urgent name first (`true`), or hold
    /// each name to its share of it.
    allocate: bool,
    legs: Vec<Leg>,
    venue: Option<PhoenixVenue>,
    asset_map: String,
    me: String,
    start_slot: u64,
    end_slot: u64,
    cap_usd: f64,
    window: u64,
    /// What it sold, by slot, market and notional.
    traded: VecDeque<(u64, usize, f64)>,
}

impl Default for Unwind {
    fn default() -> Self {
        Self::new(Mode::Reference, true)
    }
}

impl Unwind {
    pub fn new(mode: Mode, allocate: bool) -> Self {
        Self {
            mode,
            allocate,
            legs: Vec::new(),
            venue: None,
            asset_map: String::new(),
            me: String::new(),
            start_slot: 0,
            end_slot: 0,
            cap_usd: 0.0,
            window: 60,
            traded: VecDeque::new(),
        }
    }

    fn progress(&self, slot: u64) -> f64 {
        let span = self.end_slot.saturating_sub(self.start_slot).max(1) as f64;
        (slot.saturating_sub(self.start_slot) as f64 / span).min(1.0)
    }

    /// Each market's oracle, from Phoenix's perp asset map: the median of
    /// its oracles' latest reports.
    fn oracles(&self) -> Vec<Option<f64>> {
        let data = rpc_account(&self.asset_map).ok().flatten().unwrap_or_default();
        let map = PerpAssetMap::try_from_account_bytes(&data).ok();
        self.legs
            .iter()
            .map(|leg| {
                let entry = map.as_ref()?.find_by_symbol(&leg.symbol).ok()??;
                let price = &entry.metadata.oracle_price().mark_price;
                let mut ticks: Vec<u64> = price
                    .spot_price_component
                    .last_exchange_spot_price
                    .iter()
                    .map(|p| p.ticks.as_inner())
                    .filter(|&t| t > 0 && t != u64::MAX)
                    .collect();
                if ticks.is_empty() {
                    return None;
                }
                ticks.sort_unstable();
                Some(ticks_to_usd(&leg.market, ticks[ticks.len() / 2]))
            })
            .collect()
    }

    /// Room under the cap now: for every market together, or for leg `i`'s
    /// share of it.
    fn room(&self, now: u64, leg: Option<usize>) -> f64 {
        let since = now.saturating_sub(self.window + CAP_SLACK_SLOTS);
        let used: f64 = self
            .traded
            .iter()
            .filter(|(at, i, _)| *at > since && leg.is_none_or(|l| l == *i))
            .map(|(.., usd)| usd)
            .sum();
        let share = match leg {
            None => 1.0,
            Some(i) => {
                let total: f64 = self.legs.iter().map(|l| l.start * l.arrival).sum();
                self.legs[i].start * self.legs[i].arrival / total.max(1e-9)
            }
        };
        (CAP_USE * self.cap_usd * share - used).max(0.0)
    }
}

impl Strategy for Unwind {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let starts = session.start_positions()?;
        let close_outs = session.close_outs()?;
        let cap: Value = session
            .var("MTX_TRADED_CAP")
            .and_then(|c| serde_json::from_str(c).ok())
            .ok_or("no MTX_TRADED_CAP")?;
        self.cap_usd = cap["max_usd"].as_f64().ok_or("the cap has no max_usd")?;
        self.window = cap["window_slots"].as_u64().unwrap_or(60);
        let regime: Option<Value> = session
            .var("MTX_REGIME")
            .and_then(|r| serde_json::from_str(r).ok());
        for info in &session.venues {
            if !info.tradable {
                continue;
            }
            let instrument: mtx_kit::mandate::InstrumentRef = info.id.parse()?;
            let market = venue.market(&instrument.symbol)?.clone();
            let kind = format!("informed-{}", instrument.symbol.to_lowercase());
            let informed = regime.as_ref().and_then(|r| {
                r["wallets"][kind.as_str()][0]
                    .as_str()
                    .map(String::from)
            });
            if self.mode == Mode::Informed && informed.is_none() {
                return Err(format!("the informed bot needs MTX_REGIME's {kind} wallet").into());
            }
            let start = starts.get(&instrument);
            self.legs.push(Leg {
                id: info.id.clone(),
                symbol: instrument.symbol.clone(),
                orderbook: market.market_pubkey.to_string(),
                tick: ticks_to_usd(&market, 1),
                lot: venue.lots_to_base_units(&market, 1),
                market,
                start,
                arrival: 0.0,
                haircut_bps: close_outs
                    .haircut(&instrument)
                    .map_or(0.0, |h| h.0),
                informed,
                base_bps: None,
                held: start,
                read: Read::Calm,
                watch_from: 0,
                watch_oracle: 0.0,
                watch_sold: 0.0,
                high: 0.0,
                high_at: 0,
                low: f64::INFINITY,
                from: 0.0,
                takes: VecDeque::new(),
                seen: HashSet::new(),
            });
        }
        self.asset_map = venue.perp_asset_map()?.to_string();
        self.venue = Some(venue);
        self.me = session.authority.clone();
        self.start_slot = session.start_slot;
        self.end_slot = session.end_slot;
        let legs: Vec<String> = self
            .legs
            .iter()
            .map(|l| format!("{} {} at {} bp", l.start, l.symbol, l.haircut_bps))
            .collect();
        log(&format!(
            "{:?} (allocating: {}): unwind {}; at most {} USD in {} slots",
            self.mode,
            self.allocate,
            legs.join(", "),
            self.cap_usd,
            self.window
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let now = market.slot;
        let t = self.progress(now);
        let oracles = self.oracles();
        let mut views = Vec::with_capacity(self.legs.len());
        for (i, leg) in self.legs.iter_mut().enumerate() {
            let venue = market.venues.iter().find(|v| v.id == leg.id);
            let (held, bid, ask) = if i == 0 {
                (market.position.size, market.best_bid, market.best_ask)
            } else {
                (
                    venue.and_then(|v| v.position).map_or(leg.held, |p| p.size),
                    venue.and_then(|v| v.best_bid),
                    venue.and_then(|v| v.best_ask),
                )
            };
            let oracle = if i == 0 {
                market.oracle
            } else {
                oracles[i].or_else(|| Some((bid? + ask?) / 2.0)).unwrap_or(0.0)
            };
            // What it sold since the last step counts toward the cap.
            let sold = leg.held - held;
            if sold > 1e-9 {
                self.traded.push_back((now, i, sold * oracle * 1.003));
            }
            leg.held = held;
            if leg.base_bps.is_none()
                && let (Some(bid), Some(ask)) = (bid, ask)
                && oracle > 0.0
            {
                leg.base_bps = Some(((ask - bid) / 2.0 / oracle * 1e4).max(1.0));
            }
            if leg.arrival == 0.0 {
                leg.arrival = oracle;
            }
            if oracle > leg.high {
                (leg.high, leg.high_at) = (oracle, now);
            }
            leg.low = leg.low.min(oracle);
            views.push(View { oracle, bid });
        }
        while self
            .traded
            .front()
            .is_some_and(|(at, ..)| at + self.window + CAP_SLACK_SLOTS < now)
        {
            self.traded.pop_front();
        }
        if self.mode.reads() {
            self.read_tapes(now);
            for (leg, view) in self.legs.iter_mut().zip(&views) {
                update_read(self.mode, leg, view.oracle, now, self.start_slot, self.end_slot);
            }
        }

        for leg in &self.legs {
            orders.cancel_all().at(&leg.id);
        }
        let mut room = self.room(now, None);
        let mut rooms: Vec<f64> = (0..self.legs.len())
            .map(|i| self.room(now, Some(i)))
            .collect();
        // Each leg's wants: (urgency, size, limit), then a resting ask.
        let mut wants: Vec<(usize, u8, f64, f64)> = Vec::new();
        let mut rests: Vec<(usize, f64, f64)> = Vec::new();
        for (i, (leg, view)) in self.legs.iter().zip(&views).enumerate() {
            if leg.held < leg.lot || view.oracle <= 0.0 {
                continue;
            }
            let base = leg.base_bps.unwrap_or(5.0);
            let at_maker = view.oracle * (1.0 - (base + WATCH_SLACK_BPS) / 1e4);
            let worth = view.bid.is_some_and(|b| {
                b * (1.0 - TAKER_FEE_BPS / 1e4) >= view.oracle * (1.0 - leg.haircut_bps / 1e4)
            });
            let slice = SLICE_USD / view.oracle;
            let ask_price = round_up(view.oracle * (1.0 + base / 1e4) - leg.tick, leg.tick);
            let rest = |rests: &mut Vec<(usize, f64, f64)>| {
                rests.push((i, slice.min(leg.held), ask_price));
            };
            let calm_floor = |power: i32, from: f64| {
                let x = ((t - from) / (0.95 - from).max(0.05)).clamp(0.0, 1.0);
                leg.start * x.powi(power)
            };
            match self.mode {
                Mode::Dump => {
                    wants.push((i, 2, leg.held.min(URGENT_USD / view.oracle), sweep(view.oracle, leg)));
                }
                Mode::TwapEach => {
                    let steps = (self.end_slot.saturating_sub(now) / 2).max(1) as f64;
                    let size = (leg.held / steps).max(leg.lot);
                    wants.push((i, 1, size, view.bid.unwrap_or(at_maker).min(view.oracle)));
                }
                Mode::ProRata | Mode::BiggerFirst => {
                    let floor = if self.mode == Mode::ProRata {
                        calm_floor(1, 0.0)
                    } else {
                        let bigger = self
                            .legs
                            .iter()
                            .enumerate()
                            .max_by(|a, b| {
                                (a.1.start * views[a.0].oracle)
                                    .total_cmp(&(b.1.start * views[b.0].oracle))
                            })
                            .map_or(0, |(j, _)| j);
                        let (lo, hi) = if i == bigger { (0.0, 0.5) } else { (0.5, 0.95) };
                        leg.start * ((t - lo) / (hi - lo)).clamp(0.0, 1.0)
                    };
                    if (leg.sold() < floor || t > LATE) && worth {
                        wants.push((i, 1, slice.min(leg.held), view.bid.unwrap_or(at_maker)));
                    }
                    rest(&mut rests);
                }
                Mode::Reference | Mode::Informed => match leg.read {
                    Read::Falling if t <= LATE || !worth => {
                        wants.push((
                            i,
                            2,
                            leg.held.min(URGENT_USD / view.oracle),
                            sweep(view.oracle, leg),
                        ));
                    }
                    Read::Watch => {
                        let left = leg.start * WATCH_SHARE - leg.watch_sold;
                        if left > leg.lot && view.bid.is_some_and(|b| b >= at_maker) {
                            wants.push((i, 1, slice.min(left).min(leg.held), at_maker));
                        }
                    }
                    Read::Rising if t <= LATE => {}
                    _ => {
                        if (leg.sold() < calm_floor(3, leg.from) || t > LATE) && worth {
                            wants.push((i, 0, slice.min(leg.held), view.bid.unwrap_or(at_maker)));
                        }
                        rest(&mut rests);
                    }
                },
            }
        }
        // The most urgent first, when allocating; each leg's own share
        // otherwise.
        if self.allocate {
            wants.sort_by_key(|w| std::cmp::Reverse(w.1));
        }
        let urgent = wants.iter().any(|w| w.1 == 2);
        for (i, _, size, limit) in wants {
            let leg = &self.legs[i];
            let price = views[i].oracle;
            let budget = if self.allocate { room } else { rooms[i] };
            let size = round_lots(size.min(budget / price).min(leg.held), leg.lot);
            if size < leg.lot {
                continue;
            }
            orders
                .sell(size, round_down(limit, leg.tick))
                .at(&leg.id)
                .reduce_only();
            let watching = leg.read == Read::Watch;
            room -= size * price;
            rooms[i] -= size * price;
            if watching {
                self.legs[i].watch_sold += size;
            }
        }
        // Rest asks with what is left, unless a name is urgent.
        if !urgent {
            for (i, size, price) in rests {
                let leg = &self.legs[i];
                let budget = if self.allocate { room } else { rooms[i] };
                let size = round_lots(size.min(budget / price), leg.lot);
                if size < leg.lot {
                    continue;
                }
                orders.ask(price, size).at(&leg.id).post_only().reduce_only();
                room -= size * price;
                rooms[i] -= size * price;
            }
        }
        Ok(())
    }
}

impl Unwind {
    /// Each market's takes since the last step, from its order book's
    /// transactions, but its own.
    fn read_tapes(&mut self, now: u64) {
        let Some(venue) = self.venue.as_ref() else {
            return;
        };
        let start = self.start_slot;
        for leg in &mut self.legs {
            for (slot, wallet, signed) in new_takes(venue, &leg.orderbook, &leg.market, &mut leg.seen)
            {
                if wallet == self.me {
                    continue;
                }
                let into = now.saturating_sub(start);
                leg.takes
                    .push_back((slot.saturating_sub(start).min(into), wallet, signed));
            }
            // Keep slots into the session, as the reads use them.
            while leg
                .takes
                .front()
                .is_some_and(|(at, ..)| at + BURST_SLOTS.max(FLOW_SLOTS) + start < now)
            {
                leg.takes.pop_front();
            }
        }
    }
}

/// Move a leg's read on from what this step showed, and log a change.
fn update_read(mode: Mode, leg: &mut Leg, oracle: f64, now: u64, start: u64, end: u64) {
    let before = leg.read;
    read_on(mode, leg, oracle, now, start, end);
    if leg.read != before {
        log(&format!(
            "slot {}: {} {:?} -> {:?}, {:.3} held",
            now.saturating_sub(start),
            leg.symbol,
            before,
            leg.read,
            leg.held
        ));
    }
}

fn read_on(mode: Mode, leg: &mut Leg, oracle: f64, now: u64, start: u64, end: u64) {
    let into = now.saturating_sub(start);
    let burst = leg.burst(into);
    let progress = |at: u64| at.saturating_sub(start) as f64 / end.saturating_sub(start).max(1) as f64;
    match mode {
        Mode::Informed => {
            // Only the told wallet's burst counts, and it is the move's.
            if leg.read == Read::Calm
                && let Some((sign, true)) = burst
            {
                leg.read = if sign < 0.0 { Read::Falling } else { Read::Rising };
                (leg.watch_from, leg.watch_oracle, leg.low) = (now, oracle, oracle);
            }
        }
        _ => {
            if leg.read == Read::Calm
                && let Some((sign, _)) = burst
            {
                leg.read = if sign < 0.0 { Read::Watch } else { Read::Rising };
                (leg.watch_from, leg.watch_oracle, leg.watch_sold, leg.low) =
                    (now, oracle, 0.0, oracle);
            }
            let moved = (oracle / leg.watch_oracle.max(1e-9) - 1.0) * 1e4;
            match leg.read {
                Read::Watch if moved <= -CONFIRM_BPS && leg.net_flow(into) < 0.0 => {
                    leg.read = Read::Falling;
                }
                Read::Watch | Read::Rising
                    if now > leg.watch_from + WATCH_SLOTS
                        && moved.abs() < CONFIRM_BPS =>
                {
                    leg.read = Read::Calm;
                }
                _ => {}
            }
        }
    }
    if leg.read == Read::Rising {
        leg.low = leg.low.min(oracle);
        if leg.high > leg.low * (1.0 + LANDED) && now >= leg.high_at + SETTLE_SLOTS {
            leg.read = Read::Calm;
            leg.from = progress(now);
        }
    }
}

/// The lowest an urgent sale takes: through the maker into the backstop.
fn sweep(oracle: f64, leg: &Leg) -> f64 {
    oracle * (1.0 - (leg.base_bps.unwrap_or(5.0) + SWEEP_BPS) / 1e4)
}

fn round_down(usd: f64, tick: f64) -> f64 {
    (usd / tick + 1e-9).floor() * tick
}

fn round_up(usd: f64, tick: f64) -> f64 {
    (usd / tick - 1e-9).ceil() * tick
}

fn round_lots(units: f64, lot: f64) -> f64 {
    (units / lot + 1e-9).floor() * lot
}

/// The takes on `orderbook` not seen before: their slot, the first signer
/// and side times size in base units, oldest first. Post-only orders rest
/// and are left out.
fn new_takes(
    venue: &PhoenixVenue,
    orderbook: &str,
    market: &ExchangeMarketConfig,
    seen: &mut HashSet<String>,
) -> Vec<(u64, String, f64)> {
    let call = |method: &str, params: Value| -> Option<Value> {
        serde_json::from_str(&rpc(method, &params.to_string()).ok()?).ok()
    };
    let Some(sigs) = call(
        "getSignaturesForAddress",
        json!([orderbook, { "limit": 100, "commitment": "confirmed" }]),
    ) else {
        return Vec::new();
    };
    let mut fresh: Vec<(u64, String)> = sigs
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["err"].is_null())
        .filter_map(|s| Some((s["slot"].as_u64()?, s["signature"].as_str()?.to_string())))
        .filter(|(_, sig)| !seen.contains(sig))
        .collect();
    fresh.sort_by_key(|(slot, _)| *slot);
    let mut out = Vec::new();
    for (slot, sig) in fresh {
        seen.insert(sig.clone());
        let Some(tx) = call(
            "getTransaction",
            json!([sig, { "encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed" }]),
        ) else {
            continue;
        };
        out.extend(
            takes_in(&tx, venue, market)
                .into_iter()
                .map(|(signer, signed)| (slot, signer, signed)),
        );
    }
    out
}

/// The takes in one transaction (`getTransaction`, json): its first signer,
/// and side times size.
fn takes_in(tx: &Value, venue: &PhoenixVenue, market: &ExchangeMarketConfig) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    if !tx["meta"]["err"].is_null() {
        return out;
    }
    let message = &tx["transaction"]["message"];
    let keys: Vec<&str> = message["accountKeys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let Some(signer) = keys.first() else {
        return out;
    };
    for ix in message["instructions"].as_array().into_iter().flatten() {
        let data = ix["data"]
            .as_str()
            .and_then(|d| bs58::decode(d).into_vec().ok())
            .unwrap_or_default();
        let Some(kind) = data
            .get(..8)
            .and_then(|tag| PhoenixInstruction::from_discriminant(tag.try_into().ok()?))
        else {
            continue;
        };
        if !matches!(
            kind,
            PhoenixInstruction::PlaceLimitOrder | PhoenixInstruction::PlaceMarketOrder
        ) {
            continue;
        }
        let Ok(packet) = <OrderPacket as borsh::BorshDeserialize>::deserialize(&mut &data[8..])
        else {
            continue;
        };
        let Ok(value) = serde_json::to_value(&packet) else {
            continue;
        };
        let Some((variant, fields)) = value["kind"].as_object().and_then(|o| o.iter().next()) else {
            continue;
        };
        if variant.to_ascii_lowercase().contains("post") {
            continue;
        }
        let side = match fields["side"].as_str() {
            Some(s) if s.eq_ignore_ascii_case("bid") => 1.0,
            Some(_) => -1.0,
            None => continue,
        };
        let Some(lots) = fields["num_base_lots"].as_i64() else {
            continue;
        };
        let size = venue.lots_to_base_units(market, lots);
        if size > 0.0 {
            out.push((signer.to_string(), side * size));
        }
    }
    out
}

#[cfg(all(target_arch = "wasm32", feature = "export"))]
mtx_kit::export!(Unwind);
