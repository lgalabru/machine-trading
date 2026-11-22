//! Whirlpool accounts: the pool, its tick arrays (fixed and dynamic), and
//! its oracle, from `orca-so/whirlpools@cc683ec92acb5e7606caa3d8e514d9383e9c7c11`,
//! `programs/whirlpool/src/state/`.
//!
//! Each is an Anchor account: an 8-byte discriminator, then the fields in
//! declaration order, unpadded. A swap reads only part of each: the pool's
//! price, liquidity and fees, each tick's `liquidity_net` and whether it is
//! initialized, and the oracle's adaptive fee. That is all these keep.
//!
//! A tick array comes in two layouts. The fixed one (`TickArray`, 9,988
//! bytes) stores all 88 ticks at 113 bytes each. The dynamic one
//! (`DynamicTickArray`, `dynamic_tick_array.rs`) stores a bitmap of the
//! initialized ticks, then each tick as a Borsh enum: one byte for an
//! uninitialized tick, 113 for an initialized one, so it is as long as its
//! liquidity needs. Both behave alike in a swap.

use solana_pubkey::Pubkey;

use super::math::{MAX_SQRT_PRICE_X64, MAX_TICK_INDEX, MIN_SQRT_PRICE_X64, MIN_TICK_INDEX};
use crate::protocols::DecodeError;

/// Ticks in one tick array (`tick_array.rs`, `TICK_ARRAY_SIZE`).
pub const TICK_ARRAY_SIZE: usize = 88;

const WHIRLPOOL_DISCRIMINATOR: [u8; 8] = [0x3f, 0x95, 0xd1, 0x0c, 0xe1, 0x80, 0x63, 0x09];
const FIXED_TICK_ARRAY_DISCRIMINATOR: [u8; 8] = [0x45, 0x61, 0xbd, 0xbe, 0x6e, 0x07, 0x42, 0xbb];
const DYNAMIC_TICK_ARRAY_DISCRIMINATOR: [u8; 8] = [0x11, 0xd8, 0xf6, 0x8e, 0xe1, 0xc7, 0xda, 0x38];
const ORACLE_DISCRIMINATOR: [u8; 8] = [0x8b, 0xc2, 0x83, 0xb3, 0x8c, 0xb3, 0xe5, 0xf4];

/// Size of a `Whirlpool` account (`whirlpool.rs`, `Whirlpool::LEN`).
pub const WHIRLPOOL_SIZE: usize = 653;
/// Size of a fixed `TickArray` account (`fixed_tick_array.rs`).
pub const FIXED_TICK_ARRAY_SIZE: usize = 8 + 4 + TICK_SIZE * TICK_ARRAY_SIZE + 32;
/// Size of an `Oracle` account (`oracle.rs`, `Oracle::LEN`).
pub const ORACLE_SIZE: usize = 254;
/// A fixed tick: `initialized` (1), `liquidity_net` (16), then 96 bytes of
/// gross liquidity, fee and reward growth.
const TICK_SIZE: usize = 113;
/// A dynamic tick's data, after its one-byte enum tag.
const DYNAMIC_TICK_DATA_SIZE: usize = 112;
/// Where a dynamic tick array's ticks start: discriminator, start tick,
/// whirlpool, bitmap.
const DYNAMIC_TICKS_OFFSET: usize = 8 + 4 + 32 + 16;
/// The fee rate a pool may charge at most (`token_math.rs`, `MAX_FEE_RATE`).
const FEE_RATE_MAX: u16 = 60_000;
/// The protocol's share of the fee at most (`MAX_PROTOCOL_FEE_RATE`).
const PROTOCOL_FEE_RATE_MAX: u16 = 2_500;

/// The fields of a `Whirlpool` account a swap reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Whirlpool {
    /// The config the pool was created under.
    pub whirlpools_config: Pubkey,
    /// Ticks between two initializable ticks.
    pub tick_spacing: u16,
    /// The fee tier the pool was created from: its tick spacing for a
    /// static fee, another value for an adaptive fee tier.
    pub fee_tier_index: u16,
    /// The static fee, in hundredths of a basis point.
    pub fee_rate: u16,
    /// The protocol's share of the fee, in basis points.
    pub protocol_fee_rate: u16,
    /// Liquidity in range at the current price.
    pub liquidity: u128,
    /// The current price's square root, Q64.64.
    pub sqrt_price_x64: u128,
    /// The current tick.
    pub tick_current_index: i32,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    /// Fee growth per unit of liquidity in token A, Q64.64.
    pub fee_growth_global_a: u128,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
    /// Fee growth per unit of liquidity in token B, Q64.64.
    pub fee_growth_global_b: u128,
    /// When rewards were last accrued, in Unix seconds.
    pub reward_last_updated_timestamp_s: u64,
}

impl Whirlpool {
    /// Decodes a `Whirlpool` account.
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let d = Reader::new(data, "whirlpool")?;
        d.expect_len(WHIRLPOOL_SIZE)?;
        d.expect_discriminator(&WHIRLPOOL_DISCRIMINATOR)?;
        let pool = Self {
            whirlpools_config: d.key(8)?,
            tick_spacing: d.u16(41)?,
            fee_tier_index: d.u16(43)?,
            fee_rate: d.u16(45)?,
            protocol_fee_rate: d.u16(47)?,
            liquidity: d.u128(49)?,
            sqrt_price_x64: d.u128(65)?,
            tick_current_index: d.i32(81)?,
            token_mint_a: d.key(101)?,
            token_vault_a: d.key(133)?,
            fee_growth_global_a: d.u128(165)?,
            token_mint_b: d.key(181)?,
            token_vault_b: d.key(213)?,
            fee_growth_global_b: d.u128(245)?,
            reward_last_updated_timestamp_s: d.u64(261)?,
        };
        if pool.tick_spacing == 0 {
            return Err(d.error("tick spacing is zero"));
        }
        if pool.fee_rate > FEE_RATE_MAX || pool.protocol_fee_rate > PROTOCOL_FEE_RATE_MAX {
            return Err(d.error(format!(
                "fee rate {} or protocol fee rate {} above the program's maximum",
                pool.fee_rate, pool.protocol_fee_rate
            )));
        }
        if !(MIN_SQRT_PRICE_X64..=MAX_SQRT_PRICE_X64).contains(&pool.sqrt_price_x64) {
            return Err(d.error(format!("sqrt price {} out of range", pool.sqrt_price_x64)));
        }
        if !(MIN_TICK_INDEX..=MAX_TICK_INDEX).contains(&pool.tick_current_index) {
            return Err(d.error(format!("tick {} out of range", pool.tick_current_index)));
        }
        Ok(pool)
    }

    /// Whether the pool was created from an adaptive fee tier
    /// (`whirlpool.rs`, `is_initialized_with_adaptive_fee_tier`): such a
    /// pool has an oracle account, and its fee moves with volatility.
    pub fn is_adaptive_fee_tier(&self) -> bool {
        self.fee_tier_index != self.tick_spacing
    }

    /// Ticks one tick array spans.
    pub fn ticks_in_array(&self) -> i32 {
        TICK_ARRAY_SIZE as i32 * i32::from(self.tick_spacing)
    }
}

/// Which layout a tick array has on chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickArrayLayout {
    /// `TickArray`: 88 ticks of 113 bytes.
    Fixed,
    /// `DynamicTickArray`: a bitmap and variable-size ticks.
    Dynamic,
    /// No account at the array's address. The program reads such an array
    /// as 88 uninitialized ticks (`zeroed_tick_array.rs`), provided the
    /// swap passes its address.
    Uninitialized,
}

/// A tick array's start tick and the net liquidity of its initialized
/// ticks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickArray {
    pub whirlpool: Pubkey,
    /// The first tick the array holds; it holds 88 ticks a tick spacing
    /// apart.
    pub start_tick_index: i32,
    pub layout: TickArrayLayout,
    /// Each tick's net liquidity, `None` when the tick is uninitialized,
    /// by offset from the start.
    pub liquidity_net: [Option<i128>; TICK_ARRAY_SIZE],
}

impl TickArray {
    /// Decodes a `TickArray` or `DynamicTickArray` account.
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let d = Reader::new(data, "whirlpool tick array")?;
        let discriminator = d.array::<8>(0)?;
        if discriminator == FIXED_TICK_ARRAY_DISCRIMINATOR {
            Self::decode_fixed(&d)
        } else if discriminator == DYNAMIC_TICK_ARRAY_DISCRIMINATOR {
            Self::decode_dynamic(&d)
        } else {
            Err(d.error("not a fixed or dynamic tick array"))
        }
    }

    /// The array at a tick-array address with no account: every tick
    /// uninitialized.
    pub fn uninitialized(whirlpool: Pubkey, start_tick_index: i32) -> Self {
        Self {
            whirlpool,
            start_tick_index,
            layout: TickArrayLayout::Uninitialized,
            liquidity_net: [None; TICK_ARRAY_SIZE],
        }
    }

    /// How many of its ticks are initialized.
    pub fn initialized_count(&self) -> usize {
        self.liquidity_net
            .iter()
            .filter(|tick| tick.is_some())
            .count()
    }

    fn decode_fixed(d: &Reader<'_>) -> Result<Self, DecodeError> {
        d.expect_len(FIXED_TICK_ARRAY_SIZE)?;
        let mut liquidity_net = [None; TICK_ARRAY_SIZE];
        for (offset, tick) in liquidity_net.iter_mut().enumerate() {
            let at = 12 + offset * TICK_SIZE;
            *tick = match d.u8(at)? {
                0 => None,
                1 => Some(d.i128(at + 1)?),
                other => return Err(d.error(format!("tick {offset} has initialized flag {other}"))),
            };
        }
        Ok(Self {
            whirlpool: d.key(12 + TICK_SIZE * TICK_ARRAY_SIZE)?,
            start_tick_index: d.i32(8)?,
            layout: TickArrayLayout::Fixed,
            liquidity_net,
        })
    }

    fn decode_dynamic(d: &Reader<'_>) -> Result<Self, DecodeError> {
        let bitmap = d.u128(44)?;
        if bitmap >> TICK_ARRAY_SIZE != 0 {
            return Err(d.error(format!("bitmap {bitmap:#x} marks ticks past the 88th")));
        }
        let mut liquidity_net = [None; TICK_ARRAY_SIZE];
        let mut at = DYNAMIC_TICKS_OFFSET;
        for (offset, tick) in liquidity_net.iter_mut().enumerate() {
            let is_marked = bitmap & (1 << offset) != 0;
            match (d.u8(at)?, is_marked) {
                (0, false) => at += 1,
                (1, true) => {
                    *tick = Some(d.i128(at + 1)?);
                    at += 1 + DYNAMIC_TICK_DATA_SIZE;
                    if at > d.data.len() {
                        return Err(d.error(format!("tick {offset} ends past the account")));
                    }
                }
                (tag, _) => {
                    return Err(d.error(format!(
                        "tick {offset} has tag {tag}, the bitmap says {is_marked}"
                    )));
                }
            }
        }
        Ok(Self {
            whirlpool: d.key(12)?,
            start_tick_index: d.i32(8)?,
            layout: TickArrayLayout::Dynamic,
            liquidity_net,
        })
    }
}

/// A pool's adaptive fee (`oracle.rs`, `AdaptiveFeeConstants` and
/// `AdaptiveFeeVariables`), and when it opens to trading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Oracle {
    pub whirlpool: Pubkey,
    /// No swap before this, in Unix seconds.
    pub trade_enable_timestamp_s: u64,
    /// Seconds within which swaps count as one burst.
    pub filter_period_s: u16,
    /// Seconds after which volatility is forgotten.
    pub decay_period_s: u16,
    /// How much volatility survives a pause, over 10,000.
    pub reduction_factor: u16,
    /// Scales volatility into a fee, over 100,000.
    pub adaptive_fee_control_factor: u32,
    pub max_volatility_accumulator: u32,
    /// Ticks per tick group, the unit volatility is counted in.
    pub tick_group_size: u16,
    pub major_swap_threshold_ticks: u16,
    pub last_reference_update_timestamp_s: u64,
    pub last_major_swap_timestamp_s: u64,
    pub volatility_reference: u32,
    pub tick_group_index_reference: i32,
    pub volatility_accumulator: u32,
}

impl Oracle {
    /// Decodes an `Oracle` account.
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let d = Reader::new(data, "whirlpool oracle")?;
        d.expect_len(ORACLE_SIZE)?;
        d.expect_discriminator(&ORACLE_DISCRIMINATOR)?;
        let oracle = Self {
            whirlpool: d.key(8)?,
            trade_enable_timestamp_s: d.u64(40)?,
            filter_period_s: d.u16(48)?,
            decay_period_s: d.u16(50)?,
            reduction_factor: d.u16(52)?,
            adaptive_fee_control_factor: d.u32(54)?,
            max_volatility_accumulator: d.u32(58)?,
            tick_group_size: d.u16(62)?,
            major_swap_threshold_ticks: d.u16(64)?,
            last_reference_update_timestamp_s: d.u64(82)?,
            last_major_swap_timestamp_s: d.u64(90)?,
            volatility_reference: d.u32(98)?,
            tick_group_index_reference: d.i32(102)?,
            volatility_accumulator: d.u32(106)?,
        };
        // The program validates these when it sets them
        // (`AdaptiveFeeConstants::validate_constants`); the fee math
        // divides by the tick group size and relies on the bound.
        if oracle.tick_group_size == 0 {
            return Err(d.error("tick group size is zero"));
        }
        if oracle.volatility_reference > oracle.max_volatility_accumulator {
            return Err(d.error("volatility reference above its maximum"));
        }
        Ok(oracle)
    }
}

/// Reads little-endian fields at byte offsets, failing with the account's
/// name when it is too short.
struct Reader<'a> {
    data: &'a [u8],
    what: &'static str,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], what: &'static str) -> Result<Self, DecodeError> {
        if data.len() < 8 {
            return Err(DecodeError::new(
                what,
                format!("{} bytes, no discriminator", data.len()),
            ));
        }
        Ok(Self { data, what })
    }

    fn error(&self, detail: impl Into<String>) -> DecodeError {
        DecodeError::new(self.what, detail)
    }

    fn expect_len(&self, len: usize) -> Result<(), DecodeError> {
        if self.data.len() != len {
            return Err(self.error(format!("{} bytes, expected {len}", self.data.len())));
        }
        Ok(())
    }

    fn expect_discriminator(&self, discriminator: &[u8; 8]) -> Result<(), DecodeError> {
        if self.array::<8>(0)? != *discriminator {
            return Err(self.error("wrong discriminator"));
        }
        Ok(())
    }

    fn array<const N: usize>(&self, at: usize) -> Result<[u8; N], DecodeError> {
        self.data
            .get(at..at + N)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| self.error(format!("account ends before byte {}", at + N)))
    }

    fn u8(&self, at: usize) -> Result<u8, DecodeError> {
        Ok(self.array::<1>(at)?[0])
    }

    fn u16(&self, at: usize) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array(at)?))
    }

    fn u32(&self, at: usize) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array(at)?))
    }

    fn i32(&self, at: usize) -> Result<i32, DecodeError> {
        Ok(i32::from_le_bytes(self.array(at)?))
    }

    fn u64(&self, at: usize) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array(at)?))
    }

    fn u128(&self, at: usize) -> Result<u128, DecodeError> {
        Ok(u128::from_le_bytes(self.array(at)?))
    }

    fn i128(&self, at: usize) -> Result<i128, DecodeError> {
        Ok(i128::from_le_bytes(self.array(at)?))
    }

    fn key(&self, at: usize) -> Result<Pubkey, DecodeError> {
        Ok(Pubkey::new_from_array(self.array(at)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_array(start_tick_index: i32, ticks: &[(usize, i128)]) -> Vec<u8> {
        let mut data = vec![0u8; FIXED_TICK_ARRAY_SIZE];
        data[..8].copy_from_slice(&FIXED_TICK_ARRAY_DISCRIMINATOR);
        data[8..12].copy_from_slice(&start_tick_index.to_le_bytes());
        for &(offset, net) in ticks {
            let at = 12 + offset * TICK_SIZE;
            data[at] = 1;
            data[at + 1..at + 17].copy_from_slice(&net.to_le_bytes());
        }
        data
    }

    fn dynamic_array(start_tick_index: i32, ticks: &[(usize, i128)]) -> Vec<u8> {
        let mut data = DYNAMIC_TICK_ARRAY_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&start_tick_index.to_le_bytes());
        data.extend_from_slice(&[7u8; 32]);
        let bitmap: u128 = ticks.iter().map(|&(offset, _)| 1u128 << offset).sum();
        data.extend_from_slice(&bitmap.to_le_bytes());
        for offset in 0..TICK_ARRAY_SIZE {
            match ticks.iter().find(|&&(o, _)| o == offset) {
                Some(&(_, net)) => {
                    data.push(1);
                    data.extend_from_slice(&net.to_le_bytes());
                    data.extend_from_slice(&[0u8; DYNAMIC_TICK_DATA_SIZE - 16]);
                }
                None => data.push(0),
            }
        }
        data
    }

    #[test]
    fn both_layouts_decode_to_the_same_ticks() {
        let ticks = [(0, 5), (3, -7), (87, i128::MIN + 1)];
        let fixed = TickArray::decode(&fixed_array(-704, &ticks)).unwrap();
        let dynamic = TickArray::decode(&dynamic_array(-704, &ticks)).unwrap();
        assert_eq!(fixed.layout, TickArrayLayout::Fixed);
        assert_eq!(dynamic.layout, TickArrayLayout::Dynamic);
        assert_eq!(fixed.start_tick_index, -704);
        assert_eq!(fixed.liquidity_net, dynamic.liquidity_net);
        assert_eq!(fixed.initialized_count(), 3);
        assert_eq!(dynamic.liquidity_net[3], Some(-7));
        assert_eq!(dynamic.whirlpool, Pubkey::new_from_array([7; 32]));
    }

    #[test]
    fn a_dynamic_array_whose_bitmap_disagrees_with_its_ticks_is_refused() {
        let mut data = dynamic_array(0, &[(2, 9)]);
        data[44] = 0b1; // marks tick 0, whose tag says uninitialized
        assert!(TickArray::decode(&data).is_err());
        let truncated = dynamic_array(0, &[(87, 9)]);
        assert!(TickArray::decode(&truncated[..truncated.len() - 1]).is_err());
    }

    #[test]
    fn accounts_of_the_wrong_shape_are_refused() {
        assert!(TickArray::decode(&[0u8; 4]).is_err());
        assert!(TickArray::decode(&[0u8; FIXED_TICK_ARRAY_SIZE]).is_err());
        let mut short = fixed_array(0, &[]);
        short.pop();
        assert!(TickArray::decode(&short).is_err());
        let mut flag = fixed_array(0, &[(1, 1)]);
        flag[12 + TICK_SIZE] = 2;
        assert!(TickArray::decode(&flag).is_err());
        assert!(Whirlpool::decode(&[0u8; WHIRLPOOL_SIZE]).is_err());
        assert!(Oracle::decode(&[0u8; ORACLE_SIZE]).is_err());
    }
}
