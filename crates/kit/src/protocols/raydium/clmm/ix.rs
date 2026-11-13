//! Raydium CLMM instructions, as the program's IDL defines them: the
//! 8-byte Anchor discriminator, then the Borsh-encoded arguments. The `v2`
//! forms take Token-2022 mints, and a position's NFT is a Token-2022 mint.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::{
    PROGRAM_ID, PoolState, personal_position_address, protocol_position_address,
    tick_array_address, tick_array_start,
};
use crate::token::{
    ASSOCIATED_TOKEN_PROGRAM_ID, SYSTEM_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID,
    associated_token_address,
};

/// The memo program, which the `v2` instructions take for Token-2022's
/// memo-required accounts.
pub const MEMO_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const RENT_SYSVAR: Pubkey = solana_pubkey::pubkey!("SysvarRent111111111111111111111111111111111");

const SWAP_V2: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];
const OPEN_POSITION_WITH_TOKEN22_NFT: [u8; 8] = [77, 255, 174, 82, 125, 29, 201, 46];
const INCREASE_LIQUIDITY_V2: [u8; 8] = [133, 29, 89, 223, 69, 238, 176, 10];
const DECREASE_LIQUIDITY_V2: [u8; 8] = [58, 127, 188, 62, 79, 82, 196, 96];

/// A pool's two tokens, as a trader's instructions name them: each mint's
/// token program, and the trader's accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolTokens {
    pub program_0: Pubkey,
    pub program_1: Pubkey,
}

impl PoolTokens {
    /// `owner`'s associated token accounts for the pool's two mints.
    pub fn accounts(&self, pool: &PoolState, owner: &Pubkey) -> (Pubkey, Pubkey) {
        (
            associated_token_address(owner, &pool.mint_0, &self.program_0),
            associated_token_address(owner, &pool.mint_1, &self.program_1),
        )
    }
}

/// How much a swap trades, and its limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swap {
    /// Token 0 in (the price falls), or token 1 in.
    pub zero_for_one: bool,
    /// The amount in atoms: of the token in, or with `is_base_input` off,
    /// of the token out.
    pub amount: u64,
    /// The least out, or the most in.
    pub other_amount_threshold: u64,
    /// The price the swap stops at, in Q64.64; 0 for none.
    pub sqrt_price_limit_x64: u128,
    pub is_base_input: bool,
}

/// `swap_v2` for `owner`, crossing the tick arrays starting at
/// `tick_arrays` (the current one first, as [`PoolState::swap_tick_arrays`]
/// lists them), with the pool's bitmap extension where one exists.
pub fn swap_v2(
    pool: &PoolState,
    tokens: &PoolTokens,
    owner: &Pubkey,
    swap: &Swap,
    tick_arrays: &[i32],
    bitmap_extension: Option<Pubkey>,
) -> Instruction {
    let (account_0, account_1) = tokens.accounts(pool, owner);
    let (input, output, input_vault, output_vault, input_mint, output_mint) = if swap.zero_for_one {
        (
            account_0,
            account_1,
            pool.vault_0,
            pool.vault_1,
            pool.mint_0,
            pool.mint_1,
        )
    } else {
        (
            account_1,
            account_0,
            pool.vault_1,
            pool.vault_0,
            pool.mint_1,
            pool.mint_0,
        )
    };
    let mut accounts = vec![
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new_readonly(pool.amm_config, false),
        AccountMeta::new(pool.address, false),
        AccountMeta::new(input, false),
        AccountMeta::new(output, false),
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
        accounts.push(AccountMeta::new(extension, false));
    }
    accounts.extend(
        tick_arrays
            .iter()
            .map(|start| AccountMeta::new(tick_array_address(&pool.address, *start), false)),
    );
    let mut data = SWAP_V2.to_vec();
    data.extend_from_slice(&swap.amount.to_le_bytes());
    data.extend_from_slice(&swap.other_amount_threshold.to_le_bytes());
    data.extend_from_slice(&swap.sqrt_price_limit_x64.to_le_bytes());
    data.push(u8::from(swap.is_base_input));
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// A new position's range and what it puts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenPosition {
    pub tick_lower: i32,
    pub tick_upper: i32,
    /// The most of each token it takes, in atoms.
    pub amount_0_max: u64,
    pub amount_1_max: u64,
    /// The token whose maximum sets the liquidity: token 0, or token 1.
    pub base_0: bool,
}

/// `open_position_with_token22_nft`: open a position for `owner`, its NFT
/// minted at `nft_mint` (a new keypair that signs too) into `owner`'s
/// Token-2022 account, with the liquidity the base token's maximum buys.
/// The program creates the range's tick arrays if they don't exist.
pub fn open_position(
    pool: &PoolState,
    tokens: &PoolTokens,
    owner: &Pubkey,
    nft_mint: &Pubkey,
    open: &OpenPosition,
) -> Instruction {
    let (account_0, account_1) = tokens.accounts(pool, owner);
    let lower_start = tick_array_start(open.tick_lower, pool.tick_spacing);
    let upper_start = tick_array_start(open.tick_upper, pool.tick_spacing);
    let accounts = vec![
        AccountMeta::new(*owner, true),
        AccountMeta::new_readonly(*owner, false),
        AccountMeta::new(*nft_mint, true),
        AccountMeta::new(
            associated_token_address(owner, nft_mint, &TOKEN_2022_PROGRAM_ID),
            false,
        ),
        AccountMeta::new(pool.address, false),
        AccountMeta::new_readonly(
            protocol_position_address(&pool.address, open.tick_lower, open.tick_upper),
            false,
        ),
        AccountMeta::new(tick_array_address(&pool.address, lower_start), false),
        AccountMeta::new(tick_array_address(&pool.address, upper_start), false),
        AccountMeta::new(personal_position_address(nft_mint), false),
        AccountMeta::new(account_0, false),
        AccountMeta::new(account_1, false),
        AccountMeta::new(pool.vault_0, false),
        AccountMeta::new(pool.vault_1, false),
        AccountMeta::new_readonly(RENT_SYSVAR, false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false),
        AccountMeta::new_readonly(pool.mint_0, false),
        AccountMeta::new_readonly(pool.mint_1, false),
    ];
    let mut data = OPEN_POSITION_WITH_TOKEN22_NFT.to_vec();
    data.extend_from_slice(&open.tick_lower.to_le_bytes());
    data.extend_from_slice(&open.tick_upper.to_le_bytes());
    data.extend_from_slice(&lower_start.to_le_bytes());
    data.extend_from_slice(&upper_start.to_le_bytes());
    data.extend_from_slice(&0u128.to_le_bytes());
    data.extend_from_slice(&open.amount_0_max.to_le_bytes());
    data.extend_from_slice(&open.amount_1_max.to_le_bytes());
    // No metadata; the base flag, as an Option<bool>.
    data.extend_from_slice(&[0, 1, u8::from(open.base_0)]);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// The accounts a position's own instructions name.
fn position_accounts(
    pool: &PoolState,
    position: &super::PersonalPosition,
    owner: &Pubkey,
) -> (Pubkey, Pubkey, Pubkey, Pubkey) {
    (
        associated_token_address(owner, &position.nft_mint, &TOKEN_2022_PROGRAM_ID),
        protocol_position_address(&pool.address, position.tick_lower, position.tick_upper),
        tick_array_address(
            &pool.address,
            tick_array_start(position.tick_lower, pool.tick_spacing),
        ),
        tick_array_address(
            &pool.address,
            tick_array_start(position.tick_upper, pool.tick_spacing),
        ),
    )
}

/// `increase_liquidity_v2`: add to `position` the liquidity the base
/// token's maximum buys.
pub fn increase_liquidity(
    pool: &PoolState,
    tokens: &PoolTokens,
    owner: &Pubkey,
    position: &super::PersonalPosition,
    amount_0_max: u64,
    amount_1_max: u64,
    base_0: bool,
) -> Instruction {
    let (account_0, account_1) = tokens.accounts(pool, owner);
    let (nft_account, protocol, lower, upper) = position_accounts(pool, position, owner);
    let accounts = vec![
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new_readonly(nft_account, false),
        AccountMeta::new(pool.address, false),
        AccountMeta::new_readonly(protocol, false),
        AccountMeta::new(position.address, false),
        AccountMeta::new(lower, false),
        AccountMeta::new(upper, false),
        AccountMeta::new(account_0, false),
        AccountMeta::new(account_1, false),
        AccountMeta::new(pool.vault_0, false),
        AccountMeta::new(pool.vault_1, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false),
        AccountMeta::new_readonly(pool.mint_0, false),
        AccountMeta::new_readonly(pool.mint_1, false),
    ];
    let mut data = INCREASE_LIQUIDITY_V2.to_vec();
    data.extend_from_slice(&0u128.to_le_bytes());
    data.extend_from_slice(&amount_0_max.to_le_bytes());
    data.extend_from_slice(&amount_1_max.to_le_bytes());
    data.extend_from_slice(&[1, u8::from(base_0)]);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// `decrease_liquidity_v2`: take `liquidity` out of `position`, and its
/// fees, into `owner`'s token accounts, failing below the minimums (fees
/// included). Liquidity 0 collects the fees only. The program pays out the
/// position's rewards too: each of the pool's rewards ([`PoolState::rewards`])
/// is named by its vault, `owner`'s account for it and its mint, in
/// `reward_accounts`.
#[allow(clippy::too_many_arguments)]
pub fn decrease_liquidity(
    pool: &PoolState,
    tokens: &PoolTokens,
    owner: &Pubkey,
    position: &super::PersonalPosition,
    liquidity: u128,
    amount_0_min: u64,
    amount_1_min: u64,
    reward_accounts: &[Pubkey],
) -> Instruction {
    let (account_0, account_1) = tokens.accounts(pool, owner);
    let (nft_account, protocol, lower, upper) = position_accounts(pool, position, owner);
    let mut accounts = vec![
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new_readonly(nft_account, false),
        AccountMeta::new(position.address, false),
        AccountMeta::new(pool.address, false),
        AccountMeta::new_readonly(protocol, false),
        AccountMeta::new(pool.vault_0, false),
        AccountMeta::new(pool.vault_1, false),
        AccountMeta::new(lower, false),
        AccountMeta::new(upper, false),
        AccountMeta::new(account_0, false),
        AccountMeta::new(account_1, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false),
        AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
        AccountMeta::new_readonly(pool.mint_0, false),
        AccountMeta::new_readonly(pool.mint_1, false),
    ];
    accounts.extend(reward_accounts.iter().map(|a| AccountMeta::new(*a, false)));
    let mut data = DECREASE_LIQUIDITY_V2.to_vec();
    data.extend_from_slice(&liquidity.to_le_bytes());
    data.extend_from_slice(&amount_0_min.to_le_bytes());
    data.extend_from_slice(&amount_1_min.to_le_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::{SPCX_MINT, USDC_MINT, raydium::clmm::state::testing::pool_bytes};

    #[test]
    fn a_swap_names_the_way_in_and_its_tick_arrays() {
        let pool = PoolState::decode(
            Pubkey::new_unique(),
            &pool_bytes(SPCX_MINT, USDC_MINT, 1 << 64, 0),
        )
        .unwrap();
        let tokens = PoolTokens {
            program_0: TOKEN_2022_PROGRAM_ID,
            program_1: TOKEN_PROGRAM_ID,
        };
        let owner = Pubkey::new_unique();
        let swap = Swap {
            zero_for_one: true,
            amount: 1_000_000,
            other_amount_threshold: 990_000,
            sqrt_price_limit_x64: 0,
            is_base_input: true,
        };
        let ix = swap_v2(&pool, &tokens, &owner, &swap, &[0, -3600], None);
        assert_eq!(ix.accounts.len(), 15);
        assert_eq!(ix.accounts[3].pubkey, tokens.accounts(&pool, &owner).0);
        assert_eq!(ix.accounts[11].pubkey, SPCX_MINT);
        assert_eq!(
            ix.accounts[14].pubkey,
            tick_array_address(&pool.address, -3600)
        );
        assert_eq!(ix.data.len(), 8 + 8 + 8 + 16 + 1);
        assert_eq!(&ix.data[..8], &SWAP_V2);

        let open = open_position(
            &pool,
            &tokens,
            &owner,
            &Pubkey::new_unique(),
            &OpenPosition {
                tick_lower: -120,
                tick_upper: 120,
                amount_0_max: 1,
                amount_1_max: 1,
                base_0: false,
            },
        );
        assert_eq!(open.accounts.len(), 20);
        assert_eq!(open.data.len(), 8 + 4 * 4 + 16 + 8 + 8 + 1 + 2);
        assert_eq!(&open.data[open.data.len() - 3..], &[0, 1, 0]);
    }
}
