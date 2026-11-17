//! What every order-book venue's meter records: a [`MarketFill`], a match
//! between any two accounts, and the [`RecordedFill`]s an account's
//! [`AccountMeter::fills`](crate::core::AccountMeter::fills) returns from it.
//!
//! The same rules hold on every venue:
//!
//! - **One record per side the account traded.** A fill names a taker and a
//!   maker. An account that is either gets the fill on its side; one that is
//!   both (a match between two of its own accounts, or with itself) gets it
//!   twice, once on each side, so its fills sum to its change in position.
//! - **Ids from the venue's own numbering.** A fill's [`FillId`] comes from
//!   its market, its sequence and the side, so the same fill read twice has
//!   the same id, and the same sequence on two markets doesn't.
//! - **Fees where the reader decodes them.** Each side carries the fee its
//!   role paid where the venue's reader decodes a per-fill fee (Phoenix
//!   Eternal), and zero elsewhere: Phoenix v1's `Fill` event carries none,
//!   and OpenBook v2's reader doesn't decode the maker and taker fees its
//!   `FillLog` carries yet (its event-heap fills, the other source it
//!   reads, carry none).

use rust_decimal::Decimal;
use solana_pubkey::Pubkey;
use uuid::Uuid;

use crate::core::{Fill, FillId, InstrumentId, OrderId, RecordedFill, Side, VenueId};

/// A match a market recorded, between any two accounts: what an enforcer
/// counts for the sides and limit-price rules, and what makes a market's
/// tape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketFill {
    /// The slot it landed in; for a fill still waiting in a market's event
    /// queue, the slot it was read at.
    pub slot: u64,
    /// The market, as the mandate names it (`venue:kind:symbol`).
    pub instrument: InstrumentId,
    /// The venue's own order for its fills: a sequence that never repeats
    /// on the market, to count each fill once. Each venue's reader says how
    /// it builds it.
    pub sequence: u128,
    /// The taker and the maker, as the venue names them: on Phoenix the
    /// trader accounts, on OpenBook the open orders accounts (the signer for
    /// a take-only order).
    pub taker: Pubkey,
    pub maker: Pubkey,
    /// The taker's side; the maker traded the other.
    pub taker_side: Side,
    /// Price, in quote units per base unit, before fees: what the fill
    /// paid on average, its quote over its size.
    pub price: Decimal,
    /// The worst price level the taker reached, in the same units: the
    /// highest for a taker's buy, the lowest for a taker's sell. It equals
    /// [`MarketFill::price`] for a fill at one level, and for every venue
    /// whose records can't say more. The limit-price rule judges the taker
    /// at this level, so a fill can't buy through a cap or sell through a
    /// floor on the strength of its average. The maker of a spline's fill
    /// is judged at the average price: the event names only the last tick
    /// the spline reached, which is the maker's best level, not its worst
    /// ([`MarketFill::recorded`]).
    pub worst_price: Decimal,
    /// Size, in base units. Always positive.
    pub quantity: Decimal,
    /// When the venue says it happened, in Unix seconds.
    pub unix_timestamp: i64,
    /// The fee the taker paid on this fill, in quote units: zero where the
    /// venue's reader doesn't decode one (see the module).
    pub taker_fee: Decimal,
    /// The fee the maker paid on this fill, in quote units, negative for a
    /// rebate: zero where the venue's reader doesn't decode one.
    pub maker_fee: Decimal,
    /// The maker's resting order, as the venue numbers it on the market,
    /// where its records name it: the maker's side of the fill takes its
    /// [`OrderId`] from it.
    pub maker_order: Option<u128>,
}

impl MarketFill {
    /// The first side this fill traded for the account `ids` name, if
    /// either party is one of them: the taker's side as the taker, else the
    /// other as the maker. A fill between two of its ids traded both:
    /// [`MarketFill::sides_for`] gives each.
    pub fn side_for(&self, ids: &[Pubkey]) -> Option<Side> {
        self.sides_for(ids).next()
    }

    /// Each side this fill traded for the account `ids` name: the taker's
    /// side if the taker is one of them, then the maker's if the maker is.
    pub fn sides_for<'a>(&'a self, ids: &'a [Pubkey]) -> impl Iterator<Item = Side> + 'a {
        let taker = ids.contains(&self.taker).then_some(self.taker_side);
        let maker = ids
            .contains(&self.maker)
            .then_some(self.taker_side.opposite());
        taker.into_iter().chain(maker)
    }

    /// The fill as the party on `side` traded it, with that party's fee.
    ///
    /// Its [`FillId`] comes from the instrument, the sequence and the side,
    /// so re-reading gives the same id and no two markets' fills share one.
    /// Its [`OrderId`] comes from [`MarketFill::maker_order`] on the maker's
    /// side; otherwise (the taker's side, or a venue whose records don't
    /// name the order) from the fill itself, so it is as stable.
    pub fn recorded(&self, side: Side) -> RecordedFill {
        let venue = self.instrument.0.split(':').next().unwrap_or_default();
        let as_taker = side == self.taker_side;
        let id = stable_id(&[
            b"fill",
            self.instrument.0.as_bytes(),
            &self.sequence.to_le_bytes(),
            &[u8::from(as_taker)],
        ]);
        let order_id = match self.maker_order {
            Some(order) if !as_taker => {
                stable_id(&[b"order", self.instrument.0.as_bytes(), &order.to_le_bytes()])
            }
            _ => stable_id(&[b"fill-order", id.as_bytes()]),
        };
        RecordedFill {
            slot: self.slot,
            // The event names the taker's worst level; for the maker of a
            // multi-level fill that is its best, and its own worst isn't
            // named, so it gets the average.
            worst_price: if as_taker {
                self.worst_price
            } else {
                self.price
            },
            fill: Fill {
                id: FillId(id),
                order_id: OrderId(order_id),
                instrument: self.instrument.clone(),
                venue: VenueId::new(venue),
                side,
                price: self.price,
                quantity: self.quantity,
                fees: if as_taker {
                    self.taker_fee
                } else {
                    self.maker_fee
                },
                timestamp: chrono::DateTime::from_timestamp(self.unix_timestamp, 0)
                    .unwrap_or_default(),
                latency: None,
                strategy: None,
            },
        }
    }
}

/// The fills of `fills` the account `ids` name traded, each on every side
/// it traded ([`MarketFill::sides_for`]), in order.
pub fn recorded_for(fills: &[MarketFill], ids: &[Pubkey]) -> Vec<RecordedFill> {
    fills
        .iter()
        .flat_map(|fill| fill.sides_for(ids).map(|side| fill.recorded(side)))
        .collect()
}

/// A UUID that is a stable function of `parts`: 128-bit FNV-1a over each
/// part, length first. Stable across builds and platforms, unlike the
/// standard library's hasher. It is an identifier, not a commitment: it
/// keeps distinct fills apart, and doesn't hide them.
fn stable_id(parts: &[&[u8]]) -> Uuid {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = OFFSET;
    for part in parts {
        let len = (part.len() as u64).to_le_bytes();
        for &byte in len.iter().chain(part.iter()) {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    Uuid::from_u128(hash)
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn fill(instrument: &str, sequence: u128, taker: Pubkey, maker: Pubkey) -> MarketFill {
        MarketFill {
            slot: 7,
            instrument: InstrumentId(instrument.into()),
            sequence,
            taker,
            maker,
            taker_side: Side::Buy,
            price: dec!(10),
            worst_price: dec!(10),
            quantity: dec!(2),
            unix_timestamp: 1_700_000_000,
            taker_fee: dec!(0.03),
            maker_fee: dec!(-0.01),
            maker_order: Some(42),
        }
    }

    #[test]
    fn ids_are_stable_and_apart_by_market_sequence_and_side() {
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let one = fill("phoenix:perp:SOL", 5, a, b);
        assert_eq!(
            one.recorded(Side::Buy).fill.id,
            one.recorded(Side::Buy).fill.id
        );
        let ids = [
            one.recorded(Side::Buy).fill.id,
            one.recorded(Side::Sell).fill.id,
            fill("phoenix:perp:BTC", 5, a, b)
                .recorded(Side::Buy)
                .fill
                .id,
            fill("phoenix:perp:SOL", 6, a, b)
                .recorded(Side::Buy)
                .fill
                .id,
        ];
        for (i, x) in ids.iter().enumerate() {
            for y in &ids[i + 1..] {
                assert_ne!(x, y);
            }
        }
        // The maker's order id is its resting order's, shared by its fills;
        // the taker's is the fill's own.
        let next = fill("phoenix:perp:SOL", 6, a, b);
        assert_eq!(
            one.recorded(Side::Sell).fill.order_id,
            next.recorded(Side::Sell).fill.order_id
        );
        assert_ne!(
            one.recorded(Side::Buy).fill.order_id,
            next.recorded(Side::Buy).fill.order_id
        );
    }

    #[test]
    fn each_side_an_account_traded_is_one_fill_with_its_fee() {
        let (a, b, c) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let fills = [fill("x:perp:A", 1, a, b), fill("x:perp:A", 2, a, a)];
        let taker = recorded_for(&fills, &[a]);
        let sides: Vec<(Side, Decimal)> =
            taker.iter().map(|f| (f.fill.side, f.fill.fees)).collect();
        // Its taker buy; then both sides of the match with itself, which
        // net to nothing.
        assert_eq!(
            sides,
            vec![
                (Side::Buy, dec!(0.03)),
                (Side::Buy, dec!(0.03)),
                (Side::Sell, dec!(-0.01)),
            ]
        );
        let maker = recorded_for(&fills, &[b]);
        assert_eq!(maker.len(), 1);
        assert_eq!(maker[0].fill.side, Side::Sell);
        assert_eq!(maker[0].fill.fees, dec!(-0.01));
        assert!(recorded_for(&fills, &[c]).is_empty());
        assert_eq!(fills[1].side_for(&[a]), Some(Side::Buy));
        assert_eq!(fills[0].side_for(&[b]), Some(Side::Sell));
    }
}
