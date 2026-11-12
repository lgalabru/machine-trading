//! Oracle writers: the prices venues read, driven by the scenario.
//!
//! A venue reads its price from an oracle it doesn't own: Phoenix's perps
//! from the exchange's own oracle accounts, spot sessions' valuation and
//! lending markets from Pyth `PriceUpdateV2` accounts, Kamino from Scope. A
//! scenario's `oracles` section names each feed ([`Feed`]), the path its
//! price walks, and the failures it suffers on the way ([`Failure`]).
//!
//! Each feed plans its publications first ([`Feed::plan`]): a pure function
//! of the feed, the scenario's seed and the start price, so every bot meets
//! the same oracle. A writer ([`OracleSource`]) then lowers them to Surfpool
//! overrides, registered before the cheatcodes lock like the rest of the
//! scenario ([`register`]). Failures act on the plan, not on the writers, so
//! they apply to every source the same way:
//!
//! | Mode | What the oracle does |
//! |------|----------------------|
//! | `stale` | stops publishing: the account keeps its last write while the clock runs |
//! | `wide_confidence` | publishes the right price with its confidence interval multiplied |
//! | `off_price` | publishes the path's price moved by a bias, as a manipulated feed would |
//! | `delayed` | publishes what the price was `lag_slots` ago, stamped as then |
//!
//! Confidence and publish times exist for Pyth only; Phoenix's oracle has
//! neither (and restamps every reading's age whenever surfpool plays any
//! Phoenix override), and Scope's price carries a slot and time but no
//! interval. Those writers drop what their account can't hold.

use serde::{Deserialize, Serialize};
use serde_json::json;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use tracing::{info, warn};

use crate::{
    Error, Result,
    chain::{self, Override},
    phoenix::{MAX_MOVE_STEP_PCT, SLOTS_BETWEEN_STEPS, oracle_move},
    scenario::SplitMix64,
};

/// Slots between a feed's publications unless it says otherwise: a second
/// at 250 ms slots, a Pyth feed's usual pace.
const DEFAULT_CADENCE_SLOTS: u64 = 4;
/// Confidence interval, in basis points of the price, unless it says
/// otherwise.
const DEFAULT_CONF_BPS: f64 = 2.0;
/// Surfnet slot time (see `snapshot.rs`).
pub const SLOT_TIME_MS: u64 = 250;

/// A scenario's oracles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Oracles {
    /// What the failure windows' random starts come from.
    pub seed: u64,
    pub feeds: Vec<Feed>,
}

/// One price feed: where it's written, the path it walks, and how it fails.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Feed {
    pub name: String,
    pub source: Source,
    /// Slots between publications.
    #[serde(default = "default_cadence")]
    pub cadence_slots: u64,
    /// The price relative to its value at the session's start, linear
    /// between points and flat outside them. Without points the feed holds
    /// its start price.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<Level>,
    /// The confidence interval, in basis points of the price. Pyth only.
    #[serde(default = "default_conf_bps")]
    pub conf_bps: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<Failure>,
}

fn default_cadence() -> u64 {
    DEFAULT_CADENCE_SLOTS
}

fn default_conf_bps() -> f64 {
    DEFAULT_CONF_BPS
}

/// A point on a feed's path: its price `level_pct` percent from the start
/// price, `slot` slots into the session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Level {
    pub slot: u64,
    pub level_pct: f64,
}

/// A failure over a window of the session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub mode: Mode,
    pub window: Window,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mode {
    Stale,
    /// The interval is `factor` times as wide.
    WideConfidence {
        factor: f64,
    },
    /// The price is `bias_pct` percent away from the path's.
    OffPrice {
        bias_pct: f64,
    },
    /// The feed publishes the path as it was `lag_slots` earlier, stamped
    /// as of then.
    Delayed {
        lag_slots: u64,
    },
}

/// Where a failure starts and how long it lasts, in slots of the session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub from_slot: u64,
    /// With it, the start is drawn from the seed between `from_slot` and
    /// this, both included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_slot_max: Option<u64>,
    /// How long it lasts. Without it, to the end of the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<u64>,
}

/// Where a feed is written.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    Phoenix(PhoenixOracle),
    Pyth(PythOracle),
    Scope(ScopeOracle),
    Switchboard(SwitchboardOracle),
}

impl Source {
    fn writer(&self) -> &dyn OracleSource {
        match self {
            Self::Phoenix(w) => w,
            Self::Pyth(w) => w,
            Self::Scope(w) => w,
            Self::Switchboard(w) => w,
        }
    }
}

/// One publication: `price` lands at `slot`, as of `as_of_slot`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Publication {
    pub slot: u64,
    pub as_of_slot: u64,
    pub price: f64,
    /// The confidence interval as a fraction of the price.
    pub conf_fraction: f64,
    /// The slow-moving average a consumer may read beside the price: the
    /// path's own price as of `as_of_slot` and the healthy interval, which
    /// a failure doesn't move (an average of the true feed wouldn't jump
    /// with one bad report).
    pub ema_price: f64,
    pub ema_conf_fraction: f64,
}

/// What a writer needs to know about the world it writes into.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// The slot the scenario counts from.
    pub base_slot: u64,
    /// The chain's clock then, in Unix seconds.
    pub start_unix: i64,
    pub slot_time_ms: u64,
    /// The feed's price in its source's unit when the session starts.
    pub start_price: f64,
    /// The session's length, for writers that spill past it.
    pub duration_slots: u64,
    /// Phoenix's perp asset map, for a Phoenix feed.
    pub perp_asset_map: Option<Pubkey>,
}

impl Context {
    fn unix_at(&self, slot: u64) -> i64 {
        self.start_unix + (slot * self.slot_time_ms / 1000) as i64
    }
}

/// What the world already holds that a writer reads its start from.
#[derive(Debug, Clone, Copy, Default)]
pub struct World {
    /// Phoenix's perp asset map, and the scenario's market mark in ticks.
    pub phoenix: Option<(Pubkey, u64)>,
}

/// A place a price is written. The price unit is the source's own: ticks
/// for Phoenix, dollars for Pyth and Scope.
pub trait OracleSource {
    /// The price the account holds now.
    fn start_price(&self, rpc: &RpcClient, world: &World) -> Result<f64>;
    /// Overrides that publish `publications` in order.
    fn lower(&self, context: &Context, publications: &[Publication]) -> Result<Vec<Override>>;
}

impl Oracles {
    pub fn validate(&self, duration_slots: u64) -> std::result::Result<(), String> {
        for (i, feed) in self.feeds.iter().enumerate() {
            let name = &feed.name;
            if self.feeds[..i].iter().any(|f| f.name == *name) {
                return Err(format!("oracles: feed {name} appears twice"));
            }
            if feed.cadence_slots == 0 {
                return Err(format!("oracles: {name}: cadence_slots must be at least 1"));
            }
            if !feed.conf_bps.is_finite() || feed.conf_bps < 0.0 {
                return Err(format!("oracles: {name}: conf_bps must be 0 or more"));
            }
            if feed.path.windows(2).any(|w| w[1].slot <= w[0].slot)
                || feed
                    .path
                    .iter()
                    .any(|l| !l.level_pct.is_finite() || l.level_pct <= -100.0)
            {
                return Err(format!(
                    "oracles: {name}: path slots must increase and levels stay above -100%"
                ));
            }
            for failure in &feed.failures {
                let w = &failure.window;
                if w.from_slot_max.is_some_and(|max| max < w.from_slot) || w.slots == Some(0) {
                    return Err(format!("oracles: {name}: a failure window is empty"));
                }
                if w.from_slot.max(w.from_slot_max.unwrap_or(0)) > duration_slots {
                    return Err(format!(
                        "oracles: {name}: a failure starts after the session ends"
                    ));
                }
                match failure.mode {
                    Mode::WideConfidence { factor } if !(factor.is_finite() && factor >= 1.0) => {
                        return Err(format!("oracles: {name}: factor must be at least 1"));
                    }
                    Mode::OffPrice { bias_pct } if !(bias_pct.is_finite() && bias_pct > -100.0) => {
                        return Err(format!("oracles: {name}: bias_pct must stay above -100"));
                    }
                    Mode::Delayed { lag_slots: 0 } => {
                        return Err(format!("oracles: {name}: lag_slots must be at least 1"));
                    }
                    _ => {}
                }
            }
            let target = match &feed.source {
                Source::Pyth(p) => {
                    p.feed_id_bytes()
                        .map_err(|e| format!("oracles: {name}: {e}"))?;
                    format!(
                        "pyth {}",
                        p.address().map_err(|e| format!("oracles: {name}: {e}"))?
                    )
                }
                Source::Scope(s) => format!(
                    "scope {} {}",
                    s.address().map_err(|e| format!("oracles: {name}: {e}"))?,
                    s.index
                ),
                Source::Phoenix(p) => format!("phoenix {}", p.symbol),
                Source::Switchboard(_) => {
                    return Err(format!("oracles: {name}: Switchboard isn't wired yet"));
                }
            };
            // Two feeds writing one price would interleave and each undo
            // the other's failures.
            if self.feeds[..i].iter().any(|f| f.target() == target) {
                return Err(format!(
                    "oracles: {name} writes a price another feed writes"
                ));
            }
        }
        Ok(())
    }
}

impl Feed {
    /// What the feed writes, to compare feeds by.
    fn target(&self) -> String {
        match &self.source {
            Source::Pyth(p) => p
                .address()
                .map_or_else(|e| e, |address| format!("pyth {address}")),
            Source::Scope(s) => s
                .address()
                .map_or_else(|e| e, |address| format!("scope {address} {}", s.index)),
            Source::Phoenix(p) => format!("phoenix {}", p.symbol),
            Source::Switchboard(w) => format!("switchboard {}", w.account),
        }
    }

    /// The path's price at `slot`.
    fn price_at(&self, start_price: f64, slot: u64) -> f64 {
        let level = match self.path.iter().position(|l| l.slot > slot) {
            None => self.path.last().map_or(0.0, |l| l.level_pct),
            Some(0) => {
                let next = self.path[0];
                next.level_pct * slot as f64 / next.slot as f64
            }
            Some(i) => {
                let (a, b) = (self.path[i - 1], self.path[i]);
                a.level_pct
                    + (b.level_pct - a.level_pct) * (slot - a.slot) as f64
                        / (b.slot - a.slot) as f64
            }
        };
        start_price * (1.0 + level / 100.0)
    }

    /// Every publication of the session, `index` being this feed's place in
    /// the scenario. A pure function of its arguments.
    pub fn plan(
        &self,
        seed: u64,
        index: usize,
        duration_slots: u64,
        start_price: f64,
    ) -> Vec<Publication> {
        let windows: Vec<(u64, u64, Mode)> = self
            .failures
            .iter()
            .enumerate()
            .map(|(n, f)| {
                let (from, to) = f.window.resolve(seed, index as u64, n as u64);
                (from, to, f.mode)
            })
            .collect();
        let mut publications: Vec<Publication> = Vec::new();
        let mut slot = 0;
        while slot <= duration_slots {
            let active = || {
                windows
                    .iter()
                    .filter(move |(from, to, _)| (*from..*to).contains(&slot))
                    .map(|w| w.2)
            };
            if !active().any(|m| m == Mode::Stale) {
                let lag = active()
                    .filter_map(|m| match m {
                        Mode::Delayed { lag_slots } => Some(lag_slots),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0);
                let as_of_slot = slot.saturating_sub(lag);
                // Time doesn't run backwards: a delayed feed goes quiet
                // until the lagged moment passes the last one it published.
                if publications
                    .last()
                    .is_some_and(|last| as_of_slot <= last.as_of_slot)
                {
                    slot += self.cadence_slots;
                    continue;
                }
                let truth = self.price_at(start_price, as_of_slot);
                let mut price = truth;
                let mut conf_fraction = self.conf_bps / 10_000.0;
                for mode in active() {
                    match mode {
                        Mode::OffPrice { bias_pct } => price *= 1.0 + bias_pct / 100.0,
                        Mode::WideConfidence { factor } => conf_fraction *= factor,
                        Mode::Stale | Mode::Delayed { .. } => {}
                    }
                }
                publications.push(Publication {
                    slot,
                    as_of_slot,
                    price,
                    conf_fraction,
                    ema_price: truth,
                    ema_conf_fraction: self.conf_bps / 10_000.0,
                });
            }
            slot += self.cadence_slots;
        }
        publications
    }
}

impl Window {
    /// The window as slots `[from, to)`, its start drawn from the seed when
    /// the scenario leaves it a range.
    fn resolve(&self, seed: u64, feed: u64, failure: u64) -> (u64, u64) {
        let from = match self.from_slot_max {
            Some(max) => {
                let mut rng = SplitMix64(seed ^ (feed << 32) ^ failure.wrapping_mul(0x9E37));
                rng.next_u64();
                self.from_slot
                    + ((rng.next_f64() * (max - self.from_slot + 1) as f64) as u64)
                        .min(max - self.from_slot)
            }
            None => self.from_slot,
        };
        (
            from,
            self.slots.map_or(u64::MAX, |s| from.saturating_add(s)),
        )
    }
}

/// Phoenix's exchange-owned oracle, moved with the existing
/// `phoenix-oracle-price` template. Only the oracle moves: the book stays.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PhoenixOracle {
    pub symbol: String,
}

impl OracleSource for PhoenixOracle {
    fn start_price(&self, _rpc: &RpcClient, world: &World) -> Result<f64> {
        world
            .phoenix
            .map(|(_, ticks)| ticks as f64)
            .ok_or_else(|| oracle_error(&self.symbol, "no Phoenix market in this session"))
    }

    fn lower(&self, context: &Context, publications: &[Publication]) -> Result<Vec<Override>> {
        let map = context
            .perp_asset_map
            .ok_or_else(|| oracle_error(&self.symbol, "no Phoenix market in this session"))?;
        let mut overrides = Vec::new();
        let mut previous = context.start_price;
        let mut last_slot = None;
        for p in publications {
            let target = p.price.round().max(1.0);
            // Phoenix refuses a report far from the median, so a big jump
            // arrives as equal-ratio steps, none past the limit either way.
            let limit = 1.0 + MAX_MOVE_STEP_PCT / 100.0;
            let steps = ((target / previous).ln().abs() / limit.ln())
                .ceil()
                .max(1.0) as u64;
            for step in 1..=steps {
                let ticks = previous * (target / previous).powf(step as f64 / steps as f64);
                let mut slot = p.slot + (step - 1) * SLOTS_BETWEEN_STEPS;
                if let Some(last) = last_slot {
                    slot = slot.max(last + 1);
                }
                last_slot = Some(slot);
                if slot > context.duration_slots {
                    warn!(
                        symbol = self.symbol,
                        slot, "a Phoenix oracle step lands after the session ends"
                    );
                }
                overrides.push(oracle_move(map, &self.symbol, ticks.round() as u64, slot));
            }
            previous = target;
        }
        Ok(overrides)
    }
}

/// A Pyth `PriceUpdateV2` account, written with surfpool's
/// `pyth-price-feed-v2` template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PythOracle {
    /// The feed id, 64 hex digits.
    pub feed_id: String,
    #[serde(default)]
    pub shard: u16,
    /// The price's power of ten. Pyth's feeds use -8 mostly.
    #[serde(default = "default_pyth_exponent")]
    pub exponent: i32,
    /// The account, if it isn't the push oracle's for the feed and shard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

fn default_pyth_exponent() -> i32 {
    -8
}

impl PythOracle {
    fn feed_id_bytes(&self) -> std::result::Result<[u8; 32], String> {
        let digits = self.feed_id.strip_prefix("0x").unwrap_or(&self.feed_id);
        if digits.len() != 64 || !digits.is_ascii() {
            return Err("feed_id is 64 hex digits".into());
        }
        let mut id = [0u8; 32];
        for (byte, pair) in id.iter_mut().zip(digits.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(|e| e.to_string())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| "feed_id is 64 hex digits")?;
        }
        Ok(id)
    }

    pub(crate) fn address(&self) -> std::result::Result<Pubkey, String> {
        match &self.account {
            Some(account) => account
                .parse()
                .map_err(|_| format!("{account} isn't a public key")),
            None => Ok(mtx_kit::protocols::pyth::feed_account(
                self.shard,
                &self.feed_id_bytes()?,
            )),
        }
    }
}

impl OracleSource for PythOracle {
    fn start_price(&self, rpc: &RpcClient, _world: &World) -> Result<f64> {
        let address = self.address().map_err(|e| oracle_error(&self.feed_id, e))?;
        let id = self
            .feed_id_bytes()
            .map_err(|e| oracle_error(&self.feed_id, e))?;
        let account = rpc.get_account(&address)?;
        let update = mtx_kit::protocols::pyth::PriceUpdate::decode_feed(&account.data, &id)
            .map_err(|e| oracle_error(&self.feed_id, e.to_string()))?;
        Ok(update.price_f64())
    }

    fn lower(&self, context: &Context, publications: &[Publication]) -> Result<Vec<Override>> {
        let address = self.address().map_err(|e| oracle_error(&self.feed_id, e))?;
        let scale = 10f64.powi(-self.exponent);
        // Publish times only go up (see [`Feed::plan`]); the first update
        // follows the account's own last by a second.
        let mut previous_time = None;
        Ok(publications
            .iter()
            .map(|p| {
                let time = context.unix_at(p.as_of_slot);
                let price = (p.price * scale).round();
                let conf = (price * p.conf_fraction).round().max(1.0);
                let prev = previous_time.replace(time).unwrap_or(time - 1);
                Override {
                    template_id: "pyth-price-feed-v2",
                    account: address,
                    values: json!({
                        "price_message.price": price as i64,
                        "price_message.conf": conf as u64,
                        "price_message.exponent": self.exponent,
                        "price_message.publish_time": time,
                        "price_message.prev_publish_time": prev,
                        "price_message.ema_price": (p.ema_price * scale).round() as i64,
                        "price_message.ema_conf": ((p.ema_price * scale).round()
                            * p.ema_conf_fraction)
                            .round()
                            .max(1.0) as u64,
                        // Where the write lands; only the publish time
                        // carries a delay.
                        "posted_slot": context.base_slot + p.slot,
                    }),
                    slot: p.slot,
                    label: format!(
                        "pyth {} @{}",
                        &self.feed_id[..8.min(self.feed_id.len())],
                        p.slot
                    ),
                }
            })
            .collect())
    }
}

/// One entry of a Kamino Scope `OraclePrices` account, written with
/// surfpool's `kamino-scope-price` template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScopeOracle {
    /// The `OraclePrices` account (a reserve names its own in
    /// `config.token_info.scope_configuration.price_feed`).
    pub account: String,
    /// The entry, 0 to 511.
    pub index: u16,
    /// The price's power of ten, as the entry holds it.
    #[serde(default = "default_scope_exp")]
    pub exp: u64,
}

fn default_scope_exp() -> u64 {
    8
}

/// Scope's account: an 8-byte discriminator, the mappings account's key,
/// then 512 entries of a price (value and exponent), the slot and Unix time
/// it was updated at, and 24 bytes of source data.
const SCOPE_ENTRIES_AT: usize = 8 + 32;
const SCOPE_ENTRY_LEN: usize = 8 + 8 + 8 + 8 + 24;
pub const SCOPE_ENTRIES: u16 = 512;

impl ScopeOracle {
    pub(crate) fn address(&self) -> std::result::Result<Pubkey, String> {
        if self.index >= SCOPE_ENTRIES {
            return Err(format!(
                "index {} is past Scope's {SCOPE_ENTRIES}",
                self.index
            ));
        }
        self.account
            .parse()
            .map_err(|_| format!("{} isn't a public key", self.account))
    }
}

impl OracleSource for ScopeOracle {
    fn start_price(&self, rpc: &RpcClient, _world: &World) -> Result<f64> {
        let address = self.address().map_err(|e| oracle_error(&self.account, e))?;
        let account = rpc.get_account(&address)?;
        scope_price(&account.data, self.index)
            .ok_or_else(|| oracle_error(&self.account, "not a Scope price account"))
    }

    fn lower(&self, context: &Context, publications: &[Publication]) -> Result<Vec<Override>> {
        let address = self.address().map_err(|e| oracle_error(&self.account, e))?;
        let scale = 10f64.powi(self.exp as i32);
        let i = self.index;
        Ok(publications
            .iter()
            .map(|p| Override {
                template_id: "kamino-scope-price",
                account: address,
                values: serde_json::Value::Object(
                    [
                        (
                            format!("prices.{i}.price.value"),
                            json!((p.price * scale).round() as u64),
                        ),
                        (format!("prices.{i}.price.exp"), json!(self.exp)),
                        (
                            format!("prices.{i}.last_updated_slot"),
                            json!(context.base_slot + p.slot),
                        ),
                        (
                            format!("prices.{i}.unix_timestamp"),
                            json!(context.unix_at(p.as_of_slot)),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                ),
                slot: p.slot,
                label: format!("scope {i} @{}", p.slot),
            })
            .collect())
    }
}

/// A Scope entry's price in dollars.
pub fn scope_price(data: &[u8], index: u16) -> Option<f64> {
    let at = SCOPE_ENTRIES_AT + index as usize * SCOPE_ENTRY_LEN;
    let entry = data.get(at..at + SCOPE_ENTRY_LEN)?;
    let value = u64::from_le_bytes(entry[0..8].try_into().ok()?);
    let exp = u64::from_le_bytes(entry[8..16].try_into().ok()?);
    Some(value as f64 / 10f64.powi(i32::try_from(exp).ok()?))
}

/// A Switchboard On-Demand quote. Typed so scenarios can name it; no
/// consumer reads it yet, so it neither reads nor writes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SwitchboardOracle {
    pub account: String,
}

impl OracleSource for SwitchboardOracle {
    fn start_price(&self, _rpc: &RpcClient, _world: &World) -> Result<f64> {
        Err(oracle_error(&self.account, "Switchboard isn't wired yet"))
    }

    fn lower(&self, _context: &Context, _publications: &[Publication]) -> Result<Vec<Override>> {
        Err(oracle_error(&self.account, "Switchboard isn't wired yet"))
    }
}

fn oracle_error(name: &str, reason: impl Into<String>) -> Error {
    Error::Oracle {
        name: name.to_string(),
        reason: reason.into(),
    }
}

/// Every feed's overrides, in feed order.
pub fn overrides(
    oracles: &Oracles,
    duration_slots: u64,
    base_slot: u64,
    start_unix: i64,
    rpc: &RpcClient,
    world: &World,
) -> Result<Vec<Override>> {
    let mut all = Vec::new();
    for (index, feed) in oracles.feeds.iter().enumerate() {
        let writer = feed.source.writer();
        let start_price = writer.start_price(rpc, world)?;
        let context = Context {
            base_slot,
            start_unix,
            slot_time_ms: SLOT_TIME_MS,
            start_price,
            duration_slots,
            perp_asset_map: world.phoenix.map(|(map, _)| map),
        };
        let publications = feed.plan(oracles.seed, index, duration_slots, start_price);
        all.extend(writer.lower(&context, &publications)?);
    }
    Ok(all)
}

/// Register the scenario's oracle writes. Call it where the scenario's own
/// overrides are registered, before the cheatcodes lock: slots count from
/// then.
pub fn register(
    rpc: &RpcClient,
    oracles: &Oracles,
    duration_slots: u64,
    world: &World,
) -> Result<()> {
    let base_slot = rpc.get_slot()?;
    let clock = solana_pubkey::pubkey!("SysvarC1ock11111111111111111111111111111111");
    let start_unix = rpc
        .get_account(&clock)?
        .data
        .get(32..40)
        .and_then(|b| b.try_into().ok())
        .map(i64::from_le_bytes)
        .ok_or_else(|| oracle_error("clock", "the clock sysvar is short"))?;
    let overrides = overrides(oracles, duration_slots, base_slot, start_unix, rpc, world)?;
    info!(
        feeds = oracles.feeds.len(),
        writes = overrides.len(),
        "oracles"
    );
    chain::register_scenario(rpc, "oracles", overrides)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL_USD: &str = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";

    fn feed(source: Source, failures: Vec<Failure>) -> Feed {
        Feed {
            name: "f".into(),
            source,
            cadence_slots: 4,
            path: vec![Level {
                slot: 40,
                level_pct: -10.0,
            }],
            conf_bps: 2.0,
            failures,
        }
    }

    fn pyth() -> Source {
        Source::Pyth(PythOracle {
            feed_id: SOL_USD.into(),
            shard: 0,
            exponent: -8,
            account: None,
        })
    }

    fn failure(mode: Mode, from_slot: u64, slots: Option<u64>) -> Failure {
        Failure {
            mode,
            window: Window {
                from_slot,
                from_slot_max: None,
                slots,
            },
        }
    }

    fn context() -> Context {
        Context {
            base_slot: 1_000,
            start_unix: 1_800_000_000,
            slot_time_ms: 250,
            start_price: 100.0,
            duration_slots: 40,
            perp_asset_map: Some(Pubkey::new_unique()),
        }
    }

    #[test]
    fn a_healthy_feed_follows_its_path() {
        let plan = feed(pyth(), vec![]).plan(1, 0, 40, 100.0);
        assert_eq!(plan.len(), 11);
        assert_eq!(plan[0].price, 100.0);
        assert!((plan[5].price - 95.0).abs() < 1e-9);
        assert!((plan[10].price - 90.0).abs() < 1e-9);
        assert!(plan.iter().all(|p| p.as_of_slot == p.slot));
        assert!((plan[0].conf_fraction - 0.0002).abs() < 1e-12);
    }

    #[test]
    fn stale_stops_publishing_for_its_window() {
        let f = feed(pyth(), vec![failure(Mode::Stale, 12, Some(16))]);
        let slots: Vec<u64> = f.plan(1, 0, 40, 100.0).iter().map(|p| p.slot).collect();
        assert_eq!(slots, [0, 4, 8, 28, 32, 36, 40]);
        let forever = feed(pyth(), vec![failure(Mode::Stale, 12, None)]);
        assert_eq!(forever.plan(1, 0, 40, 100.0).last().unwrap().slot, 8);
    }

    #[test]
    fn wide_confidence_multiplies_the_interval_only() {
        let healthy = feed(pyth(), vec![]).plan(1, 0, 40, 100.0);
        let f = feed(
            pyth(),
            vec![failure(Mode::WideConfidence { factor: 50.0 }, 8, Some(8))],
        );
        for (p, h) in f.plan(1, 0, 40, 100.0).iter().zip(&healthy) {
            assert_eq!(p.price, h.price);
            let wide = (8..16).contains(&p.slot);
            assert_eq!(
                p.conf_fraction,
                h.conf_fraction * if wide { 50.0 } else { 1.0 }
            );
        }
    }

    #[test]
    fn off_price_biases_the_price_for_its_window() {
        let healthy = feed(pyth(), vec![]).plan(1, 0, 40, 100.0);
        let f = feed(
            pyth(),
            vec![failure(Mode::OffPrice { bias_pct: 5.0 }, 8, Some(8))],
        );
        for (p, h) in f.plan(1, 0, 40, 100.0).iter().zip(&healthy) {
            let off = (8..16).contains(&p.slot);
            assert!((p.price - h.price * if off { 1.05 } else { 1.0 }).abs() < 1e-9);
        }
    }

    #[test]
    fn delayed_goes_quiet_then_publishes_the_past_stamped_as_then() {
        let f = feed(
            pyth(),
            vec![failure(Mode::Delayed { lag_slots: 8 }, 20, Some(20))],
        );
        let plan = f.plan(1, 0, 48, 100.0);
        let at = |slot| plan.iter().find(|p| p.slot == slot);
        // The lagged moment (12, 16) hasn't passed the last published (16).
        assert!(at(20).is_none() && at(24).is_none());
        assert_eq!(at(28).unwrap().as_of_slot, 20);
        assert!((at(28).unwrap().price - 95.0).abs() < 1e-9);
        assert_eq!(at(36).unwrap().as_of_slot, 28);
        // The window ends: the feed is current again, and time only went up.
        assert_eq!(at(40).unwrap().as_of_slot, 40);
        assert!(plan.windows(2).all(|w| w[1].as_of_slot > w[0].as_of_slot));
    }

    #[test]
    fn an_average_does_not_follow_a_failure() {
        let f = feed(
            pyth(),
            vec![
                failure(Mode::OffPrice { bias_pct: 10.0 }, 8, Some(8)),
                failure(Mode::WideConfidence { factor: 10.0 }, 8, Some(8)),
            ],
        );
        let plan = f.plan(1, 0, 40, 100.0);
        let bad = plan.iter().find(|p| p.slot == 8).unwrap();
        assert!((bad.price - bad.ema_price * 1.1).abs() < 1e-9);
        assert_eq!(bad.ema_conf_fraction, 0.0002);
        assert!(bad.conf_fraction > bad.ema_conf_fraction);
    }

    #[test]
    fn a_window_start_is_drawn_from_the_seed() {
        let ranged = |seed| {
            let mut f = feed(pyth(), vec![failure(Mode::Stale, 4, Some(4))]);
            f.failures[0].window.from_slot_max = Some(32);
            f.plan(seed, 0, 40, 100.0)
        };
        assert_eq!(ranged(7), ranged(7));
        let starts: std::collections::BTreeSet<usize> =
            (0..40).map(|seed| ranged(seed).len()).collect();
        assert_eq!(starts.len(), 1, "the same number are skipped");
        let gaps: std::collections::BTreeSet<u64> = (0..40)
            .map(|seed| {
                let plan = ranged(seed);
                plan.windows(2)
                    .find(|w| w[1].slot - w[0].slot > 4)
                    .map_or(0, |w| w[0].slot)
            })
            .collect();
        assert!(gaps.len() > 3, "the start varies with the seed: {gaps:?}");
        assert!(gaps.iter().all(|g| *g <= 32));
    }

    #[test]
    fn pyth_lowers_to_the_price_update_template() {
        let f = feed(
            pyth(),
            vec![failure(Mode::Delayed { lag_slots: 8 }, 20, Some(20))],
        );
        let plan = f.plan(1, 0, 40, 100.0);
        let writer = f.source.writer();
        let overrides = writer.lower(&context(), &plan).unwrap();
        assert_eq!(overrides.len(), plan.len());
        let id: [u8; 32] = f.source_pyth_id();
        let account = mtx_kit::protocols::pyth::feed_account(0, &id);
        assert!(overrides.iter().all(|o| o.account == account));
        assert!(
            overrides
                .iter()
                .all(|o| o.template_id == "pyth-price-feed-v2")
        );
        let first = &overrides[0].values;
        assert_eq!(first["price_message.price"], 10_000_000_000i64);
        assert_eq!(first["price_message.conf"], 2_000_000u64);
        assert_eq!(first["price_message.publish_time"], 1_800_000_000i64);
        assert_eq!(first["posted_slot"], 1_000u64);
        // The delayed update lands at slot 28, as of slot 20 (5 s): the
        // write is where it lands, the publish time carries the delay and
        // follows the previous update's (slot 16, 4 s).
        let delayed = overrides.iter().find(|o| o.slot == 28).unwrap();
        assert_eq!(delayed.values["posted_slot"], 1_028u64);
        assert_eq!(
            delayed.values["price_message.publish_time"],
            1_800_000_005i64
        );
        assert_eq!(
            delayed.values["price_message.prev_publish_time"],
            1_800_000_004i64
        );
        let times: Vec<i64> = overrides
            .iter()
            .map(|o| o.values["price_message.publish_time"].as_i64().unwrap())
            .collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
    }

    #[test]
    fn scope_lowers_one_entry() {
        let source = Source::Scope(ScopeOracle {
            account: Pubkey::new_unique().to_string(),
            index: 3,
            exp: 8,
        });
        let f = feed(source, vec![]);
        let plan = f.plan(1, 0, 8, 100.0);
        let overrides = f.source.writer().lower(&context(), &plan).unwrap();
        assert_eq!(overrides[1].template_id, "kamino-scope-price");
        assert_eq!(
            overrides[1].values["prices.3.price.value"],
            9_900_000_000u64
        );
        assert_eq!(overrides[1].values["prices.3.price.exp"], 8u64);
        assert_eq!(overrides[1].values["prices.3.last_updated_slot"], 1_004u64);
        assert_eq!(
            overrides[1].values["prices.3.unix_timestamp"],
            1_800_000_001i64
        );
    }

    #[test]
    fn scope_prices_decode_from_the_account() {
        let mut data = vec![0u8; SCOPE_ENTRIES_AT + 512 * SCOPE_ENTRY_LEN];
        let at = SCOPE_ENTRIES_AT + 3 * SCOPE_ENTRY_LEN;
        data[at..at + 8].copy_from_slice(&8_450_000_000u64.to_le_bytes());
        data[at + 8..at + 16].copy_from_slice(&8u64.to_le_bytes());
        assert_eq!(data.len(), 28_712);
        assert!((scope_price(&data, 3).unwrap() - 84.5).abs() < 1e-9);
        assert_eq!(scope_price(&data[..100], 3), None);
    }

    #[test]
    fn phoenix_moves_the_oracle_in_steps_it_accepts() {
        let source = Source::Phoenix(PhoenixOracle {
            symbol: "SPCX".into(),
        });
        let mut f = feed(source, vec![]);
        f.path = vec![
            Level {
                slot: 1,
                level_pct: 0.0,
            },
            Level {
                slot: 4,
                level_pct: -20.0,
            },
        ];
        let mut context = context();
        context.start_price = 10_000.0;
        let plan = f.plan(1, 0, 4, 10_000.0);
        let overrides = f.source.writer().lower(&context, &plan).unwrap();
        assert!(
            overrides
                .iter()
                .all(|o| o.template_id == "phoenix-oracle-price")
        );
        let slots: Vec<u64> = overrides.iter().map(|o| o.slot).collect();
        assert!(slots.windows(2).all(|w| w[0] < w[1]), "ordered: {slots:?}");
        let ticks: Vec<f64> = overrides
            .iter()
            .map(|o| o.values["target_ticks"].as_str().unwrap().parse().unwrap())
            .collect();
        assert_eq!(*ticks.last().unwrap(), 8_000.0);
        let mut previous = 10_000.0;
        for t in ticks {
            assert!((t / previous - 1.0).abs() * 100.0 <= MAX_MOVE_STEP_PCT + 0.01);
            previous = t;
        }
    }

    #[test]
    fn feeds_that_write_one_price_are_refused() {
        let twice = Oracles {
            seed: 1,
            feeds: vec![feed(pyth(), vec![]), {
                let mut f = feed(pyth(), vec![]);
                f.name = "g".into();
                f
            }],
        };
        assert!(twice.validate(40).unwrap_err().contains("another feed"));
        let late = Oracles {
            seed: 1,
            feeds: vec![{
                let mut f = feed(pyth(), vec![failure(Mode::Stale, 4, Some(4))]);
                f.failures[0].window.from_slot_max = Some(80);
                f
            }],
        };
        assert!(late.validate(40).is_err());
    }

    #[test]
    fn switchboard_is_typed_but_refused() {
        let source = Source::Switchboard(SwitchboardOracle {
            account: "q".into(),
        });
        let oracles = Oracles {
            seed: 1,
            feeds: vec![feed(source.clone(), vec![])],
        };
        assert!(oracles.validate(40).unwrap_err().contains("Switchboard"));
        assert!(source.writer().lower(&context(), &[]).is_err());
    }

    #[test]
    fn a_scenario_section_round_trips_and_is_checked() {
        let json = r#"{"seed":9,"feeds":[{"name":"sol","source":{"kind":"pyth","feed_id":"0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d"},
            "path":[{"slot":40,"level_pct":-5.0}],
            "failures":[{"mode":{"kind":"stale"},"window":{"from_slot":8,"slots":8}},
                        {"mode":{"kind":"wide_confidence","factor":30.0},"window":{"from_slot":8,"from_slot_max":16}}]}]}"#;
        let oracles: Oracles = serde_json::from_str(json).unwrap();
        oracles.validate(40).unwrap();
        let again: Oracles =
            serde_json::from_str(&serde_json::to_string(&oracles).unwrap()).unwrap();
        assert_eq!(oracles, again);
        assert!(serde_json::from_str::<Oracles>(r#"{"seed":1,"feeds":[],"x":1}"#).is_err());
        let mut bad = oracles.clone();
        bad.feeds[0].cadence_slots = 0;
        assert!(bad.validate(40).is_err());
        let mut bad = oracles.clone();
        bad.feeds[0].source = Source::Pyth(PythOracle {
            feed_id: "abc".into(),
            shard: 0,
            exponent: -8,
            account: None,
        });
        assert!(bad.validate(40).is_err());
    }

    /// The one live run: a Pyth SOL/USD feed with all four failures, on the
    /// spot snapshot, stepped slot by slot, decoding the account and the
    /// clock. `MTX_LIVE_SNAPSHOT=snapshots/sol-usdc-spot-... cargo test -p
    /// mtx-grader live_pyth -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs the spot snapshot (MTX_LIVE_SNAPSHOT)"]
    async fn live_pyth_feed_with_every_failure() {
        use mtx_kit::protocols::pyth::{PriceUpdate, SOL_USD_FEED_ID};
        use surfpool_sdk::SimnetCommand;

        let dir = std::env::var("MTX_LIVE_SNAPSHOT").expect("MTX_LIVE_SNAPSHOT");
        // A session's surfnet: no clock, a slot only when the test makes one.
        let (surfnet, _) = crate::snapshot::start_snapshot_stepped(std::path::Path::new(&dir))
            .await
            .unwrap();
        let internal = crate::guard::Guard::internal(surfnet.rpc_url())
            .await
            .unwrap();
        let rpc = mtx_kit::rpc_client(internal.url());
        let mut slot = rpc.get_slot().unwrap();
        let sol: String = SOL_USD_FEED_ID.iter().map(|b| format!("{b:02x}")).collect();
        let window = |from_slot, slots| Window {
            from_slot,
            from_slot_max: None,
            slots: Some(slots),
        };
        let oracles = Oracles {
            seed: 3,
            feeds: vec![Feed {
                name: "sol".into(),
                source: Source::Pyth(PythOracle {
                    feed_id: sol,
                    shard: 0,
                    exponent: -8,
                    account: None,
                }),
                cadence_slots: 4,
                path: vec![Level {
                    slot: 80,
                    level_pct: -10.0,
                }],
                conf_bps: 2.0,
                failures: vec![
                    failure_at(Mode::Stale, window(16, 16)),
                    failure_at(Mode::WideConfidence { factor: 50.0 }, window(32, 16)),
                    failure_at(Mode::OffPrice { bias_pct: 5.0 }, window(48, 16)),
                    failure_at(Mode::Delayed { lag_slots: 8 }, window(64, 16)),
                ],
            }],
        };
        oracles.validate(96).unwrap();
        let account = mtx_kit::protocols::pyth::feed_account(0, &SOL_USD_FEED_ID);
        let read = |rpc: &RpcClient| {
            let u = PriceUpdate::decode(&rpc.get_account(&account).unwrap().data).unwrap();
            let clock = rpc
                .get_account(&solana_pubkey::pubkey!(
                    "SysvarC1ock11111111111111111111111111111111"
                ))
                .unwrap()
                .data;
            let unix = i64::from_le_bytes(clock[32..40].try_into().unwrap());
            (u, unix)
        };
        let (before, _) = read(&rpc);
        register(&rpc, &oracles, 96, &World::default()).unwrap();
        let base = rpc.get_slot().unwrap();
        let (_, unix0) = read(&rpc);
        let mut rows = Vec::new();
        for _ in 0..=96 {
            let (u, unix) = read(&rpc);
            rows.push((rows.len() as u64, unix - unix0, u));
            surfnet
                .send_command(SimnetCommand::SlotForward(None))
                .unwrap();
            slot = crate::session::wait_for_slot(&rpc, slot + 1).await.unwrap();
        }
        println!(
            "start price {} publish_time {}",
            before.price_f64(),
            before.publish_time
        );
        println!("rel_slot clock_s price conf publish_time-clock0 posted_slot-base");
        for (at, secs, u) in &rows {
            println!(
                "{at:4} {secs:3} {:9.4} {:7.4} {:5} {:5}",
                u.price_f64(),
                u.confidence_f64(),
                u.publish_time - unix0,
                u.posted_slot as i64 - base as i64
            );
        }
        let at = |n: usize| &rows[n];
        // The clock: 250 ms a slot.
        assert!(
            (at(96).1 - 24).abs() <= 1,
            "clock seconds after 96 slots: {}",
            at(96).1
        );
        // Stale (16..32): the last write before it stays.
        assert_eq!(at(20).2.publish_time, at(31).2.publish_time);
        // Wide confidence (32..48).
        assert!(at(40).2.confidence_f64() > 10.0 * at(8).2.confidence_f64());
        // Off price (48..64): about 5% above the path.
        assert!(at(56).2.price_f64() > at(44).2.price_f64() * 1.03);
        // Publish time never goes down, and the writes land where posted.
        assert!(
            rows.windows(2)
                .all(|w| w[1].2.publish_time >= w[0].2.publish_time)
        );
    }

    fn failure_at(mode: Mode, window: Window) -> Failure {
        Failure { mode, window }
    }

    impl Feed {
        fn source_pyth_id(&self) -> [u8; 32] {
            match &self.source {
                Source::Pyth(p) => p.feed_id_bytes().unwrap(),
                _ => panic!("not a Pyth feed"),
            }
        }
    }
}
