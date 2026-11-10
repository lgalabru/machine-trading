//! The grader's measurements of a Phoenix v1 spot account:
//! [`PhoenixSpotMeter`], as `specs/venues/phoenix.md` (Spot markets) defines
//! them.
//!
//! The account on a market is the owner's seat, a `TraderState` in the
//! market account, with the owner's wallet token accounts for the market's
//! two mints. The kit decodes the market
//! ([`MarketState`](mtx_kit::protocols::phoenix::v1::state::MarketState)) and
//! the Pyth price the base is valued at
//! ([`PriceUpdate`](mtx_kit::protocols::pyth::PriceUpdate)); the meter reads
//! them with the grader's own client, never the bot's.

use std::collections::BTreeMap;

use mtx_core::{
    AccountMeter, Asset, Decimal, Holding, Holdings, InstrumentId, Measurement, MeterError,
    RecordedFill,
};
use mtx_kit::protocols::{
    phoenix::v1::state::{MarketState, TraderState},
    pyth,
};
use solana_pubkey::{Pubkey, pubkey};
use solana_rpc_client::rpc_client::RpcClient;

use super::{read, units, wallet_atoms};
use crate::phoenix::USDC_MINT;

/// A spot market the meter reads, and the Pyth feed its base is valued at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotMarket {
    /// The symbol `specs/venues/phoenix.md` names it by, `BASE-QUOTE`.
    pub symbol: String,
    pub address: Pubkey,
    pub base_feed: [u8; 32],
}

impl SpotMarket {
    /// `phoenix:spot:SOL-USDC`, the one market the venue profile names.
    pub fn sol_usdc() -> Self {
        Self {
            symbol: "SOL-USDC".into(),
            address: pubkey!("4DoNfFBfF7UokCC2FQzriy7yHK6DY6NVdYpuekQ5pRgg"),
            base_feed: pyth::SOL_USD_FEED_ID,
        }
    }

    /// The identifier its holdings are reported under.
    pub fn id(&self) -> InstrumentId {
        InstrumentId::new(format!("phoenix:{}", self.symbol))
    }
}

/// One owner's account on a set of Phoenix v1 spot markets: those in the
/// universe, and any others it should watch for the universe rule.
pub struct PhoenixSpotMeter<'a> {
    rpc: &'a RpcClient,
    owner: Pubkey,
    markets: Vec<SpotMarket>,
}

impl<'a> PhoenixSpotMeter<'a> {
    pub fn new(rpc: &'a RpcClient, owner: Pubkey, markets: Vec<SpotMarket>) -> Self {
        Self {
            rpc,
            owner,
            markets,
        }
    }

    fn market(&self, spot: &SpotMarket) -> Result<MarketState, MeterError> {
        let data = self.rpc.get_account_data(&spot.address).map_err(read)?;
        MarketState::decode(&data).map_err(decode("Phoenix v1 market"))
    }

    fn price(&self, feed_id: &[u8; 32]) -> Result<Decimal, MeterError> {
        let data = self
            .rpc
            .get_account_data(&pyth::feed_account(0, feed_id))
            .map_err(read)?;
        pyth::PriceUpdate::decode_feed(&data, feed_id)
            .map_err(decode("Pyth price update"))?
            .price()
            .ok_or_else(|| MeterError::Decode {
                what: "Pyth price update",
                detail: "the price doesn't fit a decimal".into(),
            })
    }
}

/// One asset the account holds, counted once however many markets hold it.
struct AssetHeld {
    decimals: u32,
    wallet: u64,
    on_venue: u128,
    /// The base's Pyth feed; `None` for USDC, which the profile values at $1.
    feed: Option<[u8; 32]>,
}

#[async_trait::async_trait]
impl AccountMeter for PhoenixSpotMeter<'_> {
    /// Every watched market's holding, flat ones included. A market outside
    /// `universe` counts against it when the seat has orders resting there:
    /// base or quote locked. Holdings of an asset count wherever they sit,
    /// since a spot position belongs to the asset, not to a market.
    async fn measure(&self, universe: &[InstrumentId]) -> Result<Measurement, MeterError> {
        let mut holdings = Vec::with_capacity(self.markets.len());
        let mut assets: BTreeMap<Pubkey, AssetHeld> = BTreeMap::new();
        let mut outside_universe = 0;
        for spot in &self.markets {
            let market = self.market(spot)?;
            let h = &market.header;
            if h.quote_mint != USDC_MINT {
                return Err(MeterError::Unsupported(format!(
                    "{}: quoted in {}, and only USDC has a reference price",
                    spot.symbol, h.quote_mint
                )));
            }
            let seat = market.seat(&self.owner).copied().unwrap_or_default();
            let lots = |lots: u64, size: u64| u128::from(lots) * u128::from(size);
            for (mint, decimals, on_venue, feed) in [
                (
                    h.base_mint,
                    h.base_decimals,
                    lots(seat.base_lots_free + seat.base_lots_locked, h.base_lot_size),
                    Some(spot.base_feed),
                ),
                (
                    h.quote_mint,
                    h.quote_decimals,
                    lots(
                        seat.quote_lots_free + seat.quote_lots_locked,
                        h.quote_lot_size,
                    ),
                    None,
                ),
            ] {
                let held = match assets.entry(mint) {
                    std::collections::btree_map::Entry::Occupied(held) => held.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => entry.insert(AssetHeld {
                        decimals,
                        wallet: wallet_atoms(self.rpc, &self.owner, &mint)?,
                        on_venue: 0,
                        feed,
                    }),
                };
                held.on_venue += on_venue;
            }
            let base_symbol = spot.symbol.split('-').next().unwrap_or(&spot.symbol);
            let decimals = u8::try_from(h.base_decimals).map_err(|_| MeterError::Decode {
                what: "Phoenix v1 market",
                detail: format!("{} base decimals", h.base_decimals),
            })?;
            let base =
                Asset::new(h.base_mint.to_string(), base_symbol, decimals).map_err(|error| {
                    MeterError::Decode {
                        what: "Phoenix v1 market",
                        detail: error.to_string(),
                    }
                })?;
            let base_units = |lots: u64| {
                units(
                    u128::from(lots) * u128::from(h.base_lot_size),
                    h.base_decimals,
                )
            };
            holdings.push((
                spot.id(),
                Holding::Spot(Holdings {
                    wallet: units(u128::from(assets[&h.base_mint].wallet), h.base_decimals)?,
                    free: base_units(seat.base_lots_free)?,
                    locked: base_units(seat.base_lots_locked)?,
                    base,
                }),
            ));
            if has_orders(&seat) && !universe.contains(&spot.id()) {
                outside_universe += 1;
            }
        }
        let mut value_usd = Decimal::ZERO;
        for held in assets.values() {
            let amount = units(u128::from(held.wallet) + held.on_venue, held.decimals)?;
            let price = match &held.feed {
                Some(feed) => self.price(feed)?,
                None => Decimal::ONE,
            };
            value_usd += amount * price;
        }
        Ok(Measurement {
            holdings,
            value_usd,
            outside_universe,
        })
    }

    async fn fills(&self, _after_slot: u64) -> Result<Vec<RecordedFill>, MeterError> {
        Err(MeterError::Unsupported(
            "Phoenix v1 fills: the reader doesn't decode the audit log's Fill events yet".into(),
        ))
    }
}

/// Whether the seat has orders resting: an ask locks base, a bid locks quote.
fn has_orders(seat: &TraderState) -> bool {
    seat.base_lots_locked > 0 || seat.quote_lots_locked > 0
}

fn decode(what: &'static str) -> impl Fn(mtx_kit::chain::ChainError) -> MeterError {
    move |error| MeterError::Decode {
        what,
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_has_orders_when_it_locks_either_side() {
        let flat = TraderState::default();
        assert!(!has_orders(&flat));
        assert!(has_orders(&TraderState {
            base_lots_locked: 1,
            ..flat
        }));
        assert!(has_orders(&TraderState {
            quote_lots_locked: 1,
            ..flat
        }));
        assert!(!has_orders(&TraderState {
            base_lots_free: 9,
            quote_lots_free: 9,
            ..flat
        }));
    }

    /// `measure` against a live chain: set `MTX_SPOT_RPC` to a mainnet RPC
    /// URL and `MTX_SPOT_OWNER` to a trader with a seat on SOL-USDC.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "reads mainnet"]
    async fn measures_a_mainnet_seat() {
        let rpc = RpcClient::new(std::env::var("MTX_SPOT_RPC").unwrap());
        let owner: Pubkey = std::env::var("MTX_SPOT_OWNER").unwrap().parse().unwrap();
        let meter = PhoenixSpotMeter::new(&rpc, owner, vec![SpotMarket::sol_usdc()]);
        let inside = meter.measure(&[SpotMarket::sol_usdc().id()]).await.unwrap();
        let outside = meter.measure(&[]).await.unwrap();
        println!("{inside:#?}");
        assert_eq!(inside.holdings.len(), 1);
        assert_eq!(inside.outside_universe, 0);
        let Holding::Spot(held) = &inside.holdings[0].1 else {
            panic!("not spot");
        };
        assert_eq!(held.base.symbol, "SOL");
        assert_eq!(
            outside.outside_universe,
            u32::from(held.locked > Decimal::ZERO)
        );
        assert!(inside.value_usd >= Decimal::ZERO);
    }
}
