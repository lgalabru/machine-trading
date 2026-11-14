//! Meteora DLMM accounts, decoded from their bytes at the IDL's offsets
//! (each account is the 8-byte Anchor discriminator, then the struct,
//! packed).

use solana_pubkey::Pubkey;

use super::BINS_PER_ARRAY;
use crate::{
    protocols::{DecodeError, Fields},
    token::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID},
};

const LB_PAIR: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13];
const BIN_ARRAY: [u8; 8] = [92, 142, 92, 220, 5, 148, 70, 181];
const POSITION_V2: [u8; 8] = [117, 176, 212, 199, 245, 180, 133, 182];

/// A pool, as far as a trader reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LbPair {
    pub address: Pubkey,
    /// The fee: `base_factor × bin_step × 10 × 10^base_fee_power_factor`
    /// billionths, plus the variable fee from the volatility accumulator.
    pub base_factor: u16,
    pub base_fee_power_factor: u8,
    pub variable_fee_control: u32,
    pub volatility_accumulator: u32,
    /// Whether swaps pay their fee in the token in (0) or always in Y (1).
    pub collect_fee_mode: u8,
    pub active_id: i32,
    /// Basis points between neighbouring bins' prices.
    pub bin_step: u16,
    /// 0 is enabled.
    pub status: u8,
    pub mint_x: Pubkey,
    pub mint_y: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub oracle: Pubkey,
    /// Which bin arrays around index 0 hold liquidity: bit 512 is array 0.
    pub bin_array_bitmap: [u64; 16],
    /// The token programs of X and Y.
    pub program_x: Pubkey,
    pub program_y: Pubkey,
}

impl LbPair {
    /// The account's size.
    pub const LEN: usize = 904;

    pub fn decode(address: Pubkey, data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new("meteora dlmm pair", data, LB_PAIR, Self::LEN)?;
        let mut bin_array_bitmap = [0u64; 16];
        for (i, word) in bin_array_bitmap.iter_mut().enumerate() {
            *word = f.u64(584 + 8 * i)?;
        }
        let program = |flag: u8| match flag {
            0 => Ok(TOKEN_PROGRAM_ID),
            1 => Ok(TOKEN_2022_PROGRAM_ID),
            other => Err(DecodeError::new(
                "meteora dlmm pair",
                format!("token program flag {other}"),
            )),
        };
        Ok(Self {
            address,
            base_factor: f.u16(8)?,
            variable_fee_control: f.u32(16)?,
            base_fee_power_factor: f.u8(34)?,
            collect_fee_mode: f.u8(36)?,
            volatility_accumulator: f.u32(40)?,
            active_id: f.i32(76)?,
            bin_step: f.u16(80)?,
            status: f.u8(82)?,
            mint_x: f.pubkey(88)?,
            mint_y: f.pubkey(120)?,
            reserve_x: f.pubkey(152)?,
            reserve_y: f.pubkey(184)?,
            oracle: f.pubkey(552)?,
            bin_array_bitmap,
            program_x: program(f.u8(880)?)?,
            program_y: program(f.u8(881)?)?,
        })
    }

    /// The swap fee now, in billionths of the amount: the base fee and the
    /// variable fee, capped at 10%.
    pub fn fee_rate(&self) -> u128 {
        let base = u128::from(self.base_factor)
            * u128::from(self.bin_step)
            * 10
            * 10u128.pow(u32::from(self.base_fee_power_factor));
        let variable = if self.variable_fee_control > 0 {
            let square =
                (u128::from(self.volatility_accumulator) * u128::from(self.bin_step)).pow(2);
            (u128::from(self.variable_fee_control) * square).div_ceil(100_000_000_000)
        } else {
            0
        };
        (base + variable).min(100_000_000)
    }

    /// The swap fee now, as a share of the amount.
    pub fn fee(&self) -> f64 {
        self.fee_rate() as f64 / 1e9
    }

    /// Whether the bin array `index` holds liquidity, where the pool's own
    /// bitmap reaches it; `None` where it doesn't.
    pub fn bin_array_has_liquidity(&self, index: i64) -> Option<bool> {
        let bit = index + 512;
        if !(0..1024).contains(&bit) {
            return None;
        }
        let bit = bit as usize;
        Some(self.bin_array_bitmap[bit / 64] >> (bit % 64) & 1 == 1)
    }

    /// The first `count` bin arrays with liquidity a swap crosses, from the
    /// active bin's: down for a swap of X in (`swap_for_y`), up otherwise.
    /// Only arrays the pool's own bitmap reaches.
    pub fn swap_bin_arrays(&self, swap_for_y: bool, count: usize) -> Vec<i64> {
        let mut index = super::bin_array_index(self.active_id);
        let mut found = Vec::new();
        while found.len() < count {
            match self.bin_array_has_liquidity(index) {
                Some(true) => found.push(index),
                Some(false) => {}
                None => break,
            }
            index += if swap_for_y { -1 } else { 1 };
        }
        found
    }
}

/// One bin: its reserves, and its liquidity's shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bin {
    /// The bin's market-making reserves, in atoms.
    pub amount_x: u64,
    pub amount_y: u64,
    /// The shares its liquidity is split into.
    pub liquidity_supply: u128,
    /// Fees per share since the bin began, in Q64.64.
    pub fee_amount_x_per_token_stored: u128,
    pub fee_amount_y_per_token_stored: u128,
}

/// Seventy consecutive bins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinArray {
    pub index: i64,
    pub pair: Pubkey,
    pub bins: Vec<Bin>,
}

impl BinArray {
    /// The account's size.
    pub const LEN: usize = 10136;
    const BIN_LEN: usize = 144;

    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new("meteora dlmm bin array", data, BIN_ARRAY, Self::LEN)?;
        let bins = (0..BINS_PER_ARRAY as usize)
            .map(|i| {
                let at = 56 + i * Self::BIN_LEN;
                Ok(Bin {
                    amount_x: f.u64(at)?,
                    amount_y: f.u64(at + 8)?,
                    liquidity_supply: f.u128(at + 32)?,
                    fee_amount_x_per_token_stored: f.u128(at + 80)?,
                    fee_amount_y_per_token_stored: f.u128(at + 96)?,
                })
            })
            .collect::<Result<_, DecodeError>>()?;
        Ok(Self {
            index: f.i64(8)?,
            pair: f.pubkey(24)?,
            bins,
        })
    }

    /// The bin `id`, if the array holds it.
    pub fn bin(&self, id: i32) -> Option<&Bin> {
        let offset = i64::from(id) - self.index * i64::from(BINS_PER_ARRAY);
        self.bins.get(usize::try_from(offset).ok()?)
    }
}

/// One bin of a position: its shares, and the fees it had counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PositionBin {
    pub shares: u128,
    pub fee_x_per_token_complete: u128,
    pub fee_y_per_token_complete: u128,
    pub fee_x_pending: u64,
    pub fee_y_pending: u64,
}

/// A liquidity position over a range of bins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub address: Pubkey,
    pub pair: Pubkey,
    pub owner: Pubkey,
    pub lower_bin_id: i32,
    pub upper_bin_id: i32,
    /// From `lower_bin_id` up.
    pub bins: Vec<PositionBin>,
}

impl Position {
    /// The account's size.
    pub const LEN: usize = 8120;
    /// Where the pair and the owner sit, for filtering a program's accounts.
    pub const PAIR_OFFSET: usize = 8;
    pub const OWNER_OFFSET: usize = 40;

    pub fn decode(address: Pubkey, data: &[u8]) -> Result<Self, DecodeError> {
        let f = Fields::new("meteora dlmm position", data, POSITION_V2, Self::LEN)?;
        let lower_bin_id = f.i32(7912)?;
        let upper_bin_id = f.i32(7916)?;
        let width = usize::try_from(upper_bin_id - lower_bin_id + 1)
            .ok()
            .filter(|w| *w <= BINS_PER_ARRAY as usize)
            .ok_or_else(|| {
                DecodeError::new(
                    "meteora dlmm position",
                    format!("bins {lower_bin_id} to {upper_bin_id}"),
                )
            })?;
        let bins = (0..width)
            .map(|i| {
                let fee = 4552 + i * 48;
                Ok(PositionBin {
                    shares: f.u128(72 + i * 16)?,
                    fee_x_per_token_complete: f.u128(fee)?,
                    fee_y_per_token_complete: f.u128(fee + 16)?,
                    fee_x_pending: f.u64(fee + 32)?,
                    fee_y_pending: f.u64(fee + 40)?,
                })
            })
            .collect::<Result<_, DecodeError>>()?;
        Ok(Self {
            address,
            pair: f.pubkey(Self::PAIR_OFFSET)?,
            owner: f.pubkey(Self::OWNER_OFFSET)?,
            lower_bin_id,
            upper_bin_id,
            bins,
        })
    }

    /// The bin arrays its instructions name: the one holding its lowest bin
    /// and the next, which between them hold any position's range.
    pub fn bin_arrays(&self) -> Vec<i64> {
        super::position_bin_arrays(self.lower_bin_id)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Accounts' bytes for tests, at the decoders' offsets.

    use super::*;

    pub fn pair_bytes(mint_x: Pubkey, mint_y: Pubkey, active_id: i32, bin_step: u16) -> Vec<u8> {
        let mut data = vec![0u8; LbPair::LEN];
        data[..8].copy_from_slice(&LB_PAIR);
        data[8..10].copy_from_slice(&10_000u16.to_le_bytes());
        data[76..80].copy_from_slice(&active_id.to_le_bytes());
        data[80..82].copy_from_slice(&bin_step.to_le_bytes());
        data[88..120].copy_from_slice(mint_x.as_ref());
        data[120..152].copy_from_slice(mint_y.as_ref());
        data[880] = 1;
        data
    }

    pub fn set_bitmap_bit(data: &mut [u8], bit: usize) {
        let at = 584 + 8 * (bit / 64);
        let mut word = u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
        word |= 1 << (bit % 64);
        data[at..at + 8].copy_from_slice(&word.to_le_bytes());
    }

    pub fn bin_array_bytes(pair: Pubkey, index: i64, bins: &[(usize, Bin)]) -> Vec<u8> {
        let mut data = vec![0u8; BinArray::LEN];
        data[..8].copy_from_slice(&BIN_ARRAY);
        data[8..16].copy_from_slice(&index.to_le_bytes());
        data[24..56].copy_from_slice(pair.as_ref());
        for (i, bin) in bins {
            let at = 56 + i * BinArray::BIN_LEN;
            data[at..at + 8].copy_from_slice(&bin.amount_x.to_le_bytes());
            data[at + 8..at + 16].copy_from_slice(&bin.amount_y.to_le_bytes());
            data[at + 32..at + 48].copy_from_slice(&bin.liquidity_supply.to_le_bytes());
            data[at + 80..at + 96]
                .copy_from_slice(&bin.fee_amount_x_per_token_stored.to_le_bytes());
            data[at + 96..at + 112]
                .copy_from_slice(&bin.fee_amount_y_per_token_stored.to_le_bytes());
        }
        data
    }

    pub fn position_bytes(
        pair: Pubkey,
        owner: Pubkey,
        lower: i32,
        bins: &[PositionBin],
    ) -> Vec<u8> {
        let mut data = vec![0u8; Position::LEN];
        data[..8].copy_from_slice(&POSITION_V2);
        data[8..40].copy_from_slice(pair.as_ref());
        data[40..72].copy_from_slice(owner.as_ref());
        for (i, bin) in bins.iter().enumerate() {
            data[72 + i * 16..88 + i * 16].copy_from_slice(&bin.shares.to_le_bytes());
            let fee = 4552 + i * 48;
            data[fee..fee + 16].copy_from_slice(&bin.fee_x_per_token_complete.to_le_bytes());
            data[fee + 16..fee + 32].copy_from_slice(&bin.fee_y_per_token_complete.to_le_bytes());
            data[fee + 32..fee + 40].copy_from_slice(&bin.fee_x_pending.to_le_bytes());
            data[fee + 40..fee + 48].copy_from_slice(&bin.fee_y_pending.to_le_bytes());
        }
        data[7912..7916].copy_from_slice(&lower.to_le_bytes());
        let upper = lower + bins.len() as i32 - 1;
        data[7916..7920].copy_from_slice(&upper.to_le_bytes());
        data
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};
    use crate::protocols::{SPCX_MINT, USDC_MINT};

    #[test]
    fn a_pair_decodes_its_fee_and_the_arrays_a_swap_crosses() {
        let mut data = pair_bytes(SPCX_MINT, USDC_MINT, 662, 75);
        for index in [8, 9, 10] {
            set_bitmap_bit(&mut data, index + 512);
        }
        let pair = LbPair::decode(Pubkey::new_unique(), &data).unwrap();
        assert_eq!(pair.program_x, TOKEN_2022_PROGRAM_ID);
        assert_eq!(pair.program_y, TOKEN_PROGRAM_ID);
        // The live SPCX/USDC pool's base fee: 0.75%.
        assert_eq!(pair.fee_rate(), 7_500_000);
        assert_eq!(pair.swap_bin_arrays(true, 3), [9, 8]);
        assert_eq!(pair.swap_bin_arrays(false, 3), [9, 10]);
        assert_eq!(pair.bin_array_has_liquidity(600), None);
    }

    #[test]
    fn a_position_decodes_its_bins() {
        let (pair, owner) = (Pubkey::new_unique(), Pubkey::new_unique());
        let bin = PositionBin {
            shares: 5 << 64,
            fee_y_pending: 7,
            ..PositionBin::default()
        };
        let data = position_bytes(pair, owner, 660, &[bin, bin, bin]);
        let position = Position::decode(Pubkey::new_unique(), &data).unwrap();
        assert_eq!((position.lower_bin_id, position.upper_bin_id), (660, 662));
        assert_eq!(position.bins.len(), 3);
        assert_eq!(position.bins[2].fee_y_pending, 7);
        assert_eq!(position.owner, owner);
        assert_eq!(position.bin_arrays(), [9, 10]);
    }
}
