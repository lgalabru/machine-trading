//! Bin math: bin `id`'s price is (1 + bin_step / 10⁴)^id Y atoms per X
//! atom, a position owns its shares of each bin's reserves, and a swap
//! empties bins one at a time from the active one, each at its own price.
//!
//! What the grader reads (prices, a position's amounts, how far a swap
//! reaches) is in `f64`: it values, reports and sizes, it doesn't settle. A
//! position's fees are counted exactly, as the program counts them.

use super::{BinArray, LbPair, Position};

/// Y per X, in whole units, at bin `id`.
pub fn bin_price(id: i32, bin_step: u16, decimals_x: u8, decimals_y: u8) -> f64 {
    (1.0 + f64::from(bin_step) / 1e4).powi(id)
        * 10f64.powi(i32::from(decimals_x) - i32::from(decimals_y))
}

/// The bin whose price is nearest at or below `price` Y per X, in whole
/// units.
pub fn bin_at_price(price: f64, bin_step: u16, decimals_x: u8, decimals_y: u8) -> i32 {
    let atoms = price * 10f64.powi(i32::from(decimals_y) - i32::from(decimals_x));
    if atoms <= 0.0 {
        return i32::MIN / 2;
    }
    // A hair up, so a price on a bin lands on it.
    (atoms.ln() / (1.0 + f64::from(bin_step) / 1e4).ln() + 1e-9).floor() as i32
}

/// What a position holds, in atoms of X and Y, from the bins of `arrays`;
/// a bin no array holds counts nothing.
pub fn position_amounts(position: &Position, arrays: &[BinArray]) -> (f64, f64) {
    let (mut x, mut y) = (0.0, 0.0);
    for (i, held) in position.bins.iter().enumerate() {
        let id = position.lower_bin_id + i as i32;
        let Some(bin) = arrays.iter().find_map(|a| a.bin(id)) else {
            continue;
        };
        if held.shares == 0 || bin.liquidity_supply == 0 {
            continue;
        }
        let share = held.shares as f64 / bin.liquidity_supply as f64;
        x += share * bin.amount_x as f64;
        y += share * bin.amount_y as f64;
    }
    (x, y)
}

/// A position's unclaimed fees, in atoms of X and Y: those counted on it,
/// and those its shares earned since.
pub fn fees_owed(position: &Position, arrays: &[BinArray]) -> (u64, u64) {
    let (mut x, mut y) = (0u64, 0u64);
    for (i, held) in position.bins.iter().enumerate() {
        let id = position.lower_bin_id + i as i32;
        x = x.saturating_add(held.fee_x_pending);
        y = y.saturating_add(held.fee_y_pending);
        let Some(bin) = arrays.iter().find_map(|a| a.bin(id)) else {
            continue;
        };
        // Shares are Q64.64 liquidity: their whole part times the growth.
        let liquidity = held.shares >> 64;
        let earned =
            |stored: u128, complete: u128| mul_shift_64(stored.wrapping_sub(complete), liquidity);
        x = x.saturating_add(earned(
            bin.fee_amount_x_per_token_stored,
            held.fee_x_per_token_complete,
        ));
        y = y.saturating_add(earned(
            bin.fee_amount_y_per_token_stored,
            held.fee_y_per_token_complete,
        ));
    }
    (x, y)
}

/// `a × b >> 64`, saturating at `u64::MAX`.
fn mul_shift_64(a: u128, b: u128) -> u64 {
    let (a_hi, a_lo) = (a >> 64, a & u128::from(u64::MAX));
    let (b_hi, b_lo) = (b >> 64, b & u128::from(u64::MAX));
    let lo = (a_lo * b_lo) >> 64;
    let mid = a_hi
        .checked_mul(b_lo)
        .and_then(|x| x.checked_add(a_lo.checked_mul(b_hi)?))
        .and_then(|x| x.checked_add(lo));
    match (mid, a_hi.checked_mul(b_hi)) {
        (Some(mid), Some(0)) => u64::try_from(mid).unwrap_or(u64::MAX),
        _ => u64::MAX,
    }
}

/// How much of X in (`swap_for_y`), or of Y in, a swap can send through the
/// bins of `arrays`, from the active bin, before the price passes `limit`
/// (Y per X atom; a sale stops below it, a purchase above it): at most
/// `cap` atoms in, fees included. Bins the arrays don't hold end it.
pub fn amount_in_to_price(
    pair: &LbPair,
    arrays: &[BinArray],
    swap_for_y: bool,
    limit: f64,
    cap: f64,
) -> f64 {
    let fee = pair.fee();
    let step = 1.0 + f64::from(pair.bin_step) / 1e4;
    let mut id = pair.active_id;
    let mut sent = 0.0;
    while sent < cap {
        let price = step.powi(id);
        let reached = if swap_for_y {
            price < limit
        } else {
            price > limit
        };
        if reached {
            break;
        }
        let Some(bin) = arrays.iter().find_map(|a| a.bin(id)) else {
            break;
        };
        // What empties the bin's other side, grossed up for the fee.
        let need = if swap_for_y {
            bin.amount_y as f64 / price
        } else {
            bin.amount_x as f64 * price
        } / (1.0 - fee);
        sent += need;
        id += if swap_for_y { -1 } else { 1 };
    }
    sent.min(cap)
}

#[cfg(test)]
mod tests {
    use solana_pubkey::Pubkey;

    use super::*;
    use crate::protocols::{
        SPCX_MINT, USDC_MINT,
        meteora::dlmm::{
            Bin,
            state::testing::{bin_array_bytes, pair_bytes, position_bytes},
        },
    };

    #[test]
    fn bin_prices_and_bins_agree() {
        // The live SPCX/USDC pool's active bin.
        assert!((bin_price(662, 75, 6, 6) - 140.678).abs() < 1e-3);
        assert_eq!(bin_at_price(140.678032690378, 75, 6, 6), 662);
        assert_eq!(bin_at_price(141.0, 75, 6, 6), 662);
        assert_eq!(bin_at_price(141.8, 75, 6, 6), 663);
        assert!((bin_price(-2468, 10, 9, 6) - 84.8588).abs() < 1e-3);
    }

    #[test]
    fn a_position_owns_its_share_of_each_bin() {
        let pair_key = Pubkey::new_unique();
        let bin = |x, y| Bin {
            amount_x: x,
            amount_y: y,
            liquidity_supply: 4 << 64,
            fee_amount_y_per_token_stored: 3 << 64,
            ..Bin::default()
        };
        // Bins 661 to 663 sit at 31 to 33 of array 9.
        let array = BinArray::decode(&bin_array_bytes(
            pair_key,
            9,
            &[(31, bin(0, 800)), (32, bin(40, 400)), (33, bin(80, 0))],
        ))
        .unwrap();
        let held = super::super::state::PositionBin {
            shares: 1 << 64,
            fee_y_per_token_complete: 1 << 64,
            fee_y_pending: 5,
            ..Default::default()
        };
        let position = Position::decode(
            Pubkey::new_unique(),
            &position_bytes(pair_key, Pubkey::new_unique(), 661, &[held; 3]),
        )
        .unwrap();
        let (x, y) = position_amounts(&position, std::slice::from_ref(&array));
        assert_eq!((x, y), (30.0, 300.0));
        // Each bin: 5 pending, and one share times a growth of 2.
        assert_eq!(fees_owed(&position, &[array]), (0, 21));
    }

    #[test]
    fn a_swap_reaches_the_limit_bin_by_bin() {
        let pair_key = Pubkey::new_unique();
        let mut data = pair_bytes(SPCX_MINT, USDC_MINT, 1, 100);
        // No fee, so the amounts are the bins' own.
        data[8..10].copy_from_slice(&0u16.to_le_bytes());
        let pair = LbPair::decode(pair_key, &data).unwrap();
        let bin = |y| Bin {
            amount_y: y,
            ..Bin::default()
        };
        let array = BinArray::decode(&bin_array_bytes(
            pair_key,
            0,
            &[(0, bin(1000)), (1, bin(1010))],
        ))
        .unwrap();
        let arrays = [array];
        // Selling X: bin 1 (price 1.01) empties for 1000 X, then bin 0.
        let one = amount_in_to_price(&pair, &arrays, true, 1.005, 1e12);
        assert!((one - 1000.0).abs() < 1e-6);
        let both = amount_in_to_price(&pair, &arrays, true, 0.5, 1e12);
        assert!((both - 2000.0).abs() < 1e-6);
        assert_eq!(amount_in_to_price(&pair, &arrays, true, 0.5, 10.0), 10.0);
    }
}
