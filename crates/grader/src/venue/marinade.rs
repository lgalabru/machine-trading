//! Marinade as a [`Venue`] (`specs/venues/marinade.md`): mSOL as a
//! `stake-pool` instrument (`specs/instruments/stake-pool.md`).
//!
//! Marinade is its own program, not an SPL stake pool. Staking mints mSOL
//! at the state's SOL per mSOL, part of it from the liquidity pool's mSOL
//! leg; the instant exit, `liquid_unstake`, sells mSOL into the pool's SOL
//! leg for a fee that grows as the leg drains; the delayed exit,
//! `order_unstake`, burns mSOL for a ticket that `claim` pays from the
//! reserve once an epoch has passed, and a further 30 minutes. A seat
//! stakes and unstakes at once; tickets are raw sends, and count in the
//! account's SOL, locked, until they are claimed.

use std::sync::Arc;

use async_trait::async_trait;
use mtx_core::{
    Asset, Decimal, Holding, Holdings, VenueId,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    Action, Planned, Sent, VenueView,
    chain::{ChainClient, RpcChainClient, token},
    protocols::spot::MarketFill,
};
use solana_keypair::Keypair;
use solana_pubkey::{Pubkey, pubkey};
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_signer::Signer;
use solana_transaction::{AccountMeta, Instruction};

use super::{
    Market, Seat, SeatResult, Venue, not_offered,
    stake_pool::{
        CLOCK, RENT, SYSTEM_PROGRAM, account_data, atoms, chain_error, epoch_stats, lamports,
        not_sent, pool_fills, send_now, set_size, sol_holding, stake_amount, token_held,
        value_token,
    },
};
use crate::{Error, Result, epochs::EpochPlan, session::INTERNAL_RPC_TIMEOUT};

pub const PROGRAM: Pubkey = pubkey!("MarBmsSgKXdrN1egZf5sqe1TMai9K1rChYNDJgjq7aD");
pub const STATE: Pubkey = pubkey!("8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC");
/// The venue and its one instrument.
pub const VENUE: &str = "marinade";
pub const SYMBOL: &str = "MSOL";
/// What `msol_price` is a fraction of.
pub const PRICE_DENOMINATOR: u64 = 1 << 32;
/// A ticket account's size: discriminator, state, beneficiary, lamports,
/// created epoch.
pub const TICKET_LEN: u64 = 88;

/// Anchor's discriminators: the first 8 bytes of SHA-256 of
/// `account:<Name>` or `global:<instruction>`.
const STATE_DISCRIMINATOR: [u8; 8] = [216, 146, 107, 94, 104, 75, 182, 177];
const TICKET_DISCRIMINATOR: [u8; 8] = [133, 77, 18, 98, 211, 1, 231, 3];
const DEPOSIT: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];
const LIQUID_UNSTAKE: [u8; 8] = [30, 30, 119, 240, 191, 227, 12, 16];
const ORDER_UNSTAKE: [u8; 8] = [97, 167, 144, 107, 117, 190, 128, 36];
const CLAIM: [u8; 8] = [62, 198, 214, 193, 213, 159, 108, 210];

/// Byte offsets of the state's fields the venue reads, and the epoch
/// writes (checked against the account, see the tests).
pub mod offsets {
    pub const MSOL_MINT: usize = 8;
    pub const TREASURY_MSOL: usize = 104;
    pub const RENT_EXEMPT_FOR_TOKEN_ACC: usize = 138;
    pub const DELAYED_UNSTAKE_COOLING_DOWN: usize = 226;
    pub const TOTAL_ACTIVE_BALANCE: usize = 376;
    pub const MSOL_LEG: usize = 420;
    pub const LP_LIQUIDITY_TARGET: usize = 452;
    pub const LP_MAX_FEE: usize = 460;
    pub const LP_MIN_FEE: usize = 464;
    pub const AVAILABLE_RESERVE_BALANCE: usize = 496;
    pub const MSOL_SUPPLY: usize = 504;
    pub const MSOL_PRICE: usize = 512;
    pub const CIRCULATING_TICKET_BALANCE: usize = 528;
    pub const EMERGENCY_COOLING_DOWN: usize = 568;
    pub const PAUSED: usize = 608;
    /// The end of the fields above.
    pub const END: usize = 609;
}

/// The state account's fields a session reads.
#[derive(Debug, Clone, PartialEq)]
pub struct MarinadeState {
    pub msol_mint: Pubkey,
    pub treasury_msol: Pubkey,
    pub msol_leg: Pubkey,
    pub rent_exempt_for_token_acc: u64,
    pub total_active_balance: u64,
    pub delayed_unstake_cooling_down: u64,
    pub emergency_cooling_down: u64,
    pub available_reserve_balance: u64,
    pub msol_supply: u64,
    pub msol_price: u64,
    pub circulating_ticket_balance: u64,
    pub lp_liquidity_target: u64,
    /// The liquid unstake fee's range, in basis points.
    pub lp_max_fee_bps: u32,
    pub lp_min_fee_bps: u32,
    pub paused: bool,
}

impl MarinadeState {
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < offsets::END || data[..8] != STATE_DISCRIMINATOR {
            return Err(Error::Transaction {
                label: "the Marinade state".into(),
                reason: "not a Marinade state account".into(),
            });
        }
        let u64_at = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().expect("8"));
        let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().expect("4"));
        let key_at = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().expect("32"));
        Ok(Self {
            msol_mint: key_at(offsets::MSOL_MINT),
            treasury_msol: key_at(offsets::TREASURY_MSOL),
            msol_leg: key_at(offsets::MSOL_LEG),
            rent_exempt_for_token_acc: u64_at(offsets::RENT_EXEMPT_FOR_TOKEN_ACC),
            total_active_balance: u64_at(offsets::TOTAL_ACTIVE_BALANCE),
            delayed_unstake_cooling_down: u64_at(offsets::DELAYED_UNSTAKE_COOLING_DOWN),
            emergency_cooling_down: u64_at(offsets::EMERGENCY_COOLING_DOWN),
            available_reserve_balance: u64_at(offsets::AVAILABLE_RESERVE_BALANCE),
            msol_supply: u64_at(offsets::MSOL_SUPPLY),
            msol_price: u64_at(offsets::MSOL_PRICE),
            circulating_ticket_balance: u64_at(offsets::CIRCULATING_TICKET_BALANCE),
            lp_liquidity_target: u64_at(offsets::LP_LIQUIDITY_TARGET),
            lp_max_fee_bps: u32_at(offsets::LP_MAX_FEE),
            lp_min_fee_bps: u32_at(offsets::LP_MIN_FEE),
            paused: data[offsets::PAUSED] != 0,
        })
    }

    /// SOL per mSOL, as `liquid_unstake` and `order_unstake` pay it.
    pub fn rate(&self) -> Decimal {
        Decimal::from(self.msol_price) / Decimal::from(PRICE_DENOMINATOR)
    }

    /// The lamports staking prices against: what Marinade controls, less
    /// what tickets owe.
    pub fn total_virtual_staked_lamports(&self) -> u64 {
        (self.total_active_balance
            + self.delayed_unstake_cooling_down
            + self.emergency_cooling_down
            + self.available_reserve_balance)
            .saturating_sub(self.circulating_ticket_balance)
    }

    /// The `msol_price` Marinade's update would write: virtual staked
    /// lamports per mSOL, in 2^-32ths.
    pub fn price_for(virtual_lamports: u64, supply: u64) -> u64 {
        if supply == 0 {
            return PRICE_DENOMINATOR;
        }
        (u128::from(PRICE_DENOMINATOR) * u128::from(virtual_lamports) / u128::from(supply)) as u64
    }

    /// The liquid unstake fee, in basis points, when the SOL leg keeps
    /// `left` lamports after paying: the most at an empty leg, falling
    /// linearly to the least at the target.
    pub fn liquid_unstake_fee_bps(&self, left: u64) -> f64 {
        if left >= self.lp_liquidity_target {
            return f64::from(self.lp_min_fee_bps);
        }
        let span = f64::from(self.lp_max_fee_bps - self.lp_min_fee_bps.min(self.lp_max_fee_bps));
        f64::from(self.lp_max_fee_bps) - span * left as f64 / self.lp_liquidity_target.max(1) as f64
    }
}

/// The program's PDAs off the state.
pub fn pda(seed: &[u8]) -> Pubkey {
    Pubkey::find_program_address(&[STATE.as_ref(), seed], &PROGRAM).0
}

pub fn reserve() -> Pubkey {
    pda(b"reserve")
}

pub fn sol_leg() -> Pubkey {
    pda(b"liq_sol")
}

/// Marinade's instructions.
pub mod ix {
    use super::*;

    fn data(discriminator: [u8; 8], amount: Option<u64>) -> Vec<u8> {
        let mut data = discriminator.to_vec();
        if let Some(amount) = amount {
            data.extend_from_slice(&amount.to_le_bytes());
        }
        data
    }

    /// `deposit`: stake `lamports` of `from`'s SOL for mSOL into `to`.
    pub fn deposit(
        state: &MarinadeState,
        from: &Pubkey,
        to: &Pubkey,
        lamports: u64,
    ) -> Instruction {
        Instruction {
            program_id: PROGRAM,
            accounts: vec![
                AccountMeta::new(STATE, false),
                AccountMeta::new(state.msol_mint, false),
                AccountMeta::new(sol_leg(), false),
                AccountMeta::new(state.msol_leg, false),
                AccountMeta::new_readonly(pda(b"liq_st_sol_authority"), false),
                AccountMeta::new(reserve(), false),
                AccountMeta::new(*from, true),
                AccountMeta::new(*to, false),
                AccountMeta::new_readonly(pda(b"st_mint"), false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
                AccountMeta::new_readonly(token::token_program(), false),
            ],
            data: data(DEPOSIT, Some(lamports)),
        }
    }

    /// `liquid_unstake`: sell `msol` of `authority`'s from `from` into the
    /// liquidity pool, for SOL paid to `to`.
    pub fn liquid_unstake(
        state: &MarinadeState,
        authority: &Pubkey,
        from: &Pubkey,
        to: &Pubkey,
        msol: u64,
    ) -> Instruction {
        Instruction {
            program_id: PROGRAM,
            accounts: vec![
                AccountMeta::new(STATE, false),
                AccountMeta::new(state.msol_mint, false),
                AccountMeta::new(sol_leg(), false),
                AccountMeta::new(state.msol_leg, false),
                AccountMeta::new(state.treasury_msol, false),
                AccountMeta::new(*from, false),
                AccountMeta::new_readonly(*authority, true),
                AccountMeta::new(*to, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
                AccountMeta::new_readonly(token::token_program(), false),
            ],
            data: data(LIQUID_UNSTAKE, Some(msol)),
        }
    }

    /// `order_unstake`: burn `msol` for a ticket in `ticket` (an account of
    /// [`TICKET_LEN`] bytes, owned by the program, all zeros).
    pub fn order_unstake(
        state: &MarinadeState,
        authority: &Pubkey,
        from: &Pubkey,
        ticket: &Pubkey,
        msol: u64,
    ) -> Instruction {
        Instruction {
            program_id: PROGRAM,
            accounts: vec![
                AccountMeta::new(STATE, false),
                AccountMeta::new(state.msol_mint, false),
                AccountMeta::new(*from, false),
                AccountMeta::new_readonly(*authority, true),
                AccountMeta::new(*ticket, false),
                AccountMeta::new_readonly(CLOCK, false),
                AccountMeta::new_readonly(RENT, false),
                AccountMeta::new_readonly(token::token_program(), false),
            ],
            data: data(ORDER_UNSTAKE, Some(msol)),
        }
    }

    /// `claim`: pay a due ticket's SOL to its beneficiary `to`.
    pub fn claim(ticket: &Pubkey, to: &Pubkey) -> Instruction {
        Instruction {
            program_id: PROGRAM,
            accounts: vec![
                AccountMeta::new(STATE, false),
                AccountMeta::new(reserve(), false),
                AccountMeta::new(*ticket, false),
                AccountMeta::new(*to, false),
                AccountMeta::new_readonly(CLOCK, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            ],
            data: data(CLAIM, None),
        }
    }

    /// Allocate `ticket` for `order_unstake`.
    pub fn allocate_ticket(payer: &Pubkey, ticket: &Pubkey, rent: u64) -> Instruction {
        let mut data = 0u32.to_le_bytes().to_vec();
        data.extend_from_slice(&rent.to_le_bytes());
        data.extend_from_slice(&TICKET_LEN.to_le_bytes());
        data.extend_from_slice(PROGRAM.as_ref());
        Instruction {
            program_id: SYSTEM_PROGRAM,
            accounts: vec![
                AccountMeta::new(*payer, true),
                AccountMeta::new(*ticket, true),
            ],
            data,
        }
    }
}

/// A delayed unstake: its account, the SOL it pays, and the epoch it was
/// ordered in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub address: Pubkey,
    pub lamports: u64,
    pub created_epoch: u64,
}

/// The tickets `owner` is the beneficiary of.
pub async fn tickets(chain: &dyn ChainClient, owner: &Pubkey) -> Result<Vec<Ticket>> {
    let accounts = chain
        .get_program_accounts(
            &PROGRAM,
            vec![
                RpcFilterType::DataSize(TICKET_LEN),
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(0, &TICKET_DISCRIMINATOR)),
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(40, owner.as_ref())),
            ],
        )
        .await
        .map_err(chain_error("the wallet's Marinade tickets"))?;
    let mut tickets: Vec<Ticket> = accounts
        .into_iter()
        .map(|(address, account)| {
            let u64_at = |at: usize| {
                u64::from_le_bytes(account.data[at..at + 8].try_into().expect("8 bytes"))
            };
            Ticket {
                address,
                lamports: u64_at(72),
                created_epoch: u64_at(80),
            }
        })
        .collect();
    tickets.sort_by_key(|t| t.address);
    Ok(tickets)
}

pub async fn read_state(chain: &dyn ChainClient) -> Result<MarinadeState> {
    MarinadeState::decode(&account_data(chain, &STATE, "the Marinade state").await?)
}

/// The SOL leg's lamports a liquid unstake can take.
async fn sol_leg_capacity(chain: &dyn ChainClient, state: &MarinadeState) -> Result<u64> {
    let leg = chain
        .get_balance(&sol_leg())
        .await
        .map_err(chain_error("Marinade's SOL leg"))?;
    Ok(leg.saturating_sub(state.rent_exempt_for_token_acc))
}

/// The SOL a small liquid unstake is quoted at.
const QUOTE_SOL: u64 = super::stake_pool::QUOTE_SOL;

/// Prices for a small trade each way, in SOL per mSOL: staking at the
/// state's virtual price, unstaking at once less the pool's fee.
async fn quote(chain: &dyn ChainClient) -> Result<(Option<f64>, Option<f64>, MarinadeState, u64)> {
    let state = read_state(chain).await?;
    let capacity = sol_leg_capacity(chain, &state).await?;
    let rate = crate::session::grader_f64(state.rate());
    let ask = state.total_virtual_staked_lamports() as f64 / state.msol_supply.max(1) as f64;
    let bid = (capacity >= QUOTE_SOL)
        .then(|| rate * (1.0 - state.liquid_unstake_fee_bps(capacity - QUOTE_SOL) / 1e4));
    Ok((bid, Some(ask), state, capacity))
}

/// Marinade, read through one endpoint.
pub struct Marinade {
    id: VenueId,
    rpc_url: String,
    plan: EpochPlan,
}

impl Marinade {
    /// Marinade, read through `rpc_url`, in a session that plans `plan`'s
    /// epoch boundaries.
    pub fn new(rpc_url: &str, plan: &EpochPlan) -> Self {
        Self {
            id: VenueId::new(VENUE),
            rpc_url: rpc_url.into(),
            plan: plan.clone(),
        }
    }

    fn instrument(&self) -> InstrumentRef {
        InstrumentRef {
            venue: self.id.0.clone(),
            kind: Kind::StakePool,
            symbol: SYMBOL.into(),
        }
    }
}

#[async_trait]
impl Venue for Marinade {
    fn id(&self) -> &VenueId {
        &self.id
    }

    fn instruments(&self) -> Vec<InstrumentRef> {
        vec![self.instrument()]
    }

    async fn market(&self, instrument: &InstrumentRef) -> Result<Box<dyn Market>> {
        if *instrument != self.instrument() {
            return Err(not_offered(&self.id, instrument));
        }
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::with_timeout(
            &self.rpc_url,
            INTERNAL_RPC_TIMEOUT,
        ));
        let state = read_state(chain.as_ref()).await?;
        Ok(Box::new(MarinadeMarket {
            instrument: self.instrument(),
            chain,
            mint: state.msol_mint,
            plan: self.plan.clone(),
        }))
    }
}

/// mSOL, as a session trades it.
pub struct MarinadeMarket {
    instrument: InstrumentRef,
    chain: Arc<dyn ChainClient>,
    mint: Pubkey,
    plan: EpochPlan,
}

impl MarinadeMarket {
    pub fn mint(&self) -> Pubkey {
        self.mint
    }
}

#[async_trait]
impl Market for MarinadeMarket {
    fn instrument(&self) -> &InstrumentRef {
        &self.instrument
    }

    fn address(&self) -> Pubkey {
        STATE
    }

    async fn top(&self) -> Result<(Option<f64>, Option<f64>)> {
        let (bid, ask, _, _) = quote(self.chain.as_ref()).await?;
        Ok((bid, ask))
    }

    async fn holdings(&self, owner: &Pubkey) -> Result<Vec<Holding>> {
        let chain = self.chain.as_ref();
        let held = token::owned_amount(chain, owner, &self.mint)
            .await
            .map_err(chain_error("the wallet's mSOL"))?;
        let waiting = tickets(chain, owner)
            .await?
            .iter()
            .map(|t| t.lamports)
            .sum();
        let msol = Asset::new(self.mint.to_string(), SYMBOL, 9).expect("9 decimals");
        Ok(vec![
            Holding::Spot(Holdings {
                wallet: lamports(held),
                ..Holdings::empty(msol)
            }),
            sol_holding(chain, owner, waiting).await?,
        ])
    }

    async fn value_usd(
        &self,
        chain: &dyn ChainClient,
        holdings: &[Holding],
    ) -> Result<Option<Decimal>> {
        let Some(token) = token_held(holdings, &self.mint) else {
            return Ok(None);
        };
        let rate = read_state(chain).await?.rate();
        Ok(Some(value_token(chain, token, rate).await?))
    }

    async fn fills(
        &self,
        after_slot: u64,
        owner: &Pubkey,
    ) -> Result<Vec<(MarketFill, Vec<Pubkey>)>> {
        // Staking pays the reserve or the pool's mSOL leg, an instant exit
        // the SOL leg: the owner's own SOL is the one side of every fill.
        let mut fills = Vec::new();
        for vault in [reserve(), sol_leg()] {
            fills.extend(
                pool_fills(
                    self.chain.as_ref(),
                    &self.instrument,
                    &vault,
                    &self.mint,
                    after_slot,
                    owner,
                )
                .await?,
            );
        }
        Ok(fills)
    }

    async fn seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>> {
        Ok(Box::new(MarinadeSeat {
            market: self.instrument.to_string(),
            chain: Arc::new(RpcChainClient::new(rpc_url)),
            signer,
            plan: self.plan.clone(),
        }))
    }
}

/// An account staking into, and unstaking from, Marinade.
pub struct MarinadeSeat {
    market: String,
    chain: Arc<dyn ChainClient>,
    signer: Keypair,
    plan: EpochPlan,
}

impl MarinadeSeat {
    async fn stake(&self, amount: Option<f64>) -> Sent {
        let owner = self.signer.pubkey();
        let chain = self.chain.as_ref();
        let (state, balance) = match (read_state(chain).await, chain.get_balance(&owner).await) {
            (Ok(state), Ok(balance)) => (state, balance),
            (Err(e), _) => return not_sent(e.to_string()),
            (_, Err(e)) => return not_sent(e.to_string()),
        };
        let lamports = match stake_amount(amount, balance) {
            Ok(lamports) => lamports,
            Err(reason) => return not_sent(reason),
        };
        let to = token::ata(&owner, &state.msol_mint);
        let instructions = vec![
            token::create_ata_idempotent(&owner, &owner, &state.msol_mint),
            ix::deposit(&state, &owner, &to, lamports),
        ];
        send_now(chain, &self.signer, "marinade deposit", instructions).await
    }

    async fn unstake(&self, amount: Option<f64>) -> Sent {
        let owner = self.signer.pubkey();
        let chain = self.chain.as_ref();
        let state = match read_state(chain).await {
            Ok(state) => state,
            Err(e) => return not_sent(e.to_string()),
        };
        let from = token::ata(&owner, &state.msol_mint);
        let msol = match amount {
            Some(amount) => match atoms(amount) {
                Some(msol) => msol,
                None => return not_sent(format!("{amount} is not an amount of mSOL")),
            },
            None => match chain.get_account(&from).await {
                Ok(Some(account)) => token::token_balance(&account).unwrap_or(0),
                _ => 0,
            },
        };
        if msol == 0 {
            return not_sent("no mSOL to unstake");
        }
        let instructions = vec![ix::liquid_unstake(&state, &owner, &from, &owner, msol)];
        send_now(chain, &self.signer, "marinade liquid unstake", instructions).await
    }
}

#[async_trait]
impl Seat for MarinadeSeat {
    fn market(&self) -> &str {
        &self.market
    }

    fn executes(&self, action: &Action) -> bool {
        matches!(action, Action::Deposit { .. } | Action::Withdraw { .. })
    }

    async fn send(&self, planned: &Planned) -> Sent {
        match planned.action {
            Action::Deposit { amount } => self.stake(amount).await,
            Action::Withdraw { amount } => self.unstake(amount).await,
            _ => not_sent("unsupported in this session"),
        }
    }

    async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
        let (bid, ask, _, _) = quote(self.chain.as_ref())
            .await
            .map_err(|e| e.to_string())?;
        Ok((bid, ask))
    }

    async fn position(&self) -> SeatResult<f64> {
        let state = read_state(self.chain.as_ref())
            .await
            .map_err(|e| e.to_string())?;
        let held =
            token::owned_amount(self.chain.as_ref(), &self.signer.pubkey(), &state.msol_mint)
                .await
                .map_err(|e| e.to_string())?;
        Ok(held as f64 / 1e9)
    }

    async fn view(&self, view: &mut VenueView) -> SeatResult {
        let chain = self.chain.as_ref();
        let (_, _, state, capacity) = quote(chain).await.map_err(|e| e.to_string())?;
        set_size(view, self.position().await?);
        view.stats = vec![
            ("rate".into(), crate::session::grader_f64(state.rate())),
            ("deposit_fee_bps".into(), 0.0),
            (
                "withdraw_fee_bps".into(),
                state.liquid_unstake_fee_bps(capacity.saturating_sub(QUOTE_SOL)),
            ),
            ("instant_capacity_sol".into(), capacity as f64 / 1e9),
        ];
        view.stats
            .extend(epoch_stats(chain, &self.plan).await.map_err(|e| e.to_string())?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state account with the fields the venue reads, at their offsets,
    /// as decoded from mainnet on 2026-11-12.
    pub(crate) fn state_bytes() -> Vec<u8> {
        let mut data = vec![0u8; 2616];
        data[..8].copy_from_slice(&STATE_DISCRIMINATOR);
        let mut put =
            |at: usize, value: u64| data[at..at + 8].copy_from_slice(&value.to_le_bytes());
        put(offsets::RENT_EXEMPT_FOR_TOKEN_ACC, 2_039_280);
        put(offsets::TOTAL_ACTIVE_BALANCE, 2_236_668_896_701_617);
        put(offsets::LP_LIQUIDITY_TARGET, 3_500_000_000_000);
        put(offsets::AVAILABLE_RESERVE_BALANCE, 23_826_430_390_025);
        put(offsets::MSOL_SUPPLY, 1_593_523_697_307_085);
        put(offsets::MSOL_PRICE, 6_089_341_553);
        put(offsets::CIRCULATING_TICKET_BALANCE, 1_220_832_480_809);
        data[offsets::LP_MAX_FEE..offsets::LP_MAX_FEE + 4].copy_from_slice(&150u32.to_le_bytes());
        data[offsets::LP_MIN_FEE..offsets::LP_MIN_FEE + 4].copy_from_slice(&3u32.to_le_bytes());
        data
    }

    #[test]
    fn the_state_s_price_is_its_virtual_staked_lamports_per_msol() {
        let state = MarinadeState::decode(&state_bytes()).unwrap();
        let rate = crate::session::grader_f64(state.rate());
        assert!((rate - 1.417_785_313).abs() < 1e-9, "{rate}");
        // The price Marinade's update would write is the one it holds.
        assert_eq!(
            MarinadeState::price_for(state.total_virtual_staked_lamports(), state.msol_supply),
            state.msol_price
        );
        assert!(!state.paused);
        let mut other = state_bytes();
        other[0] ^= 1;
        assert!(MarinadeState::decode(&other).is_err());
    }

    #[test]
    fn the_liquid_unstake_fee_grows_as_the_leg_drains() {
        let state = MarinadeState::decode(&state_bytes()).unwrap();
        assert_eq!(state.liquid_unstake_fee_bps(4_000_000_000_000), 3.0);
        assert_eq!(state.liquid_unstake_fee_bps(0), 150.0);
        let at_1088 = state.liquid_unstake_fee_bps(1_088_000_000_000);
        assert!((at_1088 - (150.0 - 147.0 * 1088.0 / 3500.0)).abs() < 1e-9);
        assert!(state.liquid_unstake_fee_bps(500_000_000_000) > at_1088);
    }

    #[test]
    fn instructions_carry_anchor_s_discriminators() {
        let state = MarinadeState::decode(&state_bytes()).unwrap();
        let user = Pubkey::new_unique();
        let deposit = ix::deposit(&state, &user, &user, 9);
        assert_eq!(deposit.data[..8], DEPOSIT);
        assert_eq!(deposit.data[8..], 9u64.to_le_bytes());
        assert!(deposit.accounts[6].is_signer);
        assert_eq!(ix::claim(&user, &user).data, CLAIM);
        assert_eq!(
            reserve(),
            Pubkey::find_program_address(&[STATE.as_ref(), b"reserve"], &PROGRAM).0
        );
    }
}
