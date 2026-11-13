//! Raydium CLMM accounts, decoded from their bytes at the IDL's offsets
//! (each account is the 8-byte Anchor discriminator, then the struct,
//! packed).

use solana_pubkey::Pubkey;

use crate::protocols::{DecodeError, Fields};

const POOL_STATE: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70];
const AMM_CONFIG: [u8; 8] = [218, 244, 33, 104, 203, 203, 43, 111];
const PERSONAL_POSITION: [u8; 8] = [70, 111, 150, 126, 230, 15, 25, 117];
const TICK_ARRAY: [u8; 8] = [192, 155, 85, 205, 49, 249, 129, 42];

/// A pool, as far as a trader reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub address: Pubkey,
    pub amm_config: Pubkey,
    pub mint_0: Pubkey,
    pub mint_1: Pubkey,
    pub vault_0: Pubkey,
    pub vault_1: Pubkey,
    pub observation: Pubkey,
    pub decimals_0: u8,
    pub decimals_1: u8,
    pub tick_spacing: u16,
    /// The liquidity active at the current price.
    pub liquidity: u128,
    /// √(token 1 atoms per token 0 atom), in Q64.64.
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
    /// Fees per unit of liquidity since the pool began, in Q64.64.
    pub fee_growth_global_0_x64: u128,
    pub fee_growth_global_1_x64: u128,
    /// Bits that stop the pool's operations, each its own: 0 is open.
    pub status: u8,
    /// Which tick arrays around tick 0 hold initialized ticks: bit 512 is
    /// the array starting at 0.
    pub tick_array_bitmap: [u64; 16],
    /// The mint and vault of each reward the pool was ever given: taking
    /// liquidity out names them all.
    pub rewards: Vec<(Pubkey, Pubkey)>,
}

impl PoolState {
    /// The account's size.
    pub const LEN: usize = 1544;

    pub fn decode(address: Pubkey, data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new("raydium clmm pool", data, POOL_STATE, Self::LEN)?;
        let mut tick_array_bitmap = [0u64; 16];
        for (i, word) in tick_array_bitmap.iter_mut().enumerate() {
            *word = f.u64(904 + 8 * i)?;
        }
        Ok(Self {
            address,
            amm_config: f.pubkey(9)?,
            mint_0: f.pubkey(73)?,
            mint_1: f.pubkey(105)?,
            vault_0: f.pubkey(137)?,
            vault_1: f.pubkey(169)?,
            observation: f.pubkey(201)?,
            decimals_0: f.u8(233)?,
            decimals_1: f.u8(234)?,
            tick_spacing: f.u16(235)?,
            liquidity: f.u128(237)?,
            sqrt_price_x64: f.u128(253)?,
            tick_current: f.i32(269)?,
            fee_growth_global_0_x64: f.u128(277)?,
            fee_growth_global_1_x64: f.u128(293)?,
            status: f.u8(389)?,
            tick_array_bitmap,
            rewards: (0..3)
                .map(|i| Ok((f.pubkey(397 + 169 * i + 57)?, f.pubkey(397 + 169 * i + 89)?)))
                .filter(|r| !matches!(r, Ok((mint, _)) if *mint == Pubkey::default()))
                .collect::<Result<_, DecodeError>>()?,
        })
    }

    /// Whether `base` is the pool's token 0 (else its token 1).
    pub fn base_is_0(&self, base: &Pubkey) -> bool {
        self.mint_0 == *base
    }

    /// Token 1 per token 0, in whole units.
    pub fn price_1_per_0(&self) -> f64 {
        super::math::price_from_sqrt_x64(self.sqrt_price_x64, self.decimals_0, self.decimals_1)
    }

    /// The pool's price of `base`, in whole quote units per base unit.
    pub fn price(&self, base: &Pubkey) -> f64 {
        let price = self.price_1_per_0();
        if self.base_is_0(base) {
            price
        } else {
            1.0 / price
        }
    }

    /// Whether the tick array starting at `start` holds initialized ticks,
    /// where the pool's own bitmap reaches it; `None` where it doesn't.
    pub fn tick_array_initialized(&self, start: i32) -> Option<bool> {
        let span = super::TICK_ARRAY_SIZE * i32::from(self.tick_spacing);
        let bit = start.div_euclid(span) + 512;
        if !(0..1024).contains(&bit) {
            return None;
        }
        let bit = bit as usize;
        Some(self.tick_array_bitmap[bit / 64] >> (bit % 64) & 1 == 1)
    }

    /// The start ticks of the first `count` initialized tick arrays a swap
    /// crosses, from the current tick: down for a swap of token 0 in
    /// (`zero_for_one`), up otherwise. Only arrays the pool's own bitmap
    /// reaches.
    pub fn swap_tick_arrays(&self, zero_for_one: bool, count: usize) -> Vec<i32> {
        let span = super::TICK_ARRAY_SIZE * i32::from(self.tick_spacing);
        let mut start = super::tick_array_start(self.tick_current, self.tick_spacing);
        let mut found = Vec::new();
        while found.len() < count {
            match self.tick_array_initialized(start) {
                Some(true) => found.push(start),
                Some(false) => {}
                None => break,
            }
            start += if zero_for_one { -span } else { span };
        }
        found
    }
}

/// A pool's fee tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmmConfig {
    /// The swap fee, in millionths of the amount in.
    pub trade_fee_rate: u32,
    pub tick_spacing: u16,
}

impl AmmConfig {
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new("raydium clmm amm config", data, AMM_CONFIG, 117)?;
        Ok(Self {
            trade_fee_rate: f.u32(47)?,
            tick_spacing: f.u16(51)?,
        })
    }

    /// The swap fee, as a share of the amount in.
    pub fn fee(&self) -> f64 {
        f64::from(self.trade_fee_rate) / 1e6
    }
}

/// A liquidity position, owned by whoever holds its NFT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalPosition {
    pub address: Pubkey,
    pub nft_mint: Pubkey,
    pub pool: Pubkey,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: u128,
    /// Fees per unit of liquidity inside the range when last counted.
    pub fee_growth_inside_0_last_x64: u128,
    pub fee_growth_inside_1_last_x64: u128,
    /// Fees counted and not yet collected, in atoms.
    pub fees_owed_0: u64,
    pub fees_owed_1: u64,
}

impl PersonalPosition {
    /// The account's size.
    pub const LEN: usize = 281;

    pub fn decode(address: Pubkey, data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new("raydium clmm position", data, PERSONAL_POSITION, Self::LEN)?;
        Ok(Self {
            address,
            nft_mint: f.pubkey(9)?,
            pool: f.pubkey(41)?,
            tick_lower: f.i32(73)?,
            tick_upper: f.i32(77)?,
            liquidity: f.u128(81)?,
            fee_growth_inside_0_last_x64: f.u128(97)?,
            fee_growth_inside_1_last_x64: f.u128(113)?,
            fees_owed_0: f.u64(129)?,
            fees_owed_1: f.u64(137)?,
        })
    }
}

/// One tick of a tick array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tick {
    pub tick: i32,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
    /// Fees per unit of liquidity on the other side of the tick, in Q64.64.
    pub fee_growth_outside_0_x64: u128,
    pub fee_growth_outside_1_x64: u128,
}

/// Sixty consecutive ticks, `tick_spacing` apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickArray {
    pub pool: Pubkey,
    pub start_tick_index: i32,
    pub ticks: Vec<Tick>,
}

impl TickArray {
    /// The account's size.
    pub const LEN: usize = 10240;
    const TICK_LEN: usize = 168;

    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new("raydium clmm tick array", data, TICK_ARRAY, Self::LEN)?;
        let ticks = (0..super::TICK_ARRAY_SIZE as usize)
            .map(|i| {
                let at = 44 + i * Self::TICK_LEN;
                Ok(Tick {
                    tick: f.i32(at)?,
                    liquidity_net: f.i128(at + 4)?,
                    liquidity_gross: f.u128(at + 20)?,
                    fee_growth_outside_0_x64: f.u128(at + 36)?,
                    fee_growth_outside_1_x64: f.u128(at + 52)?,
                })
            })
            .collect::<Result<_, DecodeError>>()?;
        Ok(Self {
            pool: f.pubkey(8)?,
            start_tick_index: f.i32(40)?,
            ticks,
        })
    }

    /// The tick at `index`, if the array holds it; an uninitialized tick
    /// reads as zeros.
    pub fn tick(&self, index: i32, tick_spacing: u16) -> Option<Tick> {
        let offset = (index - self.start_tick_index).checked_div(i32::from(tick_spacing))?;
        if (index - self.start_tick_index) % i32::from(tick_spacing) != 0 {
            return None;
        }
        self.ticks.get(usize::try_from(offset).ok()?).copied()
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Accounts' bytes for tests, at the decoders' offsets.

    use super::*;

    pub fn pool_bytes(mint_0: Pubkey, mint_1: Pubkey, sqrt_price_x64: u128, tick: i32) -> Vec<u8> {
        let mut data = vec![0u8; PoolState::LEN];
        data[..8].copy_from_slice(&POOL_STATE);
        data[73..105].copy_from_slice(mint_0.as_ref());
        data[105..137].copy_from_slice(mint_1.as_ref());
        data[233] = 6;
        data[234] = 6;
        data[235..237].copy_from_slice(&60u16.to_le_bytes());
        data[237..253].copy_from_slice(&1_000_000u128.to_le_bytes());
        data[253..269].copy_from_slice(&sqrt_price_x64.to_le_bytes());
        data[269..273].copy_from_slice(&tick.to_le_bytes());
        data
    }

    pub fn set_bitmap_bit(data: &mut [u8], bit: usize) {
        let at = 904 + 8 * (bit / 64);
        let mut word = u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
        word |= 1 << (bit % 64);
        data[at..at + 8].copy_from_slice(&word.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};
    use crate::protocols::{SPCX_MINT, USDC_MINT};

    #[test]
    fn a_pool_decodes_and_prices_its_base_either_way() {
        // 140.27 USDC a SPCX: √140.27 × 2^64.
        let sqrt = (140.27f64.sqrt() * 2f64.powi(64)) as u128;
        let mut data = pool_bytes(SPCX_MINT, USDC_MINT, sqrt, 49438);
        let pool = PoolState::decode(Pubkey::new_unique(), &data).unwrap();
        assert_eq!(pool.tick_spacing, 60);
        assert!((pool.price(&SPCX_MINT) - 140.27).abs() < 1e-9);
        assert!((pool.price(&USDC_MINT) - 1.0 / 140.27).abs() < 1e-12);
        assert_eq!(pool.tick_array_initialized(46800), Some(false));

        // The arrays at and below the current tick, then above it.
        for start in [46800, 43200, 50400] {
            set_bitmap_bit(&mut data, (start / 3600 + 512) as usize);
        }
        let pool = PoolState::decode(Pubkey::new_unique(), &data).unwrap();
        assert_eq!(pool.swap_tick_arrays(true, 3), [46800, 43200]);
        assert_eq!(pool.swap_tick_arrays(false, 1), [46800]);
        assert_eq!(pool.swap_tick_arrays(false, 3), [46800, 50400]);

        assert!(pool.rewards.is_empty());

        // A reward: its mint and vault, in the second of three slots.
        let (mint, vault) = (Pubkey::new_unique(), Pubkey::new_unique());
        let at = 397 + 169 + 57;
        data[at..at + 32].copy_from_slice(mint.as_ref());
        data[at + 32..at + 64].copy_from_slice(vault.as_ref());
        let pool = PoolState::decode(Pubkey::new_unique(), &data).unwrap();
        assert_eq!(pool.rewards, [(mint, vault)]);

        data[0] ^= 1;
        assert!(PoolState::decode(Pubkey::new_unique(), &data).is_err());
        assert!(PoolState::decode(Pubkey::new_unique(), &data[..100]).is_err());
    }
}
