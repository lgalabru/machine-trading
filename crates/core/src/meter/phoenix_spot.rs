//! The grader's measurements of a Phoenix v1 spot account:
//! [`PhoenixSpotMeter`], as `specs/venues/phoenix.md` (Spot markets) defines
//! them.
//!
//! The account on a market is the owner's seat, a `TraderState` in the
//! market account, with the owner's wallet token accounts for the market's
//! two mints. Phoenix's own crate (`phoenix-v1`) pins Solana 1.14, which
//! this workspace can't build beside its own, so [`Market`] decodes the few
//! fields the profile needs from the account's layout:
//!
//! - the `MarketHeader` (576 bytes): the sizes of the three trees at 16, the
//!   base and quote `TokenParams` (decimals, mint) at 40 and 120, with the
//!   base lot size at 112 and the quote lot size at 192;
//! - the `FIFOMarket`, from 576: 256 bytes of padding and six `u64`s, then
//!   the bids, asks and traders trees, each a `lib-sokoban` red-black tree:
//!   a 32-byte header (the root's 1-based node index first, the node count
//!   at 16), then its nodes, each four `u32` registers (left, right,
//!   parent, colour), the key and the value. An order node is 64 bytes; a
//!   trader node is 144, its key the trader's pubkey and its value the
//!   `TraderState`: quote lots locked, quote lots free, base lots locked,
//!   base lots free, then padding.
//!
//! The decoder checks the account's size against the tree sizes in its
//! header, so a layout it doesn't know fails to decode rather than misread.

use std::collections::BTreeMap;

use mtx_botkit::{
    Decimal,
    luce::core::{
        AccountMeter, Asset, Holding, Holdings, InstrumentId, Measurement, MeterError, RecordedFill,
    },
};
use solana_pubkey::{Pubkey, pubkey};
use solana_rpc_client::rpc_client::RpcClient;

use super::{pyth, read, units, wallet_atoms};
use crate::phoenix::USDC_MINT;

/// The Phoenix v1 program.
pub const PROGRAM: Pubkey = pubkey!("PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY");

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
            base_feed: pyth::SOL_USD,
        }
    }

    /// The `luce` identifier its holdings are reported under.
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

    fn market(&self, spot: &SpotMarket) -> Result<Market, MeterError> {
        let data = self.rpc.get_account_data(&spot.address).map_err(read)?;
        Market::decode(&data)
    }

    fn price(&self, feed_id: &[u8; 32]) -> Result<Decimal, MeterError> {
        let data = self
            .rpc
            .get_account_data(&pyth::feed_address(feed_id))
            .map_err(read)?;
        let price = pyth::decode(&data)?;
        if &price.feed_id != feed_id {
            return Err(MeterError::Decode {
                what: "Pyth price update",
                detail: "the account holds another feed".into(),
            });
        }
        Ok(price.price)
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
            if market.quote_mint != USDC_MINT {
                return Err(MeterError::Unsupported(format!(
                    "{}: quoted in {}, and only USDC has a reference price",
                    spot.symbol, market.quote_mint
                )));
            }
            let seat = market.seat(&self.owner).unwrap_or_default();
            for (mint, decimals, on_venue, feed) in [
                (
                    market.base_mint,
                    market.base_decimals,
                    seat.base_atoms(&market),
                    Some(spot.base_feed),
                ),
                (
                    market.quote_mint,
                    market.quote_decimals,
                    seat.quote_atoms(&market),
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
            let decimals = u8::try_from(market.base_decimals).map_err(|_| MeterError::Decode {
                what: "Phoenix v1 market",
                detail: format!("{} base decimals", market.base_decimals),
            })?;
            let base = Asset::new(market.base_mint.to_string(), base_symbol, decimals).map_err(
                |error| MeterError::Decode {
                    what: "Phoenix v1 market",
                    detail: error.to_string(),
                },
            )?;
            let lots = |lots: u64| {
                units(
                    u128::from(lots) * u128::from(market.base_lot_size),
                    market.base_decimals,
                )
            };
            holdings.push((
                spot.id(),
                Holding::Spot(Holdings {
                    wallet: units(
                        u128::from(assets[&market.base_mint].wallet),
                        market.base_decimals,
                    )?,
                    free: lots(seat.base_lots_free)?,
                    locked: lots(seat.base_lots_locked)?,
                    base,
                }),
            ));
            if seat.has_orders() && !universe.contains(&spot.id()) {
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

/// The fields of a Phoenix v1 market account the spot profile measures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Market {
    pub base_mint: Pubkey,
    pub base_decimals: u32,
    /// Base atoms per base lot.
    pub base_lot_size: u64,
    pub quote_mint: Pubkey,
    pub quote_decimals: u32,
    /// Quote atoms per quote lot.
    pub quote_lot_size: u64,
    seats: Tree,
    data: Vec<u8>,
}

/// A trader's balances on a market, in lots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraderState {
    pub quote_lots_locked: u64,
    pub quote_lots_free: u64,
    pub base_lots_locked: u64,
    pub base_lots_free: u64,
}

impl TraderState {
    fn base_atoms(&self, market: &Market) -> u128 {
        u128::from(self.base_lots_free + self.base_lots_locked) * u128::from(market.base_lot_size)
    }

    fn quote_atoms(&self, market: &Market) -> u128 {
        u128::from(self.quote_lots_free + self.quote_lots_locked)
            * u128::from(market.quote_lot_size)
    }

    /// Whether orders rest in the book: an ask locks base, a bid locks quote.
    pub fn has_orders(&self) -> bool {
        self.base_lots_locked > 0 || self.quote_lots_locked > 0
    }
}

const HEADER: usize = 576;
const TREES: usize = HEADER + 256 + 6 * 8;
const TREE_HEADER: usize = 32;
const ORDER_NODE: usize = 16 + 16 + 32;
const TRADER_NODE: usize = 16 + 32 + 96;

/// A red-black tree's place in the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tree {
    at: usize,
    capacity: usize,
    node: usize,
}

impl Tree {
    fn len(&self) -> usize {
        TREE_HEADER + self.capacity * self.node
    }
}

impl Market {
    pub fn decode(data: &[u8]) -> Result<Self, MeterError> {
        let bad = |detail: String| MeterError::Decode {
            what: "Phoenix v1 market",
            detail,
        };
        if data.len() < TREES {
            return Err(bad(format!("{} bytes is too short", data.len())));
        }
        let u64_at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().expect("8 bytes"));
        let u32_at = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().expect("4 bytes"));
        let key_at =
            |o: usize| Pubkey::new_from_array(data[o..o + 32].try_into().expect("32 bytes"));
        let size = |o: usize| usize::try_from(u64_at(o)).map_err(|_| bad("tree size".into()));
        let bids = Tree {
            at: TREES,
            capacity: size(16)?,
            node: ORDER_NODE,
        };
        let asks = Tree {
            at: bids.at + bids.len(),
            capacity: size(24)?,
            node: ORDER_NODE,
        };
        let seats = Tree {
            at: asks.at + asks.len(),
            capacity: size(32)?,
            node: TRADER_NODE,
        };
        if seats.at + seats.len() != data.len() {
            return Err(bad(format!(
                "{} bytes, but trees of {}, {} and {} nodes need {}",
                data.len(),
                bids.capacity,
                asks.capacity,
                seats.capacity,
                seats.at + seats.len()
            )));
        }
        Ok(Self {
            base_decimals: u32_at(40),
            base_mint: key_at(48),
            base_lot_size: u64_at(112),
            quote_decimals: u32_at(120),
            quote_mint: key_at(128),
            quote_lot_size: u64_at(192),
            seats,
            data: data.to_vec(),
        })
    }

    fn u32_at(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.data[o..o + 4].try_into().expect("4 bytes"))
    }

    fn u64_at(&self, o: usize) -> u64 {
        u64::from_le_bytes(self.data[o..o + 8].try_into().expect("8 bytes"))
    }

    /// Where 1-based node `index` of the seats tree starts, if it's in it.
    fn seat_node(&self, index: u32) -> Option<usize> {
        let index = usize::try_from(index).ok()?.checked_sub(1)?;
        (index < self.seats.capacity).then(|| self.seats.at + TREE_HEADER + index * self.seats.node)
    }

    fn state_at(&self, node: usize) -> TraderState {
        let at = node + 16 + 32;
        TraderState {
            quote_lots_locked: self.u64_at(at),
            quote_lots_free: self.u64_at(at + 8),
            base_lots_locked: self.u64_at(at + 16),
            base_lots_free: self.u64_at(at + 24),
        }
    }

    /// `trader`'s seat, if it has one: a search down the seats tree, which
    /// orders traders by their pubkey's bytes.
    pub fn seat(&self, trader: &Pubkey) -> Option<TraderState> {
        let mut index = self.u32_at(self.seats.at);
        // A tree's height is bounded; a cycle in a corrupt account isn't.
        for _ in 0..=self.seats.capacity {
            let node = self.seat_node(index)?;
            let key = &self.data[node + 16..node + 48];
            index = match trader.as_ref().cmp(key) {
                std::cmp::Ordering::Equal => return Some(self.state_at(node)),
                std::cmp::Ordering::Less => self.u32_at(node),
                std::cmp::Ordering::Greater => self.u32_at(node + 4),
            };
        }
        None
    }

    /// Every seat, walking the tree from its root: the nodes in use, and
    /// none of the freed ones.
    pub fn seats(&self) -> Vec<(Pubkey, TraderState)> {
        let mut seats = Vec::new();
        let mut pending = vec![self.u32_at(self.seats.at)];
        while let Some(index) = pending.pop() {
            if seats.len() > self.seats.capacity {
                break;
            }
            let Some(node) = self.seat_node(index) else {
                continue;
            };
            let key =
                Pubkey::new_from_array(self.data[node + 16..node + 48].try_into().expect("32"));
            seats.push((key, self.state_at(node)));
            pending.push(self.u32_at(node));
            pending.push(self.u32_at(node + 4));
        }
        seats
    }

    /// The seats tree's node count, as its allocator records it.
    pub fn seat_count(&self) -> u64 {
        self.u64_at(self.seats.at + 16)
    }
}

#[cfg(test)]
mod tests {
    use solana_pubkey::pubkey;

    use super::*;

    const SOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

    /// A market account with room for `capacity` seats, the given traders
    /// in a balanced tree.
    fn account(traders: &[(Pubkey, TraderState)], capacity: usize) -> Vec<u8> {
        let (bids, asks) = (3, 2);
        let seats_at = TREES + 2 * TREE_HEADER + (bids + asks) * ORDER_NODE;
        let mut data = vec![0u8; seats_at + TREE_HEADER + capacity * TRADER_NODE];
        let mut put = |o: usize, bytes: &[u8]| data[o..o + bytes.len()].copy_from_slice(bytes);
        put(16, &(bids as u64).to_le_bytes());
        put(24, &(asks as u64).to_le_bytes());
        put(32, &(capacity as u64).to_le_bytes());
        put(40, &9u32.to_le_bytes());
        put(48, SOL.as_ref());
        put(112, &1_000_000u64.to_le_bytes());
        put(120, &6u32.to_le_bytes());
        put(128, USDC_MINT.as_ref());
        put(192, &1u64.to_le_bytes());

        let mut sorted = traders.to_vec();
        sorted.sort_by_key(|(key, _)| *key);
        // Node i + 1 holds sorted[i]; subtrees from the middle out.
        fn link(lo: usize, hi: usize, links: &mut Vec<(usize, u32, u32)>) -> u32 {
            if lo >= hi {
                return 0;
            }
            let mid = (lo + hi) / 2;
            let left = link(lo, mid, links);
            let right = link(mid + 1, hi, links);
            links.push((mid, left, right));
            mid as u32 + 1
        }
        let mut links = Vec::new();
        let root = link(0, sorted.len(), &mut links);
        put(seats_at, &root.to_le_bytes());
        put(seats_at + 16, &(sorted.len() as u64).to_le_bytes());
        for (i, left, right) in links {
            let node = seats_at + TREE_HEADER + i * TRADER_NODE;
            let (key, state) = sorted[i];
            put(node, &left.to_le_bytes());
            put(node + 4, &right.to_le_bytes());
            put(node + 16, key.as_ref());
            for (k, lots) in [
                state.quote_lots_locked,
                state.quote_lots_free,
                state.base_lots_locked,
                state.base_lots_free,
            ]
            .into_iter()
            .enumerate()
            {
                put(node + 48 + 8 * k, &lots.to_le_bytes());
            }
        }
        data
    }

    fn state(n: u64) -> TraderState {
        TraderState {
            quote_lots_locked: n,
            quote_lots_free: 10 * n,
            base_lots_locked: 100 * n,
            base_lots_free: 1000 * n,
        }
    }

    #[test]
    fn finds_every_seat_and_no_other() {
        let traders: Vec<_> = (1..=7).map(|n| (Pubkey::new_unique(), state(n))).collect();
        let market = Market::decode(&account(&traders, 9)).unwrap();
        assert_eq!(
            (market.base_mint, market.base_decimals, market.base_lot_size),
            (SOL, 9, 1_000_000)
        );
        assert_eq!((market.quote_mint, market.quote_decimals), (USDC_MINT, 6));
        for (trader, state) in &traders {
            assert_eq!(market.seat(trader), Some(*state));
        }
        assert_eq!(market.seat(&Pubkey::new_unique()), None);
        assert_eq!(market.seats().len(), 7);
        assert_eq!(market.seat_count(), 7);

        let seat = market.seat(&traders[2].0).unwrap();
        assert!(seat.has_orders());
        // 3 000 + 300 lots of 0.001 SOL.
        assert_eq!(seat.base_atoms(&market), 3_300 * 1_000_000);
        assert!(!TraderState::default().has_orders());
    }

    #[test]
    fn refuses_an_account_its_layout_doesnt_fit() {
        let mut data = account(&[(Pubkey::new_unique(), state(1))], 4);
        data.push(0);
        assert!(Market::decode(&data).is_err());
        assert!(Market::decode(&data[..HEADER]).is_err());
    }

    /// The live SOL-USDC market: set `MTX_PHOENIX_V1_MARKET` to a dump of
    /// its account data.
    #[test]
    #[ignore = "needs a dump of the mainnet market account"]
    fn reads_the_mainnet_sol_usdc_market() {
        let path = std::env::var("MTX_PHOENIX_V1_MARKET").unwrap();
        let market = Market::decode(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!((market.base_mint, market.quote_mint), (SOL, USDC_MINT));
        assert_eq!(
            (market.base_lot_size, market.quote_lot_size),
            (1_000_000, 1)
        );
        let seats = market.seats();
        assert_eq!(seats.len() as u64, market.seat_count());
        for (trader, state) in &seats {
            assert_eq!(market.seat(trader).as_ref(), Some(state));
        }
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
