//! Raydium CLMM accounts: the pool, its fee config, its tick arrays and its
//! tick array bitmap extension, and how a swap finds the next initialized
//! tick array.
//!
//! All four are Anchor zero-copy accounts (`#[repr(C, packed)]`): an 8-byte
//! discriminator, then the fields in declaration order, unpadded. Offsets
//! are from `programs/amm/src/states/{pool,pool_fee,config,tick_array,
//! tickarray_bitmap_extension}.rs` at
//! `raydium-io/raydium-clmm@6d7a0f82d2f293c553c69aaf943377367c6c4cc6`, and
//! were checked against the SOL/USDC pool's accounts on mainnet.
//!
//! Which tick arrays exist is a bitmap: one bit per array, 1,024 of them in
//! the pool around tick 0, and the extension's 14 × 512 on either side
//! beyond that, which only pools with a tick spacing under 15 can reach.

use solana_pubkey::Pubkey;

use super::{ClmmError, math};
use crate::protocols::DecodeError;

/// A tick array holds 60 ticks (`TICK_ARRAY_SIZE`).
pub const TICK_ARRAY_SIZE: usize = 60;
/// Arrays the pool's own bitmap covers, half below tick 0
/// (`TICK_ARRAY_BITMAP_SIZE` is the half).
const BITMAP_HALF_ARRAYS: i32 = 512;
/// Bitmaps on each side in the extension (`EXTENSION_TICKARRAY_BITMAP_SIZE`).
const EXTENSION_BITMAPS: usize = 14;
/// Bitmap searches a lookup for the next tick array makes at most: the
/// pool's bitmap and the extension's 14 per side, alternately, with margin.
const BITMAP_SEARCHES_MAX: usize = 2 * (EXTENSION_BITMAPS + 2);

pub const POOL_SIZE: usize = 1544;
pub const AMM_CONFIG_SIZE: usize = 117;
pub const TICK_ARRAY_ACCOUNT_SIZE: usize = 10240;
pub const BITMAP_EXTENSION_SIZE: usize = 1832;
const TICK_SIZE: usize = 168;
const TICKS_OFFSET: usize = 44;

/// sha256("account:PoolState")[..8], and so on: Anchor's discriminators.
const POOL_DISCRIMINATOR: [u8; 8] = [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];
const AMM_CONFIG_DISCRIMINATOR: [u8; 8] = [0xda, 0xf4, 0x21, 0x68, 0xcb, 0xcb, 0x2b, 0x6f];
const TICK_ARRAY_DISCRIMINATOR: [u8; 8] = [0xc0, 0x9b, 0x55, 0xcd, 0x31, 0xf9, 0x81, 0x2a];
const BITMAP_EXTENSION_DISCRIMINATOR: [u8; 8] = [0x3c, 0x96, 0x24, 0xdb, 0x61, 0x80, 0x8b, 0x99];

/// Which token the pool takes its trading fee in (`CollectFeeOn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeOn {
    /// The swap's input token: every pool created before fee options.
    Input,
    Token0,
    Token1,
}

/// One of a pool's two tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolToken {
    Token0,
    Token1,
}

/// A pool's dynamic fee (`DynamicFeeInfo`): a fee that grows with how far
/// the price has moved recently. All zero on a pool without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DynamicFee {
    pub filter_period_s: u16,
    pub decay_period_s: u16,
    /// Of 10,000.
    pub reduction_factor: u16,
    /// Of 100,000.
    pub dynamic_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub tick_spacing_index_reference: i32,
    pub volatility_reference: u32,
    pub volatility_accumulator: u32,
    pub last_update_timestamp_s: u64,
}

/// A pool (`PoolState`): what a swap reads of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub amm_config: Pubkey,
    pub token_mint_0: Pubkey,
    pub token_mint_1: Pubkey,
    pub token_vault_0: Pubkey,
    pub token_vault_1: Pubkey,
    /// The observation account `swap_v2` must be given.
    pub observation: Pubkey,
    pub mint_decimals_0: u8,
    pub mint_decimals_1: u8,
    pub tick_spacing: u16,
    pub liquidity: u128,
    /// `sqrt(token_1 / token_0)` in atoms, Q64.64.
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
    /// Bit 4 set disables swaps.
    pub status: u8,
    pub fee_on: FeeOn,
    /// The pool's bitmap of initialized tick arrays.
    pub tick_array_bitmap: [u64; 16],
    /// Swaps are refused until after this time, in Unix seconds.
    pub open_time_s: u64,
    /// `None` on a pool without a dynamic fee (all its fields zero).
    pub dynamic_fee: Option<DynamicFee>,
}

/// A pool's fee config (`AmmConfig`). Rates are millionths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmmConfig {
    pub trade_fee_rate: u32,
    /// The protocol's share of the trade fee, in millionths of the fee.
    pub protocol_fee_rate: u32,
    /// The fund's share of the trade fee, in millionths of the fee.
    pub fund_fee_rate: u32,
    pub tick_spacing: u16,
}

/// What a swap reads of a tick (`TickState`): liquidity, and the limit
/// orders resting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tick {
    pub tick: i32,
    /// Liquidity added when the price crosses the tick upward.
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
    /// Limit orders never matched, in the token they sell.
    pub orders_amount: u64,
    /// What is left of the partly matched cohort.
    pub part_filled_orders_remaining: u64,
}

impl Tick {
    pub fn has_liquidity(&self) -> bool {
        self.liquidity_gross > 0
    }

    pub fn has_limit_orders(&self) -> bool {
        self.orders_amount > 0 || self.part_filled_orders_remaining > 0
    }

    /// Whether the swap stops at the tick (`is_initialized`).
    pub fn is_initialized(&self) -> bool {
        self.has_liquidity() || self.has_limit_orders()
    }

    pub fn limit_order_unfilled_amount(&self) -> Result<u64, ClmmError> {
        self.orders_amount
            .checked_add(self.part_filled_orders_remaining)
            .ok_or(ClmmError::Math("limit order amounts"))
    }
}

/// A tick array (`TickArrayState`): 60 ticks from `start_tick_index`, one
/// tick spacing apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickArray {
    pub pool: Pubkey,
    pub start_tick_index: i32,
    pub ticks: [Tick; TICK_ARRAY_SIZE],
    pub initialized_tick_count: u8,
}

/// The pool's tick array bitmap extension (`TickArrayBitmapExtension`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapExtension {
    pub pool: Pubkey,
    pub positive: [[u64; 8]; EXTENSION_BITMAPS],
    pub negative: [[u64; 8]; EXTENSION_BITMAPS],
}

/// An account's bytes, of a size checked once, so that every read below is
/// in bounds.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn new<'a>(
        data: &'a [u8],
        what: &'static str,
        size: usize,
        discriminator: &[u8; 8],
    ) -> Result<Bytes<'a>, DecodeError> {
        if data.len() != size {
            return Err(DecodeError::new(
                what,
                format!("{} bytes, expected {size}", data.len()),
            ));
        }
        if data[..8] != discriminator[..] {
            return Err(DecodeError::new(what, "wrong discriminator"));
        }
        Ok(Bytes(data))
    }

    fn array<const N: usize>(&self, at: usize) -> [u8; N] {
        // Every offset below is inside the size `new` checked.
        self.0[at..at + N]
            .try_into()
            .expect("offset inside the checked size")
    }

    fn u8(&self, at: usize) -> u8 {
        self.0[at]
    }

    fn u16(&self, at: usize) -> u16 {
        u16::from_le_bytes(self.array(at))
    }

    fn u32(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.array(at))
    }

    fn i32(&self, at: usize) -> i32 {
        i32::from_le_bytes(self.array(at))
    }

    fn u64(&self, at: usize) -> u64 {
        u64::from_le_bytes(self.array(at))
    }

    fn u128(&self, at: usize) -> u128 {
        u128::from_le_bytes(self.array(at))
    }

    fn i128(&self, at: usize) -> i128 {
        i128::from_le_bytes(self.array(at))
    }

    fn key(&self, at: usize) -> Pubkey {
        Pubkey::new_from_array(self.array(at))
    }

    fn words<const N: usize>(&self, at: usize) -> [u64; N] {
        std::array::from_fn(|i| self.u64(at + 8 * i))
    }
}

impl Pool {
    /// Decodes a `PoolState` account.
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let what = "raydium clmm pool";
        let d = Bytes::new(data, what, POOL_SIZE, &POOL_DISCRIMINATOR)?;
        let fee_on = match d.u8(390) {
            0 => FeeOn::Input,
            1 => FeeOn::Token0,
            2 => FeeOn::Token1,
            // The program reads any other value as `FromInput`, but
            // `set_fee_on` never writes one: refuse what we don't expect.
            other => return Err(DecodeError::new(what, format!("fee_on {other}"))),
        };
        let tick_spacing = d.u16(235);
        if tick_spacing == 0 {
            return Err(DecodeError::new(what, "zero tick spacing"));
        }
        let dynamic_fee = DynamicFee {
            filter_period_s: d.u16(1096),
            decay_period_s: d.u16(1098),
            reduction_factor: d.u16(1100),
            dynamic_fee_control: d.u32(1102),
            max_volatility_accumulator: d.u32(1106),
            tick_spacing_index_reference: d.i32(1110),
            volatility_reference: d.u32(1114),
            volatility_accumulator: d.u32(1118),
            last_update_timestamp_s: d.u64(1122),
        };
        // The program compares the whole struct, padding included, to its
        // default; the padding is never written, so the fields decide.
        let has_dynamic_fee =
            dynamic_fee != DynamicFee::default() || data[1130..1176].iter().any(|&b| b != 0);
        Ok(Self {
            amm_config: d.key(9),
            token_mint_0: d.key(73),
            token_mint_1: d.key(105),
            token_vault_0: d.key(137),
            token_vault_1: d.key(169),
            observation: d.key(201),
            mint_decimals_0: d.u8(233),
            mint_decimals_1: d.u8(234),
            tick_spacing,
            liquidity: d.u128(237),
            sqrt_price_x64: d.u128(253),
            tick_current: d.i32(269),
            status: d.u8(389),
            fee_on,
            tick_array_bitmap: d.words(904),
            open_time_s: d.u64(1080),
            dynamic_fee: has_dynamic_fee.then_some(dynamic_fee),
        })
    }

    /// Whether swaps are enabled (`get_status_by_bit(Swap)`).
    pub fn is_swap_enabled(&self) -> bool {
        self.status & (1 << 4) == 0
    }

    /// `is_fee_on_input`.
    pub fn is_fee_on_input(&self, zero_for_one: bool) -> bool {
        match self.fee_on {
            FeeOn::Input => true,
            FeeOn::Token0 => zero_for_one,
            FeeOn::Token1 => !zero_for_one,
        }
    }

    /// The token the trading fee is taken in (`is_fee_on_token0`).
    pub fn fee_token(&self, zero_for_one: bool) -> PoolToken {
        let is_token_0 = match self.fee_on {
            FeeOn::Input => zero_for_one,
            FeeOn::Token0 => true,
            FeeOn::Token1 => false,
        };
        if is_token_0 {
            PoolToken::Token0
        } else {
            PoolToken::Token1
        }
    }

    /// Ticks in one tick array.
    pub fn ticks_per_array(&self) -> i32 {
        TICK_ARRAY_SIZE as i32 * i32::from(self.tick_spacing)
    }
}

impl AmmConfig {
    /// Decodes an `AmmConfig` account.
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let d = Bytes::new(
            data,
            "raydium clmm amm config",
            AMM_CONFIG_SIZE,
            &AMM_CONFIG_DISCRIMINATOR,
        )?;
        let config = Self {
            protocol_fee_rate: d.u32(43),
            trade_fee_rate: d.u32(47),
            tick_spacing: d.u16(51),
            fund_fee_rate: d.u32(53),
        };
        if config.trade_fee_rate >= math::FEE_RATE_DENOMINATOR
            || config.protocol_fee_rate > math::FEE_RATE_DENOMINATOR
            || config.fund_fee_rate > math::FEE_RATE_DENOMINATOR
        {
            return Err(DecodeError::new(
                "raydium clmm amm config",
                format!("fee rates {config:?}"),
            ));
        }
        Ok(config)
    }
}

impl TickArray {
    /// Decodes a `TickArrayState` account.
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let what = "raydium clmm tick array";
        let d = Bytes::new(
            data,
            what,
            TICK_ARRAY_ACCOUNT_SIZE,
            &TICK_ARRAY_DISCRIMINATOR,
        )?;
        let ticks = std::array::from_fn(|i| {
            let at = TICKS_OFFSET + i * TICK_SIZE;
            Tick {
                tick: d.i32(at),
                liquidity_net: d.i128(at + 4),
                liquidity_gross: d.u128(at + 20),
                orders_amount: d.u64(at + 124),
                part_filled_orders_remaining: d.u64(at + 132),
            }
        });
        Ok(Self {
            pool: d.key(8),
            start_tick_index: d.i32(40),
            ticks,
            initialized_tick_count: d.u8(TICKS_OFFSET + TICK_ARRAY_SIZE * TICK_SIZE),
        })
    }

    /// `next_initialized_tick`: the offset of the next tick the price stops
    /// at, from `tick_current` in the swap's direction (inclusive going
    /// down), if it is in this array.
    pub(crate) fn next_initialized_offset(
        &self,
        tick_current: i32,
        tick_spacing: u16,
        zero_for_one: bool,
    ) -> Option<usize> {
        if array_start_index(tick_current, tick_spacing) != self.start_tick_index {
            return None;
        }
        let offset = ((tick_current - self.start_tick_index) / i32::from(tick_spacing)) as usize;
        assert!(offset < TICK_ARRAY_SIZE);
        if zero_for_one {
            (0..=offset).rev().find(|&i| self.ticks[i].is_initialized())
        } else {
            (offset + 1..TICK_ARRAY_SIZE).find(|&i| self.ticks[i].is_initialized())
        }
    }

    /// `first_initialized_tick`: the first tick the price stops at entering
    /// the array.
    pub(crate) fn first_initialized_offset(&self, zero_for_one: bool) -> Result<usize, ClmmError> {
        let found = if zero_for_one {
            (0..TICK_ARRAY_SIZE)
                .rev()
                .find(|&i| self.ticks[i].is_initialized())
        } else {
            (0..TICK_ARRAY_SIZE).find(|&i| self.ticks[i].is_initialized())
        };
        found.ok_or(ClmmError::InvalidTick(
            "tick array without an initialized tick",
        ))
    }

    /// `get_tick_offset_in_array`.
    pub(crate) fn offset_of(&self, tick: i32, tick_spacing: u16) -> Result<usize, ClmmError> {
        if array_start_index(tick, tick_spacing) != self.start_tick_index {
            return Err(ClmmError::InvalidTick("tick outside its tick array"));
        }
        Ok(((tick - self.start_tick_index) / i32::from(tick_spacing)) as usize)
    }
}

impl BitmapExtension {
    /// Decodes a `TickArrayBitmapExtension` account.
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let d = Bytes::new(
            data,
            "raydium clmm tick array bitmap extension",
            BITMAP_EXTENSION_SIZE,
            &BITMAP_EXTENSION_DISCRIMINATOR,
        )?;
        Ok(Self {
            pool: d.key(8),
            positive: std::array::from_fn(|i| d.words(40 + 64 * i)),
            negative: std::array::from_fn(|i| d.words(40 + 64 * EXTENSION_BITMAPS + 64 * i)),
        })
    }

    /// `get_bitmap_offset` and `get_bitmap`.
    fn bitmap(&self, start_index: i32, tick_spacing: u16) -> Result<&[u64; 8], ClmmError> {
        if !is_valid_start_index(start_index, tick_spacing) {
            return Err(ClmmError::InvalidTick("tick array start index"));
        }
        let ticks_in_bitmap = ticks_in_one_bitmap(tick_spacing);
        if ticks_in_bitmap >= math::MAX_TICK
            || (-ticks_in_bitmap..ticks_in_bitmap).contains(&start_index)
        {
            return Err(ClmmError::InvalidTick(
                "tick array inside the pool's own bitmap",
            ));
        }
        let mut offset = start_index.abs() / ticks_in_bitmap - 1;
        if start_index < 0 && start_index.abs() % ticks_in_bitmap == 0 {
            offset -= 1;
        }
        let offset =
            usize::try_from(offset).map_err(|_| ClmmError::InvalidTick("bitmap offset"))?;
        let bitmaps = if start_index < 0 {
            &self.negative
        } else {
            &self.positive
        };
        bitmaps
            .get(offset)
            .ok_or(ClmmError::InvalidTick("past the extension's bitmaps"))
    }

    /// `check_tick_array_is_initialized`.
    fn is_initialized(&self, start_index: i32, tick_spacing: u16) -> Result<bool, ClmmError> {
        let bitmap = self.bitmap(start_index, tick_spacing)?;
        Ok(bit(
            bitmap,
            offset_in_extension_bitmap(start_index, tick_spacing),
        ))
    }

    /// `next_initialized_tick_array_from_one_bitmap`: `(found, start)`;
    /// when not found, `start` is where the search continues.
    fn next_from_one_bitmap(
        &self,
        last_start_index: i32,
        tick_spacing: u16,
        zero_for_one: bool,
    ) -> Result<(bool, i32), ClmmError> {
        let ticks_per_array = ticks_per_array(tick_spacing);
        let next_start = if zero_for_one {
            last_start_index - ticks_per_array
        } else {
            last_start_index + ticks_per_array
        };
        if next_start < array_start_index(math::MIN_TICK, tick_spacing)
            || next_start > array_start_index(math::MAX_TICK, tick_spacing)
        {
            return Ok((false, next_start));
        }
        let bitmap = self.bitmap(next_start, tick_spacing)?;
        let (bitmap_min, bitmap_max) = bitmap_tick_boundary(next_start, tick_spacing);
        let offset = offset_in_extension_bitmap(next_start, tick_spacing);
        if zero_for_one {
            let found = (0..=offset).rev().find(|&i| bit(bitmap, i));
            Ok(match found {
                Some(i) => (true, next_start - (offset - i) as i32 * ticks_per_array),
                None => (false, bitmap_min),
            })
        } else {
            let found = (offset..512).find(|&i| bit(bitmap, i));
            Ok(match found {
                Some(i) => (true, next_start + (i - offset) as i32 * ticks_per_array),
                None => (false, bitmap_max - ticks_per_array),
            })
        }
    }
}

fn bit<const N: usize>(words: &[u64; N], index: usize) -> bool {
    assert!(index < 64 * N);
    words[index / 64] >> (index % 64) & 1 == 1
}

fn ticks_per_array(tick_spacing: u16) -> i32 {
    TICK_ARRAY_SIZE as i32 * i32::from(tick_spacing)
}

/// `max_tick_in_tickarray_bitmap`: ticks one 512-array bitmap spans.
fn ticks_in_one_bitmap(tick_spacing: u16) -> i32 {
    ticks_per_array(tick_spacing) * BITMAP_HALF_ARRAYS
}

/// `TickArrayState::get_array_start_index`: the start of the tick array
/// holding `tick`.
pub fn array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    tick.div_euclid(ticks_per_array(tick_spacing)) * ticks_per_array(tick_spacing)
}

/// `check_is_valid_start_index`.
fn is_valid_start_index(start_index: i32, tick_spacing: u16) -> bool {
    if !(math::MIN_TICK..=math::MAX_TICK).contains(&start_index) {
        if start_index > math::MAX_TICK {
            return false;
        }
        return start_index == array_start_index(math::MIN_TICK, tick_spacing);
    }
    start_index % ticks_per_array(tick_spacing) == 0
}

/// `get_bitmap_tick_boundary`.
fn bitmap_tick_boundary(start_index: i32, tick_spacing: u16) -> (i32, i32) {
    let ticks_in_bitmap = ticks_in_one_bitmap(tick_spacing);
    let mut m = start_index.abs() / ticks_in_bitmap;
    if start_index < 0 && start_index.abs() % ticks_in_bitmap != 0 {
        m += 1;
    }
    let min_value = ticks_in_bitmap * m;
    if start_index < 0 {
        (-min_value, -min_value + ticks_in_bitmap)
    } else {
        (min_value, min_value + ticks_in_bitmap)
    }
}

/// `tick_array_offset_in_bitmap`.
fn offset_in_extension_bitmap(start_index: i32, tick_spacing: u16) -> usize {
    let m = start_index.abs() % ticks_in_one_bitmap(tick_spacing);
    let mut offset = m / ticks_per_array(tick_spacing);
    if start_index < 0 && m != 0 {
        offset = BITMAP_HALF_ARRAYS - offset;
    }
    assert!((0..BITMAP_HALF_ARRAYS).contains(&offset));
    offset as usize
}

/// `tick_array_start_index_range`: the start indexes the pool's own bitmap
/// covers, `[min, max)`.
fn default_bitmap_range(tick_spacing: u16) -> (i32, i32) {
    let mut max = ticks_in_one_bitmap(tick_spacing);
    let mut min = -max;
    if max > math::MAX_TICK {
        max = array_start_index(math::MAX_TICK, tick_spacing) + ticks_per_array(tick_spacing);
    }
    if min < math::MIN_TICK {
        min = array_start_index(math::MIN_TICK, tick_spacing);
    }
    (min, max)
}

/// `is_overflow_default_tickarray_bitmap` for one tick: whether its tick
/// array is beyond the pool's own bitmap, in the extension.
pub fn is_in_extension(tick: i32, tick_spacing: u16) -> bool {
    let (min, max) = default_bitmap_range(tick_spacing);
    let start = array_start_index(tick, tick_spacing);
    start >= max || start < min
}

/// The bit of the pool's own bitmap for a tick's array, and that array's
/// start (`check_current_tick_array_is_initialized`'s arithmetic).
fn default_bitmap_position(tick: i32, tick_spacing: u16) -> (i32, i32) {
    let multiplier = ticks_per_array(tick_spacing);
    let compressed = tick.div_euclid(multiplier) + BITMAP_HALF_ARRAYS;
    (compressed, (compressed - BITMAP_HALF_ARRAYS) * multiplier)
}

/// `next_initialized_tick_array_start_index` over the pool's own bitmap.
fn next_in_default_bitmap(
    bitmap: &[u64; 16],
    last_start_index: i32,
    tick_spacing: u16,
    zero_for_one: bool,
) -> Result<(bool, i32), ClmmError> {
    if !is_valid_start_index(last_start_index, tick_spacing) {
        return Err(ClmmError::InvalidTick("tick array start index"));
    }
    let boundary = ticks_in_one_bitmap(tick_spacing);
    let multiplier = ticks_per_array(tick_spacing);
    let next_start = if zero_for_one {
        last_start_index - multiplier
    } else {
        last_start_index + multiplier
    };
    if next_start < -boundary || next_start >= boundary {
        return Ok((false, last_start_index));
    }
    let (bit_position, _) = default_bitmap_position(next_start, tick_spacing);
    let bit_position = bit_position as usize;
    if zero_for_one {
        Ok(match (0..=bit_position).rev().find(|&i| bit(bitmap, i)) {
            Some(i) => (true, (i as i32 - BITMAP_HALF_ARRAYS) * multiplier),
            None => (false, -boundary),
        })
    } else {
        Ok(match (bit_position..1024).find(|&i| bit(bitmap, i)) {
            Some(i) => (true, (i as i32 - BITMAP_HALF_ARRAYS) * multiplier),
            None => (false, boundary - multiplier),
        })
    }
}

/// Where a swap starts and how it finds the tick arrays after: the pool's
/// bitmap and, past it, the extension.
pub(crate) struct ArrayNavigator<'a> {
    pub pool: &'a Pool,
    pub extension: Option<&'a BitmapExtension>,
    /// Set once a lookup reads the extension.
    pub used_extension: bool,
}

impl ArrayNavigator<'_> {
    fn extension(&mut self) -> Result<&BitmapExtension, ClmmError> {
        self.used_extension = true;
        self.extension.ok_or(ClmmError::MissingBitmapExtension)
    }

    /// `first_tick_array_index_with_extension_info`: whether the pool's
    /// current tick array is initialized, and the first initialized one
    /// in the swap's direction.
    pub fn first(&mut self, zero_for_one: bool) -> Result<(bool, i32), ClmmError> {
        let tick_current = self.pool.tick_current;
        let tick_spacing = self.pool.tick_spacing;
        if !(math::MIN_TICK..=math::MAX_TICK).contains(&tick_current) {
            return Err(ClmmError::InvalidTick("pool tick out of range"));
        }
        let current_start = array_start_index(tick_current, tick_spacing);
        let is_initialized = if is_in_extension(tick_current, tick_spacing) {
            self.extension()?
                .is_initialized(current_start, tick_spacing)?
        } else {
            let (bit_position, start) = default_bitmap_position(tick_current, tick_spacing);
            debug_assert_eq!(start, current_start);
            bit(&self.pool.tick_array_bitmap, bit_position as usize)
        };
        if is_initialized {
            return Ok((true, current_start));
        }
        let next = self
            .next(current_start, zero_for_one)?
            .ok_or(ClmmError::LiquidityInsufficient)?;
        Ok((false, next))
    }

    /// `next_tick_array_index_with_extension_info`: the next initialized
    /// tick array after `last_start_index`, if any.
    pub fn next(
        &mut self,
        last_start_index: i32,
        zero_for_one: bool,
    ) -> Result<Option<i32>, ClmmError> {
        let tick_spacing = self.pool.tick_spacing;
        let mut last = array_start_index(last_start_index, tick_spacing);
        for _ in 0..BITMAP_SEARCHES_MAX {
            let (found, start) = next_in_default_bitmap(
                &self.pool.tick_array_bitmap,
                last,
                tick_spacing,
                zero_for_one,
            )?;
            if found {
                return Ok(Some(start));
            }
            // From a tick spacing of 15 the pool's bitmap spans every tick.
            if tick_spacing >= 15 {
                return Ok(None);
            }
            let (found, start) =
                self.extension()?
                    .next_from_one_bitmap(start, tick_spacing, zero_for_one)?;
            if found {
                return Ok(Some(start));
            }
            last = start;
            if !(math::MIN_TICK..=math::MAX_TICK).contains(&last) {
                return Ok(None);
            }
        }
        Err(ClmmError::Limit("bitmap searches"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tick_array.rs`'s `get_array_start_index_test`.
    #[test]
    fn array_start_indexes_round_toward_negative_infinity() {
        assert_eq!(array_start_index(120, 3), 0);
        assert_eq!(array_start_index(1002, 30), 0);
        assert_eq!(array_start_index(-120, 3), -180);
        assert_eq!(array_start_index(-1002, 30), -1800);
        assert_eq!(array_start_index(-20, 10), -600);
        assert_eq!(array_start_index(20, 10), 0);
        assert_eq!(array_start_index(-1002, 10), -1200);
        assert_eq!(array_start_index(-600, 10), -600);
        assert_eq!(array_start_index(-30720, 1), -30720);
        assert_eq!(array_start_index(30720, 1), 30720);
        assert_eq!(array_start_index(math::MIN_TICK, 1), -443640);
    }

    /// `tick_array_bit_map.rs`'s searches over a full bitmap, spacing 10.
    #[test]
    fn default_bitmap_search_finds_the_neighbouring_array() {
        let full = [u64::MAX; 16];
        assert_eq!(
            next_in_default_bitmap(&full, -38400 + 600, 10, true),
            Ok((true, -38400))
        );
        assert_eq!(
            next_in_default_bitmap(&full, -38400, 10, true),
            Ok((true, -39000))
        );
        assert_eq!(
            next_in_default_bitmap(&full, -307200, 10, true),
            Ok((false, -307200))
        );
        assert_eq!(
            next_in_default_bitmap(&full, 38400 - 600, 10, false),
            Ok((true, 38400))
        );
        assert_eq!(
            next_in_default_bitmap(&full, 306600, 10, false),
            Ok((false, 306600))
        );
        let empty = [0u64; 16];
        assert_eq!(
            next_in_default_bitmap(&empty, 0, 10, true),
            Ok((false, -307200))
        );
        assert_eq!(
            next_in_default_bitmap(&empty, 0, 10, false),
            Ok((false, 306600))
        );
    }

    /// `tickarray_bitmap_extension.rs`'s offsets, spacing 1.
    #[test]
    fn extension_offsets_count_away_from_the_pools_bitmap() {
        assert_eq!(offset_in_extension_bitmap(60 * 512, 1), 0);
        assert_eq!(offset_in_extension_bitmap(60 * 513, 1), 1);
        assert_eq!(offset_in_extension_bitmap(-60 * 513, 1), 511);
        assert_eq!(offset_in_extension_bitmap(-60 * 1024, 1), 0);
        assert_eq!(bitmap_tick_boundary(60 * 513, 1), (30720, 61440));
        assert_eq!(bitmap_tick_boundary(-60 * 513, 1), (-61440, -30720));
        assert!(is_in_extension(30720, 1));
        assert!(!is_in_extension(30719, 1));
        assert!(is_in_extension(-30721, 1));
        assert!(!is_in_extension(-30720, 1));
    }

    #[test]
    fn extension_search_walks_each_bitmap_then_reports_its_edge() {
        let mut extension = BitmapExtension {
            pool: Pubkey::default(),
            positive: [[0; 8]; EXTENSION_BITMAPS],
            negative: [[0; 8]; EXTENSION_BITMAPS],
        };
        // The array at 60 * 520 is bit 8 of the first positive bitmap.
        extension.positive[0][0] = 1 << 8;
        assert_eq!(
            extension.next_from_one_bitmap(30660, 1, false),
            Ok((true, 60 * 520))
        );
        assert_eq!(
            extension.next_from_one_bitmap(60 * 520, 1, false),
            Ok((false, 61440 - 60))
        );
        assert_eq!(
            extension.next_from_one_bitmap(61440, 1, true),
            Ok((true, 60 * 520))
        );
        assert_eq!(
            extension.next_from_one_bitmap(60 * 520, 1, true),
            Ok((false, 30720))
        );
    }
}
