//! Orca as a [`Venue`] (`specs/venues/orca.md`): the Whirlpool pools its
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
        orca::whirlpool::{
            self as wp, PoolState, Position, TickArray,
            ix::{self, Liquidity, PoolTokens, Swap},
            math,
        },
    },
    token::{TOKEN_2022_PROGRAM_ID, associated_token_address},
};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};

use super::{
    Market, SeatResult, Venue, not_offered,
    pools::{
        Deposit, Lp, Pool, PoolAssets, PoolMarket, PoolPrice, SwapOrder, named, pool_assets, range,
        swap_atoms, units,
    },
};
use crate::{Error, Result, session::INTERNAL_RPC_TIMEOUT};

/// Where a mint's decimals sit, in a token mint of either program.
const MINT_DECIMALS: usize = 44;

/// Orca, read and traded through one endpoint.
pub struct Orca {
    id: VenueId,
    rpc_url: String,
}

impl Orca {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            id: VenueId::new(mtx_kit::protocols::orca::VENUE),
            rpc_url: rpc_url.into(),
        }
    }
}

#[async_trait]
impl Venue for Orca {
    fn id(&self) -> &VenueId {
        &self.id
    }

    fn instruments(&self) -> Vec<InstrumentRef> {
        wp::POOLS
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
        let Some(pool) = named(wp::POOLS, &instrument.symbol)
            .filter(|_| instrument.venue == self.id.0 && instrument.kind != Kind::Perp)
        else {
            return Err(not_offered(&self.id, instrument));
        };
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::with_timeout(
            &self.rpc_url,
            INTERNAL_RPC_TIMEOUT,
        ));
        let pool = WhirlpoolPool::connect(chain.as_ref(), pool).await?;
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

/// An Orca Whirlpool pool.
pub struct WhirlpoolPool {
    assets: PoolAssets,
    /// Whether the base is the pool's token A.
    base_is_a: bool,
    tokens: PoolTokens,
}

impl WhirlpoolPool {
    pub async fn connect(chain: &dyn ChainClient, named: &NamedMarket) -> Result<Self> {
        let state = PoolState::decode(
            named.address,
            &account(chain, &named.address, "orca pool").await?,
        )
        .map_err(read_error("orca pool"))?;
        let base_is_a = state.base_is_a(&named.base_mint);
        // Each mint's token program (its account's owner) and decimals.
        let mint = |mint: Pubkey| async move {
            let account = chain
                .get_account(&mint)
                .await
                .map_err(read_error("a pool's mint"))?
                .ok_or_else(|| Error::UnknownMarket(format!("mint {mint} doesn't exist")))?;
            let decimals = account
                .data
                .get(MINT_DECIMALS)
                .copied()
                .ok_or_else(|| Error::UnknownMarket(format!("{mint} is not a token mint")))?;
            Ok::<_, Error>((account.owner, decimals))
        };
        let ((base_program, base_decimals), (quote_program, quote_decimals)) =
            (mint(named.base_mint).await?, mint(named.quote_mint).await?);
        let (base, quote) = pool_assets(named, base_decimals, quote_decimals)?;
        let (base_vault, quote_vault) = if base_is_a {
            (state.vault_a, state.vault_b)
        } else {
            (state.vault_b, state.vault_a)
        };
        let tokens = if base_is_a {
            PoolTokens {
                program_a: base_program,
                program_b: quote_program,
            }
        } else {
            PoolTokens {
                program_a: quote_program,
                program_b: base_program,
            }
        };
        Ok(Self {
            assets: PoolAssets {
                address: named.address,
                base,
                quote,
                base_mint: named.base_mint,
                quote_mint: named.quote_mint,
                base_program,
                quote_program,
                base_vault,
                quote_vault,
            },
            base_is_a,
            tokens,
        })
    }

    async fn state(&self, chain: &dyn ChainClient) -> Result<PoolState> {
        PoolState::decode(
            self.assets.address,
            &account(chain, &self.assets.address, "orca pool").await?,
        )
        .map_err(read_error("orca pool"))
    }

    /// Token B per token A for `price` quote per base.
    fn price_b_per_a(&self, price: f64) -> f64 {
        if self.base_is_a { price } else { 1.0 / price }
    }

    /// A base-and-quote pair as token A and token B, or back.
    fn tokens_of<T>(&self, base: T, quote: T) -> (T, T) {
        if self.base_is_a {
            (base, quote)
        } else {
            (quote, base)
        }
    }

    /// The decimals of token A and token B.
    fn decimals(&self) -> (u8, u8) {
        self.tokens_of(self.assets.base.decimals, self.assets.quote.decimals)
    }

    async fn tick_array(&self, chain: &dyn ChainClient, start: i32) -> Option<TickArray> {
        let address = wp::tick_array_address(&self.assets.address, start);
        let account = chain.get_account(&address).await.ok()??;
        TickArray::decode(&account.data).ok()
    }

    async fn tick_array_exists(&self, chain: &dyn ChainClient, start: i32) -> bool {
        let address = wp::tick_array_address(&self.assets.address, start);
        matches!(chain.get_account(&address).await, Ok(Some(_)))
    }

    /// A position as the amm profile reads it: base and quote, fees, range.
    async fn read_position(
        &self,
        chain: &dyn ChainClient,
        state: &PoolState,
        position: &Position,
    ) -> Lp {
        let tick = |index| async move {
            let start = wp::tick_array_start(index, state.tick_spacing);
            self.tick_array(chain, start)
                .await
                .and_then(|a| a.tick(index, state.tick_spacing))
                .unwrap_or(wp::Tick {
                    tick: index,
                    ..wp::Tick::default()
                })
        };
        let (lower, upper) = (
            tick(position.tick_lower).await,
            tick(position.tick_upper).await,
        );
        let (fees_a, fees_b) = math::fees_owed(position, state, &lower, &upper);
        let (amount_a, amount_b) = math::position_amounts(position, state);
        let (base, quote) = self.tokens_of(amount_a, amount_b);
        let (fees_base, fees_quote) = self.tokens_of(fees_a as f64, fees_b as f64);
        let (base, quote) = units(&self.assets, base, quote);
        let (fees_base, fees_quote) = units(&self.assets, fees_base, fees_quote);
        let (decimals_a, decimals_b) = self.decimals();
        let at = |tick: i32| {
            math::sqrt_at_tick(tick).powi(2)
                * 10f64.powi(i32::from(decimals_a) - i32::from(decimals_b))
        };
        let (lo, hi) = (at(position.tick_lower), at(position.tick_upper));
        let range = if self.base_is_a {
            range(lo, hi)
        } else {
            range(1.0 / hi, 1.0 / lo)
        };
        Lp {
            id: position.position_mint,
            range,
            base,
            quote,
            fees_base,
            fees_quote,
            live: position.liquidity > 0,
        }
    }

    /// Whether `owner` holds `position`'s NFT, which makes the position its
    /// own.
    async fn holds_nft(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        position: &Position,
    ) -> bool {
        let nft = associated_token_address(owner, &position.position_mint, &TOKEN_2022_PROGRAM_ID);
        match chain.get_account(&nft).await {
            Ok(Some(account)) => {
                mtx_kit::chain::token::token_balance(&account).is_ok_and(|n| n > 0)
            }
            _ => false,
        }
    }

    async fn position_of(
        &self,
        chain: &dyn ChainClient,
        position_mint: &Pubkey,
    ) -> SeatResult<Position> {
        let address = wp::position_address(position_mint);
        let data = account(chain, &address, "orca position")
            .await
            .map_err(|e| e.to_string())?;
        Position::decode(address, &data).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl Pool for WhirlpoolPool {
    fn assets(&self) -> &PoolAssets {
        &self.assets
    }

    async fn price(&self, chain: &dyn ChainClient) -> Result<PoolPrice> {
        let state = self.state(chain).await?;
        let (a, b) = self.decimals();
        let price = state.price_b_per_a(a, b);
        Ok(PoolPrice {
            price: if self.base_is_a { price } else { 1.0 / price },
            fee: state.fee(),
        })
    }

    async fn stats(&self, chain: &dyn ChainClient) -> Result<Vec<(String, f64)>> {
        let state = self.state(chain).await?;
        let price = self.price(chain).await?;
        Ok(vec![
            ("price".into(), price.price),
            ("fee".into(), price.fee),
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
        let state = self.state(chain).await.map_err(|e| e.to_string())?;
        let atoms = swap_atoms(order, &self.assets)?;
        let a_to_b = atoms.base_in == self.base_is_a;
        let fee = state.fee();
        // A taker's swap stops at its limit, the fee counted, and takes no
        // minimum: the price it stops at is its protection.
        let (threshold, sqrt_price_limit_x64) = if order.partial {
            let stop = if atoms.base_in {
                order.limit / (1.0 - fee)
            } else {
                order.limit * (1.0 - fee)
            };
            let (a, b) = self.decimals();
            let limit = math::sqrt_x64_from_price(self.price_b_per_a(stop), a, b);
            let past = if a_to_b {
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
        Ok(vec![ix::swap_v2(
            &state,
            &self.tokens,
            owner,
            &Swap {
                a_to_b,
                amount: atoms.amount,
                other_amount_threshold: threshold,
                sqrt_price_limit_x64,
                is_input: atoms.exact_in,
            },
            &state.swap_tick_arrays(a_to_b),
        )])
    }

    async fn positions(&self, chain: &dyn ChainClient, owner: &Pubkey) -> Result<Vec<Lp>> {
        let state = self.state(chain).await?;
        let mut accounts = chain
            .get_program_accounts(
                &wp::PROGRAM_ID,
                vec![
                    RpcFilterType::DataSize(Position::LEN as u64),
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                        8,
                        self.assets.address.as_ref(),
                    )),
                ],
            )
            .await
            .map_err(read_error("orca positions"))?;
        accounts.sort_by_key(|(address, _)| *address);
        let mut held = Vec::new();
        for (address, account) in accounts {
            let Ok(position) = Position::decode(address, &account.data) else {
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
        let Ok(held) = self.position_of(chain, id).await else {
            return Ok(None);
        };
        if held.pool != self.assets.address || !self.holds_nft(chain, owner, &held).await {
            return Ok(None);
        }
        let state = self.state(chain).await?;
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
        let state = self.state(chain).await.map_err(|e| e.to_string())?;
        let existing = match position {
            Some(position_mint) => Some(self.position_of(chain, position_mint).await?),
            None => None,
        };
        let spacing = state.tick_spacing;
        let (decimals_a, decimals_b) = self.decimals();
        let (tick_lower, tick_upper) = match (&existing, deposit.range) {
            (Some(p), _) => (p.tick_lower, p.tick_upper),
            (None, Some(r)) => {
                let (a, b) = (self.price_b_per_a(r.lower), self.price_b_per_a(r.upper));
                let (lo, hi) = (a.min(b), a.max(b));
                let lower = math::tick_at_price(lo, decimals_a, decimals_b, spacing);
                let upper = math::tick_at_price(hi, decimals_a, decimals_b, spacing);
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
        let (amount_a, amount_b) = self.tokens_of(base_atoms, quote_atoms);
        let current = state.sqrt_price_x64 as f64 / 18_446_744_073_709_551_616.0;
        let (lower, upper) = (
            math::sqrt_at_tick(tick_lower),
            math::sqrt_at_tick(tick_upper),
        );
        // The token whose amount buys less liquidity sets it; the program
        // takes what that liquidity needs of the other, up to its amount.
        let liquidity = match math::liquidity_for_amounts(amount_a, amount_b, current, lower, upper)
        {
            (Some(a), Some(b)) => a.min(b),
            (Some(l), None) | (None, Some(l)) => l,
            (None, None) => 0.0,
        };
        // A hair under, so rounding what it costs up never passes the maxima.
        let liquidity = (liquidity * (1.0 - 1e-6)).floor();
        if liquidity.is_nan() || liquidity < 1.0 {
            return Err("this range at this price takes the other asset, or too little".into());
        }
        let as_u64 = |x: f64| x.min(u64::MAX as f64) as u64;
        let held = Position {
            address: wp::position_address(new),
            pool: self.assets.address,
            position_mint: *new,
            liquidity: 0,
            tick_lower,
            tick_upper,
            fee_growth_checkpoint_a_x64: 0,
            fee_growth_checkpoint_b_x64: 0,
            fee_owed_a: 0,
            fee_owed_b: 0,
        };
        let target = existing.as_ref().unwrap_or(&held);
        let mut instructions = Vec::new();
        if existing.is_none() {
            // A position's two tick arrays must exist; the program makes
            // none. A range in one array names it once.
            let mut starts = vec![
                wp::tick_array_start(tick_lower, spacing),
                wp::tick_array_start(tick_upper, spacing),
            ];
            starts.dedup();
            for start in starts {
                if !self.tick_array_exists(chain, start).await {
                    instructions.push(ix::initialize_tick_array(&state, owner, start));
                }
            }
            instructions.push(ix::open_position(
                &state, owner, new, tick_lower, tick_upper,
            ));
        }
        instructions.push(ix::increase_liquidity(
            &state,
            &self.tokens,
            owner,
            target,
            &Liquidity {
                liquidity: liquidity as u128,
                token_a: as_u64(amount_a),
                token_b: as_u64(amount_b),
            },
        ));
        Ok((instructions, existing.is_none()))
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
        let state = self.state(chain).await.map_err(|e| e.to_string())?;
        let held = self.position_of(chain, &position.id).await?;
        let liquidity = if share >= 1.0 {
            held.liquidity
        } else {
            (held.liquidity as f64 * share).floor() as u128
        };
        // The program pays the liquidity out in one instruction and the
        // fees in another, so a minimum counting both is the liquidity's
        // less the fees already earned, which only grow.
        let atoms = |units: Option<f64>, fees: f64, decimals: u8| {
            let scale = 10f64.powi(i32::from(decimals));
            units.map_or(0, |u| {
                ((u.max(0.0) - fees.max(0.0)).max(0.0) * scale).floor() as u64
            })
        };
        let (min_a, min_b) = self.tokens_of(
            atoms(min_base, position.fees_base, self.assets.base.decimals),
            atoms(min_quote, position.fees_quote, self.assets.quote.decimals),
        );
        let mut instructions = vec![ix::update_fees_and_rewards(&state, owner, &held)];
        if liquidity > 0 {
            instructions.push(ix::decrease_liquidity(
                &state,
                &self.tokens,
                owner,
                &held,
                &Liquidity {
                    liquidity,
                    token_a: min_a,
                    token_b: min_b,
                },
            ));
        }
        instructions.push(ix::collect_fees(&state, &self.tokens, owner, &held));
        Ok(instructions)
    }
}
