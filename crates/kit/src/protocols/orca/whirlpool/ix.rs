//! Whirlpool instructions, as the program defines them: the 8-byte Anchor
//! discriminator, then the Borsh-encoded arguments. The `v2` forms take
//! Token-2022 mints, and a position's NFT is a Token-2022 mint.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::{
    PROGRAM_ID, PoolState, Position, oracle_address, position_address, tick_array_address,
    tick_array_start,
};
use crate::token::{
    ASSOCIATED_TOKEN_PROGRAM_ID, SYSTEM_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, associated_token_address,
};

/// The memo program, which the `v2` instructions take for Token-2022's
/// memo-required accounts.
pub const MEMO_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
/// The authority position NFTs' metadata names; the instruction takes it
/// even when a position has no metadata.
const METADATA_UPDATE_AUTH: Pubkey =
    solana_pubkey::pubkey!("3axbTs2z5GBy6usVbNVoqEgZMng3vZvMnAoX29BFfwhr");

const SWAP_V2: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];
const INITIALIZE_DYNAMIC_TICK_ARRAY: [u8; 8] = [41, 33, 165, 200, 120, 231, 142, 50];
const OPEN_POSITION_WITH_TOKEN_EXTENSIONS: [u8; 8] = [212, 47, 95, 92, 114, 102, 131, 250];
const INCREASE_LIQUIDITY_V2: [u8; 8] = [133, 29, 89, 223, 69, 238, 176, 10];
const DECREASE_LIQUIDITY_V2: [u8; 8] = [58, 127, 188, 62, 79, 82, 196, 96];
const UPDATE_FEES_AND_REWARDS: [u8; 8] = [154, 230, 250, 13, 236, 209, 75, 223];
const COLLECT_FEES_V2: [u8; 8] = [207, 117, 95, 191, 229, 180, 226, 15];

/// A pool's two tokens, as a trader's instructions name them: each mint's
/// token program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolTokens {
    pub program_a: Pubkey,
    pub program_b: Pubkey,
}

impl PoolTokens {
    /// `owner`'s associated token accounts for the pool's two mints.
    pub fn accounts(&self, pool: &PoolState, owner: &Pubkey) -> (Pubkey, Pubkey) {
        (
            associated_token_address(owner, &pool.mint_a, &self.program_a),
            associated_token_address(owner, &pool.mint_b, &self.program_b),
        )
    }
}

/// How much a swap trades, and its limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swap {
    /// Token A in (the price falls), or token B in.
    pub a_to_b: bool,
    /// The amount in atoms: of the token in, or with `is_input` off, of the
    /// token out.
    pub amount: u64,
    /// The least out, or the most in.
    pub other_amount_threshold: u64,
    /// The price the swap stops at, in Q64.64; 0 for the program's bound.
    pub sqrt_price_limit_x64: u128,
    /// `amount` is what goes in (else what comes out).
    pub is_input: bool,
}

/// `swap_v2` for `owner`, crossing the tick arrays starting at
/// `tick_arrays` (as [`PoolState::swap_tick_arrays`] lists them; the last is
/// repeated to make three).
pub fn swap_v2(
    pool: &PoolState,
    tokens: &PoolTokens,
    owner: &Pubkey,
    swap: &Swap,
    tick_arrays: &[i32],
) -> Instruction {
    let (account_a, account_b) = tokens.accounts(pool, owner);
    let array = |i: usize| {
        let start = tick_arrays
            .get(i)
            .or(tick_arrays.last())
            .copied()
            .unwrap_or_else(|| tick_array_start(pool.tick_current, pool.tick_spacing));
        AccountMeta::new(tick_array_address(&pool.address, start), false)
    };
    let accounts = vec![
        AccountMeta::new_readonly(tokens.program_a, false),
        AccountMeta::new_readonly(tokens.program_b, false),
        AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new(pool.address, false),
        AccountMeta::new_readonly(pool.mint_a, false),
        AccountMeta::new_readonly(pool.mint_b, false),
        AccountMeta::new(account_a, false),
        AccountMeta::new(pool.vault_a, false),
        AccountMeta::new(account_b, false),
        AccountMeta::new(pool.vault_b, false),
        array(0),
        array(1),
        array(2),
        AccountMeta::new(oracle_address(&pool.address), false),
    ];
    let mut data = SWAP_V2.to_vec();
    data.extend_from_slice(&swap.amount.to_le_bytes());
    data.extend_from_slice(&swap.other_amount_threshold.to_le_bytes());
    data.extend_from_slice(&swap.sqrt_price_limit_x64.to_le_bytes());
    data.push(u8::from(swap.is_input));
    data.push(u8::from(swap.a_to_b));
    // No remaining-accounts info: no transfer-hook or supplemental arrays.
    data.push(0);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// `initialize_dynamic_tick_array`, idempotent: make the tick array
/// starting at `start` if it doesn't exist, `funder` paying its rent. A
/// position's two arrays must exist before liquidity goes into it.
pub fn initialize_tick_array(pool: &PoolState, funder: &Pubkey, start: i32) -> Instruction {
    let accounts = vec![
        AccountMeta::new_readonly(pool.address, false),
        AccountMeta::new(*funder, true),
        AccountMeta::new(tick_array_address(&pool.address, start), false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
    ];
    let mut data = INITIALIZE_DYNAMIC_TICK_ARRAY.to_vec();
    data.extend_from_slice(&start.to_le_bytes());
    data.push(1);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// `open_position_with_token_extensions`: open an empty position over
/// `tick_lower..tick_upper` for `owner`, its NFT minted at `position_mint`
/// (a new keypair that signs too) into `owner`'s Token-2022 account, with
/// no metadata. Liquidity goes in with [`increase_liquidity`].
pub fn open_position(
    pool: &PoolState,
    owner: &Pubkey,
    position_mint: &Pubkey,
    tick_lower: i32,
    tick_upper: i32,
) -> Instruction {
    let accounts = vec![
        AccountMeta::new(*owner, true),
        AccountMeta::new_readonly(*owner, false),
        AccountMeta::new(position_address(position_mint), false),
        AccountMeta::new(*position_mint, true),
        AccountMeta::new(
            associated_token_address(owner, position_mint, &TOKEN_2022_PROGRAM_ID),
            false,
        ),
        AccountMeta::new_readonly(pool.address, false),
        AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(METADATA_UPDATE_AUTH, false),
    ];
    let mut data = OPEN_POSITION_WITH_TOKEN_EXTENSIONS.to_vec();
    data.extend_from_slice(&tick_lower.to_le_bytes());
    data.extend_from_slice(&tick_upper.to_le_bytes());
    data.push(0);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// The accounts a position's own instructions name.
fn position_accounts(
    pool: &PoolState,
    position: &Position,
    owner: &Pubkey,
) -> (Pubkey, Pubkey, Pubkey) {
    (
        associated_token_address(owner, &position.position_mint, &TOKEN_2022_PROGRAM_ID),
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

/// What `increase_liquidity_v2` and `decrease_liquidity_v2` take: the
/// liquidity, and the most of each token it may cost or the least of each it
/// must return, in atoms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liquidity {
    pub liquidity: u128,
    pub token_a: u64,
    pub token_b: u64,
}

fn modify_liquidity(
    discriminator: [u8; 8],
    pool: &PoolState,
    tokens: &PoolTokens,
    owner: &Pubkey,
    position: &Position,
    liquidity: &Liquidity,
) -> Instruction {
    let (account_a, account_b) = tokens.accounts(pool, owner);
    let (nft_account, lower, upper) = position_accounts(pool, position, owner);
    let accounts = vec![
        AccountMeta::new(pool.address, false),
        AccountMeta::new_readonly(tokens.program_a, false),
        AccountMeta::new_readonly(tokens.program_b, false),
        AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new(position.address, false),
        AccountMeta::new_readonly(nft_account, false),
        AccountMeta::new_readonly(pool.mint_a, false),
        AccountMeta::new_readonly(pool.mint_b, false),
        AccountMeta::new(account_a, false),
        AccountMeta::new(account_b, false),
        AccountMeta::new(pool.vault_a, false),
        AccountMeta::new(pool.vault_b, false),
        AccountMeta::new(lower, false),
        AccountMeta::new(upper, false),
    ];
    let mut data = discriminator.to_vec();
    data.extend_from_slice(&liquidity.liquidity.to_le_bytes());
    data.extend_from_slice(&liquidity.token_a.to_le_bytes());
    data.extend_from_slice(&liquidity.token_b.to_le_bytes());
    data.push(0);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// `increase_liquidity_v2`: add `liquidity.liquidity` to `position`,
/// failing if it costs more than `token_a` or `token_b`.
pub fn increase_liquidity(
    pool: &PoolState,
    tokens: &PoolTokens,
    owner: &Pubkey,
    position: &Position,
    liquidity: &Liquidity,
) -> Instruction {
    modify_liquidity(
        INCREASE_LIQUIDITY_V2,
        pool,
        tokens,
        owner,
        position,
        liquidity,
    )
}

/// `decrease_liquidity_v2`: take `liquidity.liquidity` out of `position`
/// into `owner`'s token accounts, failing below `token_a` or `token_b`. The
/// fees stay on the position until [`collect_fees`].
pub fn decrease_liquidity(
    pool: &PoolState,
    tokens: &PoolTokens,
    owner: &Pubkey,
    position: &Position,
    liquidity: &Liquidity,
) -> Instruction {
    modify_liquidity(
        DECREASE_LIQUIDITY_V2,
        pool,
        tokens,
        owner,
        position,
        liquidity,
    )
}

/// `update_fees_and_rewards`: count the fees `position` has earned into its
/// owed amounts, which [`collect_fees`] pays.
pub fn update_fees_and_rewards(
    pool: &PoolState,
    owner: &Pubkey,
    position: &Position,
) -> Instruction {
    let (_, lower, upper) = position_accounts(pool, position, owner);
    let accounts = vec![
        AccountMeta::new(pool.address, false),
        AccountMeta::new(position.address, false),
        AccountMeta::new_readonly(lower, false),
        AccountMeta::new_readonly(upper, false),
    ];
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data: UPDATE_FEES_AND_REWARDS.to_vec(),
    }
}

/// `collect_fees_v2`: pay `position`'s owed fees to `owner`'s token
/// accounts.
pub fn collect_fees(
    pool: &PoolState,
    tokens: &PoolTokens,
    owner: &Pubkey,
    position: &Position,
) -> Instruction {
    let (account_a, account_b) = tokens.accounts(pool, owner);
    let (nft_account, ..) = position_accounts(pool, position, owner);
    let accounts = vec![
        AccountMeta::new_readonly(pool.address, false),
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new(position.address, false),
        AccountMeta::new_readonly(nft_account, false),
        AccountMeta::new_readonly(pool.mint_a, false),
        AccountMeta::new_readonly(pool.mint_b, false),
        AccountMeta::new(account_a, false),
        AccountMeta::new(pool.vault_a, false),
        AccountMeta::new(account_b, false),
        AccountMeta::new(pool.vault_b, false),
        AccountMeta::new_readonly(tokens.program_a, false),
        AccountMeta::new_readonly(tokens.program_b, false),
        AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
    ];
    let mut data = COLLECT_FEES_V2.to_vec();
    data.push(0);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocols::{SPCX_MINT, USDC_MINT, orca::whirlpool::state::testing::pool_bytes},
        token::TOKEN_PROGRAM_ID,
    };

    fn pool() -> PoolState {
        PoolState::decode(
            Pubkey::new_unique(),
            &pool_bytes(SPCX_MINT, USDC_MINT, 1 << 64, 0),
        )
        .unwrap()
    }

    fn tokens() -> PoolTokens {
        PoolTokens {
            program_a: TOKEN_2022_PROGRAM_ID,
            program_b: TOKEN_PROGRAM_ID,
        }
    }

    fn position(pool: &PoolState) -> Position {
        Position {
            address: Pubkey::new_unique(),
            pool: pool.address,
            position_mint: Pubkey::new_unique(),
            liquidity: 10,
            tick_lower: -128,
            tick_upper: 6000,
            fee_growth_checkpoint_a_x64: 0,
            fee_growth_checkpoint_b_x64: 0,
            fee_owed_a: 0,
            fee_owed_b: 0,
        }
    }

    #[test]
    fn a_swap_names_the_way_in_and_three_tick_arrays() {
        let (pool, tokens, owner) = (pool(), tokens(), Pubkey::new_unique());
        let swap = Swap {
            a_to_b: true,
            amount: 1_000_000,
            other_amount_threshold: 990_000,
            sqrt_price_limit_x64: 0,
            is_input: true,
        };
        let ix = swap_v2(&pool, &tokens, &owner, &swap, &[0, -5632]);
        assert_eq!(ix.accounts.len(), 15);
        assert_eq!(ix.accounts[7].pubkey, tokens.accounts(&pool, &owner).0);
        assert_eq!(ix.accounts[5].pubkey, SPCX_MINT);
        assert_eq!(ix.accounts[11].pubkey, tick_array_address(&pool.address, 0));
        assert_eq!(
            ix.accounts[13].pubkey,
            tick_array_address(&pool.address, -5632)
        );
        assert_eq!(ix.accounts[14].pubkey, oracle_address(&pool.address));
        assert_eq!(ix.data.len(), 8 + 8 + 8 + 16 + 1 + 1 + 1);
        assert_eq!(&ix.data[..8], &SWAP_V2);
        assert_eq!(&ix.data[ix.data.len() - 3..], &[1, 1, 0]);
    }

    #[test]
    fn a_position_opens_and_moves_liquidity_through_its_own_accounts() {
        let (pool, tokens, owner) = (pool(), tokens(), Pubkey::new_unique());
        let mint = Pubkey::new_unique();
        let open = open_position(&pool, &owner, &mint, -128, 6000);
        assert_eq!(open.accounts.len(), 10);
        assert_eq!(open.accounts[3].pubkey, mint);
        assert!(open.accounts[3].is_signer && open.accounts[0].is_signer);
        assert_eq!(open.accounts[2].pubkey, position_address(&mint));
        assert_eq!(open.data.len(), 8 + 4 + 4 + 1);

        let position = position(&pool);
        let liquidity = Liquidity {
            liquidity: 5,
            token_a: 1,
            token_b: 2,
        };
        let add = increase_liquidity(&pool, &tokens, &owner, &position, &liquidity);
        let sub = decrease_liquidity(&pool, &tokens, &owner, &position, &liquidity);
        assert_eq!((add.accounts.len(), sub.accounts.len()), (15, 15));
        assert_eq!(add.data.len(), 8 + 16 + 8 + 8 + 1);
        assert_eq!(&add.data[..8], &INCREASE_LIQUIDITY_V2);
        assert_eq!(&sub.data[..8], &DECREASE_LIQUIDITY_V2);
        // The position's two tick arrays, then none of the owner's.
        assert_eq!(
            add.accounts[13].pubkey,
            tick_array_address(&pool.address, -5632)
        );
        assert_eq!(
            add.accounts[14].pubkey,
            tick_array_address(&pool.address, 5632)
        );

        let update = update_fees_and_rewards(&pool, &owner, &position);
        assert_eq!((update.accounts.len(), update.data.len()), (4, 8));
        let collect = collect_fees(&pool, &tokens, &owner, &position);
        assert_eq!(collect.accounts.len(), 13);
        assert_eq!(collect.accounts[6].pubkey, tokens.accounts(&pool, &owner).0);

        let array = initialize_tick_array(&pool, &owner, -5632);
        assert_eq!(
            array.accounts[2].pubkey,
            tick_array_address(&pool.address, -5632)
        );
        assert_eq!(array.data.len(), 8 + 4 + 1);
    }
}
