//! Meteora DLMM instructions, as the program's IDL defines them: the 8-byte
//! Anchor discriminator, then the Borsh-encoded arguments. The `2` forms
//! take Token-2022 mints; their transfer-hook accounts are none here (the
//! named pools' mints have no hook program), so each passes an empty
//! `remaining_accounts_info` and then the bin arrays it crosses. Optional
//! accounts that are absent are passed as the program's own address,
//! Anchor's convention for `None`.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::{LbPair, PROGRAM_ID, bin_array_address, event_authority};
use crate::token::{SYSTEM_PROGRAM_ID, associated_token_address};

/// The memo program, which the `2` instructions take.
pub const MEMO_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const RENT_SYSVAR: Pubkey = solana_pubkey::pubkey!("SysvarRent111111111111111111111111111111111");

const SWAP2: [u8; 8] = [65, 75, 63, 76, 235, 91, 91, 136];
const SWAP_EXACT_OUT2: [u8; 8] = [43, 215, 247, 132, 137, 60, 243, 81];
const INITIALIZE_POSITION: [u8; 8] = [219, 192, 234, 71, 190, 191, 102, 80];
const INITIALIZE_BIN_ARRAY: [u8; 8] = [35, 86, 19, 185, 78, 212, 75, 211];
const ADD_LIQUIDITY_BY_STRATEGY2: [u8; 8] = [3, 221, 149, 218, 111, 141, 118, 213];
const REMOVE_LIQUIDITY_BY_RANGE2: [u8; 8] = [204, 2, 195, 145, 53, 145, 145, 205];
const CLAIM_FEE2: [u8; 8] = [112, 191, 101, 171, 28, 144, 127, 187];

/// An empty `RemainingAccountsInfo`: no slices.
const NO_HOOKS: [u8; 4] = [0; 4];

/// `owner`'s associated token accounts for the pool's X and Y.
pub fn token_accounts(pair: &LbPair, owner: &Pubkey) -> (Pubkey, Pubkey) {
    (
        associated_token_address(owner, &pair.mint_x, &pair.program_x),
        associated_token_address(owner, &pair.mint_y, &pair.program_y),
    )
}

fn bin_arrays(pair: &LbPair, indexes: &[i64]) -> impl Iterator<Item = AccountMeta> {
    indexes
        .iter()
        .map(|i| AccountMeta::new(bin_array_address(&pair.address, *i), false))
        .collect::<Vec<_>>()
        .into_iter()
}

/// How much a swap trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swap {
    /// Exactly `amount_in` atoms in, at least `min_out` out.
    ExactIn { amount_in: u64, min_out: u64 },
    /// Exactly `amount_out` atoms out, at most `max_in` in.
    ExactOut { amount_out: u64, max_in: u64 },
}

/// `swap2` or `swap_exact_out2` for `owner`: X in (`swap_for_y`) or Y in,
/// crossing the bin arrays `arrays` (the active one first, as
/// [`LbPair::swap_bin_arrays`] lists them).
pub fn swap(
    pair: &LbPair,
    owner: &Pubkey,
    swap_for_y: bool,
    swap: Swap,
    arrays: &[i64],
) -> Instruction {
    let (x, y) = token_accounts(pair, owner);
    let (user_in, user_out) = if swap_for_y { (x, y) } else { (y, x) };
    let mut accounts = vec![
        AccountMeta::new(pair.address, false),
        AccountMeta::new_readonly(PROGRAM_ID, false),
        AccountMeta::new(pair.reserve_x, false),
        AccountMeta::new(pair.reserve_y, false),
        AccountMeta::new(user_in, false),
        AccountMeta::new(user_out, false),
        AccountMeta::new_readonly(pair.mint_x, false),
        AccountMeta::new_readonly(pair.mint_y, false),
        AccountMeta::new(pair.oracle, false),
        AccountMeta::new_readonly(PROGRAM_ID, false),
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new_readonly(pair.program_x, false),
        AccountMeta::new_readonly(pair.program_y, false),
        AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
        AccountMeta::new_readonly(event_authority(), false),
        AccountMeta::new_readonly(PROGRAM_ID, false),
    ];
    accounts.extend(bin_arrays(pair, arrays));
    let mut data = Vec::with_capacity(28);
    let (first, second) = match swap {
        Swap::ExactIn { amount_in, min_out } => {
            data.extend_from_slice(&SWAP2);
            (amount_in, min_out)
        }
        Swap::ExactOut { amount_out, max_in } => {
            data.extend_from_slice(&SWAP_EXACT_OUT2);
            (max_in, amount_out)
        }
    };
    data.extend_from_slice(&first.to_le_bytes());
    data.extend_from_slice(&second.to_le_bytes());
    data.extend_from_slice(&NO_HOOKS);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// `initialize_bin_array`: create the pool's bin array `index`, paid by
/// `funder`. A position's range must lie in existing arrays.
pub fn initialize_bin_array(pair: &LbPair, funder: &Pubkey, index: i64) -> Instruction {
    let mut data = INITIALIZE_BIN_ARRAY.to_vec();
    data.extend_from_slice(&index.to_le_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(pair.address, false),
            AccountMeta::new(bin_array_address(&pair.address, index), false),
            AccountMeta::new(*funder, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

/// `initialize_position`: a position of `width` bins from `lower_bin_id`
/// (at most 70), at `position` (a new keypair that signs too), owned and
/// paid for by `owner`.
pub fn initialize_position(
    pair: &LbPair,
    owner: &Pubkey,
    position: &Pubkey,
    lower_bin_id: i32,
    width: i32,
) -> Instruction {
    let mut data = INITIALIZE_POSITION.to_vec();
    data.extend_from_slice(&lower_bin_id.to_le_bytes());
    data.extend_from_slice(&width.to_le_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new(*position, true),
            AccountMeta::new_readonly(pair.address, false),
            AccountMeta::new_readonly(*owner, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(RENT_SYSVAR, false),
            AccountMeta::new_readonly(event_authority(), false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
        ],
        data,
    }
}

/// How a deposit spreads over its bins: the program's strategies for any
/// mix of X and Y.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// The same in every bin.
    Spot,
    /// Most at the active bin, less away from it.
    Curve,
    /// Least at the active bin, more away from it.
    BidAsk,
}

impl Strategy {
    /// The program's `StrategyType` for a deposit of X and Y in any
    /// proportion.
    fn imbalanced(self) -> u8 {
        match self {
            Self::Spot => 6,
            Self::Curve => 7,
            Self::BidAsk => 8,
        }
    }
}

/// What a deposit puts where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deposit {
    pub amount_x: u64,
    pub amount_y: u64,
    /// The active bin the depositor saw, and how far it may have moved.
    pub active_id: i32,
    pub max_active_bin_slippage: i32,
    pub min_bin_id: i32,
    pub max_bin_id: i32,
    pub strategy: Strategy,
}

/// The accounts a position's liquidity instructions share, up to the token
/// programs.
fn liquidity_accounts(pair: &LbPair, owner: &Pubkey, position: &Pubkey) -> Vec<AccountMeta> {
    let (x, y) = token_accounts(pair, owner);
    vec![
        AccountMeta::new(*position, false),
        AccountMeta::new(pair.address, false),
        AccountMeta::new_readonly(PROGRAM_ID, false),
        AccountMeta::new(x, false),
        AccountMeta::new(y, false),
        AccountMeta::new(pair.reserve_x, false),
        AccountMeta::new(pair.reserve_y, false),
        AccountMeta::new_readonly(pair.mint_x, false),
        AccountMeta::new_readonly(pair.mint_y, false),
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new_readonly(pair.program_x, false),
        AccountMeta::new_readonly(pair.program_y, false),
    ]
}

/// `add_liquidity_by_strategy2`: deposit into `position` over its bins'
/// arrays `arrays`.
pub fn add_liquidity(
    pair: &LbPair,
    owner: &Pubkey,
    position: &Pubkey,
    deposit: &Deposit,
    arrays: &[i64],
) -> Instruction {
    let mut accounts = liquidity_accounts(pair, owner, position);
    accounts.push(AccountMeta::new_readonly(event_authority(), false));
    accounts.push(AccountMeta::new_readonly(PROGRAM_ID, false));
    accounts.extend(bin_arrays(pair, arrays));
    let mut data = ADD_LIQUIDITY_BY_STRATEGY2.to_vec();
    data.extend_from_slice(&deposit.amount_x.to_le_bytes());
    data.extend_from_slice(&deposit.amount_y.to_le_bytes());
    data.extend_from_slice(&deposit.active_id.to_le_bytes());
    data.extend_from_slice(&deposit.max_active_bin_slippage.to_le_bytes());
    data.extend_from_slice(&deposit.min_bin_id.to_le_bytes());
    data.extend_from_slice(&deposit.max_bin_id.to_le_bytes());
    data.push(deposit.strategy.imbalanced());
    data.extend_from_slice(&[0u8; 64]);
    data.extend_from_slice(&NO_HOOKS);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// `remove_liquidity_by_range2`: take `bps` (of 10,000) of each of the
/// position's bins from `from_bin_id` to `to_bin_id`. The program takes no
/// minimum.
pub fn remove_liquidity(
    pair: &LbPair,
    owner: &Pubkey,
    position: &super::Position,
    bps: u16,
) -> Instruction {
    let mut accounts = liquidity_accounts(pair, owner, &position.address);
    accounts.push(AccountMeta::new_readonly(MEMO_PROGRAM_ID, false));
    accounts.push(AccountMeta::new_readonly(event_authority(), false));
    accounts.push(AccountMeta::new_readonly(PROGRAM_ID, false));
    accounts.extend(bin_arrays(pair, &position.bin_arrays()));
    let mut data = REMOVE_LIQUIDITY_BY_RANGE2.to_vec();
    data.extend_from_slice(&position.lower_bin_id.to_le_bytes());
    data.extend_from_slice(&position.upper_bin_id.to_le_bytes());
    data.extend_from_slice(&bps.to_le_bytes());
    data.extend_from_slice(&NO_HOOKS);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// `claim_fee2`: collect the position's fees into `owner`'s accounts.
pub fn claim_fee(pair: &LbPair, owner: &Pubkey, position: &super::Position) -> Instruction {
    let (x, y) = token_accounts(pair, owner);
    let mut accounts = vec![
        AccountMeta::new(pair.address, false),
        AccountMeta::new(position.address, false),
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new(pair.reserve_x, false),
        AccountMeta::new(pair.reserve_y, false),
        AccountMeta::new(x, false),
        AccountMeta::new(y, false),
        AccountMeta::new_readonly(pair.mint_x, false),
        AccountMeta::new_readonly(pair.mint_y, false),
        AccountMeta::new_readonly(pair.program_x, false),
        AccountMeta::new_readonly(pair.program_y, false),
        AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
        AccountMeta::new_readonly(event_authority(), false),
        AccountMeta::new_readonly(PROGRAM_ID, false),
    ];
    accounts.extend(bin_arrays(pair, &position.bin_arrays()));
    let mut data = CLAIM_FEE2.to_vec();
    data.extend_from_slice(&position.lower_bin_id.to_le_bytes());
    data.extend_from_slice(&position.upper_bin_id.to_le_bytes());
    data.extend_from_slice(&NO_HOOKS);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::{SPCX_MINT, USDC_MINT, meteora::dlmm::state::testing::pair_bytes};

    #[test]
    fn instructions_carry_their_arguments_and_bin_arrays() {
        let pair = LbPair::decode(
            Pubkey::new_unique(),
            &pair_bytes(SPCX_MINT, USDC_MINT, 662, 75),
        )
        .unwrap();
        let owner = Pubkey::new_unique();
        let ix = swap(
            &pair,
            &owner,
            true,
            Swap::ExactIn {
                amount_in: 10,
                min_out: 9,
            },
            &[9, 8],
        );
        assert_eq!(ix.accounts.len(), 18);
        assert_eq!(ix.accounts[4].pubkey, token_accounts(&pair, &owner).0);
        assert_eq!(ix.accounts[17].pubkey, bin_array_address(&pair.address, 8));
        assert_eq!(
            ix.data,
            [
                &SWAP2[..],
                &10u64.to_le_bytes(),
                &9u64.to_le_bytes(),
                &NO_HOOKS
            ]
            .concat()
        );

        let deposit = Deposit {
            amount_x: 1,
            amount_y: 2,
            active_id: 662,
            max_active_bin_slippage: 3,
            min_bin_id: 640,
            max_bin_id: 680,
            strategy: Strategy::Curve,
        };
        let add = add_liquidity(&pair, &owner, &Pubkey::new_unique(), &deposit, &[9]);
        assert_eq!(add.data.len(), 8 + 16 + 16 + 1 + 64 + 4);
        assert_eq!(add.data[8 + 32], 7);
    }
}
