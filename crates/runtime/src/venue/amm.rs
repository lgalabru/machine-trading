//! Concentrated-liquidity pools as [`Venue`]s (`specs/instruments/amm.md`):
//! Orca Whirlpool (venue `orca`) and Raydium CLMM (venue `raydium`), the
//! two programs that carried about three quarters of atomic arbitrage's
//! profit (`docs/rfd/0005-atomic-arbitrage-tasks.md`). Each pool is an
//! `amm` instrument named by its account address, since a pair has many
//! pools: `orca:amm:Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE`.
//!
//! A pool's base is its first token (Whirlpool's A, Raydium's 0) and its
//! quote the second: SOL/USDC on both. Every price here comes from the
//! kit's exact quotes (`mtx_kit::protocols::orca::whirlpool`,
//! `mtx_kit::protocols::raydium::clmm`), which reproduce each program's
//! integer math, so what a market or a seat says a swap does is what the
//! program will do on the same state, to the atom. A route lives on its
//! last atom.
//!
//! - **Top**: the price of a small swap each way ([`TOP_SWAP_BASE_DIVISOR`]),
//!   fees included, as the profile's Prices section says.
//! - **Holdings**: both assets in the owner's wallet, base first, as a spot
//!   market reports them (a pool has no account of the owner's to hold
//!   anything else in, until liquidity positions are offered).
//! - **Fills**: one per transaction that moved the pool's two vaults in
//!   opposite directions, at the price the vaults recorded. A transaction
//!   that swaps through the same pool twice nets to one fill, and the
//!   taker is the transaction's fee payer; both are exact for a seat's own
//!   swaps, which are one per transaction and signed by their owner.
//! - **Seat**: executes `swap` only, as `swap_v2` after its token accounts,
//!   with the line's limit as the program's own threshold
//!   ([`SwapOrder::from_swap`]), so a pool that moved past the limit
//!   refuses the swap on chain. It reads and sends through an injected
//!   [`ChainClient`], so the same seat runs on any client a host hands it.
//!
//! (A probe: this line changes nothing, and asks CI for main's own anchor digests.)
//!
//! Not covered: Token-2022 transfer fees and hooks (the kit's quotes don't
//! model them), liquidity positions, and the other pool programs the
//! profile lists (Raydium AMM v4 and CPMM, Meteora).

use std::{str::FromStr as _, sync::Arc};

use async_trait::async_trait;
use mtx_core::{
    Asset, Decimal, Holding, Holdings, InstrumentId, Side as CoreSide, VenueId,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    Action, AmountUnit, Planned, SendState, Sent, Side,
    chain::{
        AddressLookupTableAccount, ChainClient, RpcChainClient, TransactionPlan,
        TransactionSummary, token,
    },
    protocols::{USDC_MINT, orca::whirlpool, raydium::clmm, spot::MarketFill},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Instruction;

use super::{Built, Market, Seat, SeatChain, SeatResult, Venue, not_offered, sent_on};
use crate::{Error, Result, reads::INTERNAL_RPC_TIMEOUT};

/// The venues this module offers pools on.
pub const VENUES: [&str; 2] = [whirlpool::VENUE, clmm::VENUE];

/// A small swap, for the top: a base unit divided by this. A thousandth of
/// a SOL is 15 cents at $150, small enough to move no pool, and its output
/// is hundreds of thousands of USDC atoms, so the price reads to about six
/// digits.
pub const TOP_SWAP_BASE_DIVISOR: u64 = 1_000;

/// Tick arrays read ahead of a Raydium swap, each way: as many as the kit's
/// fork test reads, more than its largest swap (1,500 SOL) entered.
const CLMM_TICK_ARRAYS_READ_MAX: usize = 16;

/// Transactions [`Market::fills`] reads in one call. A session step lists a
/// few hundred at most; a mainnet pool sees thousands a minute, which this
/// reader isn't for, so more is refused rather than read for minutes.
const FILL_TRANSACTIONS_MAX: usize = 10_000;

/// Decimals a mint may have: `10^decimals` must fit the arithmetic here.
const MINT_DECIMALS_MAX: u8 = 18;

/// The clock sysvar, whose Unix time the quotes need: a Raydium pool's
/// open time and both programs' dynamic fees depend on it.
const CLOCK_SYSVAR: Pubkey = solana_pubkey::pubkey!("SysvarC1ock11111111111111111111111111111111");

const WRAPPED_SOL_MINT: Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");

/// The pool programs this module trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    Whirlpool,
    RaydiumClmm,
}

impl Program {
    /// The program a venue's pools run on.
    pub fn of_venue(venue: &str) -> Option<Self> {
        match venue {
            whirlpool::VENUE => Some(Self::Whirlpool),
            clmm::VENUE => Some(Self::RaydiumClmm),
            _ => None,
        }
    }

    pub fn id(self) -> Pubkey {
        match self {
            Self::Whirlpool => whirlpool::PROGRAM_ID,
            Self::RaydiumClmm => clmm::PROGRAM_ID,
        }
    }

    /// Compute units one swap may need: each kit's own figure for a swap
    /// that crosses many ticks.
    pub fn compute_unit_limit(self) -> u32 {
        match self {
            Self::Whirlpool => whirlpool::COMPUTE_UNIT_LIMIT,
            Self::RaydiumClmm => clmm::COMPUTE_UNIT_LIMIT,
        }
    }
}

/// One of a pool's two tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub mint: Pubkey,
    /// The mint's owner: SPL Token or Token-2022.
    pub token_program: Pubkey,
    /// The pool's account holding it.
    pub vault: Pubkey,
    pub asset: Asset,
}

impl Token {
    fn units(&self, atoms: i128) -> Decimal {
        Decimal::from_i128_with_scale(atoms, u32::from(self.asset.decimals))
    }
}

/// A pool, as read once: what never changes about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub instrument: InstrumentRef,
    pub address: Pubkey,
    pub program: Program,
    /// Token A (Whirlpool) or 0 (Raydium).
    pub base: Token,
    /// Token B or 1.
    pub quote: Token,
}

impl Pool {
    /// The pool `instrument` names, read through `chain`: it must be an
    /// account of its venue's program.
    pub async fn read(chain: &dyn ChainClient, instrument: &InstrumentRef) -> Result<Self> {
        let label = instrument.to_string();
        let program = Program::of_venue(&instrument.venue)
            .filter(|_| instrument.kind == Kind::Amm)
            .ok_or_else(|| not_offered(&VenueId::new(&instrument.venue), instrument))?;
        let address: Pubkey = instrument
            .symbol
            .parse()
            .map_err(|_| venue_error(&label, "a pool's symbol is its address"))?;
        let account = chain
            .get_account(&address)
            .await
            .map_err(|e| venue_error(&label, e))?
            .ok_or_else(|| venue_error(&label, "no pool at its address"))?;
        if account.owner != program.id() {
            return Err(venue_error(
                &label,
                format!("owned by {}, not {}", account.owner, program.id()),
            ));
        }
        let [(base_mint, base_vault), (quote_mint, quote_vault)] = match program {
            Program::Whirlpool => {
                let pool = whirlpool::Whirlpool::decode(&account.data)
                    .map_err(|e| venue_error(&label, e))?;
                [
                    (pool.token_mint_a, pool.token_vault_a),
                    (pool.token_mint_b, pool.token_vault_b),
                ]
            }
            Program::RaydiumClmm => {
                let pool = clmm::Pool::decode(&account.data).map_err(|e| venue_error(&label, e))?;
                [
                    (pool.token_mint_0, pool.token_vault_0),
                    (pool.token_mint_1, pool.token_vault_1),
                ]
            }
        };
        assert_ne!(base_mint, quote_mint, "{label}: a pool of one mint");
        Ok(Self {
            instrument: instrument.clone(),
            address,
            program,
            base: token_of(chain, &label, base_mint, base_vault).await?,
            quote: token_of(chain, &label, quote_mint, quote_vault).await?,
        })
    }
}

/// A pool's token: its mint read for its program and decimals.
async fn token_of(
    chain: &dyn ChainClient,
    label: &str,
    mint: Pubkey,
    vault: Pubkey,
) -> Result<Token> {
    let read = token::read_mint(chain, &mint)
        .await
        .map_err(|e| venue_error(label, format!("mint {mint}: {e}")))?;
    if read.decimals > MINT_DECIMALS_MAX {
        return Err(venue_error(
            label,
            format!("mint {mint} has {} decimals", read.decimals),
        ));
    }
    let symbol = match mint {
        WRAPPED_SOL_MINT => "SOL".to_string(),
        USDC_MINT => "USDC".to_string(),
        other => other.to_string(),
    };
    Ok(Token {
        mint,
        token_program: read.program,
        vault,
        asset: Asset::new(mint.to_string(), symbol, read.decimals)
            .map_err(|e| venue_error(label, e))?,
    })
}

fn venue_error(label: impl Into<String>, reason: impl ToString) -> Error {
    Error::Transaction {
        label: label.into(),
        reason: reason.to_string(),
    }
}

/// The pools of one venue (`orca` or `raydium`) a session trades, read
/// through one client.
pub struct Amm {
    id: VenueId,
    chain: Arc<dyn ChainClient>,
    pools: Vec<Pool>,
}

impl Amm {
    /// The venue `venue`, offering the pools `instruments` name.
    pub async fn connect(
        venue: &str,
        chain: Arc<dyn ChainClient>,
        instruments: &[InstrumentRef],
    ) -> Result<Self> {
        let id = VenueId::new(venue);
        let mut pools = Vec::with_capacity(instruments.len());
        for instrument in instruments {
            if instrument.venue != venue {
                return Err(not_offered(&id, instrument));
            }
            pools.push(Pool::read(chain.as_ref(), instrument).await?);
        }
        Ok(Self { id, chain, pools })
    }

    pub fn pools(&self) -> &[Pool] {
        &self.pools
    }
}

#[async_trait]
impl Venue for Amm {
    fn id(&self) -> &VenueId {
        &self.id
    }

    fn instruments(&self) -> Vec<InstrumentRef> {
        self.pools.iter().map(|p| p.instrument.clone()).collect()
    }

    async fn market(&self, instrument: &InstrumentRef) -> Result<Box<dyn Market>> {
        let pool = self
            .pools
            .iter()
            .find(|p| p.instrument == *instrument)
            .ok_or_else(|| not_offered(&self.id, instrument))?;
        Ok(Box::new(PoolMarket {
            pool: pool.clone(),
            chain: self.chain.clone(),
        }))
    }
}

/// Which token a swap pays in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Pays base, receives quote: a sell.
    BaseIn,
    /// Pays quote, receives base: a buy.
    QuoteIn,
}

/// Which side of a swap its amount fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The amount is what goes in, fee included.
    ExactIn,
    /// The amount is what comes out.
    ExactOut,
}

/// A swap, quoted on one state of its pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quoted {
    pub direction: Direction,
    pub mode: Mode,
    /// Atoms of the token paid, fee included: the input vault's increase.
    pub amount_in: u64,
    /// Atoms of the token received: the output vault's decrease.
    pub amount_out: u64,
    /// The tick arrays `swap_v2` needs, in order.
    tick_arrays: Vec<Pubkey>,
}

impl Quoted {
    /// The amount the swap fixed: what goes in for exact in, out for exact
    /// out.
    pub fn fixed(&self) -> u64 {
        match self.mode {
            Mode::ExactIn => self.amount_in,
            Mode::ExactOut => self.amount_out,
        }
    }

    /// Quote units per base unit, fees included.
    pub fn price(&self, pool: &Pool) -> Option<f64> {
        let (base, quote) = match self.direction {
            Direction::BaseIn => (self.amount_in, self.amount_out),
            Direction::QuoteIn => (self.amount_out, self.amount_in),
        };
        if base == 0 {
            return None;
        }
        let units =
            |atoms: u64, token: &Token| atoms as f64 / 10f64.powi(token.asset.decimals.into());
        Some(units(quote, &pool.quote) / units(base, &pool.base))
    }
}

/// What a quote through a pool needs, read at one moment: the pool's
/// state, the tick arrays a swap either way may enter, and the clock.
#[derive(Debug, Clone)]
pub enum State {
    Whirlpool {
        pool: whirlpool::Whirlpool,
        tick_arrays: Vec<whirlpool::TickArray>,
        oracle: Option<whirlpool::Oracle>,
        timestamp_s: u64,
    },
    RaydiumClmm {
        pool: clmm::Pool,
        config: clmm::AmmConfig,
        /// Boxed: its bitmaps are kilobytes, the rest of the state isn't.
        extension: Option<Box<clmm::BitmapExtension>>,
        tick_arrays: Vec<clmm::TickArray>,
        timestamp_s: u64,
    },
}

impl State {
    /// `pool`'s state now.
    pub async fn read(chain: &dyn ChainClient, pool: &Pool) -> Result<Self> {
        let label = pool.instrument.to_string();
        let data = read_data(chain, &label, &pool.address)
            .await?
            .ok_or_else(|| venue_error(&label, "the pool is gone"))?;
        match pool.program {
            Program::Whirlpool => {
                let decoded =
                    whirlpool::Whirlpool::decode(&data).map_err(|e| venue_error(&label, e))?;
                read_whirlpool(chain, &label, &pool.address, decoded).await
            }
            Program::RaydiumClmm => {
                let decoded = clmm::Pool::decode(&data).map_err(|e| venue_error(&label, e))?;
                read_clmm(chain, &label, &pool.address, decoded).await
            }
        }
    }

    /// The swap of `amount` atoms (in for [`Mode::ExactIn`], out for
    /// [`Mode::ExactOut`]) that `swap_v2` would execute on this state, with
    /// no price limit: an exact-out swap must fill entirely.
    pub fn quote(
        &self,
        address: &Pubkey,
        direction: Direction,
        mode: Mode,
        amount: u64,
    ) -> SeatResult<Quoted> {
        let quoted = self.quote_limited(address, direction, mode, amount, 0)?;
        // A swap with no price limit stops short only at the curve's end.
        if quoted.fixed() != amount {
            return Err(format!(
                "the pool fills {} of its {amount} atoms",
                quoted.fixed()
            ));
        }
        Ok(quoted)
    }

    /// The pool's square-root price now, Q64.64 per atom.
    pub fn sqrt_price_x64(&self) -> u128 {
        match self {
            Self::Whirlpool { pool, .. } => pool.sqrt_price_x64,
            Self::RaydiumClmm { pool, .. } => pool.sqrt_price_x64,
        }
    }

    /// The pool's price now, quote units per base unit, before fees: its
    /// square-root price squared, in whole units.
    pub fn mid(&self, pool: &Pool) -> f64 {
        let decimals = i32::from(pool.base.asset.decimals) - i32::from(pool.quote.asset.decimals);
        (self.sqrt_price_x64() as f64 / 2f64.powi(64)).powi(2) * 10f64.powi(decimals)
    }

    /// The pool's swap fee, as a fraction of what goes in: both programs
    /// count it in millionths. A Whirlpool's adaptive fee, where its
    /// oracle sets one, comes on top and isn't counted.
    pub fn fee_fraction(&self) -> f64 {
        let millionths = match self {
            Self::Whirlpool { pool, .. } => u32::from(pool.fee_rate),
            Self::RaydiumClmm { config, .. } => config.trade_fee_rate,
        };
        f64::from(millionths) / 1e6
    }

    /// The pool's active liquidity now.
    pub fn liquidity(&self) -> u128 {
        match self {
            Self::Whirlpool { pool, .. } => pool.liquidity,
            Self::RaydiumClmm { pool, .. } => pool.liquidity,
        }
    }

    /// The exact-in swap of at most `max_in` atoms that moves the pool's
    /// price to `price` (quote units per base unit, before fees), and the
    /// square-root price that stops it, or `None` if the pool is there
    /// already. What an arbitrageur sends to bring a pool back to a
    /// reference: the program stops the swap at the limit, so the amount is
    /// a cap, not a guess.
    pub fn quote_to_price(
        &self,
        pool: &Pool,
        price: f64,
        max_in: u64,
    ) -> SeatResult<Option<(Quoted, u128)>> {
        let decimals = i32::from(pool.quote.asset.decimals) - i32::from(pool.base.asset.decimals);
        let per_atom = price * 10f64.powi(decimals);
        let target = (per_atom.sqrt() * 2f64.powi(64)) as u128;
        if !(per_atom.is_finite() && per_atom > 0.0) || target == 0 {
            return Err(format!("{price} isn't a price a pool can reach"));
        }
        let current = self.sqrt_price_x64();
        let direction = match target.cmp(&current) {
            std::cmp::Ordering::Equal => return Ok(None),
            std::cmp::Ordering::Less => Direction::BaseIn,
            std::cmp::Ordering::Greater => Direction::QuoteIn,
        };
        let quoted = self.quote_limited(&pool.address, direction, Mode::ExactIn, max_in, target)?;
        assert!(
            quoted.amount_in <= max_in,
            "a limited swap takes at most its cap"
        );
        Ok((quoted.amount_in > 0 && quoted.amount_out > 0).then_some((quoted, target)))
    }

    /// [`State::quote`] with a price limit, `0` for none; the swap may then
    /// stop short of `amount`.
    fn quote_limited(
        &self,
        address: &Pubkey,
        direction: Direction,
        mode: Mode,
        amount: u64,
        sqrt_price_limit_x64: u128,
    ) -> SeatResult<Quoted> {
        if amount == 0 {
            return Err("a swap of nothing".into());
        }
        let (amount_in, amount_out, tick_arrays) = match self {
            Self::Whirlpool {
                pool,
                tick_arrays,
                oracle,
                timestamp_s,
            } => {
                let request = whirlpool::SwapRequest {
                    direction: whirlpool_direction(direction),
                    mode: whirlpool_mode(mode),
                    amount,
                    sqrt_price_limit_x64,
                    timestamp_s: *timestamp_s,
                };
                let quote = whirlpool::quote(address, pool, tick_arrays, oracle.as_ref(), &request)
                    .map_err(|e| e.to_string())?;
                (
                    quote.amount_in,
                    quote.amount_out,
                    quote.tick_arrays.to_vec(),
                )
            }
            Self::RaydiumClmm {
                pool,
                config,
                extension,
                tick_arrays,
                timestamp_s,
            } => {
                let request = clmm::SwapRequest {
                    direction: clmm_direction(direction),
                    mode: clmm_mode(mode),
                    amount,
                    sqrt_price_limit_x64,
                    block_timestamp_s: *timestamp_s,
                };
                let quote = clmm::quote(
                    address,
                    pool,
                    config,
                    tick_arrays,
                    extension.as_deref(),
                    &request,
                )
                .map_err(|e| e.to_string())?;
                (quote.amount_in, quote.amount_out, quote.tick_arrays)
            }
        };
        Ok(Quoted {
            direction,
            mode,
            amount_in,
            amount_out,
            tick_arrays,
        })
    }

    /// The best bid and ask: what a small swap's base costs, and what it
    /// sells for, fees included. A side the pool can't fill has none.
    pub fn top(&self, pool: &Pool) -> (Option<f64>, Option<f64>) {
        let scale = 10u64.pow(pool.base.asset.decimals.into());
        let small = (scale / TOP_SWAP_BASE_DIVISOR).max(1);
        let price = |direction, mode| {
            self.quote(&pool.address, direction, mode, small)
                .ok()
                .and_then(|q| q.price(pool))
        };
        let bid = price(Direction::QuoteIn, Mode::ExactOut);
        let ask = price(Direction::BaseIn, Mode::ExactIn);
        if let (Some(bid), Some(ask)) = (bid, ask) {
            // Each side pays the fee, so buying costs more than selling gets.
            debug_assert!(bid >= ask, "bid {bid} under ask {ask}");
        }
        (bid, ask)
    }
}

fn whirlpool_direction(direction: Direction) -> whirlpool::SwapDirection {
    match direction {
        Direction::BaseIn => whirlpool::SwapDirection::AToB,
        Direction::QuoteIn => whirlpool::SwapDirection::BToA,
    }
}

fn whirlpool_mode(mode: Mode) -> whirlpool::SwapMode {
    match mode {
        Mode::ExactIn => whirlpool::SwapMode::ExactIn,
        Mode::ExactOut => whirlpool::SwapMode::ExactOut,
    }
}

fn clmm_direction(direction: Direction) -> clmm::SwapDirection {
    match direction {
        Direction::BaseIn => clmm::SwapDirection::ZeroForOne,
        Direction::QuoteIn => clmm::SwapDirection::OneForZero,
    }
}

fn clmm_mode(mode: Mode) -> clmm::SwapMode {
    match mode {
        Mode::ExactIn => clmm::SwapMode::ExactIn,
        Mode::ExactOut => clmm::SwapMode::ExactOut,
    }
}

async fn read_data(chain: &dyn ChainClient, label: &str, key: &Pubkey) -> Result<Option<Vec<u8>>> {
    Ok(chain
        .get_account(key)
        .await
        .map_err(|e| venue_error(label, e))?
        .map(|a| a.data))
}

/// A Whirlpool's tick arrays either way (an address with no account is an
/// uninitialized array), its oracle if it has one, and the clock.
async fn read_whirlpool(
    chain: &dyn ChainClient,
    label: &str,
    address: &Pubkey,
    pool: whirlpool::Whirlpool,
) -> Result<State> {
    let mut arrays: Vec<(i32, Pubkey)> = Vec::new();
    for direction in [
        whirlpool::SwapDirection::AToB,
        whirlpool::SwapDirection::BToA,
    ] {
        for found in whirlpool::tick_array_addresses(address, &pool, direction) {
            if !arrays.contains(&found) {
                arrays.push(found);
            }
        }
    }
    let mut keys: Vec<Pubkey> = arrays.iter().map(|&(_, key)| key).collect();
    keys.extend([whirlpool::ix::oracle_address(address), CLOCK_SYSVAR]);
    let mut accounts = chain
        .get_multiple_accounts(&keys)
        .await
        .map_err(|e| venue_error(label, e))?;
    assert_eq!(accounts.len(), keys.len(), "one answer per key");
    let clock = accounts.pop().flatten();
    let oracle = match accounts.pop().flatten() {
        Some(account) => {
            Some(whirlpool::Oracle::decode(&account.data).map_err(|e| venue_error(label, e))?)
        }
        None => None,
    };
    let mut tick_arrays = Vec::with_capacity(arrays.len());
    for (account, &(start, _)) in accounts.into_iter().zip(&arrays) {
        tick_arrays.push(match account {
            Some(account) => {
                whirlpool::TickArray::decode(&account.data).map_err(|e| venue_error(label, e))?
            }
            None => whirlpool::TickArray::uninitialized(*address, start),
        });
    }
    Ok(State::Whirlpool {
        pool,
        tick_arrays,
        oracle,
        timestamp_s: clock_timestamp_s(label, clock.map(|a| a.data))?,
    })
}

/// A Raydium pool's config, bitmap extension, the initialized tick arrays
/// a swap either way enters first, and the clock.
async fn read_clmm(
    chain: &dyn ChainClient,
    label: &str,
    address: &Pubkey,
    pool: clmm::Pool,
) -> Result<State> {
    let keys = [
        pool.amm_config,
        clmm::ix::bitmap_extension_address(address),
        CLOCK_SYSVAR,
    ];
    let [config, extension, clock]: [_; 3] = chain
        .get_multiple_accounts(&keys)
        .await
        .map_err(|e| venue_error(label, e))?
        .try_into()
        .map_err(|_| venue_error(label, "one answer per key"))?;
    let config = config.ok_or_else(|| venue_error(label, "the pool's config is gone"))?;
    let config = clmm::AmmConfig::decode(&config.data).map_err(|e| venue_error(label, e))?;
    let extension = match extension {
        Some(account) => Some(Box::new(
            clmm::BitmapExtension::decode(&account.data).map_err(|e| venue_error(label, e))?,
        )),
        None => None,
    };
    let mut starts: Vec<i32> = Vec::new();
    for direction in [Direction::BaseIn, Direction::QuoteIn] {
        let found = clmm::tick_arrays_in_direction(
            &pool,
            extension.as_deref(),
            clmm_direction(direction),
            CLMM_TICK_ARRAYS_READ_MAX,
        )
        .map_err(|e| venue_error(label, e))?;
        for start in found {
            if !starts.contains(&start) {
                starts.push(start);
            }
        }
    }
    let array_keys: Vec<Pubkey> = starts
        .iter()
        .map(|&start| clmm::ix::tick_array_address(address, start))
        .collect();
    let mut tick_arrays = Vec::with_capacity(array_keys.len());
    let accounts = chain
        .get_multiple_accounts(&array_keys)
        .await
        .map_err(|e| venue_error(label, e))?;
    // The bitmaps list initialized arrays only; one that is gone was
    // closed since, and a quote that needs it says so.
    for account in accounts.into_iter().flatten() {
        tick_arrays
            .push(clmm::TickArray::decode(&account.data).map_err(|e| venue_error(label, e))?);
    }
    Ok(State::RaydiumClmm {
        pool,
        config,
        extension,
        tick_arrays,
        timestamp_s: clock_timestamp_s(label, clock.map(|a| a.data))?,
    })
}

/// The clock sysvar's Unix time: after the slot, the epoch start, the epoch
/// and the leader schedule's epoch.
fn clock_timestamp_s(label: &str, clock: Option<Vec<u8>>) -> Result<u64> {
    let timestamp = clock
        .as_deref()
        .and_then(|data| data.get(32..40))
        .and_then(|bytes| bytes.try_into().ok())
        .map(i64::from_le_bytes)
        .ok_or_else(|| venue_error(label, "the clock sysvar"))?;
    u64::try_from(timestamp).map_err(|_| venue_error(label, format!("the clock reads {timestamp}")))
}

/// One pool, as a session trades it.
pub struct PoolMarket {
    pool: Pool,
    chain: Arc<dyn ChainClient>,
}

impl PoolMarket {
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// The pool's state now, to quote on.
    pub async fn state(&self) -> Result<State> {
        State::read(self.chain.as_ref(), &self.pool).await
    }
}

#[async_trait]
impl Market for PoolMarket {
    fn instrument(&self) -> &InstrumentRef {
        &self.pool.instrument
    }

    fn address(&self) -> Pubkey {
        self.pool.address
    }

    async fn top(&self) -> Result<(Option<f64>, Option<f64>)> {
        Ok(self.state().await?.top(&self.pool))
    }

    /// The base asset, then the quote, in the owner's wallet.
    async fn holdings(&self, owner: &Pubkey) -> Result<Vec<Holding>> {
        wallet_holdings(self.chain.as_ref(), &self.pool, owner).await
    }

    async fn fills(
        &self,
        after_slot: u64,
        owner: &Pubkey,
    ) -> Result<Vec<(MarketFill, Vec<Pubkey>)>> {
        let label = self.pool.instrument.to_string();
        let chain = self.chain.as_ref();
        let signatures = chain
            .signatures_for_address(&self.pool.address, after_slot)
            .await
            .map_err(|e| venue_error(&label, e))?;
        if signatures.len() > FILL_TRANSACTIONS_MAX {
            return Err(venue_error(
                &label,
                format!(
                    "{} transactions since slot {after_slot}, more than the {FILL_TRANSACTIONS_MAX} a read takes",
                    signatures.len()
                ),
            ));
        }
        let mut fills = Vec::new();
        // The listing is oldest first, in block order within a slot; a
        // slot's transactions are listed all or none, so a transaction's
        // place in its slot is the same on every read.
        let mut place: (u64, u64) = (0, 0);
        for (signature, slot) in signatures {
            assert!(slot >= place.0, "the listing is oldest first");
            place = if slot == place.0 {
                (slot, place.1 + 1)
            } else {
                (slot, 0)
            };
            let summary = chain
                .get_transaction(&signature)
                .await
                .map_err(|e| venue_error(&label, e))?
                .ok_or_else(|| {
                    venue_error(
                        &label,
                        format!("transaction {signature} is listed but not found"),
                    )
                })?;
            let sequence = (u128::from(slot) << 64) | u128::from(place.1);
            if let Some(fill) = fill_in(&self.pool, &summary, sequence) {
                fills.push((fill, vec![*owner]));
            }
        }
        fills.sort_by_key(|(fill, _)| (fill.slot, fill.sequence));
        Ok(fills)
    }

    async fn seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>> {
        let on = SeatChain {
            chain: Arc::new(RpcChainClient::with_timeout(rpc_url, INTERNAL_RPC_TIMEOUT)),
            pooled: false,
            lookup_tables: Vec::new(),
        };
        self.seat_on(signer, on).await
    }

    async fn seat_on(&self, signer: Keypair, on: SeatChain) -> Result<Box<dyn Seat>> {
        Ok(Box::new(
            PoolSeat::new(self.pool.clone(), on.chain, signer)
                .pooled(on.pooled)
                .with_lookup_tables(on.lookup_tables),
        ))
    }
}

/// `owner`'s wallet holdings of both of the pool's tokens, base first, as
/// the spot profile counts a wallet: every token account of the mint it
/// owns (for SOL, wrapped SOL only).
async fn wallet_holdings(
    chain: &dyn ChainClient,
    pool: &Pool,
    owner: &Pubkey,
) -> Result<Vec<Holding>> {
    let mut held = Vec::with_capacity(2);
    for token in [&pool.base, &pool.quote] {
        let atoms = token::owned_amount(chain, owner, &token.mint)
            .await
            .map_err(|e| venue_error(pool.instrument.to_string(), e))?;
        held.push(Holding::Spot(Holdings {
            wallet: token.units(atoms.into()),
            ..Holdings::empty(token.asset.clone())
        }));
    }
    Ok(held)
}

/// The swap a landed transaction made through `pool`, from its vaults: the
/// base vault's change is the size, the quote vault's the price's. Vaults
/// that moved the same way (liquidity added or taken out) or not at all
/// are no swap. `sequence` orders it on the pool: its slot, then its place
/// in the slot.
fn fill_in(pool: &Pool, summary: &TransactionSummary, sequence: u128) -> Option<MarketFill> {
    if summary.error.is_some() {
        return None;
    }
    let change = |vault: &Pubkey| -> i128 {
        summary
            .token_balance_changes
            .iter()
            .filter(|c| c.account == *vault)
            .map(|c| i128::from(c.post_amount) - i128::from(c.pre_amount))
            .sum()
    };
    let (base, quote) = (change(&pool.base.vault), change(&pool.quote.vault));
    let taker_side = match (base.signum(), quote.signum()) {
        (1, -1) => CoreSide::Sell,
        (-1, 1) => CoreSide::Buy,
        _ => return None,
    };
    let quantity = pool.base.units(base.abs());
    let price = pool.quote.units(quote.abs()) / quantity;
    let taker = *summary.account_keys.first()?;
    assert_eq!(
        (sequence >> 64) as u64,
        summary.slot,
        "a sequence starts with its slot"
    );
    Some(MarketFill {
        slot: summary.slot,
        instrument: InstrumentId::new(pool.instrument.to_string()),
        sequence,
        taker,
        maker: pool.address,
        taker_side,
        price,
        // The vaults record one average; the worst tick isn't named.
        worst_price: price,
        quantity,
        unix_timestamp: summary.block_time.map_or(0, |t| t.timestamp()),
        // The fee is in the price, which the vaults record fees included.
        taker_fee: Decimal::ZERO,
        maker_fee: Decimal::ZERO,
        maker_order: None,
    })
}

/// A swap as `swap_v2` takes it: what is paid, which amount is fixed, and
/// the threshold on the other that the line's limit sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapOrder {
    pub direction: Direction,
    pub mode: Mode,
    /// Atoms of the fixed side: paid for exact in, received for exact out.
    pub amount: u64,
    /// The least received (exact in) or most paid (exact out), in atoms of
    /// the other token: the limit price, rounded so no fill passes it.
    pub other_amount_threshold: u64,
}

impl SwapOrder {
    /// The WIT `swap` record on `pool`: buy (`bid`) or sell (`ask`) the
    /// base, `amount` of it (`unit` base) or of the quote, at no worse than
    /// `limit` quote units per base unit. An amount rounds down to whole
    /// atoms. A sell may set a limit of 0, any price; a buy can't.
    pub fn from_swap(
        pool: &Pool,
        side: Side,
        amount: f64,
        unit: AmountUnit,
        limit: f64,
    ) -> SeatResult<Self> {
        let (direction, mode) = match (side, unit) {
            (Side::Bid, AmountUnit::Base) => (Direction::QuoteIn, Mode::ExactOut),
            (Side::Bid, AmountUnit::Quote) => (Direction::QuoteIn, Mode::ExactIn),
            (Side::Ask, AmountUnit::Base) => (Direction::BaseIn, Mode::ExactIn),
            (Side::Ask, AmountUnit::Quote) => (Direction::BaseIn, Mode::ExactOut),
        };
        let token = match unit {
            AmountUnit::Base => &pool.base,
            AmountUnit::Quote => &pool.quote,
        };
        let amount = atoms(amount, token.asset.decimals).ok_or_else(|| {
            format!("a swap amount of {amount} is not a positive number of atoms")
        })?;
        let limit = Limit::parse(limit)?;
        if side == Side::Bid && limit.mantissa == 0 {
            return Err("a buy needs a positive limit".into());
        }
        let threshold = limit.threshold(
            direction,
            mode,
            amount,
            pool.base.asset.decimals,
            pool.quote.asset.decimals,
        )?;
        Ok(Self {
            direction,
            mode,
            amount,
            other_amount_threshold: threshold,
        })
    }

    /// Whether a swap filled as `quoted` meets the threshold.
    pub fn accepts(&self, quoted: &Quoted) -> bool {
        assert_eq!(
            (quoted.direction, quoted.mode),
            (self.direction, self.mode),
            "a quote of another swap"
        );
        match self.mode {
            Mode::ExactIn => quoted.amount_out >= self.other_amount_threshold,
            Mode::ExactOut => quoted.amount_in <= self.other_amount_threshold,
        }
    }
}

/// Whole atoms in `units` of a token with `decimals`, rounded down: `None`
/// for a non-finite or negative amount, nothing, or more than a `u64`.
fn atoms(units: f64, decimals: u8) -> Option<u64> {
    let units = exact_decimal(units)?;
    let scaled = units.checked_mul(Decimal::from(10u64.checked_pow(decimals.into())?))?;
    let mut whole = scaled.floor();
    whole.rescale(0);
    u64::try_from(whole.mantissa()).ok().filter(|&a| a > 0)
}

/// `value` as the decimal it prints as: the shortest text that reads back
/// as the same `f64`, so a limit of 150.1 is 150.1, not the binary
/// fraction nearest it.
fn exact_decimal(value: f64) -> Option<Decimal> {
    if !(value.is_finite() && value >= 0.0) {
        return None;
    }
    Decimal::from_str(&value.to_string()).ok()
}

/// A limit price as an exact fraction, `mantissa / 10^scale` quote units per
/// base unit, so a threshold rounds once, at the last atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Limit {
    mantissa: u128,
    scale: u32,
}

impl Limit {
    fn parse(limit: f64) -> SeatResult<Self> {
        let decimal = exact_decimal(limit)
            .ok_or_else(|| format!("a limit of {limit} is not a price"))?
            .normalize();
        let mantissa = u128::try_from(decimal.mantissa()).expect("not negative");
        Ok(Self {
            mantissa,
            scale: decimal.scale(),
        })
    }

    /// The other side's atoms at this price for `amount` atoms of the fixed
    /// side, rounded against the trader: up for what it must receive, down
    /// for what it may pay. With base `b` and quote `q` in atoms, the price
    /// is `q * 10^base_decimals / (b * 10^quote_decimals)`.
    fn threshold(
        self,
        direction: Direction,
        mode: Mode,
        amount: u64,
        base_decimals: u8,
        quote_decimals: u8,
    ) -> SeatResult<u64> {
        let overflow = || "the limit and the amount are too large to compare".to_string();
        let pow = |exponent: u32| 10u128.checked_pow(exponent).ok_or_else(overflow);
        let amount = u128::from(amount);
        // Quote atoms per base atom = numerator / denominator.
        let numerator = self
            .mantissa
            .checked_mul(pow(quote_decimals.into())?)
            .ok_or_else(overflow)?;
        let denominator = pow(self
            .scale
            .checked_add(base_decimals.into())
            .ok_or_else(overflow)?)?;
        let fixed_is_base = (direction == Direction::BaseIn) == (mode == Mode::ExactIn);
        let is_minimum = mode == Mode::ExactIn;
        let threshold = if fixed_is_base {
            // Quote atoms for `amount` base atoms.
            let quote = amount.checked_mul(numerator).ok_or_else(overflow)?;
            if is_minimum {
                quote.div_ceil(denominator)
            } else {
                quote / denominator
            }
        } else if numerator == 0 {
            // A sell at any price may pay any base for its quote.
            assert!(!is_minimum, "a buy's limit is positive");
            u128::from(u64::MAX)
        } else {
            // Base atoms for `amount` quote atoms.
            let base = amount.checked_mul(denominator).ok_or_else(overflow)?;
            if is_minimum {
                base.div_ceil(numerator)
            } else {
                base / numerator
            }
        };
        let threshold = u64::try_from(threshold).unwrap_or(u64::MAX);
        if !is_minimum && threshold == 0 {
            return Err("at its limit, the swap may pay nothing".into());
        }
        Ok(threshold)
    }
}

/// An account swapping through one pool, signed by its host, reading and
/// sending through the client it is given.
pub struct PoolSeat {
    market: String,
    pool: Pool,
    chain: Arc<dyn ChainClient>,
    signer: Keypair,
    /// Its chain pools sends (see [`SeatChain::pooled`]).
    pooled: bool,
    /// Lookup tables its swaps may use: a session's table of the pool's
    /// accounts, which a route needs to fit in a transaction.
    lookup_tables: Vec<AddressLookupTableAccount>,
}

impl PoolSeat {
    pub fn new(pool: Pool, chain: Arc<dyn ChainClient>, signer: Keypair) -> Self {
        Self {
            market: pool.instrument.to_string(),
            pool,
            chain,
            signer,
            pooled: false,
            lookup_tables: Vec::new(),
        }
    }

    /// This seat, its sends into a pool if `pooled` (see
    /// [`SeatChain::pooled`]).
    pub fn pooled(self, pooled: bool) -> Self {
        Self { pooled, ..self }
    }

    /// This seat, its swaps free to use `tables`.
    pub fn with_lookup_tables(self, tables: Vec<AddressLookupTableAccount>) -> Self {
        Self {
            lookup_tables: tables,
            ..self
        }
    }

    /// `swap_v2` for `order`, filled as `quoted` on `state`, after the
    /// owner's token accounts for both tokens.
    pub fn swap_instructions(
        &self,
        state: &State,
        order: &SwapOrder,
        quoted: &Quoted,
    ) -> Vec<Instruction> {
        self.swap_instructions_limited(state, order, quoted, 0)
    }

    /// The swap [`State::quote_to_price`] quoted: in at most what it takes,
    /// out at least what it gives, stopped by the program at `limit`.
    pub fn swap_to_price(&self, state: &State, quoted: &Quoted, limit: u128) -> Vec<Instruction> {
        assert_eq!(
            quoted.mode,
            Mode::ExactIn,
            "a swap to a price fixes what goes in"
        );
        let order = SwapOrder {
            direction: quoted.direction,
            mode: Mode::ExactIn,
            amount: quoted.amount_in,
            other_amount_threshold: quoted.amount_out,
        };
        self.swap_instructions_limited(state, &order, quoted, limit)
    }

    fn swap_instructions_limited(
        &self,
        state: &State,
        order: &SwapOrder,
        quoted: &Quoted,
        sqrt_price_limit_x64: u128,
    ) -> Vec<Instruction> {
        assert_eq!(
            (quoted.direction, quoted.mode, quoted.fixed()),
            (order.direction, order.mode, order.amount),
            "the quote is of this order"
        );
        let owner = self.signer.pubkey();
        let pool = &self.pool;
        match state {
            State::Whirlpool { pool: decoded, .. } => {
                let programs = whirlpool::ix::MintPrograms {
                    token_a: pool.base.token_program,
                    token_b: pool.quote.token_program,
                };
                let tick_arrays: [Pubkey; whirlpool::SWAP_TICK_ARRAYS_MAX] = quoted
                    .tick_arrays
                    .as_slice()
                    .try_into()
                    .expect("a Whirlpool quote names three arrays");
                let args = whirlpool::ix::SwapV2Args {
                    direction: whirlpool_direction(order.direction),
                    mode: whirlpool_mode(order.mode),
                    amount: order.amount,
                    other_amount_threshold: order.other_amount_threshold,
                    sqrt_price_limit_x64,
                };
                let mut out =
                    whirlpool::ix::create_token_accounts(&owner, &owner, decoded, &programs)
                        .to_vec();
                out.push(whirlpool::ix::swap_v2(
                    &owner,
                    &pool.address,
                    decoded,
                    &programs,
                    &tick_arrays,
                    &args,
                ));
                out
            }
            State::RaydiumClmm {
                pool: decoded,
                extension,
                ..
            } => {
                let programs = clmm::ix::MintPrograms {
                    token_0: pool.base.token_program,
                    token_1: pool.quote.token_program,
                };
                let extension_address = clmm::ix::bitmap_extension_address(&pool.address);
                let args = clmm::ix::SwapV2Args {
                    direction: clmm_direction(order.direction),
                    mode: clmm_mode(order.mode),
                    amount: order.amount,
                    other_amount_threshold: order.other_amount_threshold,
                    sqrt_price_limit_x64,
                };
                let mut out =
                    clmm::ix::create_token_accounts(&owner, &owner, decoded, &programs).to_vec();
                out.push(clmm::ix::swap_v2(
                    &owner,
                    &pool.address,
                    decoded,
                    &programs,
                    extension.is_some().then_some(&extension_address),
                    &quoted.tick_arrays,
                    &args,
                ));
                out
            }
        }
    }
}

#[async_trait]
impl Seat for PoolSeat {
    fn market(&self) -> &str {
        &self.market
    }

    fn executes(&self, action: &Action) -> bool {
        matches!(action, Action::Swap { .. })
    }

    async fn send(&self, planned: &Planned) -> Sent {
        let built = match self.instructions(planned).await {
            Ok(built) => built,
            Err(reason) => {
                return Sent {
                    state: SendState::NotSent(reason),
                    sent: false,
                };
            }
        };
        sent_on(
            self.chain.as_ref(),
            &self.signer,
            &built.plan(),
            self.pooled,
        )
        .await
    }

    async fn send_plan(&self, plan: &TransactionPlan) -> Sent {
        sent_on(self.chain.as_ref(), &self.signer, plan, self.pooled).await
    }

    /// The swap, quoted on the pool now and refused if that quote passes
    /// the limit; on chain, the limit's threshold refuses it if the pool
    /// moves past it first. In an atomic group, the pool the swap meets is
    /// the one the group's earlier lines leave, not the one quoted now: only
    /// the threshold on chain judges it. A line's reduce-only flag has
    /// nothing to set on a pool: the sheet's planner already cut the swap
    /// to the position.
    async fn instructions(&self, planned: &Planned) -> SeatResult<Built> {
        let &Action::Swap {
            side,
            amount,
            unit,
            limit,
        } = &planned.action
        else {
            return Err("a pool executes swaps only".into());
        };
        let order = SwapOrder::from_swap(&self.pool, side, amount, unit, limit)?;
        let state = State::read(self.chain.as_ref(), &self.pool)
            .await
            .map_err(|e| e.to_string())?;
        let quoted = state.quote(
            &self.pool.address,
            order.direction,
            order.mode,
            order.amount,
        )?;
        let grouped = planned.tx.is_some_and(|tx| tx.atomic.is_some());
        if !grouped && !order.accepts(&quoted) {
            let price = quoted.price(&self.pool).unwrap_or(f64::NAN);
            return Err(format!(
                "the pool fills it at {price}, past the limit {limit}"
            ));
        }
        let instructions = self.swap_instructions(&state, &order, &quoted);
        Ok(Built {
            instructions,
            lookup_tables: self.lookup_tables.clone(),
            compute_unit_limit: self.pool.program.compute_unit_limit(),
            label: format!("{} {:?}", self.market, planned.action),
        })
    }

    async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
        let state = State::read(self.chain.as_ref(), &self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(state.top(&self.pool))
    }

    /// The base the wallet holds, in base units.
    async fn position(&self) -> SeatResult<f64> {
        let atoms = token::owned_amount(
            self.chain.as_ref(),
            &self.signer.pubkey(),
            &self.pool.base.mint,
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(atoms as f64 / 10f64.powi(self.pool.base.asset.decimals.into()))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, io::Read as _};

    use base64::Engine as _;
    use mtx_kit::{
        Signature,
        chain::{TokenBalanceChange, mock::MockChainClient},
        token::TOKEN_PROGRAM_ID,
    };
    use solana_account::Account;

    use super::*;
    use crate::venue::MarketKind;

    /// After both fixture pools' open times; their fees are static, so the
    /// exact time doesn't matter (the kit's own tests use it).
    const TIMESTAMP_S: i64 = 1_763_700_000;
    const SOL_ATOMS: u64 = 1_000_000_000;
    const USDC_ATOMS: u64 = 1_000_000;

    #[derive(serde::Deserialize)]
    struct Fixture {
        pool: String,
        accounts: HashMap<String, FixtureAccount>,
    }

    #[derive(serde::Deserialize)]
    struct FixtureAccount {
        data: String,
    }

    /// A chain holding one of the kit's mainnet fixtures (the pool and its
    /// accounts, owned by `program`), both mints and the clock.
    fn chain_of(fixture: &str, program: Program) -> (Arc<MockChainClient>, InstrumentRef) {
        let path = format!(
            "{}/../kit/tests/fixtures/{fixture}/accounts.json.gz",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut json = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(path).expect("fixture"))
            .read_to_string(&mut json)
            .expect("gzip");
        let fixture: Fixture = serde_json::from_str(&json).expect("fixture json");
        let chain = MockChainClient::new();
        let owned = |owner: Pubkey, data: Vec<u8>| Account {
            lamports: 1_000_000_000,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        };
        for (key, account) in &fixture.accounts {
            let data = base64::engine::general_purpose::STANDARD
                .decode(&account.data)
                .expect("base64");
            chain.set_account(key.parse().expect("key"), owned(program.id(), data));
        }
        for (mint, decimals) in [(WRAPPED_SOL_MINT, 9), (USDC_MINT, 6)] {
            // An SPL mint: authority, supply, then decimals and the
            // initialized flag at 44 and 45.
            let mut data = vec![0u8; 82];
            data[44] = decimals;
            data[45] = 1;
            chain.set_account(mint, owned(TOKEN_PROGRAM_ID, data));
        }
        let mut clock = vec![0u8; 40];
        clock[32..40].copy_from_slice(&TIMESTAMP_S.to_le_bytes());
        chain.set_account(CLOCK_SYSVAR, owned(Pubkey::default(), clock));
        let instrument = InstrumentRef {
            venue: match program {
                Program::Whirlpool => "orca".into(),
                Program::RaydiumClmm => "raydium".into(),
            },
            kind: Kind::Amm,
            symbol: fixture.pool,
        };
        (Arc::new(chain), instrument)
    }

    async fn market_of(fixture: &str, program: Program) -> (Arc<MockChainClient>, PoolMarket) {
        let (chain, instrument) = chain_of(fixture, program);
        let venue = Amm::connect(
            &instrument.venue,
            chain.clone(),
            std::slice::from_ref(&instrument),
        )
        .await
        .expect("venue");
        assert_eq!(venue.instruments(), vec![instrument.clone()]);
        let pool = venue.pools()[0].clone();
        let market = PoolMarket {
            pool,
            chain: chain.clone(),
        };
        (chain, market)
    }

    const ORCA: (&str, Program) = ("orca-whirlpool-sol-usdc", Program::Whirlpool);
    const RAYDIUM: (&str, Program) = ("raydium-clmm-sol-usdc", Program::RaydiumClmm);

    #[tokio::test]
    async fn a_pool_reads_as_sol_against_usdc() {
        for (fixture, program) in [ORCA, RAYDIUM] {
            let (_, market) = market_of(fixture, program).await;
            let pool = market.pool();
            assert_eq!(pool.program, program);
            assert_eq!(
                (pool.base.mint, pool.quote.mint),
                (WRAPPED_SOL_MINT, USDC_MINT)
            );
            assert_eq!(
                (pool.base.asset.decimals, pool.quote.asset.decimals),
                (9, 6)
            );
            assert_eq!(market.info(true).kind, MarketKind::Amm);
        }
        // A venue offers its own program's pools only.
        let (chain, mut instrument) = chain_of(ORCA.0, ORCA.1);
        instrument.venue = "raydium".into();
        let refused = Amm::connect("raydium", chain, &[instrument]).await;
        assert!(refused.err().unwrap().to_string().contains("owned by"));
    }

    #[tokio::test]
    async fn the_top_is_a_small_swap_each_way_fees_included() {
        for (fixture, program) in [ORCA, RAYDIUM] {
            let (_, market) = market_of(fixture, program).await;
            let (bid, ask) = market.top().await.expect("top");
            let (bid, ask) = (bid.expect("a bid"), ask.expect("an ask"));
            assert!((50.0..1_000.0).contains(&ask), "{fixture}: ask {ask}");
            // Both pools charge 4 bp each way: a spread of about 8 bp.
            let spread_bps = (bid / ask - 1.0) * 10_000.0;
            assert!(
                (7.0..9.0).contains(&spread_bps),
                "{fixture}: {spread_bps} bp"
            );
        }
    }

    fn sol_usdc() -> Pool {
        let token = |mint: Pubkey, decimals| Token {
            mint,
            token_program: TOKEN_PROGRAM_ID,
            vault: Pubkey::new_unique(),
            asset: Asset::new(mint.to_string(), "", decimals).unwrap(),
        };
        Pool {
            instrument: InstrumentRef {
                venue: "orca".into(),
                kind: Kind::Amm,
                symbol: whirlpool::SOL_USDC_POOL.to_string(),
            },
            address: whirlpool::SOL_USDC_POOL,
            program: Program::Whirlpool,
            base: token(WRAPPED_SOL_MINT, 9),
            quote: token(USDC_MINT, 6),
        }
    }

    #[test]
    fn a_limit_is_the_threshold_rounded_against_the_trader() {
        let pool = sol_usdc();
        let order =
            |side, amount, unit, limit| SwapOrder::from_swap(&pool, side, amount, unit, limit);
        let cases = [
            // Sell 1.5 SOL for at least 1.5 x 150.25 USDC.
            (
                Side::Ask,
                1.5,
                AmountUnit::Base,
                150.25,
                Direction::BaseIn,
                Mode::ExactIn,
                1_500_000_000,
                225_375_000,
            ),
            // Buy 2 SOL for at most 2 x 150.1 USDC.
            (
                Side::Bid,
                2.0,
                AmountUnit::Base,
                150.1,
                Direction::QuoteIn,
                Mode::ExactOut,
                2 * SOL_ATOMS,
                300_200_000,
            ),
            // Pay 100 USDC for at least 100 / 150 SOL, rounded up.
            (
                Side::Bid,
                100.0,
                AmountUnit::Quote,
                150.0,
                Direction::QuoteIn,
                Mode::ExactIn,
                100 * USDC_ATOMS,
                666_666_667,
            ),
            // Get 100 USDC for at most 100 / 150 SOL, rounded down.
            (
                Side::Ask,
                100.0,
                AmountUnit::Quote,
                150.0,
                Direction::BaseIn,
                Mode::ExactOut,
                100 * USDC_ATOMS,
                666_666_666,
            ),
            // A sell at any price.
            (
                Side::Ask,
                1.0,
                AmountUnit::Base,
                0.0,
                Direction::BaseIn,
                Mode::ExactIn,
                SOL_ATOMS,
                0,
            ),
            (
                Side::Ask,
                1.0,
                AmountUnit::Quote,
                0.0,
                Direction::BaseIn,
                Mode::ExactOut,
                USDC_ATOMS,
                u64::MAX,
            ),
            // An amount rounds down to whole atoms.
            (
                Side::Ask,
                1.234_567_890_9,
                AmountUnit::Base,
                1.0,
                Direction::BaseIn,
                Mode::ExactIn,
                1_234_567_890,
                1_234_568,
            ),
        ];
        for (side, amount, unit, limit, direction, mode, atoms, threshold) in cases {
            assert_eq!(
                order(side, amount, unit, limit),
                Ok(SwapOrder {
                    direction,
                    mode,
                    amount: atoms,
                    other_amount_threshold: threshold
                }),
                "{side:?} {amount} {unit:?} at {limit}"
            );
        }
        for (side, amount, limit, refusal) in [
            (Side::Bid, 1.0, 0.0, "positive limit"),
            (Side::Bid, 1.0, f64::NAN, "not a price"),
            (Side::Ask, 1.0, -1.0, "not a price"),
            (Side::Ask, f64::NAN, 1.0, "atoms"),
            (Side::Ask, -1.0, 1.0, "atoms"),
            (Side::Ask, 1e-10, 1.0, "atoms"),
            (Side::Ask, 1e30, 1.0, "atoms"),
            // 1 atom of SOL for at most 10^-12 USDC per SOL: nothing.
            (Side::Bid, 1e-9, 1e-12, "pay nothing"),
        ] {
            let error = order(side, amount, AmountUnit::Base, limit).unwrap_err();
            assert!(error.contains(refusal), "{amount} at {limit}: {error}");
        }
    }

    fn planned(action: Action) -> Planned {
        Planned {
            line: 0,
            venue: None,
            client_id: None,
            action,
            reduce_only: false,
            expires_slot: None,
            tx: None,
        }
    }

    fn sell(amount: f64, limit: f64) -> Planned {
        planned(Action::Swap {
            side: Side::Ask,
            amount,
            unit: AmountUnit::Base,
            limit,
        })
    }

    /// The `swap_v2` data both programs share: discriminator, amount,
    /// threshold.
    fn amount_and_threshold(swap: &Instruction) -> (u64, u64) {
        let word = |at: usize| u64::from_le_bytes(swap.data[at..at + 8].try_into().unwrap());
        (word(8), word(16))
    }

    #[tokio::test]
    async fn a_seat_builds_the_swap_the_quote_and_the_limit_say() {
        for (fixture, program) in [ORCA, RAYDIUM] {
            let (chain, market) = market_of(fixture, program).await;
            let (_, ask) = market.top().await.unwrap();
            let ask = ask.unwrap();
            let seat = PoolSeat::new(market.pool().clone(), chain.clone(), Keypair::new());
            assert!(seat.executes(&sell(1.0, 0.0).action));
            assert!(!seat.executes(&Action::Deposit { amount: None }));

            let limit = (ask * 0.99 * 100.0).floor() / 100.0;
            let built = seat.instructions(&sell(2.0, limit)).await.expect("built");
            assert_eq!(
                built.instructions.len(),
                3,
                "{fixture}: two token accounts, the swap"
            );
            assert_eq!(built.compute_unit_limit, program.compute_unit_limit());
            let swap = &built.instructions[2];
            assert_eq!(swap.program_id, program.id());
            let order =
                SwapOrder::from_swap(market.pool(), Side::Ask, 2.0, AmountUnit::Base, limit)
                    .unwrap();
            assert_eq!(
                amount_and_threshold(swap),
                (2 * SOL_ATOMS, order.other_amount_threshold)
            );
            let state = market.state().await.unwrap();
            let quoted = state
                .quote(
                    &market.pool().address,
                    Direction::BaseIn,
                    Mode::ExactIn,
                    2 * SOL_ATOMS,
                )
                .unwrap();
            assert!(quoted.amount_out >= order.other_amount_threshold);
            for array in &quoted.tick_arrays {
                assert!(
                    swap.accounts
                        .iter()
                        .any(|a| a.pubkey == *array && a.is_writable)
                );
            }

            // A limit the pool can't meet isn't sent.
            let refused = seat.send(&sell(2.0, ask * 1.01)).await;
            assert!(!refused.sent);
            assert!(
                refused.state.reason().unwrap().contains("past the limit"),
                "{refused:?}"
            );
            assert!(chain.sent().is_empty());

            // One it can is simulated, then sent, as one transaction.
            let sent = seat.send(&sell(2.0, limit)).await;
            assert!(sent.sent, "{sent:?}");
            assert_eq!(sent.state, SendState::Delayed, "no receipt yet");
            assert_eq!(chain.simulated().len(), 1);
            assert_eq!(chain.sent().len(), 1);
            let tx = &chain.sent()[0];
            assert_eq!(
                tx.message.instructions().len(),
                4,
                "the compute budget first"
            );
        }
    }

    /// A swap to a price stops where the price is, whatever its cap: down
    /// selling base, up buying it; and a
    /// pooled seat reports its send as landed, with its signature, for the
    /// desk to follow.
    #[tokio::test]
    async fn a_swap_to_a_price_stops_there_and_a_pooled_seat_reports_its_signature() {
        for (fixture, program) in [ORCA, RAYDIUM] {
            let (chain, market) = market_of(fixture, program).await;
            let state = market.state().await.unwrap();
            let pool = market.pool();
            let (bid, ask) = state.top(pool);
            let mid = (bid.unwrap() + ask.unwrap()) / 2.0;
            let (down, limit) = state
                .quote_to_price(pool, mid * 0.999, 100_000 * 1_000_000_000)
                .unwrap()
                .expect("a move down");
            assert_eq!(down.direction, Direction::BaseIn);
            assert!(limit < state.sqrt_price_x64());
            let (capped, _) = state
                .quote_to_price(pool, mid * 0.999, 200_000 * 1_000_000_000)
                .unwrap()
                .unwrap();
            assert_eq!(
                capped.amount_in, down.amount_in,
                "the limit binds, not the cap"
            );
            let (up, _) = state
                .quote_to_price(pool, mid * 1.001, 100_000_000 * USDC_ATOMS)
                .unwrap()
                .unwrap();
            assert_eq!(up.direction, Direction::QuoteIn);
            assert!(state.quote_to_price(pool, -1.0, SOL_ATOMS).is_err());

            let signer = Keypair::new();
            let seat = PoolSeat::new(pool.clone(), chain.clone(), signer.insecure_clone());
            let swap = seat.swap_to_price(&state, &down, limit);
            assert!(!swap.is_empty());
            let pooled = PoolSeat::new(pool.clone(), chain.clone(), signer).pooled(true);
            let sent = pooled.send(&sell(1.0, mid * 0.9)).await;
            assert!(
                matches!(&sent.state, SendState::Landed(signature) if !signature.is_empty()),
                "{:?}",
                sent.state
            );
            assert!(sent.sent);
        }
    }

    #[tokio::test]
    async fn a_seat_refuses_what_a_pool_doesnt_do() {
        let (chain, market) = market_of(ORCA.0, ORCA.1).await;
        let seat = PoolSeat::new(market.pool().clone(), chain.clone(), Keypair::new());
        let deposit = seat
            .send(&planned(Action::Deposit { amount: Some(1.0) }))
            .await;
        assert!(deposit.state.reason().unwrap().contains("swaps only"));
        // More than three tick arrays away: the program would refuse it too.
        let huge = seat.send(&sell(10_000_000.0, 0.0)).await;
        assert!(!huge.sent, "{huge:?}");
        assert!(chain.simulated().is_empty());
    }

    fn summary(
        pool: &Pool,
        payer: Pubkey,
        base: (u64, u64),
        quote: (u64, u64),
    ) -> TransactionSummary {
        let change = |token: &Token, (pre_amount, post_amount)| TokenBalanceChange {
            account: token.vault,
            mint: token.mint,
            owner: Some(pool.address),
            pre_amount,
            post_amount,
        };
        TransactionSummary {
            signature: Signature::from([7u8; 64]),
            slot: 42,
            block_time: None,
            fee_lamports: 5_000,
            error: None,
            logs: Vec::new(),
            account_keys: vec![payer, pool.address],
            pre_balances: Vec::new(),
            post_balances: Vec::new(),
            token_balance_changes: vec![change(&pool.base, base), change(&pool.quote, quote)],
            inner_instructions: Vec::new(),
        }
    }

    #[test]
    fn a_fill_is_what_the_vaults_recorded() {
        let pool = sol_usdc();
        let payer = Pubkey::new_unique();
        // 2 SOL in, 300.5 USDC out: the payer sold at 150.25.
        let sold = fill_in(
            &pool,
            &summary(
                &pool,
                payer,
                (10 * SOL_ATOMS, 12 * SOL_ATOMS),
                (1_000 * USDC_ATOMS, 699_500_000),
            ),
            42 << 64,
        )
        .expect("a swap");
        assert_eq!(sold.taker_side, CoreSide::Sell);
        assert_eq!((sold.taker, sold.maker), (payer, pool.address));
        assert_eq!(sold.quantity, Decimal::from(2));
        assert_eq!(sold.price, Decimal::from_str("150.25").unwrap());
        assert_eq!(sold.worst_price, sold.price);
        assert_eq!(sold.slot, 42);
        let bought = fill_in(
            &pool,
            &summary(
                &pool,
                payer,
                (12 * SOL_ATOMS, 11 * SOL_ATOMS),
                (0, 151 * USDC_ATOMS),
            ),
            (42 << 64) | 1,
        )
        .expect("a swap");
        assert_eq!(
            (bought.taker_side, bought.price),
            (CoreSide::Buy, Decimal::from(151))
        );

        // Liquidity in both vaults, nothing moved, or a failed transaction:
        // no swap.
        let both = summary(&pool, payer, (0, SOL_ATOMS), (0, USDC_ATOMS));
        assert_eq!(fill_in(&pool, &both, 42 << 64), None);
        let still = summary(&pool, payer, (SOL_ATOMS, SOL_ATOMS), (0, USDC_ATOMS));
        assert_eq!(fill_in(&pool, &still, 42 << 64), None);
        let mut failed = summary(&pool, payer, (0, SOL_ATOMS), (USDC_ATOMS, 0));
        failed.error = Some(solana_transaction::TransactionError::AccountInUse);
        assert_eq!(fill_in(&pool, &failed, 42 << 64), None);
    }

    #[tokio::test]
    async fn fills_come_from_the_pools_transactions_in_block_order() {
        let (chain, market) = market_of(ORCA.0, ORCA.1).await;
        let owner = Pubkey::new_unique();
        let pool = market.pool().clone();
        // Two swaps in slot 10, a sell then a buy; the buy's signature
        // sorts first, so only the listing can order them.
        for (signature, base, quote) in [
            (9u8, (0, SOL_ATOMS), (150 * USDC_ATOMS, 0)),
            (1u8, (SOL_ATOMS, 0), (0, 151 * USDC_ATOMS)),
        ] {
            let mut landed = summary(&pool, owner, base, quote);
            landed.slot = 10;
            landed.signature = Signature::from([signature; 64]);
            chain.record_transaction(landed, &[pool.address]);
        }
        let fills = market.fills(5, &owner).await.unwrap();
        let sides: Vec<Option<CoreSide>> =
            fills.iter().map(|(fill, ids)| fill.side_for(ids)).collect();
        assert_eq!(sides, [Some(CoreSide::Sell), Some(CoreSide::Buy)]);
        assert_eq!(
            fills.iter().map(|(f, _)| f.sequence).collect::<Vec<_>>(),
            [10 << 64, (10 << 64) | 1]
        );
        assert_eq!(fills[0].0.instrument.0, pool.instrument.to_string());
        assert!(market.fills(10, &owner).await.unwrap().is_empty());
    }
}
