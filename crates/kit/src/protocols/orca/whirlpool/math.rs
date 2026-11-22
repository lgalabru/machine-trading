//! The Whirlpool program's swap math, ported function by function from
//! `orca-so/whirlpools@cc683ec92acb5e7606caa3d8e514d9383e9c7c11`,
//! `programs/whirlpool/src/math/` (`tick_math.rs`, `token_math.rs`,
//! `swap_math.rs`, `bit_math.rs`).
//!
//! Prices are square roots in Q64.64 (`sqrt_price_x64`: the square root of
//! token B atoms per token A atom, times 2^64). Fee rates are in hundredths
//! of a basis point (millionths), protocol fee rates in basis points. Every
//! rounding direction is the program's: what a trader pays rounds up, what
//! a trader receives rounds down. Where the program fails, these return
//! the [`ProgramFailure`] it fails with.

use super::error::ProgramFailure;
use crate::protocols::wide::U256;

/// The highest tick a pool can reach.
pub const MAX_TICK_INDEX: i32 = 443_636;
/// The lowest tick a pool can reach.
pub const MIN_TICK_INDEX: i32 = -443_636;
/// `sqrt_price_from_tick_index(MAX_TICK_INDEX)`.
pub const MAX_SQRT_PRICE_X64: u128 = 79_226_673_515_401_279_992_447_579_055;
/// `sqrt_price_from_tick_index(MIN_TICK_INDEX)`.
pub const MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;
/// A fee rate's denominator: a rate of 400 is 4 bp.
pub const FEE_RATE_MUL_VALUE: u128 = 1_000_000;
/// A protocol fee rate's denominator: a rate of 1,300 is 13% of the fee.
pub const PROTOCOL_FEE_RATE_MUL_VALUE: u128 = 10_000;

const Q64_RESOLUTION: u32 = 64;
const Q64_MASK: u128 = 0xFFFF_FFFF_FFFF_FFFF;

/// The bit table below covers ticks up to 2^19 in magnitude.
const TICK_MAGNITUDE_MAX: i32 = (1 << 19) - 1;

/// The square root of `1.0001^tick` in Q64.64, exactly as the program
/// derives it (`tick_math.rs`, `sqrt_price_from_tick_index`).
pub fn sqrt_price_from_tick_index(tick: i32) -> u128 {
    // Above the max tick the program fails with a down-cast error; no
    // caller here asks for one.
    assert!(tick <= MAX_TICK_INDEX, "tick {tick} above the max tick");
    assert!(
        tick >= -TICK_MAGNITUDE_MAX,
        "tick {tick} below the bit table"
    );
    if tick >= 0 {
        sqrt_price_positive_tick(tick)
    } else {
        sqrt_price_negative_tick(tick)
    }
}

/// The tick whose price is at or below `sqrt_price_x64`
/// (`tick_math.rs`, `tick_index_from_sqrt_price`): an approximate log,
/// then the exact price of the candidate above decides.
pub fn tick_index_from_sqrt_price(sqrt_price_x64: u128) -> i32 {
    assert!(sqrt_price_x64 > 0, "a zero sqrt price has no tick");
    const LOG_B_2_X32: i128 = 59_543_866_431_248;
    const BIT_PRECISION: u32 = 14;
    const LOG_B_P_ERR_MARGIN_LOWER_X64: i128 = 184_467_440_737_095_516;
    const LOG_B_P_ERR_MARGIN_UPPER_X64: i128 = 15_793_534_762_490_258_745;

    let msb: u32 = 128 - sqrt_price_x64.leading_zeros() - 1;
    let log2p_integer_x32 = (i128::from(msb) - 64) << 32;

    let mut bit: i128 = 0x8000_0000_0000_0000;
    let mut precision = 0;
    let mut log2p_fraction_x64: i128 = 0;
    let mut r = if msb >= 64 {
        sqrt_price_x64 >> (msb - 63)
    } else {
        sqrt_price_x64 << (63 - msb)
    };
    while bit > 0 && precision < BIT_PRECISION {
        r *= r;
        let is_r_more_than_two = (r >> 127) as u32;
        r >>= 63 + is_r_more_than_two;
        log2p_fraction_x64 += bit * i128::from(is_r_more_than_two);
        bit >>= 1;
        precision += 1;
    }

    let log2p_fraction_x32 = log2p_fraction_x64 >> 32;
    let log2p_x32 = log2p_integer_x32 + log2p_fraction_x32;
    let logbp_x64 = log2p_x32 * LOG_B_2_X32;
    let tick_low = i32::try_from((logbp_x64 - LOG_B_P_ERR_MARGIN_LOWER_X64) >> 64)
        .expect("a u128 sqrt price's log fits an i32");
    let tick_high = i32::try_from((logbp_x64 + LOG_B_P_ERR_MARGIN_UPPER_X64) >> 64)
        .expect("a u128 sqrt price's log fits an i32");

    if tick_low == tick_high {
        return tick_low;
    }
    if sqrt_price_from_tick_index(tick_high) <= sqrt_price_x64 {
        tick_high
    } else {
        tick_low
    }
}

fn mul_shift_96(a: u128, b: u128) -> u128 {
    U256::mul_u128(a, b)
        .shr(96)
        .to_u128()
        .expect("ratios up to the max tick fit a u128")
}

fn sqrt_price_positive_tick(tick: i32) -> u128 {
    const FACTORS: [u128; 18] = [
        79_236_085_330_515_764_027_303_304_731,
        79_244_008_939_048_815_603_706_035_061,
        79_259_858_533_276_714_757_314_932_305,
        79_291_567_232_598_584_799_939_703_904,
        79_355_022_692_464_371_645_785_046_466,
        79_482_085_999_252_804_386_437_311_141,
        79_736_823_300_114_093_921_829_183_326,
        80_248_749_790_819_932_309_965_073_892,
        81_282_483_887_344_747_381_513_967_011,
        83_390_072_131_320_151_908_154_831_281,
        87_770_609_709_833_776_024_991_924_138,
        97_234_110_755_111_693_312_479_820_773,
        119_332_217_159_966_728_226_237_229_890,
        179_736_315_981_702_064_433_883_588_727,
        407_748_233_172_238_350_107_850_275_304,
        2_098_478_828_474_011_932_436_660_412_517,
        55_581_415_166_113_811_149_459_800_483_533,
        38_992_368_544_603_139_932_233_054_999_993_551,
    ];
    let mut ratio: u128 = if tick & 1 != 0 {
        79_232_123_823_359_799_118_286_999_567
    } else {
        79_228_162_514_264_337_593_543_950_336
    };
    for (bit, factor) in FACTORS.iter().enumerate() {
        if tick & (2 << bit) != 0 {
            ratio = mul_shift_96(ratio, *factor);
        }
    }
    ratio >> 32
}

fn sqrt_price_negative_tick(tick: i32) -> u128 {
    const FACTORS: [u128; 18] = [
        18_444_899_583_751_176_498,
        18_443_055_278_223_354_162,
        18_439_367_220_385_604_838,
        18_431_993_317_065_449_817,
        18_417_254_355_718_160_513,
        18_387_811_781_193_591_352,
        18_329_067_761_203_520_168,
        18_212_142_134_806_087_854,
        17_980_523_815_641_551_639,
        17_526_086_738_831_147_013,
        16_651_378_430_235_024_244,
        15_030_750_278_693_429_944,
        12_247_334_978_882_834_399,
        8_131_365_268_884_726_200,
        3_584_323_654_723_342_297,
        696_457_651_847_595_233,
        26_294_789_957_452_057,
        37_481_735_321_082,
    ];
    let abs_tick = tick.abs();
    let mut ratio: u128 = if abs_tick & 1 != 0 {
        18_445_821_805_675_392_311
    } else {
        18_446_744_073_709_551_616
    };
    for (bit, factor) in FACTORS.iter().enumerate() {
        if abs_tick & (2 << bit) != 0 {
            // ratio <= 2^64 and factor < 2^64: the product fits.
            ratio = (ratio * factor) >> 64;
        }
    }
    ratio
}

/// A token amount that may not fit a `u64` (`token_math.rs`,
/// `AmountDeltaU64`): a swap step only needs to know it is too big.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmountDelta {
    Valid(u64),
    ExceedsMax(ProgramFailure),
}

impl AmountDelta {
    fn lte(self, other: u64) -> bool {
        match self {
            Self::Valid(value) => value <= other,
            Self::ExceedsMax(_) => false,
        }
    }

    fn into_result(self) -> Result<u64, ProgramFailure> {
        match self {
            Self::Valid(value) => Ok(value),
            Self::ExceedsMax(failure) => Err(failure),
        }
    }
}

fn increasing_price_order(sqrt_price_0: u128, sqrt_price_1: u128) -> (u128, u128) {
    if sqrt_price_0 > sqrt_price_1 {
        (sqrt_price_1, sqrt_price_0)
    } else {
        (sqrt_price_0, sqrt_price_1)
    }
}

/// Token A between two prices: `L * (√P_upper - √P_lower) / (√P_upper * √P_lower)`.
pub fn try_get_amount_delta_a(
    sqrt_price_0: u128,
    sqrt_price_1: u128,
    liquidity: u128,
    round_up: bool,
) -> Result<AmountDelta, ProgramFailure> {
    let (sqrt_price_lower, sqrt_price_upper) = increasing_price_order(sqrt_price_0, sqrt_price_1);
    assert!(sqrt_price_lower > 0, "a zero sqrt price");
    let numerator = U256::mul_u128(liquidity, sqrt_price_upper - sqrt_price_lower)
        .checked_shl(Q64_RESOLUTION)
        .ok_or(ProgramFailure::MultiplicationOverflow)?;
    let denominator = U256::mul_u128(sqrt_price_upper, sqrt_price_lower);
    let quotient = numerator
        .div_round(denominator, round_up)
        .expect("a quotient below 2^256 plus one fits");
    match quotient.to_u128() {
        None => Ok(AmountDelta::ExceedsMax(ProgramFailure::NumberDownCastError)),
        Some(value) => match u64::try_from(value) {
            Ok(value) => Ok(AmountDelta::Valid(value)),
            Err(_) => Ok(AmountDelta::ExceedsMax(ProgramFailure::TokenMaxExceeded)),
        },
    }
}

/// Token B between two prices: `L * (√P_upper - √P_lower)`.
pub fn try_get_amount_delta_b(
    sqrt_price_0: u128,
    sqrt_price_1: u128,
    liquidity: u128,
    round_up: bool,
) -> Result<AmountDelta, ProgramFailure> {
    let (sqrt_price_lower, sqrt_price_upper) = increasing_price_order(sqrt_price_0, sqrt_price_1);
    let price_difference = sqrt_price_upper - sqrt_price_lower;
    if liquidity == 0 || price_difference == 0 {
        return Ok(AmountDelta::Valid(0));
    }
    let Some(product) = liquidity.checked_mul(price_difference) else {
        return Ok(AmountDelta::ExceedsMax(
            ProgramFailure::MultiplicationShiftRightOverflow,
        ));
    };
    // The program casts with `as u64`, keeping the low 64 bits.
    let result = (product >> Q64_RESOLUTION) as u64;
    let should_round = round_up && product & Q64_MASK > 0;
    if should_round && result == u64::MAX {
        return Ok(AmountDelta::ExceedsMax(
            ProgramFailure::MultiplicationOverflow,
        ));
    }
    Ok(AmountDelta::Valid(result + u64::from(should_round)))
}

pub fn get_amount_delta_a(
    sqrt_price_0: u128,
    sqrt_price_1: u128,
    liquidity: u128,
    round_up: bool,
) -> Result<u64, ProgramFailure> {
    try_get_amount_delta_a(sqrt_price_0, sqrt_price_1, liquidity, round_up)?.into_result()
}

pub fn get_amount_delta_b(
    sqrt_price_0: u128,
    sqrt_price_1: u128,
    liquidity: u128,
    round_up: bool,
) -> Result<u64, ProgramFailure> {
    try_get_amount_delta_b(sqrt_price_0, sqrt_price_1, liquidity, round_up)?.into_result()
}

/// The price after `amount` of token A enters (or leaves) the pool,
/// rounded up: `√P * L / (L ± amount * √P)`.
fn get_next_sqrt_price_from_a_round_up(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount: u64,
    amount_specified_is_input: bool,
) -> Result<u128, ProgramFailure> {
    if amount == 0 {
        return Ok(sqrt_price_x64);
    }
    let product = U256::mul_u128(sqrt_price_x64, u128::from(amount));
    let numerator = U256::mul_u128(liquidity, sqrt_price_x64)
        .checked_shl(Q64_RESOLUTION)
        .ok_or(ProgramFailure::MultiplicationOverflow)?;
    let liquidity_shift_left = U256::from_u128(liquidity).shl(Q64_RESOLUTION);
    if !amount_specified_is_input && liquidity_shift_left <= product {
        return Err(ProgramFailure::DivideByZero);
    }
    let denominator = if amount_specified_is_input {
        liquidity_shift_left
            .checked_add(product)
            .expect("two values below 2^192 add below 2^256")
    } else {
        liquidity_shift_left
            .checked_sub(product)
            .expect("checked above: the product is the smaller")
    };
    // amount > 0 and sqrt_price_x64 > 0, so the denominator isn't zero.
    let price = numerator
        .div_round(denominator, true)
        .expect("a quotient below 2^256 plus one fits")
        .to_u128()
        .ok_or(ProgramFailure::NumberDownCastError)?;
    if price < MIN_SQRT_PRICE_X64 {
        return Err(ProgramFailure::TokenMinSubceeded);
    }
    if price > MAX_SQRT_PRICE_X64 {
        return Err(ProgramFailure::TokenMaxExceeded);
    }
    Ok(price)
}

/// The price after `amount` of token B enters (or leaves) the pool,
/// rounded down: `√P ± amount / L`.
fn get_next_sqrt_price_from_b_round_down(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount: u64,
    amount_specified_is_input: bool,
) -> Result<u128, ProgramFailure> {
    if liquidity == 0 {
        return Err(ProgramFailure::DivideByZero);
    }
    let amount_x64 = u128::from(amount) << Q64_RESOLUTION;
    let mut delta = amount_x64 / liquidity;
    if !amount_specified_is_input && !amount_x64.is_multiple_of(liquidity) {
        delta += 1;
    }
    if amount_specified_is_input {
        sqrt_price_x64
            .checked_add(delta)
            .ok_or(ProgramFailure::SqrtPriceOutOfBounds)
    } else {
        sqrt_price_x64
            .checked_sub(delta)
            .ok_or(ProgramFailure::SqrtPriceOutOfBounds)
    }
}

/// The price after a swap of `amount` within one liquidity range, rounded
/// so the fixed side is never exceeded.
pub fn get_next_sqrt_price(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount: u64,
    amount_specified_is_input: bool,
    a_to_b: bool,
) -> Result<u128, ProgramFailure> {
    if amount_specified_is_input == a_to_b {
        get_next_sqrt_price_from_a_round_up(
            sqrt_price_x64,
            liquidity,
            amount,
            amount_specified_is_input,
        )
    } else {
        get_next_sqrt_price_from_b_round_down(
            sqrt_price_x64,
            liquidity,
            amount,
            amount_specified_is_input,
        )
    }
}

/// One step of a swap: within one liquidity range, towards one target
/// price (`swap_math.rs`, `SwapStepComputation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapStep {
    /// Input atoms the step moves the price with, fee excluded.
    pub amount_in: u64,
    /// Output atoms.
    pub amount_out: u64,
    /// Where the price ends.
    pub next_sqrt_price_x64: u128,
    /// Input atoms charged as fee.
    pub fee_amount: u64,
}

/// One swap step (`swap_math.rs`, `compute_swap`): from
/// `sqrt_price_current_x64` towards `sqrt_price_target_x64`, with
/// `amount_remaining` left to swap (input fee included, or output).
pub fn compute_swap(
    amount_remaining: u64,
    fee_rate: u32,
    liquidity: u128,
    sqrt_price_current_x64: u128,
    sqrt_price_target_x64: u128,
    amount_specified_is_input: bool,
    a_to_b: bool,
) -> Result<SwapStep, ProgramFailure> {
    assert!(
        u128::from(fee_rate) < FEE_RATE_MUL_VALUE,
        "fee rate {fee_rate} is 100% or more"
    );
    let fixed_is_a = a_to_b == amount_specified_is_input;
    let initial_amount_fixed_delta = if fixed_is_a {
        try_get_amount_delta_a(
            sqrt_price_current_x64,
            sqrt_price_target_x64,
            liquidity,
            amount_specified_is_input,
        )?
    } else {
        try_get_amount_delta_b(
            sqrt_price_current_x64,
            sqrt_price_target_x64,
            liquidity,
            amount_specified_is_input,
        )?
    };

    let amount_calc = if amount_specified_is_input {
        // amount * (1 - fee rate), rounded down: at most `amount`.
        let net = u128::from(amount_remaining) * (FEE_RATE_MUL_VALUE - u128::from(fee_rate))
            / FEE_RATE_MUL_VALUE;
        u64::try_from(net).expect("at most the amount remaining")
    } else {
        amount_remaining
    };

    let next_sqrt_price_x64 = if initial_amount_fixed_delta.lte(amount_calc) {
        sqrt_price_target_x64
    } else {
        get_next_sqrt_price(
            sqrt_price_current_x64,
            liquidity,
            amount_calc,
            amount_specified_is_input,
            a_to_b,
        )?
    };
    let is_max_swap = next_sqrt_price_x64 == sqrt_price_target_x64;

    let amount_unfixed_delta = if fixed_is_a {
        get_amount_delta_b(
            sqrt_price_current_x64,
            next_sqrt_price_x64,
            liquidity,
            !amount_specified_is_input,
        )?
    } else {
        get_amount_delta_a(
            sqrt_price_current_x64,
            next_sqrt_price_x64,
            liquidity,
            !amount_specified_is_input,
        )?
    };

    let amount_fixed_delta = match initial_amount_fixed_delta {
        AmountDelta::Valid(value) if is_max_swap => value,
        _ => {
            if fixed_is_a {
                get_amount_delta_a(
                    sqrt_price_current_x64,
                    next_sqrt_price_x64,
                    liquidity,
                    amount_specified_is_input,
                )?
            } else {
                get_amount_delta_b(
                    sqrt_price_current_x64,
                    next_sqrt_price_x64,
                    liquidity,
                    amount_specified_is_input,
                )?
            }
        }
    };

    let (amount_in, mut amount_out) = if amount_specified_is_input {
        (amount_fixed_delta, amount_unfixed_delta)
    } else {
        (amount_unfixed_delta, amount_fixed_delta)
    };
    if !amount_specified_is_input && amount_out > amount_remaining {
        amount_out = amount_remaining;
    }

    let fee_amount = if amount_specified_is_input && !is_max_swap {
        // The step stops short of its target: everything left is used, and
        // what didn't move the price is the fee. The price's rounding keeps
        // the input within what remains; were it not, the program's
        // checked arithmetic would abort the swap.
        amount_remaining
            .checked_sub(amount_in)
            .ok_or(ProgramFailure::AmountRemainingOverflow)?
    } else {
        let fee = mul_div_round_up(
            u128::from(amount_in),
            u128::from(fee_rate),
            FEE_RATE_MUL_VALUE - u128::from(fee_rate),
        );
        u64::try_from(fee).map_err(|_| ProgramFailure::NumberCastError)?
    };

    Ok(SwapStep {
        amount_in,
        amount_out,
        next_sqrt_price_x64,
        fee_amount,
    })
}

/// `n0 * n1 / d`, rounded up, for operands whose product fits a `u128`
/// (`bit_math.rs`, `checked_mul_div_round_up`): an input amount (< 2^64)
/// times a fee rate (< 2^20).
fn mul_div_round_up(n0: u128, n1: u128, d: u128) -> u128 {
    assert!(d > 0, "fee denominator is zero");
    let product = n0
        .checked_mul(n1)
        .expect("an amount times a fee rate fits a u128");
    product.div_ceil(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tick_math.rs`, `sqrt_price_from_tick_index_tests::test_exact_bit_values`.
    #[test]
    fn sqrt_prices_at_each_bit_of_the_tick_match_the_program() {
        let conditions: &[(i32, u128, u128)] = &[
            (0, 18446744073709551616, 18446744073709551616),
            (1, 18447666387855959850, 18445821805675392311),
            (2, 18448588748116922571, 18444899583751176498),
            (4, 18450433606991734263, 18443055278223354162),
            (8, 18454123878217468680, 18439367220385604838),
            (16, 18461506635090006701, 18431993317065449817),
            (32, 18476281010653910144, 18417254355718160513),
            (64, 18505865242158250041, 18387811781193591352),
            (128, 18565175891880433522, 18329067761203520168),
            (256, 18684368066214940582, 18212142134806087854),
            (512, 18925053041275764671, 17980523815641551639),
            (1024, 19415764168677886926, 17526086738831147013),
            (2048, 20435687552633177494, 16651378430235024244),
            (4096, 22639080592224303007, 15030750278693429944),
            (8192, 27784196929998399742, 12247334978882834399),
            (16384, 41848122137994986128, 8131365268884726200),
            (32768, 94936283578220370716, 3584323654723342297),
            (65536, 488590176327622479860, 696457651847595233),
            (131072, 12941056668319229769860, 26294789957452057),
            (262144, 9078618265828848800676189, 37481735321082),
        ];
        for &(tick, positive, negative) in conditions {
            assert_eq!(sqrt_price_from_tick_index(tick), positive, "tick {tick}");
            assert_eq!(sqrt_price_from_tick_index(-tick), negative, "tick -{tick}");
        }
        assert_eq!(
            sqrt_price_from_tick_index(MAX_TICK_INDEX),
            MAX_SQRT_PRICE_X64
        );
        assert_eq!(
            sqrt_price_from_tick_index(MIN_TICK_INDEX),
            MIN_SQRT_PRICE_X64
        );
    }

    /// `tick_math.rs`, `test_tick_index_from_sqrt_price`.
    #[test]
    fn ticks_from_sqrt_prices_match_the_program_at_the_edges() {
        assert_eq!(
            tick_index_from_sqrt_price(MAX_SQRT_PRICE_X64),
            MAX_TICK_INDEX
        );
        assert_eq!(
            tick_index_from_sqrt_price(MIN_SQRT_PRICE_X64),
            MIN_TICK_INDEX
        );
        assert_eq!(
            tick_index_from_sqrt_price(MIN_SQRT_PRICE_X64 + 1),
            MIN_TICK_INDEX
        );
        assert_eq!(
            tick_index_from_sqrt_price(MAX_SQRT_PRICE_X64 - 1),
            MAX_TICK_INDEX - 1
        );
        assert_eq!(tick_index_from_sqrt_price(u64::MAX as u128 + 1), 0);
        assert_eq!(tick_index_from_sqrt_price(u64::MAX as u128 + 2), 0);
        assert_eq!(tick_index_from_sqrt_price(u64::MAX as u128), -1);
    }

    #[test]
    fn ticks_and_sqrt_prices_round_trip() {
        let mut tick = MIN_TICK_INDEX;
        while tick < MAX_TICK_INDEX {
            let sqrt_price = sqrt_price_from_tick_index(tick);
            assert_eq!(tick_index_from_sqrt_price(sqrt_price), tick);
            assert_eq!(tick_index_from_sqrt_price(sqrt_price + 1), tick);
            assert_eq!(tick_index_from_sqrt_price(sqrt_price - 1), tick - 1);
            tick += 997;
        }
    }

    /// `token_math.rs`, `test_get_amount_delta`.
    #[test]
    fn amount_deltas_match_the_program() {
        assert_eq!(get_amount_delta_a(4 << 64, 2 << 64, 4, true), Ok(1));
        assert_eq!(get_amount_delta_a(4 << 64, 2 << 64, 4, false), Ok(1));
        assert_eq!(get_amount_delta_b(4 << 64, 2 << 64, 4, true), Ok(8));
        assert_eq!(get_amount_delta_b(4 << 64, 2 << 64, 4, false), Ok(8));
        assert_eq!(get_amount_delta_a(4 << 64, 4 << 64, 4, true), Ok(0));
        assert_eq!(get_amount_delta_b(4 << 64, 4 << 64, 4, false), Ok(0));
        assert!(get_amount_delta_a(1 << 64, 2 << 64, u128::MAX, true).is_err());
        assert!(get_amount_delta_a(1 << 64, 2 << 64, (u64::MAX as u128) << 2, true).is_err());
        assert!(get_amount_delta_a(1 << 64, 2 << 64, (u64::MAX as u128) << 1, true).is_ok());
        assert!(get_amount_delta_a(1 << 64, 2 << 64, u64::MAX as u128, true).is_ok());
    }

    #[test]
    fn an_exact_in_step_short_of_its_target_charges_the_rest_as_fee() {
        let liquidity = 1_000_000_000_000u128;
        let current = sqrt_price_from_tick_index(0);
        let target = sqrt_price_from_tick_index(-1000);
        let step = compute_swap(1_000_000, 400, liquidity, current, target, true, true).unwrap();
        assert!(step.next_sqrt_price_x64 < current && step.next_sqrt_price_x64 > target);
        assert_eq!(step.amount_in + step.fee_amount, 1_000_000);
        // 4 bp of a million, give or take the rounding of the price.
        assert!(
            (399..=401).contains(&step.fee_amount),
            "fee {}",
            step.fee_amount
        );
        assert!(step.amount_out < step.amount_in);
    }

    #[test]
    fn an_exact_out_step_never_gives_more_than_asked() {
        let liquidity = 1_000_000_000_000u128;
        let current = sqrt_price_from_tick_index(0);
        let target = sqrt_price_from_tick_index(1000);
        let step = compute_swap(1_000_000, 3000, liquidity, current, target, false, false).unwrap();
        assert_eq!(step.amount_out, 1_000_000);
        assert!(step.amount_in > step.amount_out);
        assert_eq!(
            u128::from(step.fee_amount),
            (u128::from(step.amount_in) * 3000).div_ceil(1_000_000 - 3000)
        );
    }
}
