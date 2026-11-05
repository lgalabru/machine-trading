//! Pinned real-program instruction adapters. All amounts are native integers.
use std::str::FromStr;

use anyhow::Result;
use sha2::{Digest, Sha256};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use crate::{CPMM, WHIRLPOOL};

pub const RAYDIUM_REV: &str = "3d69122790e97cd6aff24e8ba95940793773f58c";
pub const ORCA_REV: &str = "cc683ec92acb5e7606caa3d8e514d9383e9c7c11";
pub const SURFPOOL_REV: &str = "64d531fb264bb976f1569f509362a7dfa30b4c26";
pub const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
pub const RENT: &str = "SysvarRent111111111111111111111111111111111";
pub const CPMM_FEE_RECEIVER: &str = "DNXgeM9EiiaAbaWvwjHj9fQQLAX5ZsfHyvmYUNRAdNC8";

pub fn key(value: &str) -> Pubkey {
    Pubkey::from_str(value).expect("validated public key")
}
pub fn ro(key: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(key, false)
}
pub fn rw(key: Pubkey) -> AccountMeta {
    AccountMeta::new(key, false)
}
pub fn signer(key: Pubkey) -> AccountMeta {
    AccountMeta::new(key, true)
}
pub fn pda(program: Pubkey, seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &program).0
}
pub fn ata(owner: Pubkey, mint: Pubkey) -> Pubkey {
    spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
        &owner,
        &mint,
        &key(TOKEN),
    )
}
pub fn discriminator(name: &str) -> [u8; 8] {
    Sha256::digest(format!("global:{name}").as_bytes())[..8]
        .try_into()
        .unwrap()
}
pub fn anchor(program: Pubkey, name: &str, accounts: Vec<AccountMeta>, args: &[u8]) -> Instruction {
    let mut data = discriminator(name).to_vec();
    data.extend_from_slice(args);
    Instruction {
        program_id: program,
        accounts,
        data,
    }
}

pub fn cpmm_config(owner: Pubkey, index: u16, fee: u64) -> (Pubkey, Instruction) {
    let program = key(CPMM);
    let config = pda(program, &[b"amm_config", &index.to_be_bytes()]);
    let mut args = index.to_le_bytes().to_vec();
    for n in [fee, 0, 0, 0, 0] {
        args.extend_from_slice(&n.to_le_bytes());
    }
    (
        config,
        anchor(
            program,
            "create_amm_config",
            vec![signer(owner), rw(config), ro(Pubkey::default())],
            &args,
        ),
    )
}

pub struct CpmmInit {
    pub state: Pubkey,
    pub authority: Pubkey,
    pub vaults: [Pubkey; 2],
    pub observation: Pubkey,
    pub instruction: Instruction,
}

pub fn cpmm_initialize(
    owner: Pubkey,
    config: Pubkey,
    mints: [Pubkey; 2],
    amounts: [u64; 2],
) -> CpmmInit {
    let program = key(CPMM);
    let state = pda(
        program,
        &[
            b"pool",
            config.as_ref(),
            mints[0].as_ref(),
            mints[1].as_ref(),
        ],
    );
    let authority = pda(program, &[b"vault_and_lp_mint_auth_seed"]);
    let vaults = mints.map(|m| pda(program, &[b"pool_vault", state.as_ref(), m.as_ref()]));
    let lp = pda(program, &[b"pool_lp_mint", state.as_ref()]);
    let observation = pda(program, &[b"observation", state.as_ref()]);
    let mut args = Vec::new();
    for n in [amounts[0], amounts[1], 0] {
        args.extend_from_slice(&n.to_le_bytes());
    }
    let instruction = anchor(
        program,
        "initialize",
        vec![
            signer(owner),
            ro(config),
            ro(authority),
            rw(state),
            ro(mints[0]),
            ro(mints[1]),
            rw(lp),
            rw(ata(owner, mints[0])),
            rw(ata(owner, mints[1])),
            rw(ata(owner, lp)),
            rw(vaults[0]),
            rw(vaults[1]),
            rw(key(CPMM_FEE_RECEIVER)),
            rw(observation),
            ro(key(TOKEN)),
            ro(key(TOKEN)),
            ro(key(TOKEN)),
            ro(key(ATA_PROGRAM)),
            ro(Pubkey::default()),
            ro(key(RENT)),
        ],
        &args,
    );
    CpmmInit {
        state,
        authority,
        vaults,
        observation,
        instruction,
    }
}

pub fn cpmm_swap(
    pool: &crate::fixture::Pool,
    owner: Pubkey,
    a_to_b: bool,
    amount: u64,
    min_out: u64,
) -> Instruction {
    let i = usize::from(!a_to_b);
    let j = 1 - i;
    let mints = pool.mints.map(|m| key(&pool.token_addresses[m]));
    let mut args = amount.to_le_bytes().to_vec();
    args.extend_from_slice(&min_out.to_le_bytes());
    anchor(
        key(CPMM),
        "swap_base_input",
        vec![
            AccountMeta::new_readonly(owner, true),
            ro(pda(key(CPMM), &[b"vault_and_lp_mint_auth_seed"])),
            ro(key(&pool.config)),
            rw(key(&pool.address)),
            rw(ata(owner, mints[i])),
            rw(ata(owner, mints[j])),
            rw(key(&pool.vaults[i])),
            rw(key(&pool.vaults[j])),
            ro(key(TOKEN)),
            ro(key(TOKEN)),
            ro(mints[i]),
            ro(mints[j]),
            rw(key(&pool.observation)),
        ],
        &args,
    )
}

pub fn whirlpool_address(config: Pubkey, mints: [Pubkey; 2], spacing: u16) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            b"whirlpool",
            config.as_ref(),
            mints[0].as_ref(),
            mints[1].as_ref(),
            &spacing.to_le_bytes(),
        ],
        &key(WHIRLPOOL),
    )
}
pub fn tick_array(pool: Pubkey, start: i32) -> Pubkey {
    pda(
        key(WHIRLPOOL),
        &[b"tick_array", pool.as_ref(), start.to_string().as_bytes()],
    )
}
pub fn whirlpool_swap(
    pool: &crate::fixture::Pool,
    owner: Pubkey,
    a_to_b: bool,
    amount: u64,
    min_out: u64,
    current_tick: i32,
) -> Result<Instruction> {
    let mints = pool.mints.map(|m| key(&pool.token_addresses[m]));
    let span = i32::from(pool.tick_spacing) * 88;
    let start = if a_to_b {
        current_tick
    } else {
        current_tick + i32::from(pool.tick_spacing)
    }
    .div_euclid(span)
        * span;
    let arrays = [0, 1, 2].map(|n| {
        tick_array(
            key(&pool.address),
            start + if a_to_b { -n * span } else { n * span },
        )
    });
    let mut args = amount.to_le_bytes().to_vec();
    args.extend_from_slice(&min_out.to_le_bytes());
    let limit: u128 = if a_to_b {
        4_295_048_016
    } else {
        79_226_673_515_401_279_992_447_579_055
    };
    args.extend_from_slice(&limit.to_le_bytes());
    args.extend_from_slice(&[1, u8::from(a_to_b)]);
    Ok(anchor(
        key(WHIRLPOOL),
        "swap",
        vec![
            ro(key(TOKEN)),
            AccountMeta::new_readonly(owner, true),
            rw(key(&pool.address)),
            rw(ata(owner, mints[0])),
            rw(key(&pool.vaults[0])),
            rw(ata(owner, mints[1])),
            rw(key(&pool.vaults[1])),
            rw(arrays[0]),
            rw(arrays[1]),
            rw(arrays[2]),
            ro(key(&pool.observation)),
        ],
        &args,
    ))
}
