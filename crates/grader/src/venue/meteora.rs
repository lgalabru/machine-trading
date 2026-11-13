//! Meteora as a [`Venue`] (`specs/venues/meteora.md`): the DLMM pools its
//! profile names, each a spot and an amm instrument ([`super::pools`]).

use std::sync::Arc;

use async_trait::async_trait;
use mtx_core::{
    VenueId,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    Instruction, LiquidityShape,
    chain::{ChainClient, RpcChainClient},
    protocols::{
        NamedMarket,
        meteora::dlmm::{
            self, BINS_PER_ARRAY, BinArray, LbPair, Position,
            ix::{self, Strategy, Swap},
            math,
        },
    },
};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};

use super::{
    Market, SeatResult, Venue, not_offered,
    pools::{
        Allowance, Allowed, Deposit, Lp, Pool, PoolAssets, PoolMarket, PoolPrice, SwapOrder, Use,
        anchor_instruction, named, pool_assets, range, swap_atoms, units,
    },
};
use crate::{Error, Result, session::INTERNAL_RPC_TIMEOUT};

/// What a bot may run of Meteora DLMM's instructions (`specs/venues/meteora.md`, "What
/// a bot may run"): swaps, and positions in the session's pool, each with
/// the slot of the pool's account among its accounts (from its IDL).
const ALLOWED: Allowed = &[
    ("swap", Use::Swap, Some(0)),
    ("swap2", Use::Swap, Some(0)),
    ("swap_exact_out", Use::Swap, Some(0)),
    ("swap_exact_out2", Use::Swap, Some(0)),
    ("swap_with_price_impact", Use::Swap, Some(0)),
    ("swap_with_price_impact2", Use::Swap, Some(0)),
    ("initialize_position", Use::Liquidity, Some(2)),
    ("initialize_position2", Use::Liquidity, Some(2)),
    ("initialize_position_pda", Use::Liquidity, Some(3)),
    ("initialize_bin_array", Use::Liquidity, Some(0)),
    ("add_liquidity", Use::Liquidity, Some(1)),
    ("add_liquidity2", Use::Liquidity, Some(1)),
    ("add_liquidity_by_strategy", Use::Liquidity, Some(1)),
    ("add_liquidity_by_strategy2", Use::Liquidity, Some(1)),
    (
        "add_liquidity_by_strategy_one_side",
        Use::Liquidity,
        Some(1),
    ),
    ("add_liquidity_by_weight", Use::Liquidity, Some(1)),
    ("add_liquidity_by_weight2", Use::Liquidity, Some(1)),
    ("add_liquidity_one_side", Use::Liquidity, Some(1)),
    ("add_liquidity_one_side_precise", Use::Liquidity, Some(1)),
    ("add_liquidity_one_side_precise2", Use::Liquidity, Some(1)),
    ("remove_liquidity", Use::Liquidity, Some(1)),
    ("remove_liquidity2", Use::Liquidity, Some(1)),
    ("remove_liquidity_by_range", Use::Liquidity, Some(1)),
    ("remove_liquidity_by_range2", Use::Liquidity, Some(1)),
    ("remove_all_liquidity", Use::Liquidity, Some(1)),
    ("claim_fee", Use::Liquidity, Some(0)),
    ("claim_fee2", Use::Liquidity, Some(0)),
    ("claim_reward", Use::Liquidity, Some(0)),
    ("claim_reward2", Use::Liquidity, Some(0)),
    ("update_fees_and_rewards", Use::Liquidity, Some(1)),
    ("update_fees_and_reward2", Use::Liquidity, Some(1)),
    ("close_position", Use::Position, None),
    ("close_position2", Use::Position, None),
    ("close_position_if_empty", Use::Position, None),
    ("increase_position_length", Use::Position, None),
    ("increase_position_length2", Use::Position, None),
    ("decrease_position_length", Use::Position, None),
];
/// The program's other instructions, by name, as the pinned program's IDL lists them.
const OTHERS: &[&str] = &[
    "cancel_limit_order",
    "close_bin_array",
    "close_claim_fee_operator_account",
    "close_limit_order_if_empty",
    "close_operator_account",
    "close_preset_parameter",
    "close_preset_parameter2",
    "close_token_badge",
    "create_operator_account",
    "fund_reward",
    "go_to_a_bin",
    "increase_oracle_length",
    "initialize_bin_array_bitmap_extension",
    "initialize_customizable_permissionless_lb_pair",
    "initialize_customizable_permissionless_lb_pair2",
    "initialize_lb_pair",
    "initialize_lb_pair2",
    "initialize_permission_lb_pair",
    "initialize_position_by_operator",
    "initialize_preset_parameter",
    "initialize_reward",
    "initialize_token_badge",
    "place_limit_order",
    "rebalance_liquidity",
    "set_activation_point",
    "set_pair_status",
    "set_pair_status_permissionless",
    "set_permissionless_operation_bits",
    "set_pre_activation_duration",
    "set_pre_activation_swap_address",
    "update_base_fee_parameters",
    "update_dynamic_fee_parameters",
    "update_position_operator",
    "update_reward_duration",
    "update_reward_funder",
    "withdraw_ineligible_reward",
    "withdraw_protocol_fee",
    "zap_protocol_fee",
];

/// Bin arrays a swap may cross: the active one and two beyond.
const SWAP_BIN_ARRAYS: usize = 3;
/// How far the active bin may move between a deposit's planning and its
/// landing.
const MAX_ACTIVE_BIN_SLIPPAGE: i32 = BINS_PER_ARRAY;
/// The most accounts one `getMultipleAccounts` reads.
const MAX_ACCOUNTS_PER_READ: usize = 100;

/// Meteora, read and traded through one endpoint.
pub struct Meteora {
    id: VenueId,
    rpc_url: String,
}

impl Meteora {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            id: VenueId::new(mtx_kit::protocols::meteora::VENUE),
            rpc_url: rpc_url.into(),
        }
    }
}

#[async_trait]
impl Venue for Meteora {
    fn id(&self) -> &VenueId {
        &self.id
    }

    fn instruments(&self) -> Vec<InstrumentRef> {
        dlmm::POOLS
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
        let Some(pool) = named(dlmm::POOLS, &instrument.symbol)
            .filter(|_| instrument.venue == self.id.0 && instrument.kind != Kind::Perp)
        else {
            return Err(not_offered(&self.id, instrument));
        };
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::with_timeout(
            &self.rpc_url,
            INTERNAL_RPC_TIMEOUT,
        ));
        let pool = DlmmPool::connect(chain.as_ref(), pool).await?;
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

/// A Meteora DLMM pool whose X is the base and Y the quote.
pub struct DlmmPool {
    assets: PoolAssets,
}

impl DlmmPool {
    pub async fn connect(chain: &dyn ChainClient, named: &NamedMarket) -> Result<Self> {
        let pair = Self::read(chain, &named.address).await?;
        if pair.mint_x != named.base_mint || pair.mint_y != named.quote_mint {
            return Err(Error::UnknownMarket(format!(
                "meteora pool {}: its X and Y are not {}'s base and quote",
                named.address, named.symbol
            )));
        }
        let decimals = |mint: Pubkey| async move {
            let account = chain
                .get_account(&mint)
                .await
                .map_err(read_error("a pool's mint"))?
                .ok_or_else(|| Error::UnknownMarket(format!("mint {mint} doesn't exist")))?;
            mtx_kit::token::Mint::decode(&account.owner, &account.data)
                .map(|m| m.decimals)
                .map_err(read_error("a pool's mint"))
        };
        let (base, quote) = pool_assets(
            named,
            decimals(pair.mint_x).await?,
            decimals(pair.mint_y).await?,
        )?;
        Ok(Self {
            assets: PoolAssets {
                address: named.address,
                program: dlmm::PROGRAM_ID,
                base,
                quote,
                base_mint: pair.mint_x,
                quote_mint: pair.mint_y,
                base_program: pair.program_x,
                quote_program: pair.program_y,
                base_vault: pair.reserve_x,
                quote_vault: pair.reserve_y,
            },
        })
    }

    async fn read(chain: &dyn ChainClient, address: &Pubkey) -> Result<LbPair> {
        let account = chain
            .get_account(address)
            .await
            .map_err(read_error("meteora pool"))?
            .ok_or_else(|| Error::UnknownMarket(format!("meteora pool {address} doesn't exist")))?;
        LbPair::decode(*address, &account.data).map_err(read_error("meteora pool"))
    }

    async fn pair(&self, chain: &dyn ChainClient) -> Result<LbPair> {
        Self::read(chain, &self.assets.address).await
    }

    /// Y atoms per X atom, from whole units.
    fn atoms_price(&self, price: f64) -> f64 {
        price
            * 10f64
                .powi(i32::from(self.assets.quote.decimals) - i32::from(self.assets.base.decimals))
    }

    fn bin_price(&self, pair: &LbPair, id: i32) -> f64 {
        math::bin_price(
            id,
            pair.bin_step,
            self.assets.base.decimals,
            self.assets.quote.decimals,
        )
    }

    async fn bin_arrays(&self, chain: &dyn ChainClient, indexes: &[i64]) -> Vec<BinArray> {
        let mut arrays = Vec::new();
        for index in indexes {
            let address = dlmm::bin_array_address(&self.assets.address, *index);
            if let Ok(Some(account)) = chain.get_account(&address).await
                && let Ok(array) = BinArray::decode(&account.data)
            {
                arrays.push(array);
            }
        }
        arrays
    }

    fn lp(&self, pair: &LbPair, position: &Position, arrays: &[BinArray]) -> Lp {
        let (x, y) = math::position_amounts(position, arrays);
        let (fees_x, fees_y) = math::fees_owed(position, arrays);
        let (base, quote) = units(&self.assets, x, y);
        let (fees_base, fees_quote) = units(&self.assets, fees_x as f64, fees_y as f64);
        // A position covers its bins whole: from the lowest's price to past
        // the highest's.
        let step = 1.0 + f64::from(pair.bin_step) / 1e4;
        Lp {
            id: position.address,
            range: range(
                self.bin_price(pair, position.lower_bin_id),
                self.bin_price(pair, position.upper_bin_id) * step,
            ),
            base,
            quote,
            fees_base,
            fees_quote,
            live: position.bins.iter().any(|b| b.shares > 0),
        }
    }

    /// `initialize_bin_array`, paid by `owner`, for each of `indexes` not
    /// made yet.
    async fn missing_bin_arrays(
        &self,
        chain: &dyn ChainClient,
        pair: &LbPair,
        owner: &Pubkey,
        indexes: &[i64],
    ) -> Vec<Instruction> {
        let mut instructions = Vec::new();
        for index in indexes {
            let address = dlmm::bin_array_address(&self.assets.address, *index);
            if !matches!(chain.get_account(&address).await, Ok(Some(_))) {
                instructions.push(ix::initialize_bin_array(pair, owner, *index));
            }
        }
        instructions
    }

    async fn read_position(
        &self,
        chain: &dyn ChainClient,
        address: &Pubkey,
    ) -> SeatResult<Position> {
        let account = chain
            .get_account(address)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no position {address}"))?;
        Position::decode(*address, &account.data).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl Pool for DlmmPool {
    fn assets(&self) -> &PoolAssets {
        &self.assets
    }

    fn instruction(&self, data: &[u8]) -> (String, Option<Allowance>) {
        anchor_instruction(data, ALLOWED, OTHERS)
    }

    async fn price(&self, chain: &dyn ChainClient) -> Result<PoolPrice> {
        let pair = self.pair(chain).await?;
        Ok(PoolPrice {
            price: self.bin_price(&pair, pair.active_id),
            fee: pair.fee(),
        })
    }

    async fn stats(&self, chain: &dyn ChainClient) -> Result<Vec<(String, f64)>> {
        let pair = self.pair(chain).await?;
        Ok(vec![
            ("price".into(), self.bin_price(&pair, pair.active_id)),
            ("fee".into(), pair.fee()),
            ("active_bin".into(), f64::from(pair.active_id)),
            ("bin_step_bps".into(), f64::from(pair.bin_step)),
        ])
    }

    async fn swap(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        order: &SwapOrder,
    ) -> SeatResult<Vec<Instruction>> {
        let pair = self.pair(chain).await.map_err(|e| e.to_string())?;
        if pair.status != 0 {
            return Err("the pool is disabled".into());
        }
        let mut atoms = swap_atoms(order, &self.assets)?;
        let swap_for_y = atoms.base_in;
        let indexes = pair.swap_bin_arrays(swap_for_y, SWAP_BIN_ARRAYS);
        if indexes.is_empty() {
            return Err("no liquidity that way".into());
        }
        if order.partial {
            // The program has no price limit: send only what the bins hold
            // before the limit, the fee counted, as an exact-in swap.
            let fee = pair.fee();
            let stop = self.atoms_price(if swap_for_y {
                order.limit / (1.0 - fee)
            } else {
                order.limit * (1.0 - fee)
            });
            let arrays = self.bin_arrays(chain, &indexes).await;
            let cap = if atoms.exact_in {
                atoms.amount as f64
            } else if swap_for_y {
                // A sale for quote units: at most the base the limit allows.
                atoms.threshold as f64
            } else {
                // A purchase of base units: what they cost at the active
                // bin, the fee counted, and never more than at the limit.
                let price = self.atoms_price(self.bin_price(&pair, pair.active_id));
                (atoms.amount as f64 * price / (1.0 - fee)).min(atoms.threshold as f64)
            };
            let amount_in =
                math::amount_in_to_price(&pair, &arrays, swap_for_y, stop, cap).floor() as u64;
            if amount_in == 0 {
                return Err("nothing to take before the limit".into());
            }
            atoms.exact_in = true;
            atoms.amount = amount_in;
            atoms.threshold = 0;
        }
        let swap = if atoms.exact_in {
            Swap::ExactIn {
                amount_in: atoms.amount,
                min_out: atoms.threshold,
            }
        } else {
            Swap::ExactOut {
                amount_out: atoms.amount,
                max_in: atoms.threshold,
            }
        };
        Ok(vec![ix::swap(&pair, owner, swap_for_y, swap, &indexes)])
    }

    async fn positions(&self, chain: &dyn ChainClient, owner: &Pubkey) -> Result<Vec<Lp>> {
        let pair = self.pair(chain).await?;
        // Any size: `increase_position_length` grows a position's account
        // past `Position::LEN`.
        let mut accounts = chain
            .get_program_accounts(
                &dlmm::PROGRAM_ID,
                vec![
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(0, &Position::DISCRIMINATOR)),
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                        Position::PAIR_OFFSET,
                        self.assets.address.as_ref(),
                    )),
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                        Position::OWNER_OFFSET,
                        owner.as_ref(),
                    )),
                ],
            )
            .await
            .map_err(read_error("meteora positions"))?;
        accounts.sort_by_key(|(address, _)| *address);
        let mut held = Vec::new();
        for (address, account) in accounts {
            let Ok(position) = Position::decode(address, &account.data) else {
                continue;
            };
            let arrays = self.bin_arrays(chain, &position.bin_arrays()).await;
            let lp = self.lp(&pair, &position, &arrays);
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
        let Ok(position) = self.read_position(chain, id).await else {
            return Ok(None);
        };
        if position.owner != *owner || position.pair != self.assets.address {
            return Ok(None);
        }
        let pair = self.pair(chain).await?;
        let arrays = self.bin_arrays(chain, &position.bin_arrays()).await;
        Ok(Some(self.lp(&pair, &position, &arrays)))
    }

    /// A position's owner is in its account, whatever its size.
    async fn position_owners(
        &self,
        chain: &dyn ChainClient,
        accounts: &[Pubkey],
    ) -> Result<Vec<Pubkey>> {
        let mut owners = Vec::new();
        for chunk in accounts.chunks(MAX_ACCOUNTS_PER_READ) {
            let read = chain
                .get_multiple_accounts(chunk)
                .await
                .map_err(read_error("meteora positions"))?;
            owners.extend(chunk.iter().zip(read).filter_map(|(address, account)| {
                let account = account.filter(|a| a.owner == dlmm::PROGRAM_ID)?;
                Position::decode(*address, &account.data)
                    .ok()
                    .filter(|p| p.pair == self.assets.address)
                    .map(|p| p.owner)
            }));
        }
        Ok(owners)
    }

    async fn deposit(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        position: Option<&Pubkey>,
        new: &Pubkey,
        deposit: &Deposit,
    ) -> SeatResult<(Vec<Instruction>, bool)> {
        let pair = self.pair(chain).await.map_err(|e| e.to_string())?;
        if pair.status != 0 {
            return Err("the pool is disabled".into());
        }
        let existing = match position {
            Some(address) => Some(self.read_position(chain, address).await?),
            None => None,
        };
        let (lower, upper) = match (&existing, deposit.range) {
            (Some(p), _) => (p.lower_bin_id, p.upper_bin_id),
            (None, Some(r)) => {
                let bin = |price| {
                    math::bin_at_price(
                        price,
                        pair.bin_step,
                        self.assets.base.decimals,
                        self.assets.quote.decimals,
                    )
                };
                // Bins whose price lies in the range.
                let (lower, upper) = (bin(r.lower), bin(r.upper));
                if upper - lower + 1 > BINS_PER_ARRAY {
                    return Err(format!(
                        "a Meteora position spans at most {BINS_PER_ARRAY} bins; this range spans {}",
                        upper - lower + 1
                    ));
                }
                (lower, upper)
            }
            // The widest position, centred on the active bin.
            (None, None) => (
                pair.active_id - BINS_PER_ARRAY / 2 + 1,
                pair.active_id + BINS_PER_ARRAY / 2,
            ),
        };
        let decimals = |d: u8| 10f64.powi(i32::from(d));
        let amount_x = (deposit.base * decimals(self.assets.base.decimals)).floor() as u64;
        let amount_y = (deposit.quote * decimals(self.assets.quote.decimals)).floor() as u64;
        if lower > pair.active_id && amount_x == 0 || upper < pair.active_id && amount_y == 0 {
            return Err("this range at this price takes the other asset: none given".into());
        }
        // The range's bin arrays must exist: a grown position's may reach
        // arrays nobody has made yet.
        let indexes = match &existing {
            Some(p) => p.bin_arrays(),
            None => dlmm::position_bin_arrays(lower),
        };
        let mut instructions = self.missing_bin_arrays(chain, &pair, owner, &indexes).await;
        let target = match &existing {
            Some(p) => p.address,
            None => {
                instructions.push(ix::initialize_position(
                    &pair,
                    owner,
                    new,
                    lower,
                    upper - lower + 1,
                ));
                *new
            }
        };
        let strategy = match deposit.shape {
            None | Some(LiquidityShape::Spot) => Strategy::Spot,
            Some(LiquidityShape::Curve) => Strategy::Curve,
            Some(LiquidityShape::BidAsk) => Strategy::BidAsk,
        };
        instructions.push(ix::add_liquidity(
            &pair,
            owner,
            &target,
            &ix::Deposit {
                amount_x,
                amount_y,
                active_id: pair.active_id,
                max_active_bin_slippage: MAX_ACTIVE_BIN_SLIPPAGE,
                min_bin_id: lower,
                max_bin_id: upper,
                strategy,
            },
            &indexes,
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
        let pair = self.pair(chain).await.map_err(|e| e.to_string())?;
        let held = self.read_position(chain, &position.id).await?;
        // The program takes no minimum: the grader checks the position's
        // share and fees against it as it sends.
        let expected = |amount: f64, fees: f64| amount * share + fees;
        if min_base.is_some_and(|m| expected(position.base, position.fees_base) < m)
            || min_quote.is_some_and(|m| expected(position.quote, position.fees_quote) < m)
        {
            return Err("the position would pay out less than the minimum".into());
        }
        let bps = (share * 10_000.0).round() as u16;
        let mut instructions = self
            .missing_bin_arrays(chain, &pair, owner, &held.bin_arrays())
            .await;
        if bps > 0 && position.live {
            instructions.push(ix::remove_liquidity(&pair, owner, &held, bps));
        }
        instructions.push(ix::claim_fee(&pair, owner, &held));
        Ok(instructions)
    }
}
