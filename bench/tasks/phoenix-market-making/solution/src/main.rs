//! Reference: quote one tick inside the competing makers, skew against
//! inventory, and trade with the flow when it turns toxic.
//!
//! The competitors' half-spread is read once, from their spline account; the
//! quotes sit that far less one tick either side of a reservation price:
//! the oracle, shifted against what the bot holds, so a long inventory lowers
//! both quotes and sheds itself (Avellaneda-Stoikov's idea, with a linear
//! skew). Holding a quarter of the cap, it stops quoting the side that would
//! add to it. Fills that pile up one way in a few slots look like informed
//! flow ahead of a jump: it buys (or sells) a few units the burst's way, pulls
//! the quote the burst was hitting for a while, and lets the skew unwind the
//! position after. Only fills that grow the inventory count toward a burst:
//! the skew unwinding a lean is a run of one-sided fills too, and a maker
//! that took it for one would lean back and forth on its own. And it leans
//! with the drift: the further the oracle runs from its recent average, the
//! further the reservation price follows, so it sells into a falling market
//! rather than buying all the way down.
//!
//! It prices off the oracle, not Phoenix's mark: the mark blends in the
//! book, and the bot's own best bid or ask pulls it along, so a maker that
//! quotes around the mark chases its own quotes.
//!
//! Its PnL is a published baseline, not the reward's scale (that's the
//! spread bound plus the hindsight bound). It reacts to its own fills only:
//! it doesn't read the tape, doesn't tell a fake-out from a real burst, and
//! leans the same few units whatever the burst's size.

use std::collections::VecDeque;

use anyhow::Result;
use mth_botkit::{BotEnv, Side, Trader, Turns};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// How far the reservation price moves per base unit held, in basis points.
const SKEW_BPS_PER_UNIT: f64 = 2.5;
/// How much of the oracle's distance from its recent average the
/// reservation price follows, how many slots that average spans, and the
/// most it moves the quotes, in basis points: noise shouldn't pull a quote
/// through the oracle.
const DRIFT_WEIGHT: f64 = 0.5;
const DRIFT_SLOTS: f64 = 80.0;
const DRIFT_MAX_BPS: f64 = 5.0;
/// Size of each quote, in base units.
const QUOTE_SIZE: f64 = 1.0;
/// The share of the position cap past which the side that would add to the
/// inventory stops quoting.
const INVENTORY_LIMIT: f64 = 0.25;
/// Fills of this many base units one way within the window look toxic.
const TOXIC_UNITS: f64 = 1.5;
const TOXIC_WINDOW_SLOTS: u64 = 8;
/// Units to hold with a toxic burst's direction, until the skew unwinds them,
/// and how long the side the burst was hitting stays pulled.
const LEAN_UNITS: f64 = 5.0;
const TOXIC_SLOTS: u64 = 24;
/// Requote when the quotes would move by at least this many basis points,
/// or after this many slots regardless.
const REQUOTE_BPS: f64 = 1.5;
const REQUOTE_SLOTS: u64 = 12;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    // Answer the grader's hello first: the world waits for it.
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let mandate = env.mandate()?;
    let cap = mandate.universe[0].max_position;
    let trader = Trader::connect(&env)?;
    let half_spread_bps = competitor_half_spread_bps(&trader).await?;
    info!(half_spread_bps, "one tick inside the competitor");
    info!(cap, "quoting");

    // (slot, position, position from its quotes' fills) at each pass, for
    // the toxicity window. Its own market orders aren't flow: `own` is what
    // they've added to the position.
    let mut history: VecDeque<(u64, f64, f64)> = VecDeque::new();
    let mut own = 0.0;
    let mut toxic_until = 0u64;
    // Which way the toxic fills took the inventory: +1 long, -1 short.
    let mut toxic_side = 0.0;
    let mut quoted: Option<(u64, f64, f64)> = None;
    let mut requotes = 0u64;
    // The oracle's recent average, and the slot it was last updated at.
    let mut average: Option<(u64, f64)> = None;
    while let Some(step) = turns.next()? {
        let slot = step.slot;
        let (Ok(mark), Ok(position)) = (trader.oracle_usd().await, trader.position().await) else {
            continue;
        };
        let held = position.size;
        let filled = held - own;
        history.push_back((slot, held, filled));
        while history
            .front()
            .is_some_and(|(s, ..)| s + TOXIC_WINDOW_SLOTS < slot)
        {
            history.pop_front();
        }
        // A run of fills one way that grows the inventory: fills that shrink
        // it are the skew unwinding a lean, not a burst.
        if let Some(&(_, held_then, then)) = history.front()
            && (filled - then).abs() >= TOXIC_UNITS
            && held.abs() > held_then.abs()
            && toxic_until <= slot
        {
            warn!(slot, held, then, "one-sided fills; backing off");
            toxic_until = slot + TOXIC_SLOTS;
            toxic_side = (filled - then).signum();
            // The burst is the informed flow's footprint, and a jump its way
            // usually follows: lean with it, a few units against the side it
            // was hitting, and let the skew unwind them after.
            let lean = -toxic_side * LEAN_UNITS - held;
            if lean.abs() >= 0.5 {
                let (side, limit) = if lean > 0.0 {
                    (Side::Bid, mark * 1.003)
                } else {
                    (Side::Ask, mark * 0.997)
                };
                let _ = trader.cancel_all();
                if let Err(e) = trader
                    .place_market(side, lean.abs().min(cap), (limit * 100.0).round() / 100.0)
                    .await
                {
                    warn!(%e, "lean failed");
                }
                if let Ok(after) = trader.position().await {
                    own += after.size - held;
                }
                continue;
            }
        }
        let toxic = slot < toxic_until;

        let mean = match average {
            Some((at, mean)) => {
                let weight = 1.0 - (-((slot - at) as f64) / DRIFT_SLOTS).exp();
                mean + weight * (mark - mean)
            }
            None => mark,
        };
        average = Some((slot, mean));
        let drift_bps =
            (DRIFT_WEIGHT * (mark / mean - 1.0) * 10_000.0).clamp(-DRIFT_MAX_BPS, DRIFT_MAX_BPS);
        let reservation = mark * (1.0 + (drift_bps - SKEW_BPS_PER_UNIT * held) / 10_000.0);
        let half = half_spread_bps;
        let bid = reservation * (1.0 - half / 10_000.0);
        let ask = reservation * (1.0 + half / 10_000.0);
        let moved = quoted.is_none_or(|(at, b, a)| {
            slot >= at + REQUOTE_SLOTS
                || ((bid - b) / mark).abs() * 10_000.0 >= REQUOTE_BPS
                || ((ask - a) / mark).abs() * 10_000.0 >= REQUOTE_BPS
        });
        if !moved {
            continue;
        }

        let size = QUOTE_SIZE;
        if let Err(e) = trader.cancel_all() {
            warn!(%e, "cancel failed");
            continue;
        }
        // Never let a fill take the position past the cap.
        let bid_size = size.min(cap - held);
        let ask_size = size.min(cap + held);
        let mut quotes = [
            // While the flow looks toxic, the side it was hitting goes:
            // short from a run of buyers, it stops offering and only bids.
            (
                Side::Bid,
                bid,
                bid_size,
                held < INVENTORY_LIMIT * cap && !(toxic && toxic_side > 0.0),
            ),
            (
                Side::Ask,
                ask,
                ask_size,
                held > -INVENTORY_LIMIT * cap && !(toxic && toxic_side < 0.0),
            ),
        ];
        // Both quotes land within the turn, before the next step's flow;
        // alternating which goes first costs nothing and keeps neither side
        // systematically ahead if they ever don't.
        if requotes % 2 == 1 {
            quotes.reverse();
        }
        requotes += 1;
        for (side, price, size, wanted) in quotes {
            if wanted
                && size >= 0.01
                && let Err(e) = trader.place_limit(side, price, size).await
            {
                warn!(%e, ?side, "quote failed");
            }
        }
        quoted = Some((slot, bid, ask));
    }
    let position = trader.position().await?;
    info!(
        size = position.size,
        oracle = trader.oracle_usd().await?,
        mark = trader.mark_usd().await?,
        "done; inventory valued at the oracle"
    );
    Ok(())
}

/// The competing makers' half-spread less one tick, in basis points of the
/// oracle: the closest any enabled spline quotes to its mid.
async fn competitor_half_spread_bps(trader: &Trader) -> Result<f64> {
    use mth_botkit::phoenix_rise::accounts::spline_collection::SplineCollection;
    let market = trader.phoenix.market(&trader.env.symbol)?;
    let address: mth_botkit::Pubkey = market.spline_pubkey.parse()?;
    let account = trader.rpc.get_account(&address)?;
    let collection = SplineCollection::try_from_account_bytes(&account.data)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let inner = collection
        .splines()
        .filter_map(|s| s.ok())
        .filter(|s| s.is_enabled())
        .flat_map(|s| {
            s.active_bid_regions()
                .iter()
                .chain(s.active_ask_regions())
                .map(|r| r.start_offset().as_inner())
                .collect::<Vec<_>>()
        })
        .min()
        .ok_or_else(|| anyhow::anyhow!("no competing quotes"))?;
    let tick_usd = trader.phoenix.ticks_to_usd(market, 1);
    let oracle = trader.oracle_usd().await?;
    Ok(((inner.saturating_sub(1)) as f64 * tick_usd / oracle * 10_000.0).max(1.0))
}
