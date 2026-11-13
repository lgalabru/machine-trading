//! Pools: what Raydium's, Meteora's and Orca's markets share.
//!
//! A pool offers two instruments, each its own [`PoolMarket`]:
//!
//! - **spot** (`<venue>:spot:<BASE-QUOTE>`, `specs/instruments/spot.md`):
//!   swapping against the pool. The holding is the base asset in the
//!   wallet, as the spot profile counts it; nothing rests and nothing is
//!   locked. Its seat executes swaps and market orders, each one swap.
//! - **amm** (`<venue>:amm:<BASE-QUOTE>`, `specs/instruments/amm.md`):
//!   providing liquidity. The holdings are the account's positions in the
//!   pool, each a [`Holding::Amm`]; its seat adds and removes liquidity.
//!
//! A pool's program is behind [`Pool`]: Raydium CLMM's in
//! [`super::raydium`], Meteora DLMM's in [`super::meteora`], Orca
//! Whirlpool's in [`super::orca`]. What is the same for all is here: a market's top (the price of a small trade each
//! way: the pool's price less or plus its fee), its fills (read from the
//! pool's vaults, which every swap moves: the vault deltas give the side,
//! the size and the price paid, fee included, and the fee payer the taker),
//! the valuation of a position (its assets at the reference prices, not the
//! pool's own, so a pool moved by its holder can't inflate it), and the
//! seats, which send each order as one transaction.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use mtx_core::{
    Asset, Decimal, Holding, Holdings, InstrumentId, PoolPosition, Side as CoreSide,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    Action, AmountUnit, Instruction, LiquidityPosition, LiquidityShape, Planned, PriceRange,
    SendState, Sent, Side, VenueInfo, VenueView,
    chain::{ChainClient, InstructionGroup, RpcChainClient, TransactionPlan, token},
    protocols::{NamedMarket, spot::MarketFill},
};
use sha2::{Digest, Sha256};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

use super::{Market, MarketKind, Seat, SeatResult, price_usd};
use crate::{Error, Result, session::grader_f64};

/// Compute units a pool transaction asks for: a swap across several tick
/// or bin arrays, or a position opened with its arrays.
const COMPUTE_UNIT_LIMIT: u32 = 1_000_000;

/// A pool's assets and accounts, read once.
#[derive(Debug, Clone)]
pub struct PoolAssets {
    pub address: Pubkey,
    pub base: Asset,
    pub quote: Asset,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    /// The token program of each mint.
    pub base_program: Pubkey,
    pub quote_program: Pubkey,
    /// The pool's vaults of each.
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
}

impl PoolAssets {
    /// `atoms` of the base, in base units.
    fn base_units(&self, atoms: f64) -> f64 {
        atoms / 10f64.powi(i32::from(self.base.decimals))
    }

    fn quote_units(&self, atoms: f64) -> f64 {
        atoms / 10f64.powi(i32::from(self.quote.decimals))
    }
}

/// The pool's state as a trader reads it now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolPrice {
    /// Quote per base, in whole units.
    pub price: f64,
    /// The fee a swap pays now, as a share of the amount.
    pub fee: f64,
}

/// What a swap trades: `side` of the base asset, `amount` of the base or of
/// the quote, at no worse than `limit` quote per base, fee included. A
/// `partial` swap is a taker's: it trades up to `amount` and stops where the
/// next unit would cost more than `limit`, instead of failing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwapOrder {
    pub side: Side,
    pub amount: f64,
    pub unit: AmountUnit,
    pub limit: f64,
    pub partial: bool,
}

/// What a deposit asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Deposit {
    pub base: f64,
    pub quote: f64,
    pub range: Option<PriceRange>,
    pub shape: Option<LiquidityShape>,
}

/// One of an account's positions in a pool.
#[derive(Debug, Clone, PartialEq)]
pub struct Lp {
    /// The position's id: its NFT's mint (Raydium, Orca), its account
    /// (Meteora).
    pub id: Pubkey,
    pub range: Option<PriceRange>,
    /// In whole units.
    pub base: f64,
    pub quote: f64,
    pub fees_base: f64,
    pub fees_quote: f64,
    /// Whether it still holds liquidity (not only fees).
    pub live: bool,
}

/// A pool's program: how its state reads and its orders build.
#[async_trait]
pub trait Pool: Send + Sync {
    fn assets(&self) -> &PoolAssets;

    /// The pool's price and fee now.
    async fn price(&self, chain: &dyn ChainClient) -> Result<PoolPrice>;

    /// The pool's own numbers, by name, for a step's venue view.
    async fn stats(&self, chain: &dyn ChainClient) -> Result<Vec<(String, f64)>>;

    /// The instructions of one swap by `owner`.
    async fn swap(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        order: &SwapOrder,
    ) -> SeatResult<Vec<Instruction>>;

    /// `owner`'s positions in the pool.
    async fn positions(&self, chain: &dyn ChainClient, owner: &Pubkey) -> Result<Vec<Lp>>;

    /// `owner`'s position `id` in the pool, if it has it.
    async fn position(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        id: &Pubkey,
    ) -> Result<Option<Lp>>;

    /// The instructions of a deposit by `owner`: into `position`, or into a
    /// new one at `new` (which then signs too). Returns whether `new` is
    /// used.
    async fn deposit(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        position: Option<&Pubkey>,
        new: &Pubkey,
        deposit: &Deposit,
    ) -> SeatResult<(Vec<Instruction>, bool)>;

    /// The instructions that take `share` of `position` out, and its fees,
    /// receiving at least `min_base` and `min_quote` (fees included).
    async fn withdraw(
        &self,
        chain: &dyn ChainClient,
        owner: &Pubkey,
        position: &Lp,
        share: f64,
        min_base: Option<f64>,
        min_quote: Option<f64>,
    ) -> SeatResult<Vec<Instruction>>;
}

/// One instrument of a pool's: the spot one, or the amm one.
pub struct PoolMarket {
    instrument: InstrumentRef,
    pool: Arc<dyn Pool>,
    chain: Arc<dyn ChainClient>,
}

impl PoolMarket {
    /// The market of `instrument` (its kind spot or amm) on `pool`, read
    /// through `chain`.
    pub fn new(
        instrument: InstrumentRef,
        pool: Arc<dyn Pool>,
        chain: Arc<dyn ChainClient>,
    ) -> Self {
        Self {
            instrument,
            pool,
            chain,
        }
    }

    fn assets(&self) -> &PoolAssets {
        self.pool.assets()
    }
}

/// `owner`'s balance of `mint` across its token accounts, in atoms.
pub(crate) async fn wallet_atoms(
    chain: &dyn ChainClient,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<u64> {
    let mut atoms = 0u64;
    for (_, account) in chain
        .get_token_accounts_by_owner(owner, mint)
        .await
        .map_err(chain_error("token accounts"))?
    {
        atoms = atoms
            .saturating_add(token::token_balance(&account).map_err(chain_error("token account"))?);
    }
    Ok(atoms)
}

fn chain_error<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> Error {
    move |error| Error::Transaction {
        label: what.into(),
        reason: error.to_string(),
    }
}

fn decimal(x: f64) -> Decimal {
    Decimal::try_from(x).unwrap_or_default().round_dp(12)
}

/// A position as the amm profile holds it.
fn holding(assets: &PoolAssets, lp: &Lp) -> Holding {
    Holding::Amm(PoolPosition {
        id: lp.id.to_string(),
        base: assets.base.clone(),
        quote: assets.quote.clone(),
        base_amount: decimal(lp.base),
        quote_amount: decimal(lp.quote),
        fees_base: decimal(lp.fees_base),
        fees_quote: decimal(lp.fees_quote),
        range: lp.range.map(|r| (decimal(r.lower), decimal(r.upper))),
    })
}

/// How a transaction moved a pool's base and quote vaults, in atoms.
fn vault_deltas(summary: &mtx_kit::chain::TransactionSummary, assets: &PoolAssets) -> (i128, i128) {
    let delta = |vault: &Pubkey| -> i128 {
        summary
            .token_balance_changes
            .iter()
            .filter(|c| c.account == *vault)
            .map(|c| i128::from(c.post_amount) - i128::from(c.pre_amount))
            .sum()
    };
    (delta(&assets.base_vault), delta(&assets.quote_vault))
}

/// The side of the base a transaction's liquidity moved, from its vaults'
/// deltas: base into the pool (a deposit) is a buy, base out (a withdrawal
/// or collected fees) a sell. `None` for a swap (the vaults moved opposite
/// ways) or a transaction that moved no base.
fn vault_liquidity(
    summary: &mtx_kit::chain::TransactionSummary,
    assets: &PoolAssets,
) -> Option<CoreSide> {
    let (base, quote) = vault_deltas(summary, assets);
    match (base.signum(), quote.signum()) {
        (1, 0 | 1) => Some(CoreSide::Buy),
        (-1, 0 | -1) => Some(CoreSide::Sell),
        _ => None,
    }
}

/// A transaction's swap through a pool, from its vaults' deltas: the
/// taker's side, base and quote, in atoms. `None` for a transaction that
/// moved both vaults the same way (liquidity in or out) or neither.
fn vault_swap(
    summary: &mtx_kit::chain::TransactionSummary,
    assets: &PoolAssets,
) -> Option<(CoreSide, u64, u64)> {
    let (base, quote) = vault_deltas(summary, assets);
    let atoms = |x: i128| u64::try_from(x.unsigned_abs()).ok();
    match (base.signum(), quote.signum()) {
        (1, -1) => Some((CoreSide::Sell, atoms(base)?, atoms(quote)?)),
        (-1, 1) => Some((CoreSide::Buy, atoms(base)?, atoms(quote)?)),
        _ => None,
    }
}

#[async_trait]
impl Market for PoolMarket {
    fn instrument(&self) -> &InstrumentRef {
        &self.instrument
    }

    fn address(&self) -> Pubkey {
        self.assets().address
    }

    /// Both instruments are a pool's, to a strategy.
    fn info(&self, tradable: bool) -> VenueInfo {
        VenueInfo {
            id: self.instrument.to_string(),
            kind: MarketKind::Amm,
            address: Some(self.address().to_string()),
            tradable,
        }
    }

    async fn top(&self) -> Result<(Option<f64>, Option<f64>)> {
        let PoolPrice { price, fee } = self.pool.price(self.chain.as_ref()).await?;
        Ok((Some(price * (1.0 - fee)), Some(price / (1.0 - fee))))
    }

    async fn holdings(&self, owner: &Pubkey) -> Result<Vec<Holding>> {
        let assets = self.assets();
        match self.instrument.kind {
            Kind::Amm => Ok(self
                .pool
                .positions(self.chain.as_ref(), owner)
                .await?
                .iter()
                .map(|lp| holding(assets, lp))
                .collect()),
            _ => {
                let chain = self.chain.as_ref();
                let held = |asset: &Asset, atoms: u64| Holdings {
                    wallet: Decimal::from(atoms) / Decimal::from(10u64.pow(asset.decimals.into())),
                    ..Holdings::empty(asset.clone())
                };
                let base = wallet_atoms(chain, owner, &assets.base_mint).await?;
                let quote = wallet_atoms(chain, owner, &assets.quote_mint).await?;
                Ok(vec![
                    Holding::Spot(held(&assets.base, base)),
                    Holding::Spot(held(&assets.quote, quote)),
                ])
            }
        }
    }

    /// A pool position's assets and fees at the reference prices; `None`
    /// for anything but positions in this pool's assets.
    async fn value_usd(
        &self,
        chain: &dyn ChainClient,
        holdings: &[Holding],
    ) -> Result<Option<Decimal>> {
        let assets = self.assets();
        let mut value = Decimal::ZERO;
        for holding in holdings {
            let Holding::Amm(position) = holding else {
                return Ok(None);
            };
            if position.base.id != assets.base.id || position.quote.id != assets.quote.id {
                return Ok(None);
            }
            value += (position.base_amount + position.fees_base)
                * price_usd(chain, &assets.base_mint).await?
                + (position.quote_amount + position.fees_quote)
                    * price_usd(chain, &assets.quote_mint).await?;
        }
        Ok(Some(value))
    }

    /// Every swap through the pool since `after_slot`, for the spot
    /// instrument: the pool is every trade's maker.
    async fn fills(
        &self,
        after_slot: u64,
        owner: &Pubkey,
    ) -> Result<Vec<(MarketFill, Vec<Pubkey>)>> {
        if self.instrument.kind != Kind::Spot {
            return Ok(Vec::new());
        }
        let chain = self.chain.as_ref();
        let assets = self.assets();
        let id = InstrumentId::new(self.instrument.to_string());
        let mut fills = Vec::new();
        for (signature, slot) in chain
            .signatures_for_address(&assets.address, after_slot)
            .await
            .map_err(chain_error("the pool's transactions"))?
        {
            let summary = chain
                .get_transaction(&signature)
                .await
                .map_err(chain_error("a pool transaction"))?
                .ok_or_else(|| Error::Transaction {
                    label: "a pool transaction".into(),
                    reason: format!("{signature} is listed but not found"),
                })?;
            if summary.error.is_some() {
                continue;
            }
            let Some((side, base, quote)) = vault_swap(&summary, assets) else {
                continue;
            };
            let Some(taker) = summary.account_keys.first().copied() else {
                continue;
            };
            let units = |atoms: u64, asset: &Asset| {
                Decimal::from(atoms) / Decimal::from(10u64.pow(asset.decimals.into()))
            };
            let quantity = units(base, &assets.base);
            let sequence = u128::from_le_bytes(
                signature.as_ref()[..16]
                    .try_into()
                    .expect("a signature has 64 bytes"),
            );
            fills.push((
                MarketFill {
                    slot,
                    instrument: id.clone(),
                    sequence,
                    taker,
                    maker: assets.address,
                    taker_side: side,
                    price: units(quote, &assets.quote) / quantity,
                    quantity,
                    unix_timestamp: summary.block_time.map_or(0, |t| t.timestamp()),
                },
                vec![*owner],
            ));
        }
        Ok(fills)
    }

    /// The sides of the base `owner`'s transactions through the pool's
    /// liquidity moved since `after_slot`, for the amm instrument: read from
    /// the vaults, so a bot's own transactions count as much as its seat's.
    async fn liquidity_moves(&self, after_slot: u64, owner: &Pubkey) -> Result<Vec<CoreSide>> {
        if self.instrument.kind != Kind::Amm {
            return Ok(Vec::new());
        }
        let chain = self.chain.as_ref();
        let assets = self.assets();
        let mut moves = Vec::new();
        for (signature, _) in chain
            .signatures_for_address(&assets.address, after_slot)
            .await
            .map_err(chain_error("the pool's transactions"))?
        {
            let summary = chain
                .get_transaction(&signature)
                .await
                .map_err(chain_error("a pool transaction"))?
                .ok_or_else(|| Error::Transaction {
                    label: "a pool transaction".into(),
                    reason: format!("{signature} is listed but not found"),
                })?;
            if summary.error.is_some() || summary.account_keys.first() != Some(owner) {
                continue;
            }
            if let Some(side) = vault_liquidity(&summary, assets)
                && !moves.contains(&side)
            {
                moves.push(side);
            }
        }
        Ok(moves)
    }

    async fn seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>> {
        Ok(Box::new(PoolSeat {
            market: self.instrument.to_string(),
            kind: self.instrument.kind,
            pool: self.pool.clone(),
            chain: Arc::new(RpcChainClient::new(rpc_url)),
            signer,
            opened: AtomicU64::new(0),
        }))
    }
}

/// An account trading one of a pool's instruments.
pub struct PoolSeat {
    market: String,
    kind: Kind,
    pool: Arc<dyn Pool>,
    chain: Arc<dyn ChainClient>,
    signer: Keypair,
    /// Positions this seat opened: the next new position's number.
    opened: AtomicU64,
}

impl PoolSeat {
    /// The keypair of the seat's `n`-th new position: derived from the
    /// seat's own key and the pool, so a session replays it.
    fn position_key(&self, n: u64) -> Keypair {
        let mut hash = Sha256::new();
        hash.update(b"mtx pool position");
        hash.update(self.signer.secret_bytes());
        hash.update(self.pool.assets().address.as_ref());
        hash.update(n.to_le_bytes());
        Keypair::new_from_array(hash.finalize().into())
    }

    /// The owner's token accounts for the pool's assets, created where
    /// they don't exist yet.
    async fn token_accounts(&self) -> SeatResult<Vec<Instruction>> {
        let owner = self.signer.pubkey();
        let assets = self.pool.assets();
        let mut create = Vec::new();
        for (mint, program) in [
            (assets.base_mint, assets.base_program),
            (assets.quote_mint, assets.quote_program),
        ] {
            let account = token::ata_for(&owner, &mint, &program);
            if !matches!(self.chain.get_account(&account).await, Ok(Some(_))) {
                create.push(token::create_ata_idempotent_for(
                    &owner, &owner, &mint, &program,
                ));
            }
        }
        Ok(create)
    }

    /// Sign `instructions` as one transaction, with `extra` too where it
    /// signs, and send it without waiting: the world lands it in its next
    /// block.
    async fn send_ixs(
        &self,
        label: &str,
        instructions: Vec<Instruction>,
        extra: Option<&Keypair>,
    ) -> SeatResult<mtx_kit::Signature> {
        let plan = TransactionPlan::from_group(
            &InstructionGroup::new(label, instructions),
            COMPUTE_UNIT_LIMIT,
        );
        let blockhash = self
            .chain
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let mut tx = plan.transaction(&self.signer.pubkey(), blockhash);
        let mut signers: Vec<&Keypair> = vec![&self.signer];
        signers.extend(extra);
        tx.try_sign(&signers, blockhash)
            .map_err(|e| format!("signing {label}: {e}"))?;
        self.chain
            .send(&VersionedTransaction::from(tx))
            .await
            .map_err(|e| e.to_string())
    }

    /// What became of a send the world lands in its next block: what the
    /// chain says of it now.
    async fn outcome(&self, signature: mtx_kit::Signature) -> SendState {
        match self.chain.get_transaction(&signature).await {
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

    async fn swap(&self, order: &SwapOrder) -> SeatResult<mtx_kit::Signature> {
        if !(order.amount.is_finite() && order.amount > 0.0 && order.limit.is_finite()) {
            return Err("a swap needs a positive amount and a limit".into());
        }
        let mut instructions = self.token_accounts().await?;
        instructions.extend(
            self.pool
                .swap(self.chain.as_ref(), &self.signer.pubkey(), order)
                .await?,
        );
        self.send_ixs("pool swap", instructions, None).await
    }

    /// The position a line names, or the only one.
    async fn position(&self, id: Option<&str>) -> SeatResult<Lp> {
        let owner = self.signer.pubkey();
        match id {
            Some(id) => {
                let address: Pubkey = id
                    .parse()
                    .map_err(|_| format!("{id} is not a position's address"))?;
                self.pool
                    .position(self.chain.as_ref(), &owner, &address)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("no position {id} in this pool"))
            }
            None => {
                let positions = self
                    .pool
                    .positions(self.chain.as_ref(), &owner)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut positions = positions.into_iter();
                match (positions.next(), positions.next()) {
                    (Some(only), None) => Ok(only),
                    (None, _) => Err("no position in this pool".into()),
                    (Some(_), Some(_)) => {
                        Err("several positions in this pool: name the one to remove".into())
                    }
                }
            }
        }
    }

    async fn add(&self, planned: &Planned) -> SeatResult<mtx_kit::Signature> {
        let Action::AddLiquidity {
            base,
            quote,
            range,
            position,
            shape,
        } = &planned.action
        else {
            return Err("not a deposit".into());
        };
        if !(base.is_finite() && quote.is_finite() && *base >= 0.0 && *quote >= 0.0)
            || *base + *quote <= 0.0
        {
            return Err("a deposit needs a base or quote amount of at least 0, not both 0".into());
        }
        if let Some(r) = range
            && !(r.lower.is_finite() && r.upper.is_finite() && 0.0 < r.lower && r.lower < r.upper)
        {
            return Err("a range needs 0 < lower < upper".into());
        }
        let existing = match position {
            Some(id) => Some(self.position(Some(id)).await?.id),
            None => None,
        };
        let n = self.opened.load(Ordering::SeqCst);
        let new = self.position_key(n);
        let deposit = Deposit {
            base: *base,
            quote: *quote,
            range: *range,
            shape: *shape,
        };
        let mut instructions = self.token_accounts().await?;
        let (built, opens) = self
            .pool
            .deposit(
                self.chain.as_ref(),
                &self.signer.pubkey(),
                existing.as_ref(),
                &new.pubkey(),
                &deposit,
            )
            .await?;
        instructions.extend(built);
        let sent = self
            .send_ixs("pool deposit", instructions, opens.then_some(&new))
            .await?;
        if opens {
            self.opened.store(n + 1, Ordering::SeqCst);
        }
        Ok(sent)
    }

    async fn remove(&self, planned: &Planned) -> SeatResult<mtx_kit::Signature> {
        let Action::RemoveLiquidity {
            position,
            share,
            min_base,
            min_quote,
        } = &planned.action
        else {
            return Err("not a withdrawal".into());
        };
        if !(0.0..=1.0).contains(share) {
            return Err("a share is between 0 and 1".into());
        }
        let lp = self.position(position.as_deref()).await?;
        let mut instructions = self.token_accounts().await?;
        instructions.extend(
            self.pool
                .withdraw(
                    self.chain.as_ref(),
                    &self.signer.pubkey(),
                    &lp,
                    *share,
                    *min_base,
                    *min_quote,
                )
                .await?,
        );
        self.send_ixs("pool withdrawal", instructions, None).await
    }
}

#[async_trait]
impl Seat for PoolSeat {
    fn market(&self) -> &str {
        &self.market
    }

    fn executes(&self, action: &Action) -> bool {
        match self.kind {
            Kind::Amm => matches!(
                action,
                Action::AddLiquidity { .. } | Action::RemoveLiquidity { .. }
            ),
            _ => matches!(action, Action::Swap { .. } | Action::Market { .. }),
        }
    }

    async fn send(&self, planned: &Planned) -> Sent {
        if !self.executes(&planned.action) {
            return Sent {
                state: SendState::NotSent("unsupported in this session".into()),
                sent: false,
            };
        }
        let sent = match planned.action {
            Action::Swap {
                side,
                amount,
                unit,
                limit,
            } => {
                self.swap(&SwapOrder {
                    side,
                    amount,
                    unit,
                    limit,
                    partial: false,
                })
                .await
            }
            Action::Market { side, size, limit } => {
                self.swap(&SwapOrder {
                    side,
                    amount: size,
                    unit: AmountUnit::Base,
                    limit,
                    partial: false,
                })
                .await
            }
            Action::AddLiquidity { .. } => self.add(planned).await,
            _ => self.remove(planned).await,
        };
        Sent {
            state: match sent {
                Ok(signature) => self.outcome(signature).await,
                Err(reason) => SendState::failed(reason),
            },
            sent: true,
        }
    }

    /// A deposit that takes base into the pool moves it on the buy side, a
    /// withdrawal that returns base on the sell side. A deposit of base into
    /// a range that lies wholly below the price takes none.
    async fn base_moved(&self, action: &Action) -> SeatResult<Option<CoreSide>> {
        if self.kind != Kind::Amm {
            return Ok(None);
        }
        match action {
            Action::AddLiquidity {
                base,
                range,
                position,
                ..
            } => {
                if base.is_nan() || *base <= 0.0 {
                    return Ok(None);
                }
                let range = match position {
                    Some(id) => self.position(Some(id)).await?.range,
                    None => *range,
                };
                let PoolPrice { price, .. } = self
                    .pool
                    .price(self.chain.as_ref())
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(range
                    .is_none_or(|r| r.upper > price)
                    .then_some(CoreSide::Buy))
            }
            Action::RemoveLiquidity {
                position, share, ..
            } => {
                let held = self.position(position.as_deref()).await?;
                Ok(((*share > 0.0 && held.base > 0.0) || held.fees_base > 0.0)
                    .then_some(CoreSide::Sell))
            }
            _ => Ok(None),
        }
    }

    /// A taker's swap: up to `size`, stopping at `limit`.
    async fn take(&self, side: Side, size: f64, limit: f64) -> SeatResult {
        self.swap(&SwapOrder {
            side,
            amount: size,
            unit: AmountUnit::Base,
            limit,
            partial: true,
        })
        .await
        .map(|_| ())
    }

    async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
        let PoolPrice { price, fee } = self
            .pool
            .price(self.chain.as_ref())
            .await
            .map_err(|e| e.to_string())?;
        Ok((Some(price * (1.0 - fee)), Some(price / (1.0 - fee))))
    }

    /// The base asset in the wallet, or in the account's positions.
    async fn position(&self) -> SeatResult<f64> {
        let owner = self.signer.pubkey();
        let assets = self.pool.assets();
        match self.kind {
            Kind::Amm => Ok(self
                .pool
                .positions(self.chain.as_ref(), &owner)
                .await
                .map_err(|e| e.to_string())?
                .iter()
                .map(|lp| lp.base + lp.fees_base)
                .sum()),
            _ => wallet_atoms(self.chain.as_ref(), &owner, &assets.base_mint)
                .await
                .map(|atoms| assets.base_units(atoms as f64))
                .map_err(|e| e.to_string()),
        }
    }

    /// The pool's numbers, and on the amm instrument the account's
    /// positions and the base they hold.
    async fn view(&self, view: &mut VenueView) -> SeatResult {
        view.stats = self
            .pool
            .stats(self.chain.as_ref())
            .await
            .map_err(|e| e.to_string())?;
        if self.kind != Kind::Amm {
            return Ok(());
        }
        let positions = self
            .pool
            .positions(self.chain.as_ref(), &self.signer.pubkey())
            .await
            .map_err(|e| e.to_string())?;
        view.position = Some(mtx_kit::Position {
            size: positions.iter().map(|lp| lp.base + lp.fees_base).sum(),
            entry_usd: 0.0,
        });
        view.liquidity = positions
            .into_iter()
            .map(|lp| LiquidityPosition {
                id: lp.id.to_string(),
                range: lp.range,
                base: lp.base,
                quote: lp.quote,
                fees_base: lp.fees_base,
                fees_quote: lp.fees_quote,
            })
            .collect();
        Ok(())
    }
}

/// The pool `named` names on `venue` (`raydium`, `meteora` or `orca`), read through
/// `chain`.
pub async fn connect(
    venue: &str,
    chain: &dyn ChainClient,
    named: &NamedMarket,
) -> Result<Arc<dyn Pool>> {
    Ok(match venue {
        "raydium" => Arc::new(super::raydium::ClmmPool::connect(chain, named).await?),
        "meteora" => Arc::new(super::meteora::DlmmPool::connect(chain, named).await?),
        "orca" => Arc::new(super::orca::WhirlpoolPool::connect(chain, named).await?),
        other => return Err(Error::UnknownMarket(format!("{other} has no pools"))),
    })
}

/// The pool a venue's profile names for `symbol`, among `pools`.
pub(crate) fn named(pools: &'static [NamedMarket], symbol: &str) -> Option<&'static NamedMarket> {
    pools.iter().find(|p| p.symbol == symbol)
}

/// The base asset's and the quote asset's [`Asset`]s of `named`, a pool
/// of `BASE-QUOTE`, with each mint's decimals.
pub(crate) fn pool_assets(
    named: &NamedMarket,
    base_decimals: u8,
    quote_decimals: u8,
) -> Result<(Asset, Asset)> {
    let (base, quote) = named
        .symbol
        .split_once('-')
        .ok_or_else(|| Error::UnknownMarket(format!("{} is not BASE-QUOTE", named.symbol)))?;
    let asset = |mint: &Pubkey, symbol: &str, decimals| {
        Asset::new(mint.to_string(), symbol, decimals)
            .map_err(|e| Error::UnknownMarket(e.to_string()))
    };
    Ok((
        asset(&named.base_mint, base, base_decimals)?,
        asset(&named.quote_mint, quote, quote_decimals)?,
    ))
}

/// A swap's amounts in atoms: what goes in and the least out (exact in),
/// or what comes out and the most in (exact out), and which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapAtoms {
    /// The base asset goes in (a sale).
    pub base_in: bool,
    pub exact_in: bool,
    /// The exact amount: in, or out.
    pub amount: u64,
    /// The least out, or the most in.
    pub threshold: u64,
}

/// `order` in atoms, as an exact-in or exact-out swap: a sale of base
/// units, or a purchase for quote units, is exact in; a purchase of base
/// units, or a sale for quote units, is exact out. The limit bounds the
/// other side.
pub fn swap_atoms(order: &SwapOrder, assets: &PoolAssets) -> SeatResult<SwapAtoms> {
    let base_atoms = |units: f64| units * 10f64.powi(i32::from(assets.base.decimals));
    let quote_atoms = |units: f64| units * 10f64.powi(i32::from(assets.quote.decimals));
    let whole = |atoms: f64, up: bool| -> SeatResult<u64> {
        let atoms = if up { atoms.ceil() } else { atoms.floor() };
        if atoms.is_finite() && atoms >= 0.0 {
            Ok(if atoms >= u64::MAX as f64 {
                u64::MAX
            } else {
                atoms as u64
            })
        } else {
            Err("an amount no token can hold".into())
        }
    };
    let limit = order.limit.max(0.0);
    let base_in = order.side == Side::Ask;
    let (exact_in, amount, threshold) = match (order.side, order.unit) {
        // Sell base units for at least amount × limit.
        (Side::Ask, AmountUnit::Base) => (
            true,
            whole(base_atoms(order.amount), false)?,
            whole(quote_atoms(order.amount * limit), false)?,
        ),
        // Buy base units for at most amount × limit.
        (Side::Bid, AmountUnit::Base) => (
            false,
            whole(base_atoms(order.amount), false)?,
            whole(quote_atoms(order.amount * limit), true)?,
        ),
        // Spend quote units for at least amount / limit.
        (Side::Bid, AmountUnit::Quote) => (
            true,
            whole(quote_atoms(order.amount), false)?,
            if limit > 0.0 {
                whole(base_atoms(order.amount / limit), false)?
            } else {
                return Err("a purchase for a quote amount needs a positive limit".into());
            },
        ),
        // Receive quote units for at most amount / limit.
        (Side::Ask, AmountUnit::Quote) => (
            false,
            whole(quote_atoms(order.amount), false)?,
            if limit > 0.0 {
                whole(base_atoms(order.amount / limit), true)?
            } else {
                u64::MAX
            },
        ),
    };
    if amount == 0 {
        return Err("rounds to nothing in the token's atoms".into());
    }
    Ok(SwapAtoms {
        base_in,
        exact_in,
        amount,
        threshold,
    })
}

/// A price range in quote per base, from a pool's position bounds.
pub(crate) fn range(lower: f64, upper: f64) -> Option<PriceRange> {
    (lower.is_finite() && upper.is_finite() && lower < upper).then_some(PriceRange { lower, upper })
}

/// Whole units of a pool's base and quote from atoms.
pub(crate) fn units(assets: &PoolAssets, base_atoms: f64, quote_atoms: f64) -> (f64, f64) {
    (
        grader_f64(decimal(assets.base_units(base_atoms))),
        grader_f64(decimal(assets.quote_units(quote_atoms))),
    )
}

#[cfg(test)]
mod tests {
    use mtx_kit::protocols::{SPCX_MINT, USDC_MINT};

    use super::*;

    fn assets() -> PoolAssets {
        PoolAssets {
            address: Pubkey::new_unique(),
            base: Asset::new(SPCX_MINT.to_string(), "SPCX", 6).unwrap(),
            quote: Asset::new(USDC_MINT.to_string(), "USDC", 6).unwrap(),
            base_mint: SPCX_MINT,
            quote_mint: USDC_MINT,
            base_program: Pubkey::new_unique(),
            quote_program: Pubkey::new_unique(),
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
        }
    }

    #[test]
    fn a_swap_is_exact_in_or_out_with_its_limit_on_the_other_side() {
        let assets = assets();
        let order = |side, amount, unit, limit| SwapOrder {
            side,
            amount,
            unit,
            limit,
            partial: false,
        };
        let sell = swap_atoms(&order(Side::Ask, 2.0, AmountUnit::Base, 140.0), &assets).unwrap();
        assert_eq!(
            sell,
            SwapAtoms {
                base_in: true,
                exact_in: true,
                amount: 2_000_000,
                threshold: 280_000_000
            }
        );
        let buy = swap_atoms(&order(Side::Bid, 2.0, AmountUnit::Base, 141.0), &assets).unwrap();
        assert_eq!(
            (buy.exact_in, buy.amount, buy.threshold),
            (false, 2_000_000, 282_000_000)
        );
        let spend =
            swap_atoms(&order(Side::Bid, 140.0, AmountUnit::Quote, 140.0), &assets).unwrap();
        assert_eq!(
            (spend.base_in, spend.exact_in, spend.threshold),
            (false, true, 1_000_000)
        );
        let raise = swap_atoms(&order(Side::Ask, 140.0, AmountUnit::Quote, 0.0), &assets).unwrap();
        assert_eq!((raise.exact_in, raise.threshold), (false, u64::MAX));
        assert!(swap_atoms(&order(Side::Bid, 1.0, AmountUnit::Quote, 0.0), &assets).is_err());
        assert!(swap_atoms(&order(Side::Ask, 1e-9, AmountUnit::Base, 1.0), &assets).is_err());
    }

    #[test]
    fn vault_deltas_tell_swaps_from_liquidity() {
        let assets = assets();
        let change = |account, pre, post| mtx_kit::chain::TokenBalanceChange {
            account,
            mint: Pubkey::new_unique(),
            owner: None,
            pre_amount: pre,
            post_amount: post,
        };
        let summary = |changes| mtx_kit::chain::TransactionSummary {
            signature: Default::default(),
            slot: 1,
            block_time: None,
            fee_lamports: 0,
            error: None,
            logs: Vec::new(),
            account_keys: Vec::new(),
            pre_balances: Vec::new(),
            post_balances: Vec::new(),
            token_balance_changes: changes,
            inner_instructions: Vec::new(),
        };
        let sold = summary(vec![
            change(assets.base_vault, 100, 102),
            change(assets.quote_vault, 1000, 720),
        ]);
        assert_eq!(vault_swap(&sold, &assets), Some((CoreSide::Sell, 2, 280)));
        let bought = summary(vec![
            change(assets.base_vault, 100, 99),
            change(assets.quote_vault, 1000, 1141),
        ]);
        assert_eq!(vault_swap(&bought, &assets), Some((CoreSide::Buy, 1, 141)));
        let added = summary(vec![
            change(assets.base_vault, 100, 110),
            change(assets.quote_vault, 1000, 2000),
        ]);
        assert_eq!(vault_swap(&added, &assets), None);
        assert_eq!(vault_swap(&summary(Vec::new()), &assets), None);

        // The side the liquidity moved: base in is a buy, out a sell, however
        // the quote went; a swap is neither.
        let liquidity = |base: (u64, u64), quote: (u64, u64)| {
            vault_liquidity(
                &summary(vec![
                    change(assets.base_vault, base.0, base.1),
                    change(assets.quote_vault, quote.0, quote.1),
                ]),
                &assets,
            )
        };
        assert_eq!(liquidity((100, 110), (1000, 2000)), Some(CoreSide::Buy));
        assert_eq!(liquidity((100, 110), (1000, 1000)), Some(CoreSide::Buy));
        assert_eq!(liquidity((110, 100), (2000, 1000)), Some(CoreSide::Sell));
        assert_eq!(liquidity((100, 100), (1000, 900)), None);
        assert_eq!(liquidity((100, 102), (1000, 720)), None);
        assert_eq!(vault_liquidity(&summary(Vec::new()), &assets), None);
    }

    /// A pool at a fixed price holding a fixed set of positions.
    struct Stub {
        assets: PoolAssets,
        held: Vec<Lp>,
    }

    #[async_trait]
    impl Pool for Stub {
        fn assets(&self) -> &PoolAssets {
            &self.assets
        }
        async fn price(&self, _: &dyn ChainClient) -> Result<PoolPrice> {
            Ok(PoolPrice {
                price: 140.0,
                fee: 0.003,
            })
        }
        async fn stats(&self, _: &dyn ChainClient) -> Result<Vec<(String, f64)>> {
            Ok(Vec::new())
        }
        async fn swap(
            &self,
            _: &dyn ChainClient,
            _: &Pubkey,
            _: &SwapOrder,
        ) -> SeatResult<Vec<Instruction>> {
            unreachable!()
        }
        async fn positions(&self, _: &dyn ChainClient, _: &Pubkey) -> Result<Vec<Lp>> {
            Ok(self.held.clone())
        }
        async fn position(
            &self,
            _: &dyn ChainClient,
            _: &Pubkey,
            id: &Pubkey,
        ) -> Result<Option<Lp>> {
            Ok(self.held.iter().find(|lp| lp.id == *id).cloned())
        }
        async fn deposit(
            &self,
            _: &dyn ChainClient,
            _: &Pubkey,
            _: Option<&Pubkey>,
            _: &Pubkey,
            _: &Deposit,
        ) -> SeatResult<(Vec<Instruction>, bool)> {
            unreachable!()
        }
        async fn withdraw(
            &self,
            _: &dyn ChainClient,
            _: &Pubkey,
            _: &Lp,
            _: f64,
            _: Option<f64>,
            _: Option<f64>,
        ) -> SeatResult<Vec<Instruction>> {
            unreachable!()
        }
    }

    fn lp(base: f64, fees_base: f64) -> Lp {
        Lp {
            id: Pubkey::new_unique(),
            range: Some(PriceRange {
                lower: 130.0,
                upper: 150.0,
            }),
            base,
            quote: 100.0,
            fees_base,
            fees_quote: 0.0,
            live: true,
        }
    }

    fn amm_seat(held: Vec<Lp>) -> PoolSeat {
        PoolSeat {
            market: "orca:amm:SPCX-USDC".into(),
            kind: Kind::Amm,
            pool: Arc::new(Stub {
                assets: assets(),
                held,
            }),
            chain: Arc::new(RpcChainClient::new("http://127.0.0.1:1")),
            signer: Keypair::new(),
            opened: AtomicU64::new(0),
        }
    }

    #[tokio::test]
    async fn a_deposit_that_takes_base_is_a_buy_and_a_withdrawal_that_returns_it_a_sell() {
        let held = lp(2.0, 0.0);
        let id = held.id.to_string();
        let seat = amm_seat(vec![held, lp(0.0, 0.0), lp(0.0, 0.01)]);
        let add =
            |base, range: Option<(f64, f64)>, position: Option<String>| Action::AddLiquidity {
                base,
                quote: 100.0,
                range: range.map(|(lower, upper)| PriceRange { lower, upper }),
                position,
                shape: None,
            };
        let remove = |position: Option<String>, share| Action::RemoveLiquidity {
            position,
            share,
            min_base: None,
            min_quote: None,
        };
        // A range over the price, or the whole curve, takes base in.
        let buy = Some(CoreSide::Buy);
        assert_eq!(
            seat.base_moved(&add(1.0, Some((130.0, 150.0)), None)).await,
            Ok(buy)
        );
        assert_eq!(seat.base_moved(&add(1.0, None, None)).await, Ok(buy));
        // A range below it takes quote only, and no base asked takes none.
        assert_eq!(
            seat.base_moved(&add(1.0, Some((100.0, 130.0)), None)).await,
            Ok(None)
        );
        assert_eq!(
            seat.base_moved(&add(0.0, Some((130.0, 150.0)), None)).await,
            Ok(None)
        );
        // Into a position, the position's range decides.
        assert_eq!(
            seat.base_moved(&add(1.0, None, Some(id.clone()))).await,
            Ok(buy)
        );
        // Taking out a position that holds base, or fees in base, sells.
        let sell = Some(CoreSide::Sell);
        assert_eq!(
            seat.base_moved(&remove(Some(id.clone()), 0.5)).await,
            Ok(sell)
        );
        assert_eq!(seat.base_moved(&remove(Some(id), 0.0)).await, Ok(None));
        // Not the only position: it can't say which, and the order fails on
        // its own.
        assert!(seat.base_moved(&remove(None, 1.0)).await.is_err());
        let fees_only = amm_seat(vec![lp(0.0, 0.01)]);
        assert_eq!(fees_only.base_moved(&remove(None, 0.0)).await, Ok(sell));
        let empty = amm_seat(vec![lp(0.0, 0.0)]);
        assert_eq!(empty.base_moved(&remove(None, 1.0)).await, Ok(None));
        // A swap moves no liquidity.
        let swap = Action::Swap {
            side: Side::Bid,
            amount: 1.0,
            unit: AmountUnit::Base,
            limit: 150.0,
        };
        assert_eq!(seat.base_moved(&swap).await, Ok(None));
    }
}
