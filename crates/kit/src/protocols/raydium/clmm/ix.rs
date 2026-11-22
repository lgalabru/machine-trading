//! Raydium CLMM instructions: the program's addresses and `swap_v2`, as
//! `programs/amm/src/instructions/swap_v2.rs` at
//! `raydium-io/raydium-clmm@6d7a0f82d2f293c553c69aaf943377367c6c4cc6`
//! defines them.
//!
//! `swap_v2` takes both token programs and the memo program whatever the
//! mints are, so one instruction serves SPL Token and Token-2022 mints. Its
//! remaining accounts are the pool's bitmap extension (when the swap may
//! reach past the pool's own bitmap) and the tick arrays the swap enters,
//! in order; the program skips any it doesn't need at the front, and stops
//! reading at the first account that is neither.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::{PROGRAM_ID, SwapDirection, SwapMode, state::Pool};
#[cfg(feature = "raydium-clmm")]
use crate::chain::token::create_ata_idempotent_for;
use crate::token::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, associated_token_address};

/// sha256("global:swap_v2")[..8].
pub const SWAP_V2_DISCRIMINATOR: [u8; 8] = [0x2b, 0x04, 0xed, 0x0b, 0x1a, 0xc9, 0x1e, 0x62];

/// The SPL Memo program, which `swap_v2` requires (for Token-2022 mints
/// with required memos).
pub const MEMO_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

/// The tick array of `pool` starting at `start_tick_index`: seeds
/// `tick_array`, the pool, and the index as big-endian bytes.
pub fn tick_array_address(pool: &Pubkey, start_tick_index: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"tick_array",
            pool.as_ref(),
            &start_tick_index.to_be_bytes(),
        ],
        &PROGRAM_ID,
    )
    .0
}

/// The pool's tick array bitmap extension.
pub fn bitmap_extension_address(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"pool_tick_array_bitmap_extension", pool.as_ref()],
        &PROGRAM_ID,
    )
    .0
}

/// The token program of each of a pool's mints: a mint's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MintPrograms {
    pub token_0: Pubkey,
    pub token_1: Pubkey,
}

impl Default for MintPrograms {
    /// Both mints under SPL Token, as SOL/USDC is.
    fn default() -> Self {
        Self {
            token_0: TOKEN_PROGRAM_ID,
            token_1: TOKEN_PROGRAM_ID,
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
    /// Q64.64; 0 for none, which refuses a partial fill.
    pub sqrt_price_limit_x64: u128,
}

/// `swap_v2` for `owner`, paying from and receiving into its associated
/// token accounts. `tick_arrays` are the arrays the swap enters, in order
/// ([`SwapQuote::tick_arrays`](super::SwapQuote::tick_arrays)); the
/// extension, when given, goes first.
pub fn swap_v2(
    owner: &Pubkey,
    pool_address: &Pubkey,
    pool: &Pool,
    mint_programs: &MintPrograms,
    bitmap_extension: Option<&Pubkey>,
    tick_arrays: &[Pubkey],
    args: &SwapV2Args,
) -> Instruction {
    assert!(
        !tick_arrays.is_empty(),
        "a swap enters at least one tick array"
    );
    let owner_account_0 =
        associated_token_address(owner, &pool.token_mint_0, &mint_programs.token_0);
    let owner_account_1 =
        associated_token_address(owner, &pool.token_mint_1, &mint_programs.token_1);
    let (input_account, output_account, input_vault, output_vault, input_mint, output_mint) =
        match args.direction {
            SwapDirection::ZeroForOne => (
                owner_account_0,
                owner_account_1,
                pool.token_vault_0,
                pool.token_vault_1,
                pool.token_mint_0,
                pool.token_mint_1,
            ),
            SwapDirection::OneForZero => (
                owner_account_1,
                owner_account_0,
                pool.token_vault_1,
                pool.token_vault_0,
                pool.token_mint_1,
                pool.token_mint_0,
            ),
        };
    let mut accounts = vec![
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new_readonly(pool.amm_config, false),
        AccountMeta::new(*pool_address, false),
        AccountMeta::new(input_account, false),
        AccountMeta::new(output_account, false),
        AccountMeta::new(input_vault, false),
        AccountMeta::new(output_vault, false),
        AccountMeta::new(pool.observation, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false),
        AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
        AccountMeta::new_readonly(input_mint, false),
        AccountMeta::new_readonly(output_mint, false),
    ];
    if let Some(extension) = bitmap_extension {
        accounts.push(AccountMeta::new(*extension, false));
    }
    accounts.extend(
        tick_arrays
            .iter()
            .map(|array| AccountMeta::new(*array, false)),
    );
    let mut data = Vec::with_capacity(8 + 8 + 8 + 16 + 1);
    data.extend_from_slice(&SWAP_V2_DISCRIMINATOR);
    data.extend_from_slice(&args.amount.to_le_bytes());
    data.extend_from_slice(&args.other_amount_threshold.to_le_bytes());
    data.extend_from_slice(&args.sqrt_price_limit_x64.to_le_bytes());
    data.push(u8::from(args.mode == SwapMode::ExactIn));
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// Creates `owner`'s associated token accounts for both of the pool's
/// mints, if they don't exist. Wrapping SOL into the input account is
/// [`chain::token::wrap_sol`](crate::chain::token::wrap_sol). With the
/// chain only (feature `raydium-clmm`).
#[cfg(feature = "raydium-clmm")]
pub fn create_token_accounts(
    payer: &Pubkey,
    owner: &Pubkey,
    pool: &Pool,
    mint_programs: &MintPrograms,
) -> [Instruction; 2] {
    [
        create_ata_idempotent_for(payer, owner, &pool.token_mint_0, &mint_programs.token_0),
        create_ata_idempotent_for(payer, owner, &pool.token_mint_1, &mint_programs.token_1),
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

    /// Addresses of the mainnet SOL/USDC pool's accounts; the fixture test
    /// (`tests/raydium_clmm.rs`) checks every tick array's.
    #[test]
    fn addresses_match_mainnet() {
        let pool = solana_pubkey::pubkey!("3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv");
        assert_eq!(
            bitmap_extension_address(&pool),
            solana_pubkey::pubkey!("4NFvUKqknMpoe6CWTzK758B8ojVLzURL5pC6MtiaJ8TQ")
        );
        assert_eq!(
            tick_array_address(&pool, -26580),
            solana_pubkey::pubkey!("5uZcq1jx7AvKNmdrSHN69fPRoGqdoK4WUtREMJcopEVb")
        );
    }
}
