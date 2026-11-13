//! Pools: what Raydium's and Meteora's markets share.
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
//! [`super::raydium`], Meteora DLMM's in [`super::meteora`]. What is the
//! same for both is here: a market's top (the price of a small trade each
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
    /// The position's id: its NFT's mint (Raydium), its account (Meteora).
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

    /// The pool as one read leaves it, with the session's clock: what its
    /// swaps would do now, quoted exactly ([`Loaded`]).
    async fn load(&self, chain: &dyn ChainClient) -> Result<Arc<dyn Loaded>>;
}

/// What one swap through a pool does, in atoms, exactly as its program
/// would execute it on the state it was quoted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    /// The base goes in (a sale of the base).
    pub base_in: bool,
    pub exact_in: bool,
    /// What leaves the payer, the fee included, and what it receives.
    pub amount_in: u64,
    pub amount_out: u64,
    /// The pool's fee, in atoms of `fee_mint`.
    pub fee: u64,
    pub fee_mint: Pubkey,
    /// Initialized ticks or bins it crossed.
    pub crossed: u32,
}

/// A pool read once, at a slot: its swaps quoted exactly, as its program
/// computes them, and built, with no further reads. A router's search is a
/// function of these alone.
pub trait Loaded: Send + Sync {
    fn assets(&self) -> &PoolAssets;

    /// The pool's kind, as Jupiter labels it (`Raydium CLMM`).
    fn label(&self) -> &'static str;

    /// A swap of `amount` atoms: in of the token in (`base_in`: the base),
    /// or, not `exact_in`, out of the other.
    fn quote(&self, base_in: bool, amount: u64, exact_in: bool) -> SeatResult<Quote>;

    /// `owner`'s swap as `quote` quoted it, `threshold` its least out (or
    /// most in): one instruction, naming every tick or bin array the quote
    /// crossed and the next one its way.
    fn swap(&self, owner: &Pubkey, quote: &Quote, threshold: u64) -> SeatResult<Instruction>;

    /// The accounts its swaps name, for an address lookup table.
    fn accounts(&self) -> Vec<Pubkey>;

    /// Compute units a swap that crossed `crossed` ticks or bins needs, with
    /// room to spare. [`Loaded::quote`] refuses a swap past
    /// [`MAX_SWAP_COMPUTE_UNITS`].
    fn compute_units(&self, crossed: u32) -> u32;
}

/// The most compute units one quoted swap may need: a swap that crosses more
/// ticks or bins than this pays for doesn't fit a transaction beside
/// another leg, and is refused as too big.
pub const MAX_SWAP_COMPUTE_UNITS: u32 = 1_000_000;

/// `quote`, unless its swap needs more than [`MAX_SWAP_COMPUTE_UNITS`].
pub(crate) fn within_compute(pool: &dyn Loaded, quote: Quote) -> SeatResult<Quote> {
    if pool.compute_units(quote.crossed) > MAX_SWAP_COMPUTE_UNITS {
        return Err(format!(
            "it crosses {} ticks or bins: more than one swap's compute allows",
            quote.crossed
        ));
    }
    Ok(quote)
}

/// The accounts at `addresses` that exist, by address, read in one call
/// per hundred.
pub(crate) async fn read_accounts(
    chain: &dyn ChainClient,
    addresses: &[Pubkey],
) -> Result<Vec<(Pubkey, Vec<u8>)>> {
    let mut found = Vec::new();
    for chunk in addresses.chunks(100) {
        let accounts = chain
            .get_multiple_accounts(chunk)
            .await
            .map_err(chain_error("a pool's arrays"))?;
        found.extend(
            chunk
                .iter()
                .zip(accounts)
                .filter_map(|(address, account)| account.map(|a| (*address, a.data))),
        );
    }
    Ok(found)
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

/// A transaction's swap through a pool, from its vaults' deltas: the
/// taker's side, base and quote, in atoms. `None` for a transaction that
/// moved both vaults the same way (liquidity in or out) or neither.
fn vault_swap(
    summary: &mtx_kit::chain::TransactionSummary,
    assets: &PoolAssets,
) -> Option<(CoreSide, u64, u64)> {
    let delta = |vault: &Pubkey| -> i128 {
        summary
            .token_balance_changes
            .iter()
            .filter(|c| c.account == *vault)
            .map(|c| i128::from(c.post_amount) - i128::from(c.pre_amount))
            .sum()
    };
    let (base, quote) = (delta(&assets.base_vault), delta(&assets.quote_vault));
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

/// The pool `named` names on `venue` (`raydium` or `meteora`), read through
/// `chain`.
pub async fn connect(
    venue: &str,
    chain: &dyn ChainClient,
    named: &NamedMarket,
) -> Result<Arc<dyn Pool>> {
    Ok(match venue {
        "raydium" => Arc::new(super::raydium::ClmmPool::connect(chain, named).await?),
        "meteora" => Arc::new(super::meteora::DlmmPool::connect(chain, named).await?),
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
    }

    /// Every routed pool's quotes are what its program pays: on the route
    /// snapshot, a grader-owned trader holding every pool's tokens, 50
    /// seeded sizes each way on each pool (exactly in, and every fifth also
    /// exactly out), each quote checked against a simulation of its swap on
    /// the same state, to the atom.
    #[tokio::test(flavor = "multi_thread")]
    async fn quotes_are_what_the_programs_pay() {
        use mtx_kit::token::associated_token_address;
        use solana_transaction::Transaction;

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../snapshots/spcx-route-2026-11-13")
            .canonicalize()
            .unwrap();
        let (surfnet, _) = crate::snapshot::start_snapshot_stepped(&dir).await.unwrap();
        let internal = crate::guard::Guard::internal(surfnet.rpc_url())
            .await
            .unwrap();
        let url = internal.url().to_string();
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(&url));
        let trader = Keypair::new();
        let owner = trader.pubkey();
        tokio::task::block_in_place(|| {
            let rpc = mtx_kit::rpc_client(&url);
            crate::spot::fund(&surfnet, &rpc, &trader, 100_000.0, 10_000_000.0, true).unwrap();
            crate::spot::fund_mint(&surfnet, &owner, &SPCX_MINT, 100_000_000_000).unwrap();
        });
        // A little linear congruential generator: the same sizes every run.
        let mut seed = 0x18u64;
        let mut unit = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut checked = 0;
        for venue in crate::spot::POOLS {
            for named in crate::venue::books::named_markets(venue).unwrap() {
                let pool = connect(venue, chain.as_ref(), named).await.unwrap();
                let loaded = pool.load(chain.as_ref()).await.unwrap();
                let assets = loaded.assets().clone();
                let accounts = [
                    associated_token_address(&owner, &assets.base_mint, &assets.base_program),
                    associated_token_address(&owner, &assets.quote_mint, &assets.quote_program),
                ];
                let balance = |data: Option<&[u8]>| {
                    data.map_or(0, |d| u64::from_le_bytes(d[64..72].try_into().unwrap()))
                };
                let before = chain.get_multiple_accounts(&accounts).await.unwrap();
                for base_in in [true, false] {
                    let decimals = if base_in {
                        assets.base.decimals
                    } else {
                        assets.quote.decimals
                    };
                    let (mut exact, mut tried) = (0, 0);
                    let mut most_used = (0, 0);
                    for k in 0..50 {
                        // From a thousandth of a unit to ten thousand units.
                        let units = 10f64.powf(-3.0 + 7.0 * unit());
                        let mut amount = (units * 10f64.powi(i32::from(decimals))) as u64;
                        let exact_in = k % 5 != 4;
                        // Halved until the pool's loaded liquidity takes it.
                        let mut quoted = loaded.quote(base_in, amount, exact_in);
                        while quoted.is_err() && amount > 1 {
                            amount /= 2;
                            quoted = loaded.quote(base_in, amount, exact_in);
                        }
                        let Ok(quote) = quoted else {
                            continue;
                        };
                        tried += 1;
                        let threshold = if exact_in { 0 } else { u64::MAX };
                        let ix = loaded.swap(&owner, &quote, threshold).unwrap();
                        let tx = Transaction::new_with_payer(
                            &[crate::raw::compute_limit(1_400_000), ix],
                            Some(&owner),
                        );
                        let outcome = chain
                            .simulate(&VersionedTransaction::from(tx), &accounts)
                            .await
                            .unwrap();
                        assert!(
                            outcome.error.is_none(),
                            "{venue}:{} {base_in} {amount} {exact_in}: {:?} {:?}",
                            named.symbol,
                            outcome.error,
                            outcome.logs
                        );
                        let after: Vec<u64> = outcome
                            .accounts
                            .iter()
                            .map(|(_, a)| balance(a.as_ref().map(|a| a.data.as_slice())))
                            .collect();
                        let (base_before, quote_before) = (
                            balance(before[0].as_ref().map(|a| a.data.as_slice())),
                            balance(before[1].as_ref().map(|a| a.data.as_slice())),
                        );
                        let (spent, got) = if base_in {
                            (base_before - after[0], after[1] - quote_before)
                        } else {
                            (quote_before - after[1], after[0] - base_before)
                        };
                        assert!(
                            spent.abs_diff(quote.amount_in) <= 1
                                && got.abs_diff(quote.amount_out) <= 1,
                            "{venue}:{} base in {base_in}, {amount} exact in {exact_in}: quoted {} for {}, the program took {spent} for {got}",
                            named.symbol,
                            quote.amount_in,
                            quote.amount_out
                        );
                        exact += usize::from(spent == quote.amount_in && got == quote.amount_out);
                        // The compute a route asks for this leg covers it.
                        let used = outcome.units_consumed.unwrap_or(u64::MAX);
                        assert!(
                            used <= u64::from(loaded.compute_units(quote.crossed)),
                            "{venue}:{} {amount}: {used} units for {} crossed, {} asked",
                            named.symbol,
                            quote.crossed,
                            loaded.compute_units(quote.crossed)
                        );
                        most_used = most_used.max((used, quote.crossed));
                    }
                    assert!(
                        tried >= 40,
                        "{venue}:{} {base_in}: only {tried} sizes quoted",
                        named.symbol
                    );
                    println!(
                        "{venue}:{} base in {base_in}: {exact}/{tried} to the atom, at most {} units ({} crossed)",
                        named.symbol, most_used.0, most_used.1
                    );
                    checked += tried;
                }
            }
        }
        assert!(checked >= 480);
    }
}
