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
//! way: the pool's price less or plus its fee), its fills and liquidity
//! moves (read instruction by instruction from the token transfers each of
//! the pool's instructions makes to and from its vaults: base in and quote
//! out is a sale, the reverse a purchase, and one way only a deposit or a
//! withdrawal; each is the account's whose tokens, position or signature it
//! takes), the valuation of a position (its assets at the reference prices,
//! not the pool's own, so a pool moved by its holder can't inflate it), and
//! the seats, which send each order as one transaction.

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
    chain::{
        ChainClient, ExecutedInstruction, InstructionGroup, RpcChainClient, TransactionPlan,
        TransactionSummary, token,
    },
    protocols::{NamedMarket, spot::MarketFill},
    token::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID},
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
    /// The pool's program.
    pub program: Pubkey,
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

    /// The owners of the positions among `accounts`, where a position names
    /// its owner in its own account (Meteora's). A position held by its NFT
    /// (Raydium's, Orca's) is the owner's of the token account holding it,
    /// which its instructions pass, so it needs none.
    async fn position_owners(
        &self,
        chain: &dyn ChainClient,
        accounts: &[Pubkey],
    ) -> Result<Vec<Pubkey>> {
        let _ = (chain, accounts);
        Ok(Vec::new())
    }
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

    /// Every transaction through the pool since `after_slot` that
    /// succeeded, oldest first.
    async fn transactions(&self, after_slot: u64) -> Result<Vec<TransactionSummary>> {
        let chain = self.chain.as_ref();
        let mut found = Vec::new();
        for (signature, _) in chain
            .signatures_for_address(&self.assets().address, after_slot)
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
            if summary.error.is_none() {
                found.push(summary);
            }
        }
        Ok(found)
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

/// What one of a pool's instructions moved through its vaults, from the
/// token transfers it made, in atoms, and the accounts it was made for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct VaultFlow {
    /// Where it ran in its transaction, among the pool's instructions.
    index: u16,
    base_in: u64,
    base_out: u64,
    quote_in: u64,
    quote_out: u64,
    /// The transaction's signers, the owners of the token accounts the
    /// instruction passes (a wallet's, a position NFT's), and its accounts,
    /// for the owners of positions held in an account of their own.
    parties: Vec<Pubkey>,
    accounts: Vec<Pubkey>,
}

impl VaultFlow {
    /// The taker's side, base and quote, if it was a swap: base in and quote
    /// out is a sale, base out and quote in a purchase.
    fn swap(&self) -> Option<(CoreSide, u64, u64)> {
        match (self.base_in, self.base_out, self.quote_in, self.quote_out) {
            (base, 0, 0, quote) if base > 0 && quote > 0 => Some((CoreSide::Sell, base, quote)),
            (0, base, quote, 0) if base > 0 && quote > 0 => Some((CoreSide::Buy, base, quote)),
            _ => None,
        }
    }

    /// The sides of the base a deposit or withdrawal moved, for the
    /// mandate's sides rule. Base into the pool is a buy, base out of it
    /// (principal or fees) a sell. A deposit of quote alone (a range below
    /// the price) buys base as the price falls, so it is a buy too; a
    /// deposit of base alone (a range above it) sells as the price rises, so
    /// it is a sell as well as a buy.
    fn liquidity_sides(&self) -> Vec<CoreSide> {
        if self.swap().is_some() {
            return Vec::new();
        }
        let deposit = self.base_out == 0 && self.quote_out == 0;
        let mut sides = Vec::new();
        if self.base_in > 0 || (deposit && self.quote_in > 0) {
            sides.push(CoreSide::Buy);
        }
        if self.base_out > 0 || (deposit && self.base_in > 0 && self.quote_in == 0) {
            sides.push(CoreSide::Sell);
        }
        sides
    }
}

/// A token transfer's source, destination and amount, if `ix` is one.
fn token_transfer(ix: &ExecutedInstruction) -> Option<(Pubkey, Pubkey, u64)> {
    if ix.program != TOKEN_PROGRAM_ID && ix.program != TOKEN_2022_PROGRAM_ID {
        return None;
    }
    let amount = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(
            ix.data.get(at..at + 8)?.try_into().ok()?,
        ))
    };
    let account = |i: usize| ix.accounts.get(i).copied();
    match ix.data.first()? {
        // Transfer: source, destination, authority.
        3 => Some((account(0)?, account(1)?, amount(1)?)),
        // TransferChecked: source, mint, destination, authority.
        12 => Some((account(0)?, account(2)?, amount(1)?)),
        // Token-2022's TransferCheckedWithFee, the same accounts.
        26 if ix.data.get(1) == Some(&1) => Some((account(0)?, account(2)?, amount(2)?)),
        _ => None,
    }
}

/// Every instruction of the pool's in a transaction (run by the
/// transaction or by a program it called), and what each moved through the
/// pool's vaults: a transfer counts toward the pool instruction that made
/// it, never toward the transaction's net, so a swap and a deposit in one
/// transaction are each seen.
fn vault_flows(summary: &TransactionSummary, assets: &PoolAssets) -> Vec<VaultFlow> {
    let signers = &summary.account_keys[..summary.signers.min(summary.account_keys.len())];
    let owner_of = |account: &Pubkey| {
        summary
            .token_balance_changes
            .iter()
            .find(|c| c.account == *account)
            .and_then(|c| c.owner)
    };
    let is_pool = |ix: &ExecutedInstruction| {
        ix.program == assets.program && ix.accounts.contains(&assets.address)
    };
    // Where the node gives every instruction's depth, a transfer is the
    // pool's when the pool's instruction called it; where it doesn't, when
    // it follows the pool's instruction within the same instruction of the
    // transaction's.
    let nested = summary.instructions.iter().all(|ix| ix.depth.is_some());
    let mut flows: Vec<VaultFlow> = Vec::new();
    // Per instruction run, the flow it adds to, if it is the pool's.
    let mut flow_of: Vec<Option<usize>> = Vec::with_capacity(summary.instructions.len());
    for (i, ix) in summary.instructions.iter().enumerate() {
        if is_pool(ix) {
            let mut parties = signers.to_vec();
            for owner in ix.accounts.iter().filter_map(owner_of) {
                if !parties.contains(&owner) {
                    parties.push(owner);
                }
            }
            flow_of.push(Some(flows.len()));
            flows.push(VaultFlow {
                index: u16::try_from(flows.len()).unwrap_or(u16::MAX),
                parties,
                accounts: ix.accounts.clone(),
                ..VaultFlow::default()
            });
            continue;
        }
        flow_of.push(None);
        let Some((source, destination, amount)) = token_transfer(ix) else {
            continue;
        };
        let caller = summary.instructions[..i]
            .iter()
            .enumerate()
            .rev()
            .take_while(|(_, before)| before.outer == ix.outer)
            .find(|(_, before)| {
                if nested {
                    before.depth < ix.depth
                } else {
                    is_pool(before)
                }
            })
            .and_then(|(j, _)| flow_of[j]);
        let Some(flow) = caller.map(|f| &mut flows[f]) else {
            continue;
        };
        let add = |total: &mut u64| *total = total.saturating_add(amount);
        match (source, destination) {
            (_, to) if to == assets.base_vault => add(&mut flow.base_in),
            (_, to) if to == assets.quote_vault => add(&mut flow.quote_in),
            (from, _) if from == assets.base_vault => add(&mut flow.base_out),
            (from, _) if from == assets.quote_vault => add(&mut flow.quote_out),
            _ => {}
        }
    }
    flows
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
    /// instrument: the pool is every trade's maker, and the taker is
    /// `owner` where the swap took its tokens or its signature, whoever paid
    /// the fee, and otherwise the transaction's fee payer.
    async fn fills(
        &self,
        after_slot: u64,
        owner: &Pubkey,
    ) -> Result<Vec<(MarketFill, Vec<Pubkey>)>> {
        if self.instrument.kind != Kind::Spot {
            return Ok(Vec::new());
        }
        let assets = self.assets();
        let id = InstrumentId::new(self.instrument.to_string());
        let units = |atoms: u64, asset: &Asset| {
            Decimal::from(atoms) / Decimal::from(10u64.pow(asset.decimals.into()))
        };
        let mut fills = Vec::new();
        for summary in self.transactions(after_slot).await? {
            let Some(payer) = summary.account_keys.first().copied() else {
                continue;
            };
            let first = u128::from_le_bytes(
                summary.signature.as_ref()[..16]
                    .try_into()
                    .expect("a signature has 64 bytes"),
            );
            for flow in vault_flows(&summary, assets) {
                let Some((side, base, quote)) = flow.swap() else {
                    continue;
                };
                let quantity = units(base, &assets.base);
                fills.push((
                    MarketFill {
                        slot: summary.slot,
                        instrument: id.clone(),
                        sequence: first.wrapping_add(u128::from(flow.index)),
                        taker: if flow.parties.contains(owner) {
                            *owner
                        } else {
                            payer
                        },
                        maker: assets.address,
                        taker_side: side,
                        price: units(quote, &assets.quote) / quantity,
                        quantity,
                        unix_timestamp: summary.block_time.map_or(0, |t| t.timestamp()),
                    },
                    vec![*owner],
                ));
            }
        }
        Ok(fills)
    }

    /// The sides of the base `owner`'s deposits and withdrawals through the
    /// pool moved since `after_slot`, for the amm instrument: read from the
    /// chain instruction by instruction, so a transaction the bot builds
    /// itself counts as much as its seat's, whoever pays its fee, and a
    /// deposit beside a swap is still a deposit.
    async fn liquidity_moves(&self, after_slot: u64, owner: &Pubkey) -> Result<Vec<CoreSide>> {
        if self.instrument.kind != Kind::Amm {
            return Ok(Vec::new());
        }
        let mut moves = Vec::new();
        for summary in self.transactions(after_slot).await? {
            for flow in vault_flows(&summary, self.assets()) {
                let sides = flow.liquidity_sides();
                if sides.is_empty() {
                    continue;
                }
                if !flow.parties.contains(owner)
                    && !self
                        .pool
                        .position_owners(self.chain.as_ref(), &flow.accounts)
                        .await?
                        .contains(owner)
                {
                    continue;
                }
                for side in sides {
                    if !moves.contains(&side) {
                        moves.push(side);
                    }
                }
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

    /// What the chain audit will read of the order ([`VaultFlow`]): a
    /// deposit that takes base in is a buy, and so is one of quote alone (a
    /// range below the price, which buys as the price falls); one of base
    /// alone (a range above it) is a sell too. A withdrawal that returns
    /// base, or fees in base, is a sell.
    async fn base_moved(&self, action: &Action) -> SeatResult<Vec<CoreSide>> {
        if self.kind != Kind::Amm {
            return Ok(Vec::new());
        }
        match action {
            Action::AddLiquidity {
                base,
                quote,
                range,
                position,
                ..
            } => {
                let range = match position {
                    Some(id) => self.position(Some(id)).await?.range,
                    None => *range,
                };
                let PoolPrice { price, .. } = self
                    .pool
                    .price(self.chain.as_ref())
                    .await
                    .map_err(|e| e.to_string())?;
                let (base, quote) = (*base > 0.0, *quote > 0.0);
                let above = range.is_none_or(|r| r.upper > price);
                let below = range.is_none_or(|r| r.lower < price);
                let mut sides = Vec::new();
                if (base && above) || (quote && below) {
                    sides.push(CoreSide::Buy);
                }
                if base && range.is_some_and(|r| r.lower >= price) {
                    sides.push(CoreSide::Sell);
                }
                Ok(sides)
            }
            Action::RemoveLiquidity {
                position, share, ..
            } => {
                let held = self.position(position.as_deref()).await?;
                Ok(
                    if (*share > 0.0 && held.base > 0.0) || held.fees_base > 0.0 {
                        vec![CoreSide::Sell]
                    } else {
                        Vec::new()
                    },
                )
            }
            _ => Ok(Vec::new()),
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
            program: Pubkey::new_unique(),
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

    /// A token transfer of `amount` from `from` to `to`, as Token-2022's
    /// `transfer_checked`, run at `depth` in instruction `outer`.
    fn transfer(
        outer: u8,
        depth: u32,
        from: Pubkey,
        to: Pubkey,
        amount: u64,
    ) -> ExecutedInstruction {
        let mut data = vec![12];
        data.extend(amount.to_le_bytes());
        data.push(6);
        ExecutedInstruction {
            outer,
            depth: Some(depth),
            program: TOKEN_2022_PROGRAM_ID,
            accounts: vec![from, Pubkey::new_unique(), to, Pubkey::new_unique()],
            data,
        }
    }

    /// An instruction of the pool's, passing `accounts` beside the pool.
    fn pool_ix(
        assets: &PoolAssets,
        outer: u8,
        depth: u32,
        accounts: &[Pubkey],
    ) -> ExecutedInstruction {
        ExecutedInstruction {
            outer,
            depth: Some(depth),
            program: assets.program,
            accounts: [&[assets.address][..], accounts].concat(),
            data: vec![0; 8],
        }
    }

    /// A transaction paid for and signed by `signers`, running `instructions`,
    /// where `owners` own the token accounts named.
    fn summary(
        signers: &[Pubkey],
        owners: &[(Pubkey, Pubkey)],
        instructions: Vec<ExecutedInstruction>,
    ) -> TransactionSummary {
        TransactionSummary {
            signature: Default::default(),
            slot: 1,
            block_time: None,
            fee_lamports: 0,
            error: None,
            logs: Vec::new(),
            account_keys: signers.to_vec(),
            pre_balances: Vec::new(),
            post_balances: Vec::new(),
            token_balance_changes: owners
                .iter()
                .map(|(account, owner)| mtx_kit::chain::TokenBalanceChange {
                    account: *account,
                    mint: Pubkey::new_unique(),
                    owner: Some(*owner),
                    pre_amount: 0,
                    post_amount: 0,
                })
                .collect(),
            inner_instructions: Vec::new(),
            signers: signers.len(),
            instructions,
        }
    }

    #[test]
    fn each_pool_instruction_is_read_from_its_own_transfers() {
        let assets = assets();
        let (base, quote) = (assets.base_vault, assets.quote_vault);
        let (owner, wallet_base, wallet_quote) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let owners = [(wallet_base, owner), (wallet_quote, owner)];
        let flows = |instructions| {
            vault_flows(&summary(&[owner], &owners, instructions), &assets)
                .iter()
                .map(|f| (f.swap(), f.liquidity_sides()))
                .collect::<Vec<_>>()
        };
        use CoreSide::{Buy, Sell};
        // A sale and a purchase, one swap each.
        assert_eq!(
            flows(vec![
                pool_ix(&assets, 0, 1, &[wallet_base, wallet_quote]),
                transfer(0, 2, wallet_base, base, 2),
                transfer(0, 2, quote, wallet_quote, 280),
                pool_ix(&assets, 1, 1, &[wallet_base, wallet_quote]),
                transfer(1, 2, wallet_quote, quote, 141),
                transfer(1, 2, base, wallet_base, 1),
            ]),
            vec![
                (Some((Sell, 2, 280)), vec![]),
                (Some((Buy, 1, 141)), vec![])
            ]
        );
        // A sale of 2 and a deposit of 1 base in one transaction: the vaults
        // net to a sale of 3, but each instruction is read on its own, and
        // the deposit is a buy.
        assert_eq!(
            flows(vec![
                pool_ix(&assets, 0, 1, &[wallet_base, wallet_quote]),
                transfer(0, 2, wallet_base, base, 2),
                transfer(0, 2, quote, wallet_quote, 280),
                pool_ix(&assets, 1, 1, &[wallet_base, wallet_quote]),
                transfer(1, 2, wallet_base, base, 1),
                transfer(1, 2, wallet_quote, quote, 100),
            ]),
            vec![(Some((Sell, 2, 280)), vec![]), (None, vec![Buy])]
        );
        // The same, run by another program through the pool: the transfers
        // are the pool's, one level down.
        assert_eq!(
            flows(vec![
                ExecutedInstruction {
                    outer: 0,
                    depth: Some(1),
                    program: Pubkey::new_unique(),
                    accounts: Vec::new(),
                    data: Vec::new(),
                },
                pool_ix(&assets, 0, 2, &[wallet_base, wallet_quote]),
                transfer(0, 3, wallet_base, base, 2),
                transfer(0, 3, quote, wallet_quote, 280),
                pool_ix(&assets, 0, 2, &[wallet_base, wallet_quote]),
                transfer(0, 3, wallet_base, base, 1),
                // Sent by the calling program, not the pool: a gift to the
                // vault, nobody's trade.
                transfer(0, 2, wallet_quote, quote, 100),
            ]),
            vec![(Some((Sell, 2, 280)), vec![]), (None, vec![Buy, Sell])]
        );
        // A deposit of quote alone rests to buy; one of base alone, to sell
        // (and takes base in); a withdrawal of base, or of fees in base,
        // sells; one of quote alone moves no base.
        let one = |transfers: &[(Pubkey, Pubkey, u64)]| {
            let mut instructions = vec![pool_ix(&assets, 0, 1, &[wallet_base, wallet_quote])];
            instructions.extend(transfers.iter().map(|(f, t, n)| transfer(0, 2, *f, *t, *n)));
            flows(instructions).remove(0).1
        };
        assert_eq!(one(&[(wallet_quote, quote, 100)]), vec![Buy]);
        assert_eq!(one(&[(wallet_base, base, 1)]), vec![Buy, Sell]);
        assert_eq!(
            one(&[(wallet_base, base, 1), (wallet_quote, quote, 100)]),
            vec![Buy]
        );
        assert_eq!(
            one(&[(base, wallet_base, 1), (quote, wallet_quote, 100)]),
            vec![Sell]
        );
        assert_eq!(one(&[(base, wallet_base, 1)]), vec![Sell]);
        assert_eq!(one(&[(quote, wallet_quote, 100)]), Vec::<CoreSide>::new());
        // A transfer to the vault outside any pool instruction is no one's.
        assert!(flows(vec![transfer(0, 1, wallet_base, base, 5)]).is_empty());
    }

    #[test]
    fn a_flow_is_the_owners_whoever_pays_the_fee() {
        let assets = assets();
        let (owner, payer, wallet, nft) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let deposit = |signers: &[Pubkey], owners: &[(Pubkey, Pubkey)], accounts: &[Pubkey]| {
            vault_flows(
                &summary(
                    signers,
                    owners,
                    vec![
                        pool_ix(&assets, 0, 1, accounts),
                        transfer(0, 2, wallet, assets.base_vault, 1),
                    ],
                ),
                &assets,
            )
            .remove(0)
            .parties
        };
        // A throwaway pays, the owner co-signs as the position's authority.
        assert!(deposit(&[payer, owner], &[], &[]).contains(&owner));
        // The owner doesn't sign (a delegate moves its tokens), but the
        // tokens are its own.
        assert!(deposit(&[payer], &[(wallet, owner)], &[wallet]).contains(&owner));
        // The position's NFT is the owner's, in whatever token account.
        assert!(deposit(&[payer], &[(nft, owner)], &[nft]).contains(&owner));
        // Nothing of the owner's: not its.
        assert!(!deposit(&[payer], &[(wallet, payer)], &[wallet]).contains(&owner));
    }

    #[test]
    fn without_depths_a_transfer_is_the_pool_instructions_it_follows() {
        let assets = assets();
        let owner = Pubkey::new_unique();
        let wallet = Pubkey::new_unique();
        let mut instructions = vec![
            pool_ix(&assets, 0, 1, &[wallet]),
            transfer(0, 2, wallet, assets.base_vault, 2),
            transfer(0, 2, assets.quote_vault, wallet, 280),
            pool_ix(&assets, 1, 1, &[wallet]),
            transfer(1, 2, wallet, assets.base_vault, 1),
        ];
        for ix in &mut instructions {
            if ix.depth == Some(2) {
                ix.depth = None;
            }
        }
        let flows = vault_flows(&summary(&[owner], &[], instructions), &assets);
        assert_eq!(flows[0].swap(), Some((CoreSide::Sell, 2, 280)));
        assert_eq!(
            flows[1].liquidity_sides(),
            vec![CoreSide::Buy, CoreSide::Sell]
        );
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
        use CoreSide::{Buy, Sell};
        let held = lp(2.0, 0.0);
        let id = held.id.to_string();
        let seat = amm_seat(vec![held, lp(0.0, 0.0), lp(0.0, 0.01)]);
        let add = |base, quote, range: Option<(f64, f64)>, position: Option<String>| {
            Action::AddLiquidity {
                base,
                quote,
                range: range.map(|(lower, upper)| PriceRange { lower, upper }),
                position,
                shape: None,
            }
        };
        let remove = |position: Option<String>, share| Action::RemoveLiquidity {
            position,
            share,
            min_base: None,
            min_quote: None,
        };
        let moved = |action| {
            let seat = &seat;
            async move { seat.base_moved(&action).await }
        };
        // The pool is at 140. A range over the price, or the whole curve,
        // takes base in.
        assert_eq!(
            moved(add(1.0, 100.0, Some((130.0, 150.0)), None)).await,
            Ok(vec![Buy])
        );
        assert_eq!(moved(add(1.0, 100.0, None, None)).await, Ok(vec![Buy]));
        // A range below it takes quote alone, and rests to buy base as the
        // price falls: a buy.
        assert_eq!(
            moved(add(1.0, 100.0, Some((100.0, 130.0)), None)).await,
            Ok(vec![Buy])
        );
        assert_eq!(
            moved(add(0.0, 100.0, Some((100.0, 130.0)), None)).await,
            Ok(vec![Buy])
        );
        // A range above it takes base alone, and rests to sell it as the
        // price rises: a buy and a sell.
        assert_eq!(
            moved(add(1.0, 0.0, Some((150.0, 160.0)), None)).await,
            Ok(vec![Buy, Sell])
        );
        // Nothing asked moves nothing.
        assert_eq!(
            moved(add(0.0, 0.0, Some((130.0, 150.0)), None)).await,
            Ok(vec![])
        );
        // Into a position, the position's range decides.
        assert_eq!(
            moved(add(1.0, 100.0, None, Some(id.clone()))).await,
            Ok(vec![Buy])
        );
        // Taking out a position that holds base, or fees in base, sells.
        assert_eq!(moved(remove(Some(id.clone()), 0.5)).await, Ok(vec![Sell]));
        assert_eq!(moved(remove(Some(id), 0.0)).await, Ok(vec![]));
        // Not the only position: it can't say which, and the order fails on
        // its own.
        assert!(moved(remove(None, 1.0)).await.is_err());
        let fees_only = amm_seat(vec![lp(0.0, 0.01)]);
        assert_eq!(
            fees_only.base_moved(&remove(None, 0.0)).await,
            Ok(vec![Sell])
        );
        let empty = amm_seat(vec![lp(0.0, 0.0)]);
        assert_eq!(empty.base_moved(&remove(None, 1.0)).await, Ok(vec![]));
        // A swap moves no liquidity.
        let swap = Action::Swap {
            side: Side::Bid,
            amount: 1.0,
            unit: AmountUnit::Base,
            limit: 150.0,
        };
        assert_eq!(moved(swap).await, Ok(vec![]));
    }
}
