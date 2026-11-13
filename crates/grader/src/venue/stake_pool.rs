//! SPL stake pools as a [`Venue`] (`specs/venues/jito-stake-pool.md`): the
//! pool's token as a `stake-pool` instrument (`specs/instruments/stake-pool.md`).
//!
//! One code path serves every pool on the SPL stake pool program and on
//! Sanctum's forks of it, which keep its instruction set; [`POOLS`] names
//! the ones a session can trade. The pool's exchange rate is its
//! `total_lamports` over its token supply. A seat stakes SOL for the token
//! (`DepositSol`) and redeems it at once from the pool's reserve
//! (`WithdrawSol`), which the program refuses beyond what the reserve holds:
//! the delayed exit, a stake account split off a validator
//! (`WithdrawStake`), is a raw send.
//!
//! The account a stake-pool market measures holds **native SOL**: the pool
//! takes and pays lamports, not wrapped SOL. Its holdings are the token in
//! the wallet, and SOL: the wallet's lamports, with what the market's
//! delayed exits owe it (Marinade's tickets, [`super::marinade`]) counted
//! as locked until they pay out. A stake account an SPL exit split off may
//! come from any pool's validator, so the session counts those once
//! ([`stake_accounts`]), not each pool.

use std::sync::Arc;

use async_trait::async_trait;
use mtx_core::{
    Asset, Decimal, Holding, Holdings, InstrumentId, Side as CoreSide, VenueId,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    Action, Planned, SendState, Sent,
    chain::{
        ChainClient, ExecutionPlan, FireAndForgetOptions, InstructionGroup, RpcChainClient,
        SendMode, pack, token,
    },
    protocols::spot::MarketFill,
};
use solana_keypair::Keypair;
use solana_pubkey::{Pubkey, pubkey};
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_signer::Signer;
use solana_transaction::{AccountMeta, Instruction};

use super::{Market, Seat, SeatResult, Venue, not_offered, price_usd};
use crate::{Error, Result, session::INTERNAL_RPC_TIMEOUT};

/// The SPL stake pool program, and Sanctum's forks of it.
pub const SPL_STAKE_POOL: Pubkey = pubkey!("SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy");
pub const SANCTUM_MULTI: Pubkey = pubkey!("SP12tWFxD9oJsVWNavTTBZvMbA6gkAmxtVgxdqvyvhY");
pub const SANCTUM_SINGLE: Pubkey = pubkey!("SPMBzsVUuoHA4Jm6KunbsotaahvVikZs1JyTW6iJvbn");
/// The native stake program, and the sysvars its instructions read.
pub const STAKE_PROGRAM: Pubkey = pubkey!("Stake11111111111111111111111111111111111111");
pub const CLOCK: Pubkey = pubkey!("SysvarC1ock11111111111111111111111111111111");
pub const STAKE_HISTORY: Pubkey = pubkey!("SysvarStakeHistory1111111111111111111111111");
pub const RENT: Pubkey = pubkey!("SysvarRent111111111111111111111111111111111");
pub const EPOCH_SCHEDULE: Pubkey = pubkey!("SysvarEpochSchedu1e111111111111111111111111");
pub const SYSTEM_PROGRAM: Pubkey = pubkey!("11111111111111111111111111111111");
/// A stake account's size.
pub const STAKE_ACCOUNT_LEN: u64 = 200;

/// A stake pool a session can trade: its venue, its token's symbol, and
/// the pool account.
#[derive(Debug, Clone, Copy)]
pub struct NamedPool {
    pub venue: &'static str,
    pub symbol: &'static str,
    pub program: Pubkey,
    pub address: Pubkey,
}

/// The SPL stake pools the venue profiles name.
pub const POOLS: &[NamedPool] = &[NamedPool {
    venue: "jito-stake-pool",
    symbol: "JITOSOL",
    program: SPL_STAKE_POOL,
    address: pubkey!("Jito4APyf642JPZPx3hGc6WWJ8zPKtRbRs4P815Awbb"),
}];

/// SOL in base units: what a stake pool takes and pays.
pub fn sol_asset() -> Asset {
    Asset::new(token::native_mint().to_string(), "SOL", 9).expect("9 decimals")
}

/// A fee as the program stores it: a fraction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fee {
    pub numerator: u64,
    pub denominator: u64,
}

impl Fee {
    pub fn rate(&self) -> f64 {
        if self.denominator == 0 {
            0.0
        } else {
            self.numerator as f64 / self.denominator as f64
        }
    }
}

/// The fields of a pool account a session reads, and where the epoch's
/// write goes.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolState {
    pub staker: Pubkey,
    pub stake_deposit_authority: Pubkey,
    pub validator_list: Pubkey,
    pub reserve_stake: Pubkey,
    pub pool_mint: Pubkey,
    pub manager_fee_account: Pubkey,
    pub token_program: Pubkey,
    pub total_lamports: u64,
    pub pool_token_supply: u64,
    pub last_update_epoch: u64,
    pub epoch_fee: Fee,
    pub preferred_withdraw_validator: Option<Pubkey>,
    pub stake_withdrawal_fee: Fee,
    pub sol_deposit_authority: Option<Pubkey>,
    pub sol_deposit_fee: Fee,
    pub sol_withdraw_authority: Option<Pubkey>,
    pub sol_withdrawal_fee: Fee,
}

/// A borsh reader over an account's bytes.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let bytes = self
            .data
            .get(self.at..self.at + n)
            .ok_or_else(|| decode_error(format!("account ends before byte {}", self.at + n)))?;
        self.at += n;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }

    fn key(&mut self) -> Result<Pubkey> {
        Ok(Pubkey::new_from_array(
            self.take(32)?.try_into().expect("32"),
        ))
    }

    fn fee(&mut self) -> Result<Fee> {
        let denominator = self.u64()?;
        let numerator = self.u64()?;
        Ok(Fee {
            numerator,
            denominator,
        })
    }

    fn option_key(&mut self) -> Result<Option<Pubkey>> {
        Ok(match self.u8()? {
            0 => None,
            _ => Some(self.key()?),
        })
    }

    /// A fee change scheduled for a later epoch: none, or one fee.
    fn future_fee(&mut self) -> Result<()> {
        if self.u8()? != 0 {
            self.fee()?;
        }
        Ok(())
    }
}

fn decode_error(reason: String) -> Error {
    Error::Transaction {
        label: "a stake pool account".into(),
        reason,
    }
}

impl PoolState {
    /// Decode a pool account (`AccountType::StakePool`, 1).
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut r = Reader { data, at: 0 };
        if r.u8()? != 1 {
            return Err(decode_error("not a stake pool account".into()));
        }
        let _manager = r.key()?;
        let staker = r.key()?;
        let stake_deposit_authority = r.key()?;
        let _withdraw_bump = r.u8()?;
        let validator_list = r.key()?;
        let reserve_stake = r.key()?;
        let pool_mint = r.key()?;
        let manager_fee_account = r.key()?;
        let token_program = r.key()?;
        let total_lamports = r.u64()?;
        let pool_token_supply = r.u64()?;
        let last_update_epoch = r.u64()?;
        r.take(48)?; // lockup
        let epoch_fee = r.fee()?;
        r.future_fee()?;
        let _preferred_deposit = r.option_key()?;
        let preferred_withdraw_validator = r.option_key()?;
        let _stake_deposit_fee = r.fee()?;
        let stake_withdrawal_fee = r.fee()?;
        r.future_fee()?;
        let _stake_referral_fee = r.u8()?;
        let sol_deposit_authority = r.option_key()?;
        let sol_deposit_fee = r.fee()?;
        let _sol_referral_fee = r.u8()?;
        let sol_withdraw_authority = r.option_key()?;
        let sol_withdrawal_fee = r.fee()?;
        Ok(Self {
            staker,
            stake_deposit_authority,
            validator_list,
            reserve_stake,
            pool_mint,
            manager_fee_account,
            token_program,
            total_lamports,
            pool_token_supply,
            last_update_epoch,
            epoch_fee,
            preferred_withdraw_validator,
            stake_withdrawal_fee,
            sol_deposit_authority,
            sol_deposit_fee,
            sol_withdraw_authority,
            sol_withdrawal_fee,
        })
    }

    /// SOL per token: what a redemption returns before fees.
    pub fn rate(&self) -> Decimal {
        if self.pool_token_supply == 0 {
            return Decimal::ONE;
        }
        Decimal::from(self.total_lamports) / Decimal::from(self.pool_token_supply)
    }
}

/// One validator in a pool's validator list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatorStake {
    pub active_stake_lamports: u64,
    pub transient_stake_lamports: u64,
    pub last_update_epoch: u64,
    pub transient_seed_suffix: u64,
    pub validator_seed_suffix: u32,
    /// 0 active, 1 deactivating transient, 2 ready for removal, ...
    pub status: u8,
    pub vote_account: Pubkey,
}

/// A validator list entry's size, and where the entries start (after the
/// account type, the maximum and the vector's length).
pub const VALIDATOR_STAKE_LEN: usize = 73;
pub const VALIDATOR_LIST_HEADER: usize = 9;

/// Decode a validator list account (`AccountType::ValidatorList`, 2).
pub fn decode_validator_list(data: &[u8]) -> Result<Vec<ValidatorStake>> {
    let mut r = Reader { data, at: 0 };
    if r.u8()? != 2 {
        return Err(decode_error("not a validator list".into()));
    }
    let _max = r.u32()?;
    let len = r.u32()? as usize;
    (0..len)
        .map(|_| {
            let active_stake_lamports = r.u64()?;
            let transient_stake_lamports = r.u64()?;
            let last_update_epoch = r.u64()?;
            let transient_seed_suffix = r.u64()?;
            let _unused = r.u32()?;
            let validator_seed_suffix = r.u32()?;
            let status = r.u8()?;
            let vote_account = r.key()?;
            Ok(ValidatorStake {
                active_stake_lamports,
                transient_stake_lamports,
                last_update_epoch,
                transient_seed_suffix,
                validator_seed_suffix,
                status,
                vote_account,
            })
        })
        .collect()
}

/// The pool's withdraw authority: it signs for the reserve and the
/// validator stake accounts.
pub fn withdraw_authority(program: &Pubkey, pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[pool.as_ref(), b"withdraw"], program).0
}

/// A validator's stake account in the pool.
pub fn validator_stake_address(
    program: &Pubkey,
    pool: &Pubkey,
    vote: &Pubkey,
    seed: u32,
) -> Pubkey {
    let seed = seed.to_le_bytes();
    let mut seeds: Vec<&[u8]> = vec![vote.as_ref(), pool.as_ref()];
    if u32::from_le_bytes(seed) != 0 {
        seeds.push(&seed);
    }
    Pubkey::find_program_address(&seeds, program).0
}

fn tagged(tag: u8, amount: Option<u64>) -> Vec<u8> {
    let mut data = vec![tag];
    if let Some(amount) = amount {
        data.extend_from_slice(&amount.to_le_bytes());
    }
    data
}

/// The validator nothing in a session can touch: the active one with the
/// most stake whose stake account the world doesn't hold, so no
/// transaction can split from it or update it. An epoch's reward goes to
/// it ([`crate::epochs`]), and so does what a session's setup stakes.
pub fn untouchable_validator(
    rpc: &RpcClient,
    named: &NamedPool,
    list: &[ValidatorStake],
) -> Result<usize> {
    let mut order: Vec<usize> = (0..list.len()).filter(|&i| list[i].status == 0).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(list[i].active_stake_lamports), i));
    for i in order {
        let address = validator_stake_address(
            &named.program,
            &named.address,
            &list[i].vote_account,
            list[i].validator_seed_suffix,
        );
        if rpc.get_account(&address).is_err() {
            return Ok(i);
        }
    }
    Err(decode_error(format!(
        "{}: every validator's stake account is in the world",
        named.symbol
    )))
}

/// Stake `lamports` of `owner`'s SOL in `named` for the token, then move
/// them from the pool's reserve to [`untouchable_validator`], as the
/// pool's staker delegates what its reserve takes in each epoch. Setup
/// only, by cheatcode: a session's wallet that starts with the token holds
/// it as a holder who bought it before, and the reserve's instant exits
/// stay what they were.
pub fn stake_before_session(
    surfnet: &surfpool_sdk::Surfnet,
    rpc: &RpcClient,
    owner: &Keypair,
    named: &NamedPool,
    lamports: u64,
) -> Result<()> {
    let state = PoolState::decode(&rpc.get_account(&named.address)?.data)?;
    let to = token::ata_for(&owner.pubkey(), &state.pool_mint, &state.token_program);
    mtx_kit::send(
        rpc,
        owner,
        vec![
            token::create_ata_idempotent_for(
                &owner.pubkey(),
                &owner.pubkey(),
                &state.pool_mint,
                &state.token_program,
            ),
            ix::deposit_sol(
                &named.program,
                &named.address,
                &state,
                &owner.pubkey(),
                &to,
                lamports,
            ),
        ],
        "a holding staked before the session",
    )?;
    let cheats = surfnet.cheatcodes();
    let reserve = rpc.get_account(&state.reserve_stake)?;
    cheats.set_account(
        &state.reserve_stake,
        reserve.lamports - lamports,
        &reserve.data,
        &reserve.owner,
    )?;
    let mut list = rpc.get_account(&state.validator_list)?;
    let target = untouchable_validator(rpc, named, &decode_validator_list(&list.data)?)?;
    let at = VALIDATOR_LIST_HEADER + target * VALIDATOR_STAKE_LEN;
    let active = u64::from_le_bytes(list.data[at..at + 8].try_into().expect("8 bytes"));
    list.data[at..at + 8].copy_from_slice(&(active + lamports).to_le_bytes());
    cheats.set_account(
        &state.validator_list,
        list.lamports,
        &list.data,
        &list.owner,
    )?;
    Ok(())
}

/// The program's instructions a session sends.
pub mod ix {
    use super::*;

    /// `DepositSol`: stake `lamports` of `from`'s SOL for the token, paid to
    /// `to` (a token account of the pool's mint).
    pub fn deposit_sol(
        program: &Pubkey,
        pool: &Pubkey,
        state: &PoolState,
        from: &Pubkey,
        to: &Pubkey,
        lamports: u64,
    ) -> Instruction {
        let mut accounts = vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new_readonly(withdraw_authority(program, pool), false),
            AccountMeta::new(state.reserve_stake, false),
            AccountMeta::new(*from, true),
            AccountMeta::new(*to, false),
            AccountMeta::new(state.manager_fee_account, false),
            AccountMeta::new(*to, false),
            AccountMeta::new(state.pool_mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(state.token_program, false),
        ];
        if let Some(authority) = state.sol_deposit_authority {
            accounts.push(AccountMeta::new_readonly(authority, true));
        }
        Instruction {
            program_id: *program,
            accounts,
            data: tagged(14, Some(lamports)),
        }
    }

    /// `WithdrawSol`: burn `tokens` of `authority`'s from `from` for SOL
    /// out of the reserve, paid to `to`.
    pub fn withdraw_sol(
        program: &Pubkey,
        pool: &Pubkey,
        state: &PoolState,
        authority: &Pubkey,
        from: &Pubkey,
        to: &Pubkey,
        tokens: u64,
    ) -> Instruction {
        let mut accounts = vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new_readonly(withdraw_authority(program, pool), false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*from, false),
            AccountMeta::new(state.reserve_stake, false),
            AccountMeta::new(*to, false),
            AccountMeta::new(state.manager_fee_account, false),
            AccountMeta::new(state.pool_mint, false),
            AccountMeta::new_readonly(CLOCK, false),
            AccountMeta::new_readonly(STAKE_HISTORY, false),
            AccountMeta::new_readonly(STAKE_PROGRAM, false),
            AccountMeta::new_readonly(state.token_program, false),
        ];
        if let Some(authority) = state.sol_withdraw_authority {
            accounts.push(AccountMeta::new_readonly(authority, true));
        }
        Instruction {
            program_id: *program,
            accounts,
            data: tagged(16, Some(tokens)),
        }
    }

    /// `WithdrawStake`: burn `tokens` for a stake account split off
    /// `validator_stake` into `receive` (an allocated, uninitialized stake
    /// account), owned by `new_authority`.
    #[allow(clippy::too_many_arguments)]
    pub fn withdraw_stake(
        program: &Pubkey,
        pool: &Pubkey,
        state: &PoolState,
        validator_stake: &Pubkey,
        receive: &Pubkey,
        new_authority: &Pubkey,
        authority: &Pubkey,
        from: &Pubkey,
        tokens: u64,
    ) -> Instruction {
        Instruction {
            program_id: *program,
            accounts: vec![
                AccountMeta::new(*pool, false),
                AccountMeta::new(state.validator_list, false),
                AccountMeta::new_readonly(withdraw_authority(program, pool), false),
                AccountMeta::new(*validator_stake, false),
                AccountMeta::new(*receive, false),
                AccountMeta::new_readonly(*new_authority, false),
                AccountMeta::new_readonly(*authority, true),
                AccountMeta::new(*from, false),
                AccountMeta::new(state.manager_fee_account, false),
                AccountMeta::new(state.pool_mint, false),
                AccountMeta::new_readonly(CLOCK, false),
                AccountMeta::new_readonly(state.token_program, false),
                AccountMeta::new_readonly(STAKE_PROGRAM, false),
            ],
            data: tagged(10, Some(tokens)),
        }
    }

    /// `UpdateStakePoolBalance`: the permissionless crank that sums the
    /// reserve and the validator list into the pool's `total_lamports`,
    /// takes the epoch fee from the rewards, and marks the pool current.
    pub fn update_stake_pool_balance(
        program: &Pubkey,
        pool: &Pubkey,
        state: &PoolState,
    ) -> Instruction {
        Instruction {
            program_id: *program,
            accounts: vec![
                AccountMeta::new(*pool, false),
                AccountMeta::new_readonly(withdraw_authority(program, pool), false),
                AccountMeta::new(state.validator_list, false),
                AccountMeta::new_readonly(state.reserve_stake, false),
                AccountMeta::new(state.manager_fee_account, false),
                AccountMeta::new(state.pool_mint, false),
                AccountMeta::new_readonly(state.token_program, false),
            ],
            data: tagged(7, None),
        }
    }
}

/// The native stake program's instructions a delayed exit needs.
pub mod stake_ix {
    use super::*;

    /// Allocate `account` as an uninitialized stake account, for
    /// `WithdrawStake` to split into.
    pub fn allocate(payer: &Pubkey, account: &Pubkey, rent: u64) -> Instruction {
        let mut data = 0u32.to_le_bytes().to_vec();
        data.extend_from_slice(&rent.to_le_bytes());
        data.extend_from_slice(&STAKE_ACCOUNT_LEN.to_le_bytes());
        data.extend_from_slice(STAKE_PROGRAM.as_ref());
        Instruction {
            program_id: SYSTEM_PROGRAM,
            accounts: vec![
                AccountMeta::new(*payer, true),
                AccountMeta::new(*account, true),
            ],
            data,
        }
    }

    /// `Deactivate`: the stake cools down from the next epoch.
    pub fn deactivate(stake: &Pubkey, authority: &Pubkey) -> Instruction {
        Instruction {
            program_id: STAKE_PROGRAM,
            accounts: vec![
                AccountMeta::new(*stake, false),
                AccountMeta::new_readonly(CLOCK, false),
                AccountMeta::new_readonly(*authority, true),
            ],
            data: 5u32.to_le_bytes().to_vec(),
        }
    }

    /// `Withdraw` `lamports` of an inactive stake account to `to`.
    pub fn withdraw(stake: &Pubkey, authority: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
        let mut data = 4u32.to_le_bytes().to_vec();
        data.extend_from_slice(&lamports.to_le_bytes());
        Instruction {
            program_id: STAKE_PROGRAM,
            accounts: vec![
                AccountMeta::new(*stake, false),
                AccountMeta::new(*to, false),
                AccountMeta::new_readonly(CLOCK, false),
                AccountMeta::new_readonly(STAKE_HISTORY, false),
                AccountMeta::new_readonly(*authority, true),
            ],
            data,
        }
    }
}

/// An error a chain read came back with, as the grader reports it.
pub(super) fn chain_error<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> Error + '_ {
    move |error| Error::Transaction {
        label: what.into(),
        reason: error.to_string(),
    }
}

/// An account's bytes, which must exist.
pub(super) async fn account_data(
    chain: &dyn ChainClient,
    address: &Pubkey,
    what: &str,
) -> Result<Vec<u8>> {
    chain
        .get_account(address)
        .await
        .map_err(chain_error(what))?
        .map(|a| a.data)
        .ok_or_else(|| Error::UnknownMarket(format!("{what} {address} is not on the chain")))
}

/// What `owner` holds in SOL: the wallet's lamports, and as locked
/// `waiting`, what a market's delayed exits owe it.
pub(super) async fn sol_holding(
    chain: &dyn ChainClient,
    owner: &Pubkey,
    waiting: u64,
) -> Result<Holding> {
    let wallet = chain
        .get_balance(owner)
        .await
        .map_err(chain_error("the wallet's SOL"))?;
    Ok(Holding::Spot(Holdings {
        wallet: lamports(wallet),
        locked: lamports(waiting),
        ..Holdings::empty(sol_asset())
    }))
}

/// The lamports of every stake account `owner` may withdraw: an SPL
/// pool's delayed exits, which may come from any pool's validators, so
/// the account that holds them counts them once, not each pool.
pub async fn stake_accounts(chain: &dyn ChainClient, owner: &Pubkey) -> Result<u64> {
    // Meta: the state (4), rent_exempt_reserve (8), the staker (32), then
    // the withdrawer.
    let stakes = chain
        .get_program_accounts(
            &STAKE_PROGRAM,
            vec![
                RpcFilterType::DataSize(STAKE_ACCOUNT_LEN),
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(44, owner.as_ref())),
            ],
        )
        .await
        .map_err(chain_error("the wallet's stake accounts"))?;
    Ok(stakes.iter().map(|(_, a)| a.lamports).sum())
}

/// Lamports, or token atoms of a 9-decimal token, in whole units.
pub(super) fn lamports(atoms: u64) -> Decimal {
    Decimal::from(atoms) / Decimal::from(1_000_000_000u64)
}

/// A whole-unit amount in atoms, rounded down; `None` if it isn't a
/// positive number that fits.
pub(super) fn atoms(amount: f64) -> Option<u64> {
    let atoms = (amount * 1e9).floor();
    (amount.is_finite() && atoms >= 1.0 && atoms < u64::MAX as f64).then_some(atoms as u64)
}

/// Lamports a seat leaves in the wallet when it stakes "all": what its
/// transactions cost.
pub const KEPT_LAMPORTS: u64 = 50_000_000;

/// `amount` of SOL to stake, in lamports, from a wallet of `balance`: all
/// but [`KEPT_LAMPORTS`] for none.
pub(super) fn stake_amount(amount: Option<f64>, balance: u64) -> std::result::Result<u64, String> {
    match amount {
        Some(amount) => atoms(amount).ok_or_else(|| format!("{amount} SOL is not an amount")),
        None => match balance.saturating_sub(KEPT_LAMPORTS) {
            0 => Err("no SOL to stake beyond what fees need".into()),
            all => Ok(all),
        },
    }
}

/// The bot's own instant deposits and withdrawals at a pool, read from the
/// transactions that moved `vault` (the pool's SOL) after `after_slot`:
/// the token `owner` gained or gave, against the SOL `vault` took or paid.
pub(super) async fn pool_fills(
    chain: &dyn ChainClient,
    instrument: &InstrumentRef,
    vault: &Pubkey,
    mint: &Pubkey,
    after_slot: u64,
    owner: &Pubkey,
) -> Result<Vec<(MarketFill, Vec<Pubkey>)>> {
    let mut fills = Vec::new();
    let signatures = chain
        .signatures_for_address(vault, after_slot)
        .await
        .map_err(chain_error("the pool's transactions"))?;
    for (signature, slot) in signatures {
        if slot <= after_slot {
            continue;
        }
        let Some(tx) = chain
            .get_transaction(&signature)
            .await
            .map_err(chain_error("a pool transaction"))?
        else {
            continue;
        };
        if tx.error.is_some() {
            continue;
        }
        let tokens = tx.token_change(owner, mint);
        let sol = tx.lamport_change(vault).unwrap_or(0);
        if tokens == 0 || sol == 0 || (tokens > 0) != (sol > 0) {
            continue;
        }
        let quantity =
            Decimal::from(tokens.unsigned_abs() as u64) / Decimal::from(1_000_000_000u64);
        let paid = Decimal::from(sol.unsigned_abs() as u64) / Decimal::from(1_000_000_000u64);
        let bytes: [u8; 16] = signature.as_ref()[..16].try_into().expect("16 bytes");
        fills.push((
            MarketFill {
                slot,
                instrument: InstrumentId::new(instrument.to_string()),
                sequence: u128::from_le_bytes(bytes),
                taker: *owner,
                maker: *vault,
                taker_side: if tokens > 0 {
                    CoreSide::Buy
                } else {
                    CoreSide::Sell
                },
                price: paid / quantity,
                quantity,
                unix_timestamp: tx.block_time.map_or(0, |t| t.timestamp()),
            },
            vec![*owner],
        ));
    }
    Ok(fills)
}

/// The pool's token among `holdings`, if they are just that: what a
/// stake-pool market values itself.
pub(super) fn token_held<'a>(holdings: &'a [Holding], mint: &Pubkey) -> Option<&'a Holdings> {
    match holdings {
        [Holding::Spot(asset)] if asset.base.id.0 == mint.to_string() => Some(asset),
        _ => None,
    }
}

/// `token` at `rate` SOL each, at the SOL reference price.
pub(super) async fn value_token(
    chain: &dyn ChainClient,
    token: &Holdings,
    rate: Decimal,
) -> Result<Decimal> {
    Ok(token.total() * rate * price_usd(chain, &token::native_mint()).await?)
}

/// The SPL stake pools of one venue.
pub struct SplStakePools {
    id: VenueId,
    rpc_url: String,
}

impl SplStakePools {
    /// The pools `venue` names, read through `rpc_url`.
    pub fn new(venue: &str, rpc_url: &str) -> Self {
        Self {
            id: VenueId::new(venue),
            rpc_url: rpc_url.into(),
        }
    }

    fn named(&self) -> impl Iterator<Item = &'static NamedPool> + '_ {
        POOLS.iter().filter(|p| p.venue == self.id.0)
    }
}

#[async_trait]
impl Venue for SplStakePools {
    fn id(&self) -> &VenueId {
        &self.id
    }

    fn instruments(&self) -> Vec<InstrumentRef> {
        self.named()
            .map(|p| InstrumentRef {
                venue: self.id.0.clone(),
                kind: Kind::StakePool,
                symbol: p.symbol.into(),
            })
            .collect()
    }

    async fn market(&self, instrument: &InstrumentRef) -> Result<Box<dyn Market>> {
        let Some(named) = self
            .named()
            .find(|p| instrument.kind == Kind::StakePool && p.symbol == instrument.symbol)
        else {
            return Err(not_offered(&self.id, instrument));
        };
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::with_timeout(
            &self.rpc_url,
            INTERNAL_RPC_TIMEOUT,
        ));
        Ok(Box::new(
            StakePoolMarket::connect(instrument.clone(), *named, chain).await?,
        ))
    }
}

/// One SPL stake pool, as a session trades it.
pub struct StakePoolMarket {
    instrument: InstrumentRef,
    named: NamedPool,
    chain: Arc<dyn ChainClient>,
    /// The pool's accounts, which don't change: its mint, reserve, list.
    state: PoolState,
}

impl StakePoolMarket {
    pub async fn connect(
        instrument: InstrumentRef,
        named: NamedPool,
        chain: Arc<dyn ChainClient>,
    ) -> Result<Self> {
        let state =
            PoolState::decode(&account_data(chain.as_ref(), &named.address, "stake pool").await?)?;
        Ok(Self {
            instrument,
            named,
            chain,
            state,
        })
    }

    pub fn named(&self) -> &NamedPool {
        &self.named
    }

    pub fn mint(&self) -> Pubkey {
        self.state.pool_mint
    }

    /// The pool as it is now.
    pub async fn state(&self) -> Result<PoolState> {
        read_state(self.chain.as_ref(), &self.named.address).await
    }
}

async fn read_state(chain: &dyn ChainClient, pool: &Pubkey) -> Result<PoolState> {
    PoolState::decode(&account_data(chain, pool, "stake pool").await?)
}

/// SOL the reserve can pay out at once: its lamports above its rent and
/// the stake program's minimum.
async fn instant_capacity(chain: &dyn ChainClient, state: &PoolState) -> Result<u64> {
    let reserve = chain
        .get_account(&state.reserve_stake)
        .await
        .map_err(chain_error("the pool's reserve"))?
        .ok_or_else(|| Error::UnknownMarket("the pool's reserve is not on the chain".into()))?;
    // Meta's rent_exempt_reserve, after the state's tag.
    let rent = reserve
        .data
        .get(4..12)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().expect("8 bytes")));
    Ok(reserve.lamports.saturating_sub(rent))
}

/// The smallest instant redemption the market quotes a bid for, in SOL.
pub const QUOTE_SOL: u64 = 1_000_000_000;

/// The pool's prices for a small trade each way, in SOL per token: staking
/// pays the rate grossed up by the deposit fee; redeeming at once gets it
/// less the withdrawal fee, if the reserve can pay [`QUOTE_SOL`].
async fn quote(
    chain: &dyn ChainClient,
    pool: &Pubkey,
) -> Result<(Option<f64>, Option<f64>, PoolState)> {
    let state = read_state(chain, pool).await?;
    let rate = crate::session::grader_f64(state.rate());
    let ask = rate / (1.0 - state.sol_deposit_fee.rate());
    let bid = (instant_capacity(chain, &state).await? >= QUOTE_SOL)
        .then(|| rate * (1.0 - state.sol_withdrawal_fee.rate()));
    Ok((bid, Some(ask), state))
}

#[async_trait]
impl Market for StakePoolMarket {
    fn instrument(&self) -> &InstrumentRef {
        &self.instrument
    }

    fn address(&self) -> Pubkey {
        self.named.address
    }

    async fn top(&self) -> Result<(Option<f64>, Option<f64>)> {
        let (bid, ask, _) = quote(self.chain.as_ref(), &self.named.address).await?;
        Ok((bid, ask))
    }

    async fn holdings(&self, owner: &Pubkey) -> Result<Vec<Holding>> {
        let held = token::owned_amount(self.chain.as_ref(), owner, &self.state.pool_mint)
            .await
            .map_err(chain_error("the wallet's pool token"))?;
        let token = Asset::new(self.state.pool_mint.to_string(), &self.instrument.symbol, 9)
            .map_err(|e| decode_error(e.to_string()))?;
        Ok(vec![
            Holding::Spot(Holdings {
                wallet: lamports(held),
                ..Holdings::empty(token)
            }),
            sol_holding(self.chain.as_ref(), owner, 0).await?,
        ])
    }

    async fn value_usd(
        &self,
        chain: &dyn ChainClient,
        holdings: &[Holding],
    ) -> Result<Option<Decimal>> {
        let Some(token) = token_held(holdings, &self.state.pool_mint) else {
            return Ok(None);
        };
        let rate = read_state(chain, &self.named.address).await?.rate();
        Ok(Some(value_token(chain, token, rate).await?))
    }

    async fn fills(
        &self,
        after_slot: u64,
        owner: &Pubkey,
    ) -> Result<Vec<(MarketFill, Vec<Pubkey>)>> {
        pool_fills(
            self.chain.as_ref(),
            &self.instrument,
            &self.state.reserve_stake,
            &self.state.pool_mint,
            after_slot,
            owner,
        )
        .await
    }

    async fn seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>> {
        Ok(Box::new(StakePoolSeat {
            market: self.instrument.to_string(),
            named: self.named,
            chain: Arc::new(RpcChainClient::new(rpc_url)),
            signer,
        }))
    }
}

/// An account staking into, and redeeming from, an SPL stake pool.
pub struct StakePoolSeat {
    market: String,
    named: NamedPool,
    chain: Arc<dyn ChainClient>,
    signer: Keypair,
}

/// Send `instructions` as one transaction signed by `signer`, without
/// waiting: it lands in the world's next block. What became of it, as far
/// as the chain says now.
pub(super) async fn send_now(
    chain: &dyn ChainClient,
    signer: &Keypair,
    label: &str,
    instructions: Vec<Instruction>,
) -> Sent {
    let plan: std::result::Result<ExecutionPlan, _> = pack(
        &signer.pubkey(),
        &[InstructionGroup::new(label, instructions)],
        mtx_kit::chain::DEFAULT_COMPUTE_UNIT_LIMIT,
    );
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => {
            return Sent {
                state: SendState::failed(error.to_string()),
                sent: false,
            };
        }
    };
    let mode = SendMode::FireAndForget(FireAndForgetOptions { simulate: false });
    let state = match mode.send(&plan, signer, chain).await {
        Ok(signatures) => {
            let signature = *signatures.last().expect("one transaction");
            match chain.get_transaction(&signature).await {
                Ok(Some(summary)) => match summary.error {
                    None => SendState::Landed(signature.to_string()),
                    Some(error) => SendState::Failed {
                        reason: error.to_string(),
                        signature: Some(signature.to_string()),
                    },
                },
                Ok(None) | Err(_) => SendState::Delayed,
            }
        }
        Err(error) => SendState::failed(error.to_string()),
    };
    Sent { state, sent: true }
}

pub(super) fn not_sent(reason: impl Into<String>) -> Sent {
    Sent {
        state: SendState::NotSent(reason.into()),
        sent: false,
    }
}

/// A seat's stake-pool statistics, as `venue-view.stats` names them.
pub(super) async fn epoch_stats(chain: &dyn ChainClient) -> Result<Vec<(String, f64)>> {
    let word = |data: &[u8], at: usize| {
        data.get(at..at + 8)
            .map_or(0, |b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
    };
    let clock = account_data(chain, &CLOCK, "the clock").await?;
    let schedule = account_data(chain, &EPOCH_SCHEDULE, "the epoch schedule").await?;
    // Clock: slot, epoch start, epoch. EpochSchedule: slots per epoch, the
    // leader schedule's offset, warmup, the first normal epoch and slot.
    let (slot, epoch) = (word(&clock, 0), word(&clock, 16));
    let (per_epoch, first_normal_slot) = (word(&schedule, 0), word(&schedule, 25));
    let into = slot.saturating_sub(first_normal_slot) % per_epoch.max(1);
    Ok(vec![
        ("epoch".into(), epoch as f64),
        ("slots_to_epoch_end".into(), (per_epoch - into) as f64),
    ])
}

impl StakePoolSeat {
    async fn stake(&self, amount: Option<f64>) -> Sent {
        let owner = self.signer.pubkey();
        let chain = self.chain.as_ref();
        let (state, balance) = match (
            read_state(chain, &self.named.address).await,
            chain.get_balance(&owner).await,
        ) {
            (Ok(state), Ok(balance)) => (state, balance),
            (Err(e), _) => return not_sent(e.to_string()),
            (_, Err(e)) => return not_sent(e.to_string()),
        };
        let lamports = match stake_amount(amount, balance) {
            Ok(lamports) => lamports,
            Err(reason) => return not_sent(reason),
        };
        let to = token::ata_for(&owner, &state.pool_mint, &state.token_program);
        let instructions = vec![
            token::create_ata_idempotent_for(
                &owner,
                &owner,
                &state.pool_mint,
                &state.token_program,
            ),
            ix::deposit_sol(
                &self.named.program,
                &self.named.address,
                &state,
                &owner,
                &to,
                lamports,
            ),
        ];
        send_now(chain, &self.signer, "stake pool deposit", instructions).await
    }

    async fn redeem(&self, amount: Option<f64>) -> Sent {
        let owner = self.signer.pubkey();
        let chain = self.chain.as_ref();
        let state = match read_state(chain, &self.named.address).await {
            Ok(state) => state,
            Err(e) => return not_sent(e.to_string()),
        };
        let from = token::ata_for(&owner, &state.pool_mint, &state.token_program);
        let tokens = match amount {
            Some(amount) => match atoms(amount) {
                Some(tokens) => tokens,
                None => return not_sent(format!("{amount} is not an amount of the token")),
            },
            None => match chain.get_account(&from).await {
                Ok(Some(account)) => token::token_balance(&account).unwrap_or(0),
                _ => 0,
            },
        };
        if tokens == 0 {
            return not_sent("no pool token to redeem");
        }
        let instructions = vec![ix::withdraw_sol(
            &self.named.program,
            &self.named.address,
            &state,
            &owner,
            &from,
            &owner,
            tokens,
        )];
        send_now(chain, &self.signer, "stake pool withdrawal", instructions).await
    }
}

#[async_trait]
impl Seat for StakePoolSeat {
    fn market(&self) -> &str {
        &self.market
    }

    fn executes(&self, action: &Action) -> bool {
        matches!(action, Action::Deposit { .. } | Action::Withdraw { .. })
    }

    async fn send(&self, planned: &Planned) -> Sent {
        match planned.action {
            Action::Deposit { amount } => self.stake(amount).await,
            Action::Withdraw { amount } => self.redeem(amount).await,
            _ => not_sent("unsupported in this session"),
        }
    }

    async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
        let (bid, ask, _) = quote(self.chain.as_ref(), &self.named.address)
            .await
            .map_err(|e| e.to_string())?;
        Ok((bid, ask))
    }

    async fn position(&self) -> SeatResult<f64> {
        let state = read_state(self.chain.as_ref(), &self.named.address)
            .await
            .map_err(|e| e.to_string())?;
        let held =
            token::owned_amount(self.chain.as_ref(), &self.signer.pubkey(), &state.pool_mint)
                .await
                .map_err(|e| e.to_string())?;
        Ok(held as f64 / 1e9)
    }

    async fn stats(&self) -> SeatResult<Vec<(String, f64)>> {
        let chain = self.chain.as_ref();
        let (_, _, state) = quote(chain, &self.named.address)
            .await
            .map_err(|e| e.to_string())?;
        let capacity = instant_capacity(chain, &state)
            .await
            .map_err(|e| e.to_string())?;
        let bps = |fee: Fee| fee.rate() * 1e4;
        let mut stats = vec![
            ("rate".into(), crate::session::grader_f64(state.rate())),
            ("deposit_fee_bps".into(), bps(state.sol_deposit_fee)),
            ("withdraw_fee_bps".into(), bps(state.sol_withdrawal_fee)),
            ("instant_capacity_sol".into(), capacity as f64 / 1e9),
            ("last_update_epoch".into(), state.last_update_epoch as f64),
            ("epoch_fee_bps".into(), bps(state.epoch_fee)),
        ];
        stats.extend(epoch_stats(chain).await.map_err(|e| e.to_string())?);
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Jito pool's first 435 bytes on 2026-11-12 (epoch 1085), with
    /// the fields the tests read.
    fn jito_pool() -> Vec<u8> {
        let mut data = vec![1u8];
        data.extend([7u8; 32]); // manager
        data.extend([8u8; 32]); // staker
        data.extend([9u8; 32]); // deposit authority
        data.push(253);
        for key in [3u8, 4, 5, 6] {
            data.extend([key; 32]);
        }
        data.extend(spl_token());
        data.extend(10_213_089_853_649_410u64.to_le_bytes());
        data.extend(7_788_489_417_485_372u64.to_le_bytes());
        data.extend(1085u64.to_le_bytes());
        data.extend([0u8; 48]);
        data.extend(100u64.to_le_bytes()); // epoch fee 4/100
        data.extend(4u64.to_le_bytes());
        data.push(0); // no next epoch fee
        data.push(0); // no preferred deposit validator
        data.push(0); // no preferred withdraw validator
        data.extend([0u8; 16]); // stake deposit fee
        data.extend(1000u64.to_le_bytes()); // stake withdrawal fee 1/1000
        data.extend(1u64.to_le_bytes());
        data.push(0);
        data.push(0); // stake referral fee
        data.push(0); // no SOL deposit authority
        data.extend([0u8; 16]); // SOL deposit fee
        data.push(0); // SOL referral fee
        data.push(0); // no SOL withdraw authority
        data.extend(1000u64.to_le_bytes()); // SOL withdrawal fee 1/1000
        data.extend(1u64.to_le_bytes());
        data.push(0);
        data.extend(7_788_130_368_423_133u64.to_le_bytes());
        data.extend(10_211_249_118_603_268u64.to_le_bytes());
        data
    }

    fn spl_token() -> [u8; 32] {
        token::token_program().to_bytes()
    }

    #[test]
    fn a_pool_account_decodes_to_its_rate_and_fees() {
        let state = PoolState::decode(&jito_pool()).unwrap();
        assert_eq!(state.total_lamports, 10_213_089_853_649_410);
        assert_eq!(state.last_update_epoch, 1085);
        assert_eq!(state.pool_mint, Pubkey::new_from_array([5; 32]));
        assert_eq!(state.token_program, token::token_program());
        assert_eq!(
            state.epoch_fee,
            Fee {
                numerator: 4,
                denominator: 100
            }
        );
        assert_eq!(state.sol_deposit_fee.rate(), 0.0);
        assert_eq!(state.sol_withdrawal_fee.rate(), 0.001);
        assert_eq!(state.sol_withdraw_authority, None);
        let rate = crate::session::grader_f64(state.rate());
        assert!((rate - 1.311_305_608).abs() < 1e-9, "{rate}");
        assert!(PoolState::decode(&jito_pool()[..300]).is_err());
        assert!(PoolState::decode(&[2u8; 611]).is_err());
    }

    #[test]
    fn a_validator_list_decodes_entry_by_entry() {
        let mut data = vec![2u8];
        data.extend(1000u32.to_le_bytes());
        data.extend(2u32.to_le_bytes());
        for (i, vote) in [[11u8; 32], [12u8; 32]].iter().enumerate() {
            data.extend((5_000 + i as u64).to_le_bytes());
            data.extend(0u64.to_le_bytes());
            data.extend(1085u64.to_le_bytes());
            data.extend(0u64.to_le_bytes());
            data.extend(0u32.to_le_bytes());
            data.extend((i as u32).to_le_bytes());
            data.push(0);
            data.extend(vote);
        }
        assert_eq!(data.len(), VALIDATOR_LIST_HEADER + 2 * VALIDATOR_STAKE_LEN);
        let list = decode_validator_list(&data).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].active_stake_lamports, 5_001);
        assert_eq!(list[1].validator_seed_suffix, 1);
        assert_eq!(list[1].vote_account, Pubkey::new_from_array([12; 32]));
    }

    #[test]
    fn instructions_carry_the_program_s_tags() {
        let state = PoolState::decode(&jito_pool()).unwrap();
        let (program, pool) = (POOLS[0].program, POOLS[0].address);
        let user = Pubkey::new_unique();
        let deposit = ix::deposit_sol(&program, &pool, &state, &user, &user, 5);
        assert_eq!(deposit.data, [&[14u8][..], &5u64.to_le_bytes()].concat());
        assert_eq!(deposit.accounts.len(), 10);
        assert!(deposit.accounts[3].is_signer);
        let withdraw = ix::withdraw_sol(&program, &pool, &state, &user, &user, &user, 7);
        assert_eq!(withdraw.data[0], 16);
        assert_eq!(withdraw.accounts[4].pubkey, state.reserve_stake);
        let update = ix::update_stake_pool_balance(&program, &pool, &state);
        assert_eq!(update.data, vec![7]);
        assert_eq!(
            withdraw_authority(&program, &pool),
            Pubkey::find_program_address(&[pool.as_ref(), b"withdraw"], &program).0
        );
    }

    #[test]
    fn staking_all_keeps_what_fees_need() {
        assert_eq!(stake_amount(Some(1.5), 0), Ok(1_500_000_000));
        assert_eq!(stake_amount(None, 2_000_000_000), Ok(1_950_000_000));
        assert!(stake_amount(None, KEPT_LAMPORTS).is_err());
        assert!(stake_amount(Some(-1.0), 10).is_err());
        assert!(stake_amount(Some(f64::NAN), 10).is_err());
    }
}
