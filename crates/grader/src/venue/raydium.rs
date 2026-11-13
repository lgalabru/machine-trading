//! Raydium as a [`Venue`] (`specs/venues/raydium.md`): the CLMM pools its
//! profile names, each a spot and an amm instrument ([`super::pools`]).

use std::sync::Arc;

use async_trait::async_trait;
use mtx_core::{
    VenueId,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    Instruction,
    chain::{ChainClient, RpcChainClient},
    protocols::{
        NamedMarket,
        raydium::clmm::{
            self, AmmConfig, PersonalPosition, PoolState, TickArray,
            ix::{self, OpenPosition, PoolTokens, Swap},
            math,
        },
    },
    token::associated_token_address,
};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};

use super::{
    Market, SeatResult, Venue, not_offered,
    pools::{
        Deposit, Lp, Pool, PoolAssets, PoolMarket, PoolPrice, SwapOrder, named, pool_assets, range,
        swap_atoms, units, wallet_atoms,
    },
};
use crate::{Error, Result, session::INTERNAL_RPC_TIMEOUT};

/// Tick arrays a swap may cross: the current one and two beyond.
const SWAP_TICK_ARRAYS: usize = 3;

/// Raydium, read and traded through one endpoint.
pub struct Raydium {
    id: VenueId,
    rpc_url: String,
}

impl Raydium {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            id: VenueId::new(mtx_kit::protocols::raydium::VENUE),
            rpc_url: rpc_url.into(),
        }
    }
}

#[async_trait]
impl Venue for Raydium {
    fn id(&self) -> &VenueId {
        &self.id
    }

    fn instruments(&self) -> Vec<InstrumentRef> {
        clmm::POOLS
            .iter()
            .flat_map(|p| {
                [Kind::Spot, Kind::Amm].map(|kind| InstrumentRef {
                    venue: self.id.0.clone(),
                    kind,
                    symbol: p.symbol.into(),
                })
            })
            .collect()
    }

    async fn market(&self, instrument: &InstrumentRef) -> Result<Box<dyn Market>> {
        let Some(pool) = named(clmm::POOLS, &instrument.symbol)
            .filter(|_| instrument.venue == self.id.0 && instrument.kind != Kind::Perp)
        else {
            return Err(not_offered(&self.id, instrument));
        };
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::with_timeout(
            &self.rpc_url,
            INTERNAL_RPC_TIMEOUT,
        ));
        let pool = ClmmPool::connect(chain.as_ref(), pool).await?;
        Ok(Box::new(PoolMarket::new(
            instrument.clone(),
            Arc::new(pool),
            chain,
        )))
    }
}

fn read_error<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> Error {
    move |error| Error::Transaction {
        label: what.into(),
        reason: error.to_string(),
    }
}

async fn account(chain: &dyn ChainClient, address: &Pubkey, what: &'static str) -> Result<Vec<u8>> {
    Ok(chain
        .get_account(address)
        .await
        .map_err(read_error(what))?
        .ok_or_else(|| Error::UnknownMarket(format!("{what} {address} doesn't exist")))?
        .data)
}

/// A Raydium CLMM pool.
pub struct ClmmPool {
    assets: PoolAssets,
    /// Whether the base is the pool's token 0.
    base_is_0: bool,
    tokens: PoolTokens,
}

impl ClmmPool {
    pub async fn connect(chain: &dyn ChainClient, named: &NamedMarket) -> Result<Self> {
        let state = PoolState::decode(
            named.address,
            &account(chain, &named.address, "raydium pool").await?,
        )
        .map_err(read_error("raydium pool"))?;
        let base_is_0 = state.base_is_0(&named.base_mint);
        let (base_vault, quote_vault, base_decimals, quote_decimals) = if base_is_0 {
            (
                state.vault_0,
                state.vault_1,
                state.decimals_0,
                state.decimals_1,
            )
        } else {
            (
                state.vault_1,
                state.vault_0,
                state.decimals_1,
                state.decimals_0,
            )
        };
        let program = |mint: Pubkey| async move {
            chain
                .get_account(&mint)
                .await
                .map_err(read_error("a pool's mint"))?
                .map(|a| a.owner)
                .ok_or_else(|| Error::UnknownMarket(format!("mint {mint} doesn't exist")))
        };
        let (base_program, quote_program) = (
            program(named.base_mint).await?,
            program(named.quote_mint).await?,
        );
        let (base, quote) = pool_assets(named, base_decimals, quote_decimals)?;
        let tokens = if base_is_0 {
            PoolTokens {
                program_0: base_program,
                program_1: quote_program,
            }
        } else {
            PoolTokens {
                program_0: quote_program,
                program_1: base_program,
            }
        };
        Ok(Self {
            assets: PoolAssets {
                address: named.address,
                program: clmm::PROGRAM_ID,
                base,
                quote,
                base_mint: named.base_mint,
                quote_mint: named.quote_mint,
                base_program,
                quote_program,
                base_vault,
                quote_vault,
            },
            base_is_0,
            tokens,
        })
    }

    async fn state(&self, chain: &dyn ChainClient) -> Result<(PoolState, AmmConfig)> {
        let state = PoolState::decode(
            self.assets.address,
            &account(chain, &self.assets.address, "raydium pool").await?,
        )
        .map_err(read_error("raydium pool"))?;
        let config =
            AmmConfig::decode(&account(chain, &state.amm_config, "raydium fee tier").await?)
                .map_err(read_error("raydium fee tier"))?;
        Ok((state, config))
    }

    /// Token 1 per token 0 for `price` quote per base.
    fn price_1_per_0(&self, price: f64) -> f64 {
        if self.base_is_0 { price } else { 1.0 / price }
    }

    /// A base-and-quote pair as token 0 and token 1, or back.
    fn tokens_of<T>(&self, base: T, quote: T) -> (T, T) {
        if self.base_is_0 {
            (base, quote)
        } else {
            (quote, base)
        }
    }

    async fn bitmap_extension(&self, chain: &dyn ChainClient) -> Option<Pubkey> {
        let address = clmm::bitmap_extension_address(&self.assets.address);
        matches!(chain.get_account(&address).await, Ok(Some(_))).then_some(address)
    }

    async fn tick_array(&self, chain: &dyn ChainClient, start: i32) -> Option<TickArray> {
        let address = clmm::tick_array_address(&self.assets.address, start);
        let account = chain.get_account(&address).await.ok()??;
        TickArray::decode(&account.data).ok()
    }

    /// A position as the amm profile reads it: base and quote, fees, range.
    async fn read_position(
        &self,
        chain: &dyn ChainClient,
        state: &PoolState,
        position: &PersonalPosition,
    ) -> Lp {
        let tick = |index| async move {
            let start = clmm::tick_array_start(index, state.tick_spacing);
            self.tick_array(chain, start)
                .await
                .and_then(|a| a.tick(index, state.tick_spacing))
                .unwrap_or(clmm::Tick {
                    tick: index,
                    ..clmm::Tick::default()
                })
        };
        let (lower, upper) = (
            tick(position.tick_lower).await,
            tick(position.tick_upper).await,
        );
        let (fees_0, fees_1) = math::fees_owed(position, state, &lower, &upper);
        let (amount_0, amount_1) = math::position_amounts(position, state);
        let (base, quote) = self.tokens_of(amount_0, amount_1);
        let (fees_base, fees_quote) = self.tokens_of(fees_0 as f64, fees_1 as f64);
        let (base, quote) = units(&self.assets, base, quote);
        let (fees_base, fees_quote) = units(&self.assets, fees_base, fees_quote);
        let at = |tick: i32| {
            math::sqrt_at_tick(tick).powi(2)
                * 10f64.powi(i32::from(state.decimals_0) - i32::from(state.decimals_1))
        };
        let (lo, hi) = (at(position.tick_lower), at(position.tick_upper));
        let range = if self.base_is_0 {
            range(lo, hi)
        } else {
            range(1.0 / hi, 1.0 / lo)
        };
        Lp {
            id: position.nft_mint,
            range,
            base,
            quote,
            fees_base,
            fees_quote,
            live: position.liquidity > 0,
        }
    }

    /// Whether `owner` holds `position`'s NFT, which makes the position its
    /// own: in any of its token accounts for the NFT's mint, as the wallet
    /// is read, not only the associated one. A mint the chain can't read
    /// is held by no one.
    async fn holds_nft(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        position: &PersonalPosition,
    ) -> bool {
        wallet_atoms(chain, owner, &position.nft_mint)
            .await
            .is_ok_and(|atoms| atoms > 0)
    }

    async fn personal_position(
        &self,
        chain: &dyn ChainClient,
        nft_mint: &Pubkey,
    ) -> SeatResult<PersonalPosition> {
        let address = clmm::personal_position_address(nft_mint);
        let data = account(chain, &address, "raydium position")
            .await
            .map_err(|e| e.to_string())?;
        PersonalPosition::decode(address, &data).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl Pool for ClmmPool {
    fn assets(&self) -> &PoolAssets {
        &self.assets
    }

    async fn price(&self, chain: &dyn ChainClient) -> Result<PoolPrice> {
        let (state, config) = self.state(chain).await?;
        Ok(PoolPrice {
            price: state.price(&self.assets.base_mint),
            fee: config.fee(),
        })
    }

    async fn stats(&self, chain: &dyn ChainClient) -> Result<Vec<(String, f64)>> {
        let (state, config) = self.state(chain).await?;
        Ok(vec![
            ("price".into(), state.price(&self.assets.base_mint)),
            ("fee".into(), config.fee()),
            ("liquidity".into(), state.liquidity as f64),
            ("tick".into(), f64::from(state.tick_current)),
            ("tick_spacing".into(), f64::from(state.tick_spacing)),
        ])
    }

    async fn swap(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        order: &SwapOrder,
    ) -> SeatResult<Vec<Instruction>> {
        let (state, config) = self.state(chain).await.map_err(|e| e.to_string())?;
        if state.status & (1 << 4) != 0 {
            return Err("the pool's swaps are off".into());
        }
        let atoms = swap_atoms(order, &self.assets)?;
        let zero_for_one = atoms.base_in == self.base_is_0;
        let fee = config.fee();
        // A taker's swap stops at its limit, the fee counted, and takes no
        // minimum: the price it stops at is its protection.
        let (threshold, sqrt_price_limit_x64) = if order.partial {
            let stop = if atoms.base_in {
                order.limit / (1.0 - fee)
            } else {
                order.limit * (1.0 - fee)
            };
            let limit = math::sqrt_x64_from_price(
                self.price_1_per_0(stop),
                state.decimals_0,
                state.decimals_1,
            );
            let past = if zero_for_one {
                limit >= state.sqrt_price_x64
            } else {
                limit <= state.sqrt_price_x64
            };
            if past {
                return Err("the pool's price is already past the limit".into());
            }
            (if atoms.exact_in { 0 } else { u64::MAX }, limit)
        } else {
            (atoms.threshold, 0)
        };
        let tick_arrays = state.swap_tick_arrays(zero_for_one, SWAP_TICK_ARRAYS);
        if tick_arrays.is_empty() {
            return Err("no liquidity that way".into());
        }
        Ok(vec![ix::swap_v2(
            &state,
            &self.tokens,
            owner,
            &Swap {
                zero_for_one,
                amount: atoms.amount,
                other_amount_threshold: threshold,
                sqrt_price_limit_x64,
                is_base_input: atoms.exact_in,
            },
            &tick_arrays,
            self.bitmap_extension(chain).await,
        )])
    }

    async fn positions(&self, chain: &dyn ChainClient, owner: &Pubkey) -> Result<Vec<Lp>> {
        let (state, _) = self.state(chain).await?;
        let mut accounts = chain
            .get_program_accounts(
                &clmm::PROGRAM_ID,
                vec![
                    RpcFilterType::DataSize(PersonalPosition::LEN as u64),
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                        41,
                        self.assets.address.as_ref(),
                    )),
                ],
            )
            .await
            .map_err(read_error("raydium positions"))?;
        accounts.sort_by_key(|(address, _)| *address);
        let mut held = Vec::new();
        for (address, account) in accounts {
            let Ok(position) = PersonalPosition::decode(address, &account.data) else {
                continue;
            };
            if !self.holds_nft(chain, owner, &position).await {
                continue;
            }
            let lp = self.read_position(chain, &state, &position).await;
            if lp.live || lp.fees_base > 0.0 || lp.fees_quote > 0.0 {
                held.push(lp);
            }
        }
        Ok(held)
    }

    async fn position(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        id: &Pubkey,
    ) -> Result<Option<Lp>> {
        let Ok(held) = self.personal_position(chain, id).await else {
            return Ok(None);
        };
        if held.pool != self.assets.address || !self.holds_nft(chain, owner, &held).await {
            return Ok(None);
        }
        let (state, _) = self.state(chain).await?;
        Ok(Some(self.read_position(chain, &state, &held).await))
    }

    async fn deposit(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        position: Option<&Pubkey>,
        new: &Pubkey,
        deposit: &Deposit,
    ) -> SeatResult<(Vec<Instruction>, bool)> {
        let (state, _) = self.state(chain).await.map_err(|e| e.to_string())?;
        if state.status & 1 != 0 {
            return Err("the pool takes no liquidity".into());
        }
        let existing = match position {
            Some(nft_mint) => Some(self.personal_position(chain, nft_mint).await?),
            None => None,
        };
        let spacing = state.tick_spacing;
        let (tick_lower, tick_upper) = match (&existing, deposit.range) {
            (Some(p), _) => (p.tick_lower, p.tick_upper),
            (None, Some(r)) => {
                let (a, b) = (self.price_1_per_0(r.lower), self.price_1_per_0(r.upper));
                let (lo, hi) = (a.min(b), a.max(b));
                let lower = math::tick_at_price(lo, state.decimals_0, state.decimals_1, spacing);
                let upper = math::tick_at_price(hi, state.decimals_0, state.decimals_1, spacing);
                (lower, upper.max(lower + i32::from(spacing)))
            }
            (None, None) => {
                let s = i32::from(spacing);
                (
                    math::MIN_TICK.div_euclid(s) * s + s,
                    math::MAX_TICK.div_euclid(s) * s,
                )
            }
        };
        let decimals = |d: u8| 10f64.powi(i32::from(d));
        let (base_atoms, quote_atoms) = (
            (deposit.base * decimals(self.assets.base.decimals)).floor(),
            (deposit.quote * decimals(self.assets.quote.decimals)).floor(),
        );
        let (amount_0, amount_1) = self.tokens_of(base_atoms, quote_atoms);
        let current = state.sqrt_price_x64 as f64 / 18_446_744_073_709_551_616.0;
        let (lower, upper) = (
            math::sqrt_at_tick(tick_lower),
            math::sqrt_at_tick(tick_upper),
        );
        // The token whose amount buys less liquidity sets it; the program
        // takes what that liquidity needs of the other, up to its amount.
        let base_0 = match math::liquidity_for_amounts(amount_0, amount_1, current, lower, upper) {
            (Some(l0), Some(l1)) => l0 <= l1,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if (if base_0 { amount_0 } else { amount_1 }) < 1.0 {
            return Err("this range at this price takes the other asset: none given".into());
        }
        let as_u64 = |x: f64| x.min(u64::MAX as f64) as u64;
        let ix = match &existing {
            Some(p) => ix::increase_liquidity(
                &state,
                &self.tokens,
                owner,
                p,
                as_u64(amount_0),
                as_u64(amount_1),
                base_0,
            ),
            None => ix::open_position(
                &state,
                &self.tokens,
                owner,
                new,
                &OpenPosition {
                    tick_lower,
                    tick_upper,
                    amount_0_max: as_u64(amount_0),
                    amount_1_max: as_u64(amount_1),
                    base_0,
                },
            ),
        };
        Ok((vec![ix], existing.is_none()))
    }

    async fn withdraw(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        position: &Lp,
        share: f64,
        min_base: Option<f64>,
        min_quote: Option<f64>,
    ) -> SeatResult<Vec<Instruction>> {
        let (state, _) = self.state(chain).await.map_err(|e| e.to_string())?;
        let held = self.personal_position(chain, &position.id).await?;
        let liquidity = if share >= 1.0 {
            held.liquidity
        } else {
            (held.liquidity as f64 * share).floor() as u128
        };
        let atoms = |units: Option<f64>, decimals: u8| {
            units.map_or(0, |u| {
                (u.max(0.0) * 10f64.powi(i32::from(decimals))).floor() as u64
            })
        };
        let (min_0, min_1) = self.tokens_of(
            atoms(min_base, self.assets.base.decimals),
            atoms(min_quote, self.assets.quote.decimals),
        );
        // Each reward the pool was given: its vault, the owner's account
        // for it (made if it has none), its mint.
        let mut instructions = Vec::new();
        let mut rewards = Vec::new();
        for (mint, vault) in &state.rewards {
            let program = match chain.get_account(mint).await {
                Ok(Some(account)) => account.owner,
                _ => mtx_kit::token::TOKEN_PROGRAM_ID,
            };
            let account = associated_token_address(owner, mint, &program);
            if !matches!(chain.get_account(&account).await, Ok(Some(_))) {
                instructions.push(mtx_kit::chain::token::create_ata_idempotent_for(
                    owner, owner, mint, &program,
                ));
            }
            rewards.extend([*vault, account, *mint]);
        }
        instructions.push(ix::decrease_liquidity(
            &state,
            &self.tokens,
            owner,
            &held,
            liquidity,
            min_0,
            min_1,
            &rewards,
        ));
        Ok(instructions)
    }
}
