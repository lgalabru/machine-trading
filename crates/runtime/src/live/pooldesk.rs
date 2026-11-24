//! What a strategy on pools is shown each step (`AddStrategy::pools`): the
//! bench's atomic arbitrage bots, hosted on the chain in shadow.
//!
//! A session measures its bot's wallet and tells it of each large swap as
//! a trigger. Here nothing settles and nobody lands swaps for the bot, so:
//!
//! - the wallet is on paper ([`PoolsSpec::paper_base`]): a strategy sizes
//!   its routes on what it holds, and a shadow key holds nothing;
//! - each pool is read from the node every step, for its price each way
//!   and the paper wallet's value;
//! - a pool whose price moved [`PoolsSpec::trigger_bps`] since the last
//!   step is the step's trigger, as a session's large swap is. Its size
//!   isn't known, only its side.
//!
//! Its routes go nowhere: the shadow chain writes each transaction down
//! (`live::shadow`).

use std::sync::Arc;

use mtx_core::{
    Side,
    control::PoolsSpec,
    mandate::InstrumentRef,
    session::{VenueReading, Wallet},
    turns::Trigger,
};
use mtx_kit::{chain::ChainClient, protocols::USDC_MINT};
use solana_pubkey::Pubkey;

use crate::venue::amm;

/// Pools one strategy trades at most: the bench's reference bots list 12,
/// and every one is read from the node each step.
pub const POOLS_MAX: usize = 12;

const WRAPPED_SOL_MINT: Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");

/// Whether `spec` can be hosted: pools to trade, something to trade with,
/// a trigger that is a move.
pub fn validate(spec: &PoolsSpec) -> Result<(), String> {
    if !(2..=POOLS_MAX).contains(&spec.venues.len()) {
        return Err(format!(
            "{} pools: a route needs two, and {POOLS_MAX} are read a step at most",
            spec.venues.len()
        ));
    }
    let mut seen: Vec<&InstrumentRef> = spec.venues.iter().collect();
    seen.sort();
    if seen.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("a pool named twice".into());
    }
    if !(spec.paper_base.is_finite() && spec.paper_base > 0.0) {
        return Err(format!("paper_base {}: above zero", spec.paper_base));
    }
    if !(spec.trigger_bps.is_finite() && spec.trigger_bps > 0.0) {
        return Err(format!("trigger_bps {}: above zero", spec.trigger_bps));
    }
    Ok(())
}

/// `venues` read once from `chain` for what never changes about each.
pub async fn pools(
    chain: &dyn ChainClient,
    venues: &[InstrumentRef],
) -> Result<Vec<amm::Pool>, String> {
    assert!(
        venues.len() <= POOLS_MAX,
        "validated: the pools are bounded"
    );
    let mut pools = Vec::with_capacity(venues.len());
    for instrument in venues {
        pools.push(
            amm::Pool::read(chain, instrument)
                .await
                .map_err(|e| e.to_string())?,
        );
    }
    Ok(pools)
}

/// The venue metadata a pool strategy's `start` is handed, as a session's:
/// each pool's id, address, program and mints.
pub fn metadata(pools: &[amm::Pool]) -> String {
    let listed: Vec<serde_json::Value> = pools
        .iter()
        .map(|pool| {
            serde_json::json!({
                "id": pool.instrument.to_string(),
                "address": pool.address.to_string(),
                "program": pool.program.id().to_string(),
                "base_mint": pool.base.mint.to_string(),
                "quote_mint": pool.quote.mint.to_string(),
            })
        })
        .collect();
    serde_json::json!({ "pools": listed }).to_string()
}

/// One pool as a step shows it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Top {
    /// Quote per base, before fees.
    pub mid: f64,
    /// A small swap's price each way, fees included.
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
}

/// SOL's price in dollars from `tops`: the first pool of SOL against USDC,
/// either way round. `None` if no pool is one.
pub fn sol_usd(pools: &[amm::Pool], tops: &[Top]) -> Option<f64> {
    assert_eq!(pools.len(), tops.len(), "a top a pool");
    pools.iter().zip(tops).find_map(|(pool, top)| {
        let (base, quote) = (pool.base.mint, pool.quote.mint);
        if (base, quote) == (WRAPPED_SOL_MINT, USDC_MINT) {
            Some(top.mid)
        } else if (base, quote) == (USDC_MINT, WRAPPED_SOL_MINT) && top.mid > 0.0 {
            Some(1.0 / top.mid)
        } else {
            None
        }
    })
}

/// The paper wallet at `tops`: `paper_base` of SOL, valued at `price_usd`,
/// and each pool's prices.
pub fn wallet(pools: &[amm::Pool], tops: &[Top], paper_base: f64, price_usd: f64) -> Wallet {
    assert_eq!(pools.len(), tops.len(), "a top a pool");
    Wallet {
        base: paper_base,
        quote: 0.0,
        price_usd,
        value_usd: paper_base * price_usd,
        venues: pools
            .iter()
            .zip(tops)
            .map(|(pool, top)| VenueReading {
                id: pool.instrument.to_string(),
                best_bid: top.best_bid,
                best_ask: top.best_ask,
                ..VenueReading::default()
            })
            .collect(),
    }
}

/// The pool that moved the most since `before`, if it moved `trigger_bps`
/// or more, as the step's trigger. A price that fell was sold into.
pub fn trigger(
    pools: &[amm::Pool],
    before: &[Option<f64>],
    tops: &[Top],
    trigger_bps: f64,
    slot: u64,
) -> Option<Trigger> {
    assert!(pools.len() == tops.len() && before.len() == tops.len());
    let moved = |at: usize| -> Option<f64> {
        let was = before[at].filter(|was| *was > 0.0)?;
        Some((tops[at].mid / was).ln() * 10_000.0)
    };
    let (at, move_bps) = (0..pools.len())
        .filter_map(|at| Some((at, moved(at)?)))
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))?;
    // A move that isn't a number (a price of nothing) triggers nothing.
    let is_trigger = move_bps.abs() >= trigger_bps;
    is_trigger.then(|| Trigger {
        // The swap itself isn't known here, only what it did to the price.
        signature: String::new(),
        slot,
        market: pools[at].instrument.to_string(),
        side: if move_bps < 0.0 {
            Side::Sell
        } else {
            Side::Buy
        },
        base: 0.0,
    })
}

/// A pool strategy's pools, read each step.
pub struct PoolReads {
    pools: Arc<Vec<amm::Pool>>,
    /// The node the pools are read from.
    chain: Arc<dyn ChainClient>,
    spec: PoolsSpec,
    /// Each pool's price at the last step.
    mids: Vec<Option<f64>>,
}

impl PoolReads {
    pub fn new(pools: Arc<Vec<amm::Pool>>, chain: Arc<dyn ChainClient>, spec: PoolsSpec) -> Self {
        assert!(pools.len() <= POOLS_MAX, "validated: the pools are bounded");
        Self {
            mids: vec![None; pools.len()],
            pools,
            chain,
            spec,
        }
    }

    /// Every pool's top now, from the node.
    pub async fn tops(&self) -> Result<Vec<Top>, String> {
        let mut tops = Vec::with_capacity(self.pools.len());
        for pool in self.pools.iter() {
            let state = amm::State::read(&*self.chain, pool)
                .await
                .map_err(|e| e.to_string())?;
            let (best_bid, best_ask) = state.top(pool);
            tops.push(Top {
                mid: state.mid(pool),
                best_bid,
                best_ask,
            });
        }
        Ok(tops)
    }

    /// The step at `slot`: the paper wallet at the pools' prices now, and
    /// the trigger, if a pool moved enough since the last step.
    pub async fn step(&mut self, slot: u64) -> Result<(Wallet, Option<Trigger>), String> {
        let started = std::time::Instant::now();
        let tops = self.tops().await?;
        let price_usd = sol_usd(&self.pools, &tops)
            .ok_or("no pool of SOL against USDC among its pools: nothing to value SOL at")?;
        let trigger = trigger(&self.pools, &self.mids, &tops, self.spec.trigger_bps, slot);
        self.mids = tops.iter().map(|top| Some(top.mid)).collect();
        tracing::info!(
            histogram.mtx_pool_desk_read_us =
                u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            counter.mtx_pool_desk_triggers = u64::from(trigger.is_some()),
            pools = self.pools.len(),
            "a pool strategy's pools were read"
        );
        Ok((
            wallet(&self.pools, &tops, self.spec.paper_base, price_usd),
            trigger,
        ))
    }
}

#[cfg(test)]
mod tests {
    use mtx_core::{Asset, mandate::Kind};

    use super::*;

    fn pool(address: Pubkey, base: Pubkey, quote: Pubkey) -> amm::Pool {
        let token = |mint: Pubkey| amm::Token {
            mint,
            token_program: Pubkey::default(),
            vault: Pubkey::default(),
            asset: Asset::new(mint.to_string(), "T", 9).expect("an asset"),
        };
        amm::Pool {
            instrument: InstrumentRef {
                venue: "orca".into(),
                kind: Kind::Amm,
                symbol: address.to_string(),
            },
            address,
            program: amm::Program::Whirlpool,
            base: token(base),
            quote: token(quote),
        }
    }

    fn top(mid: f64) -> Top {
        Top {
            mid,
            best_bid: Some(mid * 1.0004),
            best_ask: Some(mid * 0.9996),
        }
    }

    fn spec(venues: usize) -> PoolsSpec {
        PoolsSpec {
            venues: (0..venues)
                .map(|_| InstrumentRef {
                    venue: "orca".into(),
                    kind: Kind::Amm,
                    symbol: Pubkey::new_unique().to_string(),
                })
                .collect(),
            paper_base: 10.0,
            trigger_bps: 10.0,
        }
    }

    #[test]
    fn a_spec_is_bounded() {
        assert_eq!(validate(&spec(2)), Ok(()));
        assert!(validate(&spec(1)).is_err());
        assert!(validate(&spec(POOLS_MAX + 1)).is_err());
        let mut twice = spec(2);
        twice.venues[1] = twice.venues[0].clone();
        assert!(validate(&twice).is_err());
        for (paper_base, trigger_bps) in [(0.0, 10.0), (f64::NAN, 10.0), (1.0, 0.0), (1.0, -1.0)] {
            let bad = PoolsSpec {
                paper_base,
                trigger_bps,
                ..spec(2)
            };
            assert!(validate(&bad).is_err(), "{paper_base} {trigger_bps}");
        }
    }

    #[test]
    fn sol_is_valued_on_a_usdc_pool_either_way_round() {
        let jto = Pubkey::new_unique();
        let pools = [
            pool(Pubkey::new_unique(), jto, WRAPPED_SOL_MINT),
            pool(Pubkey::new_unique(), WRAPPED_SOL_MINT, USDC_MINT),
        ];
        let tops = [top(0.003), top(72.0)];
        assert_eq!(sol_usd(&pools, &tops), Some(72.0));
        let flipped = [pool(Pubkey::new_unique(), USDC_MINT, WRAPPED_SOL_MINT)];
        assert_eq!(sol_usd(&flipped, &[top(1.0 / 72.0)]), Some(72.0));
        assert_eq!(sol_usd(&pools[..1], &tops[..1]), None);
        let wallet = wallet(&pools, &tops, 10.0, 72.0);
        assert_eq!(
            (wallet.base, wallet.quote, wallet.value_usd),
            (10.0, 0.0, 720.0)
        );
        assert_eq!(wallet.venues.len(), 2);
        assert_eq!(wallet.venues[1].id, pools[1].instrument.to_string());
        assert_eq!(wallet.venues[1].best_bid, tops[1].best_bid);
    }

    #[test]
    fn the_pool_that_moved_the_most_is_the_trigger_past_the_threshold() {
        let pools = [
            pool(Pubkey::new_unique(), WRAPPED_SOL_MINT, USDC_MINT),
            pool(Pubkey::new_unique(), WRAPPED_SOL_MINT, USDC_MINT),
        ];
        let before = [Some(72.0), Some(72.0)];
        // Nothing read before: no move to speak of.
        assert_eq!(
            trigger(&pools, &[None, None], &[top(72.0), top(73.0)], 10.0, 5),
            None
        );
        // 5 bp: under the threshold.
        assert_eq!(
            trigger(&pools, &before, &[top(72.0), top(72.036)], 10.0, 5),
            None
        );
        // The second fell 30 bp, the first rose 12: the second, sold into.
        let found = trigger(&pools, &before, &[top(72.0864), top(71.784)], 10.0, 5).unwrap();
        assert_eq!(found.market, pools[1].instrument.to_string());
        assert_eq!((found.side, found.slot), (Side::Sell, 5));
        // A rise was bought into.
        let found = trigger(&pools, &before, &[top(72.3), top(72.0)], 10.0, 6).unwrap();
        assert_eq!(
            (found.market, found.side),
            (pools[0].instrument.to_string(), Side::Buy)
        );
    }

    #[test]
    fn the_metadata_names_each_pool_as_a_session_s_does() {
        let pools = [pool(Pubkey::new_unique(), WRAPPED_SOL_MINT, USDC_MINT)];
        let metadata: serde_json::Value = serde_json::from_str(&metadata(&pools)).unwrap();
        let listed = &metadata["pools"][0];
        assert_eq!(listed["id"], pools[0].instrument.to_string());
        assert_eq!(listed["address"], pools[0].address.to_string());
        assert_eq!(listed["program"], amm::Program::Whirlpool.id().to_string());
        assert_eq!(listed["base_mint"], WRAPPED_SOL_MINT.to_string());
        assert_eq!(listed["quote_mint"], USDC_MINT.to_string());
    }
}
