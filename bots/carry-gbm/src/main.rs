//! Reference: trade the perp's premium over its index as a carry desk does,
//! fading the premiums the crowd's positioning opened and following the
//! ones the index follows.
//!
//! Each step it reads Phoenix's index (the oracles) and the book's mid, and
//! the premium between them. An episode starts when the premium reaches
//! 0.2% and ends when it has been under 0.08% for three steps. Within one,
//! it tracks how far the index has moved toward the book since the
//! premium opened, and across the session how many episodes the index
//! followed (it reads an episode as followed when the index moved half the
//! premium, at least 0.3%, its way) and how long it took to start.
//!
//! What fading the premium earns from here to the episode's end, the
//! convergence and Phoenix's funding on it (the premium times the time over
//! the funding period, capped as the market's funding parameters say), comes
//! from a gradient-boosted model of the family, trained on 4,400 sessions of
//! the hidden family (each session draws its index's wobble) on the features
//! above and the wobble it has measured away from episodes: the reference is
//! at least as good as a bot fitted to the generator, which only draws the
//! practice family's one level of wobble. It fades the cap when the model expects more
//! than a threshold, follows the premium with the cap when it expects less
//! than minus that, and keeps its side in between; it's flat outside
//! episodes. It never carries a session past an $800 drawdown.
//!
//! The ladder's rungs run this same bot with `MTX_RUNG` set in their run.sh:
//! `fader` fades every premium at once, `follower` follows every premium at
//! once, `nosession` uses a model trained without what the session has
//! shown so far (how many episodes the index followed, its lag, its
//! wobble), `practice` uses g0's model, fitted on the practice family alone,
//! and `told` reads each episode's kind from `MTX_REGIME`.

use anyhow::{Context, Result};
use mtx_kit::{BotEnv, Side, Trader, Turns};
use serde_json::Value;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

const TH_OPEN: f64 = 0.2;
const TH_CLOSE: f64 = 0.08;
const QUIET_STEPS: u32 = 3;
/// The index has started to follow once it has moved this far toward the
/// book: the larger of an absolute and a share of the premium.
const FOLLOW_ABS: f64 = 0.2;
const FOLLOW_REL: f64 = 0.35;
/// An episode the index followed by half its premium, at least 0.3%.
const FOLLOWED_ABS: f64 = 0.3;
const FOLLOWED_REL: f64 = 0.5;
/// The model's expected fade return, in percent of the price, past which
/// it takes a side.
const EDGE: f64 = 0.3;
const HALT_DRAWDOWN_USD: f64 = 800.0;
const MAX_ORDER: f64 = 35.0;

const MODEL: &str = include_str!("../model.json");
const MODEL_NOSESSION: &str = include_str!("../model-nosession.json");
/// g0's model, fitted on the practice family alone (one level of wobble).
const MODEL_PRACTICE: &str = include_str!("../model-practice.json");

#[derive(Debug, Clone, Copy, PartialEq)]
enum Rung {
    Reference,
    Fader,
    Follower,
    NoSession,
    Practice,
    Told,
}

fn rung() -> Rung {
    match std::env::var("MTX_RUNG").as_deref() {
        Ok("fader") => Rung::Fader,
        Ok("follower") => Rung::Follower,
        Ok("nosession") => Rung::NoSession,
        Ok("practice") => Rung::Practice,
        Ok("told") => Rung::Told,
        _ => Rung::Reference,
    }
}

/// Gradient-boosted trees, as exported from the model's training.
struct Model {
    baseline: f64,
    features: Vec<usize>,
    trees: Vec<Vec<Node>>,
}

struct Node {
    feature: usize,
    threshold: f64,
    left: usize,
    right: usize,
    leaf: bool,
    value: f64,
}

impl Model {
    fn parse(text: &str) -> Result<Self> {
        let json: Value = serde_json::from_str(text)?;
        let num = |v: &Value| v.as_f64().context("a number");
        let features = json["features"]
            .as_array()
            .context("features")?
            .iter()
            .map(|f| f.as_u64().map(|f| f as usize).context("feature"))
            .collect::<Result<_>>()?;
        let mut trees = Vec::new();
        for tree in json["trees"].as_array().context("trees")? {
            let mut nodes = Vec::new();
            for n in tree.as_array().context("tree")? {
                let n = n.as_array().context("node")?;
                nodes.push(Node {
                    feature: num(&n[0])? as usize,
                    threshold: num(&n[1])?,
                    left: num(&n[2])? as usize,
                    right: num(&n[3])? as usize,
                    leaf: num(&n[4])? != 0.0,
                    value: num(&n[5])?,
                });
            }
            trees.push(nodes);
        }
        Ok(Self {
            baseline: num(&json["baseline"])?,
            features,
            trees,
        })
    }

    fn predict(&self, all: &[f64]) -> f64 {
        let x: Vec<f64> = self.features.iter().map(|&i| all[i]).collect();
        let mut sum = self.baseline;
        for tree in &self.trees {
            let mut i = 0;
            loop {
                let node = &tree[i];
                if node.leaf {
                    sum += node.value;
                    break;
                }
                i = if x[node.feature] <= node.threshold {
                    node.left
                } else {
                    node.right
                };
            }
        }
        sum
    }
}

struct Episode {
    t0: f64,
    i0: f64,
    b0: f64,
    side: f64,
    q0: f64,
    dmax: f64,
    lag: Option<f64>,
    k0: usize,
}

/// The causal features: the current episode and what the session has shown.
struct Tracker {
    ep: Option<Episode>,
    followed: u32,
    faded: u32,
    lags: Vec<f64>,
    quiet: u32,
    period: f64,
    cap_pct: f64,
    hist: Vec<(f64, f64, f64)>,
    /// The index's wobble away from episodes: an average of its absolute
    /// change per step, in percent, scaled to a tick's.
    vol: f64,
    last_index: Option<f64>,
}

impl Tracker {
    /// After a step: the features, while in an episode.
    fn update(&mut self, at: f64, index: f64, book: f64, premium: f64) -> Option<Vec<f64>> {
        self.hist.push((index, book, premium));
        if let Some(last) = self.last_index
            && self.ep.is_none()
            && premium.abs() < TH_OPEN
        {
            self.vol = 0.97 * self.vol + 0.03 * (index / last - 1.0).abs() * 100.0 * 2.5;
        }
        self.last_index = Some(index);
        if self.ep.is_none() {
            if premium.abs() < TH_OPEN {
                return None;
            }
            self.ep = Some(Episode {
                t0: at,
                i0: index,
                b0: book,
                side: premium.signum(),
                q0: premium.abs(),
                dmax: 0.0,
                lag: None,
                k0: self.hist.len() - 1,
            });
        }
        let ep = self.ep.as_mut().expect("an episode");
        let s = ep.side;
        let d = s * (index / ep.i0 - 1.0) * 100.0;
        ep.dmax = ep.dmax.max(d);
        if ep.lag.is_none() && d >= FOLLOW_ABS.max(FOLLOW_REL * ep.q0) {
            ep.lag = Some(at - ep.t0);
        }
        if at - ep.t0 < 30.0 {
            ep.q0 = ep.q0.max(premium.abs());
        }
        if premium.abs() < TH_CLOSE {
            self.quiet += 1;
            if self.quiet >= QUIET_STEPS {
                if ep.dmax >= FOLLOWED_ABS.max(FOLLOWED_REL * ep.q0) {
                    self.followed += 1;
                    if let Some(lag) = ep.lag {
                        self.lags.push(lag);
                    }
                } else {
                    self.faded += 1;
                }
                self.ep = None;
                self.quiet = 0;
                return None;
            }
        } else {
            self.quiet = 0;
        }
        let ep = self.ep.as_ref().expect("an episode");
        let tau = at - ep.t0;
        let now = self.hist.len() - 1;
        let back = |n: usize| {
            let j = ep.k0.max(now.saturating_sub(n));
            let (i, b, q) = self.hist[j];
            (
                s * (index / i - 1.0) * 100.0,
                s * (book / b - 1.0) * 100.0,
                s * q,
            )
        };
        let (d5, b5, q5) = back(5);
        let (d20, b20, q20) = back(20);
        let lag = if self.lags.is_empty() {
            -1.0
        } else {
            self.lags.iter().sum::<f64>() / self.lags.len() as f64
        };
        Some(vec![
            tau,
            s * premium,
            ep.q0,
            d,
            ep.dmax,
            d5,
            d20,
            b5,
            b20,
            q5,
            q20,
            s * (book / ep.b0 - 1.0) * 100.0,
            self.followed as f64,
            self.faded as f64,
            lag,
            self.period,
            self.cap_pct,
            at,
            self.vol,
        ])
    }
}

/// What the told rung reads: each episode's kind and timing, in slots from
/// the session's start.
struct Told {
    episodes: Vec<(bool, f64, f64, f64, f64, f64)>,
}

impl Told {
    fn from_env() -> Result<Self> {
        let regime = std::env::var("MTX_REGIME").context("the told rung needs MTX_REGIME")?;
        let shape: Value = serde_json::from_str(&regime)?;
        let episodes = shape["episodes"]
            .as_array()
            .context("episodes")?
            .iter()
            .map(|e| {
                let n = |k: &str| e[k].as_f64().unwrap_or(0.0);
                (
                    e["kind"] == "informed",
                    n("start"),
                    n("open"),
                    n("close"),
                    n("end"),
                    n("premium_pct").signum(),
                )
            })
            .collect();
        Ok(Self { episodes })
    }

    /// Fade a crowded premium from its open to its end; follow an informed
    /// one from its start until the index catches up.
    fn target(&self, at: f64, cap: f64) -> f64 {
        for &(informed, start, open, close, end, side) in &self.episodes {
            if (start..end).contains(&at) {
                return match informed {
                    false if at + 2.0 >= open => -cap * side,
                    false => 0.0,
                    true if at < close => cap * side,
                    true => 0.0,
                };
            }
        }
        0.0
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let rung = rung();
    let market = trader.phoenix.market(&env.symbol)?.clone();
    let hawkeye = trader.phoenix.hawkeye(trader.authority());
    let cap = env.max_position;
    let start_slot = turns.hello().start_slot as f64;
    let model = Model::parse(match rung {
        Rung::NoSession => MODEL_NOSESSION,
        Rung::Practice => MODEL_PRACTICE,
        _ => MODEL,
    })?;
    let told = if rung == Rung::Told {
        Some(Told::from_env()?)
    } else {
        None
    };

    // Funding: the market's interval, period and cap, from the chain, read
    // in the first turn (a read outside a turn races the world).
    let Some(first) = turns.next()? else {
        return Ok(());
    };
    let (period, cap_pct) = {
        use mtx_kit::phoenix_rise::accounts::perp_asset_map::PerpAssetMap;
        let address = trader.phoenix.perp_asset_map()?;
        let account = trader.rpc.get_account(&address)?;
        let map = PerpAssetMap::try_from_account_bytes(&account.data)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let entry = map
            .find_by_symbol(&env.symbol)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .context("the market")?;
        let funding = entry.metadata.funding_accumulator();
        let interval = funding.funding_interval_seconds.as_inner() as f64;
        let period = funding.funding_period_seconds.as_inner() as f64;
        let max_rate = funding.max_funding_rate.as_inner() as f64;
        let bbo = hawkeye.view_bbo(&env.symbol).await?.value;
        let price = trader.phoenix.ticks_to_usd(&market, bbo.index_price_ticks);
        // The cap per interval, in USD per share, as the premium it caps.
        (
            period,
            max_rate / 1_000.0 / price * period / interval * 100.0,
        )
    };
    info!(?rung, period, cap_pct, "funding read");

    let mut tracker = Tracker {
        ep: None,
        followed: 0,
        faded: 0,
        lags: Vec::new(),
        quiet: 0,
        period,
        cap_pct,
        hist: Vec::new(),
        vol: 0.06,
        last_index: None,
    };
    let mut side = 0.0;
    let mut peak_value = f64::MIN;
    let mut halted = false;
    let mut next = Some(first);
    while let Some(step) = next.take().map_or_else(|| turns.next(), |s| Ok(Some(s)))? {
        let at = step.slot as f64 - start_slot;
        let bbo = hawkeye.view_bbo(&env.symbol).await?.value;
        let usd = |t: u64| trader.phoenix.ticks_to_usd(&market, t);
        let index = usd(bbo.index_price_ticks);
        let (Some(bid), Some(ask)) = (bbo.best_bid_ticks(), bbo.best_ask_ticks()) else {
            continue;
        };
        let book = (usd(bid) + usd(ask)) / 2.0;
        let premium = (book / index - 1.0) * 100.0;
        let position = trader.position().await?.size;
        let margin = hawkeye.view_margin(trader.authority(), 0, 0).await?.value;
        let value = (margin.collateral_quote_lots
            + margin.unrealized_pnl_quote_lots
            + margin.unsettled_funding_quote_lots) as f64
            / 1e6;
        peak_value = peak_value.max(value);
        if !halted && peak_value - value > HALT_DRAWDOWN_USD {
            warn!(
                value,
                peak_value, "drawdown guard: flat for the rest of the session"
            );
            halted = true;
        }

        let features = tracker.update(at, index, book, premium);
        let target = if halted {
            0.0
        } else if let Some(told) = &told {
            told.target(at, cap)
        } else {
            match (&features, &tracker.ep) {
                (Some(f), Some(ep)) => {
                    let s = ep.side;
                    match rung {
                        Rung::Fader => -cap * s,
                        Rung::Follower => cap * s,
                        _ => {
                            let edge = model.predict(f);
                            debug!(k = step.k, ?f, edge, "model");
                            if edge > EDGE {
                                side = -1.0;
                            } else if edge < -EDGE {
                                side = 1.0;
                            }
                            side * s * cap
                        }
                    }
                }
                _ => {
                    side = 0.0;
                    0.0
                }
            }
        };

        let mut delta = target - position;
        // Two orders a turn at most: the cap in one turn.
        for _ in 0..2 {
            if delta.abs() < 0.5 {
                break;
            }
            let size = delta.abs().min(MAX_ORDER);
            let (order_side, limit) = if delta > 0.0 {
                (Side::Bid, book * 1.01)
            } else {
                (Side::Ask, book * 0.99)
            };
            match trader.place_market(order_side, size, limit).await {
                Ok(_) => delta -= size * delta.signum(),
                Err(e) => {
                    warn!("order failed: {e}");
                    break;
                }
            }
        }
    }
    Ok(())
}
