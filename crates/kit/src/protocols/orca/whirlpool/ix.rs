//! Whirlpool instructions: the program's addresses and `swap_v2`, as
//! `programs/whirlpool/src/instructions/v2/swap.rs` at
//! `orca-so/whirlpools@cc683ec92acb5e7606caa3d8e514d9383e9c7c11` defines
//! them.
//!
//! `swap_v2` takes each mint and its token program, so one instruction
//! serves SPL Token and Token-2022 mints, and the memo program, which a
//! Token-2022 account that requires memos needs. Its remaining accounts
//! (transfer-hook accounts, supplemental tick arrays) are described by a
//! `RemainingAccountsInfo` argument; this builder passes none, which serves
//! mints without transfer hooks and swaps within the three tick arrays.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::{PROGRAM_ID, SwapDirection, SwapMode, quote::SWAP_TICK_ARRAYS_MAX, state::Whirlpool};
#[cfg(feature = "orca-whirlpool")]
use crate::chain::token::create_ata_idempotent_for;
use crate::token::{TOKEN_PROGRAM_ID, associated_token_address};

/// sha256("global:swap_v2")[..8].
pub const SWAP_V2_DISCRIMINATOR: [u8; 8] = [0x2b, 0x04, 0xed, 0x0b, 0x1a, 0xc9, 0x1e, 0x62];

/// The SPL Memo program, which `swap_v2` requires.
pub const MEMO_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

/// The tick array of `pool` starting at `start_tick_index`: seeds
/// `tick_array`, the pool, and the index in decimal text
/// (`sparse_swap.rs`, `derive_tick_array_pda`).
pub fn tick_array_address(pool: &Pubkey, start_tick_index: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"tick_array",
            pool.as_ref(),
            start_tick_index.to_string().as_bytes(),
        ],
        &PROGRAM_ID,
    )
    .0
}

/// The pool's oracle, which holds its adaptive fee (`swap_v2` takes it
/// whether or not it exists).
pub fn oracle_address(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"oracle", pool.as_ref()], &PROGRAM_ID).0
}

/// The token program of each of a pool's mints: a mint's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MintPrograms {
    pub token_a: Pubkey,
    pub token_b: Pubkey,
}

impl Default for MintPrograms {
    /// Both mints under SPL Token, as SOL/USDC is.
    fn default() -> Self {
        Self {
            token_a: TOKEN_PROGRAM_ID,
            token_b: TOKEN_PROGRAM_ID,
        }
    }
}

/// A swap to send: what [`swap_v2`] encodes besides the accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapV2Args {
    pub direction: SwapDirection,
    pub mode: SwapMode,
    /// What the trader pays (exact in) or receives (exact out), in atoms.
    pub amount: u64,
    /// The least the trader accepts to receive (exact in) or the most it
    /// accepts to pay (exact out), in atoms.
    pub other_amount_threshold: u64,
    /// Q64.64; 0 for none, which refuses a partial exact-out fill.
    pub sqrt_price_limit_x64: u128,
}

/// `swap_v2` for `owner`, paying from and receiving into its associated
/// token accounts. `tick_arrays` are
/// [`SwapQuote::tick_arrays`](super::SwapQuote::tick_arrays).
pub fn swap_v2(
    owner: &Pubkey,
    pool_address: &Pubkey,
    pool: &Whirlpool,
    mint_programs: &MintPrograms,
    tick_arrays: &[Pubkey; SWAP_TICK_ARRAYS_MAX],
    args: &SwapV2Args,
) -> Instruction {
    assert!(args.amount > 0, "the program refuses a zero amount");
    let owner_account_a =
        associated_token_address(owner, &pool.token_mint_a, &mint_programs.token_a);
    let owner_account_b =
        associated_token_address(owner, &pool.token_mint_b, &mint_programs.token_b);
    let accounts = vec![
        AccountMeta::new_readonly(mint_programs.token_a, false),
        AccountMeta::new_readonly(mint_programs.token_b, false),
        AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new(*pool_address, false),
        AccountMeta::new_readonly(pool.token_mint_a, false),
        AccountMeta::new_readonly(pool.token_mint_b, false),
        AccountMeta::new(owner_account_a, false),
        AccountMeta::new(pool.token_vault_a, false),
        AccountMeta::new(owner_account_b, false),
        AccountMeta::new(pool.token_vault_b, false),
        AccountMeta::new(tick_arrays[0], false),
        AccountMeta::new(tick_arrays[1], false),
        AccountMeta::new(tick_arrays[2], false),
        AccountMeta::new(oracle_address(pool_address), false),
    ];
    let mut data = Vec::with_capacity(8 + 8 + 8 + 16 + 1 + 1 + 1);
    data.extend_from_slice(&SWAP_V2_DISCRIMINATOR);
    data.extend_from_slice(&args.amount.to_le_bytes());
    data.extend_from_slice(&args.other_amount_threshold.to_le_bytes());
    data.extend_from_slice(&args.sqrt_price_limit_x64.to_le_bytes());
    data.push(u8::from(args.mode == SwapMode::ExactIn));
    data.push(u8::from(args.direction == SwapDirection::AToB));
    // `remaining_accounts_info: Option<RemainingAccountsInfo>`: None.
    data.push(0);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// Creates `owner`'s associated token accounts for both of the pool's
/// mints, if they don't exist. Wrapping SOL into the input account is
/// [`chain::token::wrap_sol`](crate::chain::token::wrap_sol). With the
/// chain only (feature `orca-whirlpool`).
#[cfg(feature = "orca-whirlpool")]
pub fn create_token_accounts(
    payer: &Pubkey,
    owner: &Pubkey,
    pool: &Whirlpool,
    mint_programs: &MintPrograms,
) -> [Instruction; 2] {
    [
        create_ata_idempotent_for(payer, owner, &pool.token_mint_a, &mint_programs.token_a),
        create_ata_idempotent_for(payer, owner, &pool.token_mint_b, &mint_programs.token_b),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_v2_discriminator_is_anchors_hash_of_its_name() {
        // `printf global:swap_v2 | shasum -a 256` starts 2b04ed0b1ac91e62.
        assert_eq!(
            u64::from_be_bytes(SWAP_V2_DISCRIMINATOR),
            0x2b04ed0b1ac91e62
        );
    }

    /// `sparse_swap.rs`, `test_derive_tick_array_pda`: the SOL/USDC pool
    /// with tick spacing 64.
    #[test]
    fn tick_array_addresses_match_the_program() {
        let pool = solana_pubkey::pubkey!("HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ");
        let expected = [
            (
                -11264,
                solana_pubkey::pubkey!("81T5kNuPRkyVzhwbe2RpKR7wmQpGJ7RBkGPdTqyfa5vq"),
            ),
            (
                -5632,
                solana_pubkey::pubkey!("9K1HWrGKZKfjTnKfF621BmEQdai4FcUz9tsoF41jwz5B"),
            ),
            (
                0,
                solana_pubkey::pubkey!("JCpxMSDRDPBMqjoX7LkhMwro2y6r85Q8E6p5zNdBZyWa"),
            ),
            (
                5632,
                solana_pubkey::pubkey!("BW2Mr823NUQN7vnVpv5E6yCTnqEXQ3ZnqjZyiywXPcUp"),
            ),
            (
                11264,
                solana_pubkey::pubkey!("2ezvsnoXdukw5dAAZ4EkW67bmUo8PHRPX8ZDqf76BKtV"),
            ),
        ];
        for (start, address) in expected {
            assert_eq!(tick_array_address(&pool, start), address, "start {start}");
        }
    }
}
