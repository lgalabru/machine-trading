//! The basket-unwind family: a desk starts long NVDA and TSLA perps from a
//! client program and must be flat in both by the close, with a cap on the
//! notional it can trade, both markets together, in any minute of slots.
//!
//! - **The prices** walk NVDA's and TSLA's market calibrations jointly
//!   (`draw_joint`, a design correlation of 0.3 to 0.7), inside regular
//!   hours.
//! - **The informed moves.** Each market, on its own draw, is calm, or moves
//!   1.5 to 3 times its calibration's median jump down or up over a ramp of
//!   uneven steps from an onset 15% to 70% into the session. Down and up are
//!   equally likely, so a fixed schedule loses in one of them.
//! - **The flow.** Takers trade from one pool of accounts on both markets,
//!   at a rate drawn per market, either side at random. In each market one
//!   account is that market's informed trader, seated in every session: it
//!   trades noise like any account, and ahead of its market's move it sends
//!   a burst the move's way; from the onset the noise leans that way.
//!   Fake-out bursts, drawn exactly like informed ones, come from other
//!   accounts in every regime and have nothing after them.
//! - **The book.** On each market the snapshot's makers are widened to a
//!   backstop 15 to 30 bp out; near the touch the depth is the grader's
//!   maker alone, a few units a side, requoted every few slots.
//! - **The desk.** It starts long `Q` in each market (`start_positions`),
//!   may only sell (`sides`, a floor of 0), and is scored over holding them
//!   (`net`): a broker takes whatever is left at the close less a haircut
//!   per market (`close_out`). Its traded notional, both markets together,
//!   is capped over any 60 slots (`traded_cap`), at 10% to 20% of the
//!   basket's starting notional: unwinding both names takes 300 to 600
//!   slots, so the capacity spent on one name is capacity the other waits
//!   for.
//!
//! No tells: every regime runs the same machinery (the same actors, the same
//! bursts and fake-outs, the same order draws); only the moves and the lean
//! differ.

use std::collections::BTreeMap;

use mtx_core::{Side, mandate::Mandate};
use serde::Serialize;

use crate::{
    making::{Competition, Making, Reactive, TAKER_ACCOUNTS, TakerOrder, TakerSide},
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{ModelPath, Start, draw_joint},
    },
    orders::{CloseOut, NetScoring, TradedCap},
    scenario::{JointDraw, MarketScenario, Scenario, SplitMix64, VENUE},
};

/// Mixed into a session's seed for the stream its accounts are drawn from.
const ACCOUNTS_STREAM: u64 = 0x5eed_acc0_00b5;

const NVDA_CALIBRATION: &str =
    include_str!("../../../calibration/market/nvda.20261113.903d1120.toml");
const TSLA_CALIBRATION: &str =
    include_str!("../../../calibration/market/tsla.20261113.8b7bca8e.toml");

/// What one market of the basket is made of.
#[derive(Debug, Clone)]
pub struct MarketSpec {
    pub symbol: &'static str,
    calibration: &'static str,
    /// The snapshot's mark, where the session starts.
    pub arrival_usd: f64,
    /// The starting position, in base units.
    pub quantity: (u64, u64),
    /// The grader's maker's quote, and the takers' sizes: lognormal around a
    /// median, clamped, in base units.
    pub maker_size: (f64, f64),
    pub size_median: f64,
    pub size_clamp: (f64, f64),
}

impl MarketSpec {
    fn model(&self) -> MarketModel {
        let mut calibration = Calibration::parse(self.calibration).expect("the calibration is valid");
        calibration.sha256 = sha256_hex(self.calibration.as_bytes());
        MarketModel::new(
            calibration,
            ModelSpec {
                start: Start::Session {
                    kind: "regular".into(),
                },
                ..ModelSpec::default()
            },
        )
        .expect("the model's spec is valid")
    }
}

/// What a session is, beneath its paths and flow, per market by symbol.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// `calm`, `down` or `up`.
    pub regimes: BTreeMap<String, String>,
    /// The move's onset in slots and its size in percent, signed.
    pub onsets: BTreeMap<String, u64>,
    pub moves_pct: BTreeMap<String, f64>,
    /// The first slot of each burst, informed first, and whether it is.
    pub bursts: BTreeMap<String, Vec<(u64, bool)>>,
    /// The taker account each market's informed trader sends from.
    pub informed_accounts: BTreeMap<String, u8>,
    pub rho: f64,
    pub cap_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    Calm,
    Down,
    Up,
}

impl Regime {
    fn name(self) -> &'static str {
        match self {
            Self::Calm => "calm",
            Self::Down => "down",
            Self::Up => "up",
        }
    }

    fn sign(self) -> f64 {
        match self {
            Self::Calm => 0.0,
            Self::Down => -1.0,
            Self::Up => 1.0,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct BasketFamily {
    pub markets: Vec<MarketSpec>,
    pub duration: (u64, u64),
    /// The design correlation of the markets' walks.
    pub rho: (f64, f64),
    /// Odds of a calm market; down and up share the rest.
    pub calm_odds: f64,
    /// The move, as a multiple of the market's median jump; its onset, as a
    /// share of the session, and its length in slots.
    pub move_mult: (f64, f64),
    pub onset_share: (f64, f64),
    pub ramp_slots: (u64, u64),
    pub waypoint_slots: u64,
    /// Noise orders per slot, the sizes' spread, and taker limits as
    /// multiples of 1.5 times the maker's half-spread.
    pub noise_rate: (f64, f64),
    pub size_sigma: f64,
    pub limit_sigma: f64,
    pub limit_clamp: (f64, f64),
    /// Orders in a burst, how far ahead of its slot they start, and fake-out
    /// bursts per market and where they fall.
    pub burst_orders: (u64, u64),
    pub burst_lead: (u64, u64),
    pub fakeouts: (u64, u64),
    pub fakeout_share: (f64, f64),
    /// How far the noise leans the move's way from its onset.
    pub lean: (f64, f64),
    /// The grader's maker: half-spread, floor, look delay; the backstop.
    pub maker_bps: (f64, f64),
    pub floor_bps: (f64, f64),
    pub react_slots: (u64, u64),
    pub backstop_bps: (f64, f64),
    /// The broker's haircut on what is left, per market.
    pub haircut_bps: (f64, f64),
    /// The traded cap: its window, and its size as a share of the basket's
    /// starting notional.
    pub cap_window_slots: u64,
    pub cap_share: (f64, f64),
    /// The mandate's loss limit, as a share of the basket's notional: out of
    /// reach of the family's moves.
    pub drawdown_share: f64,
}

impl BasketFamily {
    pub fn nvda_tsla() -> Self {
        Self {
            markets: vec![
                MarketSpec {
                    symbol: "NVDA",
                    calibration: NVDA_CALIBRATION,
                    arrival_usd: 159.92,
                    quantity: (20, 40),
                    maker_size: (2.0, 5.0),
                    size_median: 0.15,
                    size_clamp: (0.03, 2.0),
                },
                MarketSpec {
                    symbol: "TSLA",
                    calibration: TSLA_CALIBRATION,
                    arrival_usd: 388.95,
                    quantity: (8, 16),
                    maker_size: (0.8, 2.0),
                    size_median: 0.06,
                    size_clamp: (0.012, 0.8),
                },
            ],
            duration: (1_600, 2_400),
            rho: (0.3, 0.7),
            calm_odds: 0.3,
            move_mult: (1.5, 3.0),
            onset_share: (0.15, 0.70),
            ramp_slots: (40, 240),
            waypoint_slots: 4,
            noise_rate: (0.06, 0.15),
            size_sigma: 0.7,
            limit_sigma: 0.5,
            limit_clamp: (0.5, 6.0),
            burst_orders: (6, 14),
            burst_lead: (20, 100),
            fakeouts: (0, 2),
            fakeout_share: (0.15, 0.85),
            lean: (0.10, 0.25),
            maker_bps: (3.0, 6.0),
            floor_bps: (1.0, 2.0),
            react_slots: (2, 12),
            backstop_bps: (15.0, 30.0),
            haircut_bps: (5.0, 20.0),
            cap_window_slots: 60,
            cap_share: (0.10, 0.20),
            drawdown_share: 0.05,
        }
    }

    fn regime_of(&self, draw: f64) -> Regime {
        if draw < self.calm_odds {
            Regime::Calm
        } else if draw < self.calm_odds + (1.0 - self.calm_odds) / 2.0 {
            Regime::Down
        } else {
            Regime::Up
        }
    }

    /// Each market's regime a draw seed gives, in the markets' order: read
    /// first, so a set can stratify.
    pub fn regimes(&self, seed: u64) -> Vec<Regime> {
        let mut rng = SplitMix64(seed);
        self.markets
            .iter()
            .map(|_| self.regime_of(rng.next_f64()))
            .collect()
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        let models: Vec<MarketModel> = self.markets.iter().map(MarketSpec::model).collect();
        self.draw_with(&models, name, seed)
    }

    fn draw_with(&self, models: &[MarketModel], name: &str, seed: u64) -> (Scenario, Shape) {
        let regimes = self.regimes(seed);
        let mut rng = SplitMix64(seed);
        for _ in &self.markets {
            rng.next_f64();
        }
        let duration = rng.range_u64(self.duration);
        let rho = round2(rng.range_f64(self.rho));
        let matrix = vec![vec![1.0, rho], vec![rho, 1.0]];
        let refs: Vec<&MarketModel> = models.iter().collect();
        let mut paths = draw_joint(&refs, &matrix, rng.next_u64(), duration)
            .expect("two calibrations on one block length, a valid matrix");

        // Who trades for whom: one informed account per market, never the
        // same, from a stream of their own.
        let mut seats = SplitMix64(seed ^ ACCOUNTS_STREAM);
        let accounts = u64::from(TAKER_ACCOUNTS);
        let first = seats.next_u64() % accounts;
        let second = (first + 1 + seats.next_u64() % (accounts - 1)) % accounts;
        let informed_accounts = [first as u8, second as u8];

        let mut shape = Shape {
            regimes: BTreeMap::new(),
            onsets: BTreeMap::new(),
            moves_pct: BTreeMap::new(),
            bursts: BTreeMap::new(),
            informed_accounts: BTreeMap::new(),
            rho,
            cap_usd: 0.0,
        };
        let mut markets = Vec::new();
        let mut mandate: Option<Mandate> = None;
        let mut start_positions = BTreeMap::new();
        let mut close_out = BTreeMap::new();
        let mut notional = 0.0;
        let mut records = BTreeMap::new();
        for (i, spec) in self.markets.iter().enumerate() {
            let regime = regimes[i];
            let path = &mut paths[i];
            let median_jump_pct = models[i].calibration.fair_value.jumps.abs_size_median_pct;
            // The move: drawn in every regime, laid only in an informed one.
            let size_pct = rng.range_f64(self.move_mult) * median_jump_pct;
            let onset = (rng.range_f64(self.onset_share) * duration as f64) as u64;
            let ramp = rng.range_u64(self.ramp_slots);
            let steps = uneven_ramp(&mut rng, 8);
            let sign = regime.sign();
            if sign != 0.0 {
                lay_move(path, onset, ramp, sign * size_pct, &steps);
            }

            let maker_bps = round2(rng.range_f64(self.maker_bps));
            let floor_bps = round2(rng.range_f64(self.floor_bps).min(0.5 * maker_bps));
            let maker_size = round3(rng.range_f64(spec.maker_size));
            let react_slots = rng.range_u64(self.react_slots);
            let backstop_bps = round2(rng.range_f64(self.backstop_bps));

            let rate = rng.range_f64(self.noise_rate);
            let lean = rng.range_f64(self.lean);
            let mut takers = Vec::new();
            for slot in 0..duration {
                if rng.next_f64() >= rate {
                    continue;
                }
                let tilt = if slot >= onset { sign * lean } else { 0.0 };
                let side = if rng.next_f64() < 0.5 + tilt {
                    TakerSide::Buy
                } else {
                    TakerSide::Sell
                };
                takers.push(self.order(spec, &mut rng, slot, side, maker_bps));
            }
            let mut bursts: Vec<(u64, f64, bool)> = Vec::new();
            if sign != 0.0 {
                bursts.push((onset, sign, true));
            }
            for _ in 0..rng.range_u64(self.fakeouts) {
                let at = (rng.range_f64(self.fakeout_share) * duration as f64) as u64;
                let side = if rng.next_f64() < 0.5 { 1.0 } else { -1.0 };
                bursts.push((at, side, false));
            }
            let informed_account = informed_accounts[i];
            let mut burst_starts = Vec::new();
            for &(at, side, informed) in &bursts {
                let account = if informed {
                    informed_account
                } else {
                    // Any account but the two informed traders'.
                    let others: Vec<u8> = (0..TAKER_ACCOUNTS)
                        .filter(|a| !informed_accounts.contains(a))
                        .collect();
                    others[(seats.next_u64() % others.len() as u64) as usize]
                };
                let lead = rng.range_u64(self.burst_lead).min(at.max(1));
                let mut first = at;
                for _ in 0..rng.range_u64(self.burst_orders) {
                    let slot = at - rng.range_u64((1, lead.max(1))).min(at);
                    first = first.min(slot);
                    let side = if side > 0.0 {
                        TakerSide::Buy
                    } else {
                        TakerSide::Sell
                    };
                    let order = self.order(spec, &mut rng, slot, side, maker_bps);
                    takers.push(TakerOrder { account, ..order });
                }
                burst_starts.push((first, informed));
            }
            // Stable: orders in one slot keep the order they were drawn in.
            takers.sort_by_key(|o| o.slot);

            let quantity = rng.range_u64(spec.quantity) as f64;
            let haircut_bps = round2(rng.range_f64(self.haircut_bps));
            notional += quantity * spec.arrival_usd;
            let mut allowance = Mandate::perp(VENUE, spec.symbol, quantity, 1.0);
            allowance.universe[0].min_position = Some(0.0);
            allowance.universe[0].sides = Some(vec![Side::Sell]);
            match mandate.as_mut() {
                None => mandate = Some(allowance),
                Some(m) => m.universe.extend(allowance.universe),
            }
            start_positions.insert(spec.symbol.to_string(), quantity);
            close_out.insert(spec.symbol.to_string(), CloseOut { haircut_bps });

            let symbol = spec.symbol.to_string();
            shape.regimes.insert(symbol.clone(), regime.name().into());
            shape.onsets.insert(symbol.clone(), onset);
            shape
                .moves_pct
                .insert(symbol.clone(), round3(sign * size_pct));
            shape.bursts.insert(symbol.clone(), burst_starts);
            shape
                .informed_accounts
                .insert(symbol.clone(), informed_account);
            records.insert(symbol.clone(), path.record.clone());
            markets.push(MarketScenario {
                symbol: symbol.clone(),
                path: path.waypoints(self.waypoint_slots),
                fair_path: None,
                funding: None,
                making: Some(Making {
                    competitor_bps: maker_bps,
                    competition: Competition {
                        floor_bps,
                        react_slots,
                        size: maker_size,
                        reactive: Some(Reactive::default()),
                    },
                    takers,
                    makers_bps: Some(backstop_bps),
                    roles: [(informed_kind(spec.symbol), vec![informed_account])].into(),
                }),
                tape: Vec::new(),
                takers_pricing: None,
                market_model: None,
            });
        }
        let mut mandate = mandate.expect("the basket has markets");
        // A loss limit out of reach of any move the family makes: a holding's
        // value moves with the market whatever the desk does.
        mandate.limits.max_drawdown_usd = Some((notional * self.drawdown_share).round());
        let cap_usd = round2(rng.range_f64(self.cap_share) * notional);
        shape.cap_usd = cap_usd;
        markets.sort_by(|a, b| a.symbol.cmp(&b.symbol));

        let positions: Vec<String> = self
            .markets
            .iter()
            .map(|m| format!("{} {}", start_positions[m.symbol], m.symbol))
            .collect();
        let haircuts: Vec<String> = self
            .markets
            .iter()
            .map(|m| format!("{} {} bp", m.symbol, close_out[m.symbol].haircut_bps))
            .collect();
        let description = format!(
            "unwind {} by the close, trading at most {cap_usd} USD in any {} slots; the broker takes the rest at {}",
            positions.join(" and "),
            self.cap_window_slots,
            haircuts.join(", ")
        );
        let primary = self.markets[0].symbol.to_string();
        let scenario = Scenario {
            name: name.to_string(),
            description,
            symbol: primary,
            duration_slots: duration,
            mandate,
            path: Vec::new(),
            making: None,
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            actors: None,
            takers_pricing: None,
            turn_data: None,
            order: None,
            start_positions,
            close_out,
            participation: None,
            net: Some(NetScoring {
                inventory_bps: 0.0,
                venue_position: None,
            }),
            oracles: None,
            lending: None,
            warps: Vec::new(),
            reference_pnl_usd: None,
            informed_pnl_usd: None,
            reference_max_drawdown_usd: None,
            informed_max_drawdown_usd: None,
            fair_path: None,
            bound_on: Default::default(),
            market_model: None,
            raw_sends: None,
            markets,
            joint: Some(JointDraw {
                matrix_source: "design".into(),
                matrix_sha256: None,
                assets: self.markets.iter().map(|m| m.symbol.to_string()).collect(),
                matrix,
                records,
            }),
            tx_layer: None,
            stop_out: None,
            liquidator: None,
            traded_cap: Some(TradedCap {
                window_slots: self.cap_window_slots,
                max_usd: cap_usd,
            }),
        };
        (scenario, shape)
    }

    /// A taker order: size, limit and account drawn alike for noise, bursts
    /// and fake-outs.
    fn order(
        &self,
        spec: &MarketSpec,
        rng: &mut SplitMix64,
        slot: u64,
        side: TakerSide,
        maker_bps: f64,
    ) -> TakerOrder {
        let (lo, hi) = spec.size_clamp;
        let size = (spec.size_median * (self.size_sigma * rng.normal()).exp()).clamp(lo, hi);
        let (llo, lhi) = self.limit_clamp;
        let limit = 1.5 * maker_bps * (self.limit_sigma * rng.normal()).exp().clamp(llo, lhi);
        TakerOrder {
            slot,
            side,
            size: round3(size),
            limit_bps: round2(limit),
            account: rng.range_u64((0, TAKER_ACCOUNTS as u64 - 1)) as u8,
        }
    }

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// `count` sessions named `<prefix>-<n>`, each pair of regimes in
    /// proportion to its odds (largest remainder), down and up mirrored.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let models: Vec<MarketModel> = self.markets.iter().map(MarketSpec::model).collect();
        let odds = [
            self.calm_odds,
            (1.0 - self.calm_odds) / 2.0,
            (1.0 - self.calm_odds) / 2.0,
        ];
        // Two markets: nine cells.
        let want: Vec<f64> = (0..9)
            .map(|c| odds[c / 3] * odds[c % 3] * count as f64)
            .collect();
        let mut quotas: Vec<usize> = want.iter().map(|w| w.floor() as usize).collect();
        let mut order: Vec<usize> = (0..9).collect();
        order.sort_by(|&a, &b| {
            let ra = want[a] - want[a].floor();
            let rb = want[b] - want[b].floor();
            rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(&b))
        });
        let mut short = count - quotas.iter().sum::<usize>();
        for c in order {
            if short == 0 {
                break;
            }
            quotas[c] += 1;
            short -= 1;
        }
        let mut out = Vec::with_capacity(count);
        let mut draw = 0u64;
        while out.len() < count {
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let r = self.regimes(draw_seed);
            let cell = r[0].index() * 3 + r[1].index();
            if quotas[cell] == 0 {
                continue;
            }
            quotas[cell] -= 1;
            let name = format!("{prefix}-{}", out.len() + 1);
            out.push(self.draw_with(&models, &name, draw_seed));
        }
        out
    }
}

/// The roster kind of a market's informed trader: `informed-nvda`.
pub fn informed_kind(symbol: &str) -> String {
    format!("informed-{}", symbol.to_lowercase())
}

/// `n` cumulative shares rising from 0 to 1 in uneven steps.
fn uneven_ramp(rng: &mut SplitMix64, n: usize) -> Vec<f64> {
    let mut steps: Vec<f64> = (0..n).map(|_| 0.2 + rng.next_f64()).collect();
    let total: f64 = steps.iter().sum();
    let mut acc = 0.0;
    for step in &mut steps {
        acc += *step / total;
        *step = acc;
    }
    steps
}

/// Lays a move of `size_pct` on the path from `onset` over `ramp` slots, in
/// the ramp's uneven steps, and holds it after.
fn lay_move(path: &mut ModelPath, onset: u64, ramp: u64, size_pct: f64, steps: &[f64]) {
    let lift = (1.0 + size_pct / 100.0).ln();
    let n = steps.len() as u64;
    for (slot, level) in path.log_levels.iter_mut().enumerate() {
        let slot = slot as u64;
        if slot < onset {
            continue;
        }
        let done = ((slot - onset) * n / ramp.max(1)).min(n);
        let share = if done == 0 {
            0.0
        } else {
            steps[done as usize - 1]
        };
        *level += lift * share;
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_valid_and_deterministic() {
        let family = BasketFamily::nvda_tsla();
        for seed in 0..30 {
            let (scenario, shape) = family.draw("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}");
            assert_eq!(scenario, family.sample("s", seed));
            assert_eq!(scenario.markets.len(), 2);
            let cap = scenario.traded_cap.unwrap();
            let notional: f64 = family
                .markets
                .iter()
                .map(|m| scenario.start_positions[m.symbol] * m.arrival_usd)
                .sum();
            assert!(cap.max_usd >= 0.099 * notional && cap.max_usd <= 0.201 * notional);
            let accounts: Vec<u8> = shape.informed_accounts.values().copied().collect();
            assert_ne!(accounts[0], accounts[1]);
            let v2 = crate::scenario_v2::ScenarioV2::try_from(&scenario).unwrap();
            assert_eq!(v2.into_scenario().unwrap(), scenario);
        }
    }

    /// Sets are stratified by the pair of regimes, and every regime runs the
    /// same machinery.
    #[test]
    fn sets_are_stratified() {
        let family = BasketFamily::nvda_tsla();
        let set = family.draw_set("h", 72, 3);
        let count = |a: &str, b: &str| {
            set.iter()
                .filter(|(_, s)| s.regimes["NVDA"] == a && s.regimes["TSLA"] == b)
                .count()
        };
        assert_eq!(count("down", "down"), 9);
        assert_eq!(count("calm", "calm"), 6);
        assert_eq!(set.len(), 72);
        let informed: usize = set
            .iter()
            .flat_map(|(_, s)| s.bursts.values())
            .flatten()
            .filter(|(_, i)| *i)
            .count();
        let moving: usize = set
            .iter()
            .flat_map(|(_, s)| s.regimes.values())
            .filter(|r| *r != "calm")
            .count();
        assert_eq!(informed, moving);
    }
}
