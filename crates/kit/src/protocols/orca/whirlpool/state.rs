//! Whirlpool accounts, decoded from their bytes at the program's offsets
//! (each account is the 8-byte Anchor discriminator, then the struct,
//! packed).

use solana_pubkey::Pubkey;

use crate::protocols::{DecodeError, Fields};

const WHIRLPOOL: [u8; 8] = [63, 149, 209, 12, 225, 128, 99, 9];
const POSITION: [u8; 8] = [170, 188, 143, 228, 122, 64, 247, 208];
const FIXED_TICK_ARRAY: [u8; 8] = [69, 97, 189, 190, 110, 7, 66, 187];
const DYNAMIC_TICK_ARRAY: [u8; 8] = [17, 216, 246, 142, 225, 199, 218, 56];

/// A pool, as far as a trader reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub address: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub tick_spacing: u16,
    /// The swap fee, in hundredths of a basis point of the amount in. A pool
    /// with an adaptive fee charges more in volatile moments; this is its
    /// base.
    pub fee_rate: u16,
    /// The liquidity active at the current price.
    pub liquidity: u128,
    /// √(token B atoms per token A atom), in Q64.64.
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
    /// Fees per unit of liquidity since the pool began, in Q64.64.
    pub fee_growth_global_a_x64: u128,
    pub fee_growth_global_b_x64: u128,
}

impl PoolState {
    /// The account's size.
    pub const LEN: usize = 653;

    pub fn decode(address: Pubkey, data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new("orca whirlpool", data, WHIRLPOOL, Self::LEN)?;
        Ok(Self {
            address,
            tick_spacing: f.u16(41)?,
            fee_rate: f.u16(45)?,
            liquidity: f.u128(49)?,
            sqrt_price_x64: f.u128(65)?,
            tick_current: f.i32(81)?,
            mint_a: f.pubkey(101)?,
            vault_a: f.pubkey(133)?,
            fee_growth_global_a_x64: f.u128(165)?,
            mint_b: f.pubkey(181)?,
            vault_b: f.pubkey(213)?,
            fee_growth_global_b_x64: f.u128(245)?,
        })
    }

    /// Whether `base` is the pool's token A (else its token B).
    pub fn base_is_a(&self, base: &Pubkey) -> bool {
        self.mint_a == *base
    }

    /// The swap fee as a share of the amount in: the base rate.
    pub fn fee(&self) -> f64 {
        f64::from(self.fee_rate) / 1e6
    }

    /// Token B per token A, in whole units.
    pub fn price_b_per_a(&self, decimals_a: u8, decimals_b: u8) -> f64 {
        super::math::price_from_sqrt_x64(self.sqrt_price_x64, decimals_a, decimals_b)
    }

    /// The start ticks of the three tick arrays a swap passes, from the
    /// current tick: down for a swap of token A in (`a_to_b`), up otherwise.
    /// They are consecutive whether or not they exist: the program takes
    /// arrays that don't, and fails a swap only if it must cross one. Fewer
    /// than three at the ends of the tick range.
    pub fn swap_tick_arrays(&self, a_to_b: bool) -> Vec<i32> {
        let span = super::TICK_ARRAY_SIZE * i32::from(self.tick_spacing);
        let spacing = i32::from(self.tick_spacing);
        // Selling token B in moves the price up to the next tick, which may
        // be the first of the next array.
        let shifted = self.tick_current + if a_to_b { 0 } else { spacing };
        let first = shifted.div_euclid(span);
        let lowest = super::math::MIN_TICK.div_euclid(span);
        let highest = super::math::MAX_TICK.div_euclid(span);
        (0..3)
            .map(|i| first + if a_to_b { -i } else { i })
            .filter(|index| (lowest..=highest).contains(index))
            .map(|index| index * span)
            .collect()
    }
}

/// A liquidity position, owned by whoever holds its NFT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub address: Pubkey,
    pub pool: Pubkey,
    pub position_mint: Pubkey,
    pub liquidity: u128,
    pub tick_lower: i32,
    pub tick_upper: i32,
    /// Fees per unit of liquidity inside the range when last counted.
    pub fee_growth_checkpoint_a_x64: u128,
    pub fee_growth_checkpoint_b_x64: u128,
    /// Fees counted and not yet collected, in atoms.
    pub fee_owed_a: u64,
    pub fee_owed_b: u64,
}

impl Position {
    /// The account's size.
    pub const LEN: usize = 216;

    pub fn decode(address: Pubkey, data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new("orca position", data, POSITION, Self::LEN)?;
        Ok(Self {
            address,
            pool: f.pubkey(8)?,
            position_mint: f.pubkey(40)?,
            liquidity: f.u128(72)?,
            tick_lower: f.i32(88)?,
            tick_upper: f.i32(92)?,
            fee_growth_checkpoint_a_x64: f.u128(96)?,
            fee_owed_a: f.u64(112)?,
            fee_growth_checkpoint_b_x64: f.u128(120)?,
            fee_owed_b: f.u64(136)?,
        })
    }
}

/// One tick of a tick array. An uninitialized tick reads as zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tick {
    pub tick: i32,
    pub initialized: bool,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
    /// Fees per unit of liquidity on the other side of the tick, in Q64.64.
    pub fee_growth_outside_a_x64: u128,
    pub fee_growth_outside_b_x64: u128,
}

/// Eighty-eight consecutive ticks, `tick_spacing` apart. The program keeps
/// them in two layouts, both read here: fixed (every tick takes its 113
/// bytes) and dynamic (an uninitialized tick takes one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickArray {
    pub pool: Pubkey,
    pub start_tick_index: i32,
    ticks: Vec<Option<TickData>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TickData {
    liquidity_net: i128,
    liquidity_gross: u128,
    fee_growth_outside_a_x64: u128,
    fee_growth_outside_b_x64: u128,
}

impl TickArray {
    /// A fixed array's size.
    pub const FIXED_LEN: usize = 9988;
    const FIXED_TICK_LEN: usize = 113;
    /// The discriminator, start tick, pool and bitmap.
    const DYNAMIC_HEADER: usize = 60;
    const DYNAMIC_TICK_LEN: usize = 113;

    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        if data.get(..8) == Some(&DYNAMIC_TICK_ARRAY) {
            Self::decode_dynamic(data)
        } else {
            Self::decode_fixed(data)
        }
    }

    fn data_at(f: &Fields, at: usize) -> Result<TickData, DecodeError> {
        Ok(TickData {
            liquidity_net: f.i128(at)?,
            liquidity_gross: f.u128(at + 16)?,
            fee_growth_outside_a_x64: f.u128(at + 32)?,
            fee_growth_outside_b_x64: f.u128(at + 48)?,
        })
    }

    fn decode_fixed(data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new("orca tick array", data, FIXED_TICK_ARRAY, Self::FIXED_LEN)?;
        let ticks = (0..super::TICK_ARRAY_SIZE as usize)
            .map(|i| {
                let at = 12 + i * Self::FIXED_TICK_LEN;
                (f.u8(at)? != 0)
                    .then(|| Self::data_at(&f, at + 1))
                    .transpose()
            })
            .collect::<Result<_, DecodeError>>()?;
        Ok(Self {
            start_tick_index: f.i32(8)?,
            pool: f.pubkey(Self::FIXED_LEN - 32)?,
            ticks,
        })
    }

    fn decode_dynamic(data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new(
            "orca tick array",
            data,
            DYNAMIC_TICK_ARRAY,
            Self::DYNAMIC_HEADER + super::TICK_ARRAY_SIZE as usize,
        )?;
        let bitmap = f.u128(44)?;
        let mut at = Self::DYNAMIC_HEADER;
        let mut ticks = Vec::new();
        for i in 0..super::TICK_ARRAY_SIZE as usize {
            if bitmap >> i & 1 == 1 {
                if f.u8(at)? != 1 {
                    return Err(DecodeError::new(
                        "orca tick array",
                        format!("tick {i} is in the bitmap but not initialized"),
                    ));
                }
                ticks.push(Some(Self::data_at(&f, at + 1)?));
                at += Self::DYNAMIC_TICK_LEN;
            } else {
                ticks.push(None);
                at += 1;
            }
        }
        Ok(Self {
            start_tick_index: f.i32(8)?,
            pool: f.pubkey(12)?,
            ticks,
        })
    }

    /// The tick at `index`, if the array holds it; an uninitialized tick
    /// reads as zeros.
    pub fn tick(&self, index: i32, tick_spacing: u16) -> Option<Tick> {
        let spacing = i32::from(tick_spacing);
        let offset = index - self.start_tick_index;
        if offset < 0 || offset % spacing != 0 {
            return None;
        }
        let slot = self.ticks.get(usize::try_from(offset / spacing).ok()?)?;
        Some(match slot {
            Some(t) => Tick {
                tick: index,
                initialized: true,
                liquidity_net: t.liquidity_net,
                liquidity_gross: t.liquidity_gross,
                fee_growth_outside_a_x64: t.fee_growth_outside_a_x64,
                fee_growth_outside_b_x64: t.fee_growth_outside_b_x64,
            },
            None => Tick {
                tick: index,
                ..Tick::default()
            },
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Accounts' bytes for tests, at the decoders' offsets.

    use super::*;

    pub fn pool_bytes(mint_a: Pubkey, mint_b: Pubkey, sqrt_price_x64: u128, tick: i32) -> Vec<u8> {
        let mut data = vec![0u8; PoolState::LEN];
        data[..8].copy_from_slice(&WHIRLPOOL);
        data[41..43].copy_from_slice(&64u16.to_le_bytes());
        data[45..47].copy_from_slice(&3000u16.to_le_bytes());
        data[49..65].copy_from_slice(&1_000_000u128.to_le_bytes());
        data[65..81].copy_from_slice(&sqrt_price_x64.to_le_bytes());
        data[81..85].copy_from_slice(&tick.to_le_bytes());
        data[101..133].copy_from_slice(mint_a.as_ref());
        data[181..213].copy_from_slice(mint_b.as_ref());
        data
    }

    /// A dynamic tick array of the pool, starting at `start`, with `ticks`
    /// (offset, liquidity net, fee growth outside a) initialized.
    pub fn dynamic_array_bytes(pool: Pubkey, start: i32, ticks: &[(usize, i128, u128)]) -> Vec<u8> {
        let mut bitmap = 0u128;
        for (offset, ..) in ticks {
            bitmap |= 1 << offset;
        }
        let mut data = DYNAMIC_TICK_ARRAY.to_vec();
        data.extend_from_slice(&start.to_le_bytes());
        data.extend_from_slice(pool.as_ref());
        data.extend_from_slice(&bitmap.to_le_bytes());
        for i in 0..88usize {
            match ticks.iter().find(|(offset, ..)| *offset == i) {
                Some((_, net, growth)) => {
                    data.push(1);
                    data.extend_from_slice(&net.to_le_bytes());
                    data.extend_from_slice(&(net.unsigned_abs()).to_le_bytes());
                    data.extend_from_slice(&growth.to_le_bytes());
                    data.extend_from_slice(&[0u8; 16 + 48]);
                }
                None => data.push(0),
            }
        }
        data
    }

    /// The same ticks in a fixed array.
    pub fn fixed_array_bytes(pool: Pubkey, start: i32, ticks: &[(usize, i128, u128)]) -> Vec<u8> {
        let mut data = FIXED_TICK_ARRAY.to_vec();
        data.extend_from_slice(&start.to_le_bytes());
        for i in 0..88usize {
            match ticks.iter().find(|(offset, ..)| *offset == i) {
                Some((_, net, growth)) => {
                    data.push(1);
                    data.extend_from_slice(&net.to_le_bytes());
                    data.extend_from_slice(&(net.unsigned_abs()).to_le_bytes());
                    data.extend_from_slice(&growth.to_le_bytes());
                    data.extend_from_slice(&[0u8; 16 + 48]);
                }
                None => data.extend_from_slice(&[0u8; 113]),
            }
        }
        data.extend_from_slice(pool.as_ref());
        data
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};
    use crate::protocols::{SPCX_MINT, USDC_MINT};

    #[test]
    fn a_pool_decodes_and_prices_its_base_either_way() {
        // 141.1 USDC a SPCX: √141.1 × 2^64.
        let sqrt = (141.1f64.sqrt() * 2f64.powi(64)) as u128;
        let mut data = pool_bytes(SPCX_MINT, USDC_MINT, sqrt, 49497);
        let pool = PoolState::decode(Pubkey::new_unique(), &data).unwrap();
        assert_eq!((pool.tick_spacing, pool.fee_rate), (64, 3000));
        assert!(pool.base_is_a(&SPCX_MINT) && !pool.base_is_a(&USDC_MINT));
        assert!((pool.fee() - 0.003).abs() < 1e-12);
        assert!((pool.price_b_per_a(6, 6) - 141.1).abs() < 1e-9);

        // Down for a sale of A: the arrays at and below the tick; up for a
        // sale of B: from the array the next tick is in.
        assert_eq!(pool.swap_tick_arrays(true), [45056, 39424, 33792]);
        assert_eq!(pool.swap_tick_arrays(false), [45056, 50688, 56320]);
        let mut edge = pool.clone();
        edge.tick_current = 5632 * 3 - 64;
        assert_eq!(edge.swap_tick_arrays(false)[0], 16896);
        edge.tick_current = 5632 * 3 - 65;
        assert_eq!(edge.swap_tick_arrays(false)[0], 11264);
        edge.tick_current = 443_000;
        assert_eq!(edge.swap_tick_arrays(false).len(), 1);

        data[0] ^= 1;
        assert!(PoolState::decode(Pubkey::new_unique(), &data).is_err());
        assert!(PoolState::decode(Pubkey::new_unique(), &data[..100]).is_err());
    }

    #[test]
    fn a_position_decodes() {
        let (pool, mint) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut data = vec![0u8; Position::LEN];
        data[..8].copy_from_slice(&POSITION);
        data[8..40].copy_from_slice(pool.as_ref());
        data[40..72].copy_from_slice(mint.as_ref());
        data[72..88].copy_from_slice(&5u128.to_le_bytes());
        data[88..92].copy_from_slice(&(-128i32).to_le_bytes());
        data[92..96].copy_from_slice(&192i32.to_le_bytes());
        data[112..120].copy_from_slice(&7u64.to_le_bytes());
        data[136..144].copy_from_slice(&9u64.to_le_bytes());
        let position = Position::decode(Pubkey::new_unique(), &data).unwrap();
        assert_eq!(
            (position.pool, position.position_mint, position.liquidity),
            (pool, mint, 5)
        );
        assert_eq!((position.tick_lower, position.tick_upper), (-128, 192));
        assert_eq!((position.fee_owed_a, position.fee_owed_b), (7, 9));
    }

    #[test]
    fn both_tick_array_layouts_read_the_same_ticks() {
        let pool = Pubkey::new_unique();
        let ticks = [(3usize, 500i128, 11u128), (40, -500, 22), (87, 7, 33)];
        for data in [
            dynamic_array_bytes(pool, -5632, &ticks),
            fixed_array_bytes(pool, -5632, &ticks),
        ] {
            let array = TickArray::decode(&data).unwrap();
            assert_eq!((array.pool, array.start_tick_index), (pool, -5632));
            let tick = array.tick(-5632 + 64 * 40, 64).unwrap();
            assert!(tick.initialized);
            assert_eq!(
                (tick.liquidity_net, tick.fee_growth_outside_a_x64),
                (-500, 22)
            );
            assert_eq!(array.tick(-5632 + 64 * 87, 64).unwrap().liquidity_net, 7);
            let empty = array.tick(-5632 + 64 * 4, 64).unwrap();
            assert!(!empty.initialized && empty.fee_growth_outside_a_x64 == 0);
            // Off the grid, or another array's.
            assert_eq!(array.tick(-5632 + 1, 64), None);
            assert_eq!(array.tick(0, 64), None);
        }
        let mut bad = dynamic_array_bytes(pool, 0, &ticks);
        // The first initialized tick follows three uninitialized ones.
        bad[TickArray::DYNAMIC_HEADER + 3] = 0;
        assert!(TickArray::decode(&bad).is_err());
        assert!(TickArray::decode(&bad[..100]).is_err());
    }
}
