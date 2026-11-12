//! Simulated markets: a mint and a market on a real program, created inside a
//! snapshot build and seeded around a price.
//!
//! Mainnet has no liquidity for some assets a task needs (Ondo's tokens, the
//! depth beyond a few real pools). A recipe's `create-mint`,
//! `phoenix-v1-market` and `openbook-v2-market` actions make them on the real
//! programs, so a bot trades them with the code it uses on mainnet. Everything
//! made is flagged in the manifest (`simulated`, `created`), so a task can say
//! so.
//!
//! The market's creator, authority and maker is a keypair derived from the
//! build's private secret, the recipe's name and a label. The secret is never
//! recorded, so nobody who holds the snapshot can sign for those accounts; a
//! build given the same secret (`MTX_BUILD_SECRET`) reproduces them. Mints are
//! written straight into the surfnet with no mint authority, and the books fill
//! with a fixed ladder around the price.

use std::{path::PathBuf, str::FromStr, sync::Arc};

use mtx_core::{Decimal, Side};
use mtx_kit::{
    Instruction,
    chain::{ChainClient, RpcChainClient, token::token_program},
    protocols::{
        openbook::v2::{self as openbook, OpenBookV2Order, OpenBookV2Trader},
        phoenix::v1::{self as phoenix_v1, PhoenixV1Order, PhoenixV1Trader},
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::{AccountMeta, Transaction};
use surfpool_sdk::Surfnet;
use tracing::info;

use crate::{Error, Result};

/// The Token-2022 program.
pub const TOKEN_2022: Pubkey =
    solana_pubkey::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
/// The rent-exempt balance of an 82-byte mint.
const MINT_LAMPORTS: u64 = 1_461_600;
/// The SOL a market's creator starts with.
const CREATOR_SOL: u64 = 100_000_000_000;
/// OpenBook v2's book side and event heap sizes, which the caller allocates
/// (the program allocates the market itself).
const OPENBOOK_BOOKSIDE_LEN: usize = 90_952;
const OPENBOOK_EVENT_HEAP_LEN: usize = 91_288;
/// Phoenix v1: where the market's trees start, and the bytes per node.
const PHOENIX_TREES_OFFSET: usize = 576 + 256 + 48;
const PHOENIX_TREE_HEADER: usize = 32;
const PHOENIX_ORDER_NODE: usize = 64;
const PHOENIX_SEAT_NODE: usize = 144;
/// What the Seat Manager's deposit collector holds to seat traders: it asks
/// for at least 2,976,880 lamports when it takes the market.
const SEAT_DEPOSITS: u64 = 10_000_000;
const COMPUTE_UNITS: u32 = 1_400_000;

/// Which token program owns a mint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TokenKind {
    /// The classic SPL Token program.
    #[default]
    Spl,
    /// Token-2022, without extensions.
    #[serde(rename = "token-2022")]
    Token2022,
}

impl TokenKind {
    pub fn program(self) -> Pubkey {
        match self {
            Self::Spl => token_program(),
            Self::Token2022 => TOKEN_2022,
        }
    }
}

/// Where a simulated market's price comes from.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceInput {
    /// A price in quote units per base unit.
    pub value: Option<f64>,
    /// A calibration TOML (relative to the recipe) and the dotted key of a
    /// number in it.
    pub calibration: Option<PathBuf>,
    pub key: Option<String>,
}

/// What a `create-mint` action makes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintSpec {
    pub name: String,
    pub decimals: u8,
    #[serde(default)]
    pub program: TokenKind,
}

/// The book a market starts with: `levels` orders a side around the price,
/// each `level_size` base units, the first `half_spread_bps` from the price
/// and the rest `step_bps` apart.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ladder {
    #[serde(default = "default_levels")]
    pub levels: u32,
    #[serde(default = "default_half_spread")]
    pub half_spread_bps: u32,
    #[serde(default = "default_step")]
    pub step_bps: u32,
    /// Base units per order, as a decimal string.
    pub level_size: String,
}

fn default_levels() -> u32 {
    5
}
fn default_half_spread() -> u32 {
    5
}
fn default_step() -> u32 {
    5
}

/// What a `phoenix-v1-market` action makes. Sizes are in token atoms, so the
/// grid is explicit.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoenixMarketSpec {
    pub name: String,
    pub base: String,
    pub quote: String,
    pub base_lot_atoms: u64,
    pub quote_lot_atoms: u64,
    /// One tick, in quote atoms per base unit.
    pub tick_quote_atoms_per_base_unit: u64,
    #[serde(default = "default_tree")]
    pub bids_size: u64,
    #[serde(default = "default_tree")]
    pub asks_size: u64,
    #[serde(default = "default_seats")]
    pub num_seats: u64,
    #[serde(default)]
    pub taker_fee_bps: u16,
    pub price: PriceInput,
    pub ladder: Ladder,
}

/// The sizes of Phoenix v1's SOL/USDC market. The program only runs the
/// combinations it was compiled for.
fn default_tree() -> u64 {
    4096
}
fn default_seats() -> u64 {
    8321
}

/// What an `openbook-v2-market` action makes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenbookMarketSpec {
    /// The recipe's name for it; the program's own name is at most 16 bytes.
    pub name: String,
    pub base: String,
    pub quote: String,
    pub base_lot_atoms: i64,
    pub quote_lot_atoms: i64,
    #[serde(default)]
    pub maker_fee: i64,
    #[serde(default)]
    pub taker_fee: i64,
    pub price: PriceInput,
    pub ladder: Ladder,
}

/// A keypair derived from a namespace and a label. The namespace is the
/// build's private secret and the recipe's name (see [`crate::recipe`]): with
/// the same secret a rebuild gets the same keys, and without it nobody can
/// derive them, since the recipe's name ships with the snapshot.
pub fn derived_keypair(namespace: &str, label: &str) -> Keypair {
    let seed = Sha256::digest(format!("mtx-recipe/{namespace}/{label}"));
    Keypair::new_from_array(seed.into())
}

/// An SPL mint account's bytes: no mint authority and no freeze authority, so
/// nobody can mint more (balances are set by cheatcode).
pub fn mint_data(decimals: u8) -> [u8; 82] {
    let mut data = [0u8; 82];
    data[44] = decimals;
    data[45] = 1;
    data
}

/// Writes a mint into the surfnet.
pub fn create_mint(surfnet: &Surfnet, recipe: &str, spec: &MintSpec) -> Result<Pubkey> {
    let address = derived_keypair(recipe, &format!("mint/{}", spec.name)).pubkey();
    surfnet.cheatcodes().set_account(
        &address,
        MINT_LAMPORTS,
        &mint_data(spec.decimals),
        &spec.program.program(),
    )?;
    info!(name = %spec.name, %address, "simulated mint created");
    Ok(address)
}

/// A market account's size for Phoenix v1's tree capacities.
pub fn phoenix_market_len(bids: u64, asks: u64, seats: u64) -> usize {
    PHOENIX_TREES_OFFSET
        + 3 * PHOENIX_TREE_HEADER
        + (bids + asks) as usize * PHOENIX_ORDER_NODE
        + seats as usize * PHOENIX_SEAT_NODE
}

/// Phoenix's InitializeMarket data: tag 100 and the Borsh `InitializeParams`.
#[allow(clippy::too_many_arguments)]
pub fn phoenix_initialize_data(
    bids: u64,
    asks: u64,
    seats: u64,
    quote_lots_per_quote_unit: u64,
    tick_quote_lots_per_base_unit: u64,
    base_lots_per_base_unit: u64,
    taker_fee_bps: u16,
    fee_collector: &Pubkey,
) -> Vec<u8> {
    let mut data = vec![100u8];
    for v in [
        bids,
        asks,
        seats,
        quote_lots_per_quote_unit,
        tick_quote_lots_per_base_unit,
        base_lots_per_base_unit,
    ] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    data.extend_from_slice(&taker_fee_bps.to_le_bytes());
    data.extend_from_slice(fee_collector.as_ref());
    data.push(0); // raw_base_units_per_base_unit: None
    data
}

/// OpenBook v2's CreateMarket data: the Anchor discriminator and arguments.
pub fn openbook_create_data(
    name: &str,
    quote_lot: i64,
    base_lot: i64,
    maker_fee: i64,
    taker_fee: i64,
) -> Vec<u8> {
    let mut data = Sha256::digest(b"global:create_market")[..8].to_vec();
    data.extend_from_slice(&(name.len() as u32).to_le_bytes());
    data.extend_from_slice(name.as_bytes());
    data.extend_from_slice(&0f32.to_le_bytes()); // oracle conf_filter
    data.push(0); // max_staleness_slots: None
    for v in [quote_lot, base_lot, maker_fee, taker_fee, 0i64] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    data
}

/// A price and a quantity, in quote units per base unit and base units.
pub type Order = (Decimal, Decimal);

/// The ladder's orders: `(price, quantity)` for the bids (best first) and the
/// asks, on the tick and lot grid. A bid rounds down and an ask up, so a ladder
/// never crosses.
pub fn ladder_orders(
    price: Decimal,
    ladder: &Ladder,
    tick: Decimal,
    lot: Decimal,
) -> Result<(Vec<Order>, Vec<Order>)> {
    let size = Decimal::from_str(&ladder.level_size)
        .map_err(|e| invalid(format!("ladder level_size {:?}: {e}", ladder.level_size)))?;
    let quantity = (size / lot).floor() * lot;
    if quantity <= Decimal::ZERO || tick <= Decimal::ZERO {
        return Err(invalid(format!(
            "level_size {size} is under one lot ({lot}) or the tick is zero"
        )));
    }
    let bps = Decimal::new(1, 4);
    let (mut bids, mut asks) = (Vec::new(), Vec::new());
    for i in 0..ladder.levels {
        let away =
            (Decimal::from(ladder.half_spread_bps) + Decimal::from(i * ladder.step_bps)) * bps;
        let bid = ((price * (Decimal::ONE - away)) / tick).floor() * tick;
        let ask = ((price * (Decimal::ONE + away)) / tick).ceil() * tick;
        if bid > Decimal::ZERO {
            bids.push((bid, quantity));
        }
        asks.push((ask, quantity));
    }
    Ok((bids, asks))
}

fn invalid(reason: String) -> Error {
    Error::Transaction {
        label: "simulated market".into(),
        reason,
    }
}

/// Send `instructions`, signed by `payer` and `extra`, after a simulation that
/// puts the program's logs in the error.
fn send(
    rpc: &RpcClient,
    payer: &Keypair,
    extra: &[&Keypair],
    instructions: Vec<Instruction>,
    label: &str,
) -> Result<()> {
    let mut all = vec![compute_budget(COMPUTE_UNITS)];
    all.extend(instructions);
    let blockhash = rpc.get_latest_blockhash()?;
    let mut tx = Transaction::new_with_payer(&all, Some(&payer.pubkey()));
    let mut signers: Vec<&Keypair> = vec![payer];
    signers.extend_from_slice(extra);
    tx.sign(&signers[..], blockhash);
    let simulation = rpc.simulate_transaction(&tx)?.value;
    if let Some(err) = simulation.err {
        let logs = simulation.logs.unwrap_or_default();
        let tail = logs[logs.len().saturating_sub(12)..].join("\n");
        return Err(Error::Transaction {
            label: label.to_string(),
            reason: format!("{err}\n{tail}"),
        });
    }
    rpc.send_and_confirm_transaction(&tx)?;
    Ok(())
}

fn compute_budget(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: solana_pubkey::pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data,
    }
}

fn create_account(
    from: &Pubkey,
    to: &Pubkey,
    lamports: u64,
    space: usize,
    owner: &Pubkey,
) -> Instruction {
    let mut data = 0u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&(space as u64).to_le_bytes());
    data.extend_from_slice(owner.as_ref());
    Instruction {
        program_id: Pubkey::default(),
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, true)],
        data,
    }
}

/// A system transfer.
fn transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: Pubkey::default(),
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

fn account_meta(key: Pubkey, writable: bool, signer: bool) -> AccountMeta {
    match (writable, signer) {
        (true, s) => AccountMeta::new(key, s),
        (false, s) => AccountMeta::new_readonly(key, s),
    }
}

/// What a mint account says about itself: its owner and decimals.
fn mint_facts(rpc: &RpcClient, mint: &Pubkey) -> Result<(Pubkey, u8)> {
    let account = rpc.get_account(mint)?;
    if account.data.len() < 82 {
        return Err(invalid(format!("{mint} is not a mint")));
    }
    Ok((account.owner, account.data[44]))
}

fn fund_creator(surfnet: &Surfnet, creator: &Pubkey) -> Result<()> {
    surfnet.cheatcodes().fund_sol(creator, CREATOR_SOL)?;
    Ok(())
}

/// Funds the creator with enough of both mints to post the ladder.
fn fund_maker(
    surfnet: &Surfnet,
    owner: &Pubkey,
    (base, base_owner, base_decimals): (&Pubkey, &Pubkey, u8),
    (quote, quote_owner, quote_decimals): (&Pubkey, &Pubkey, u8),
    price: Decimal,
    ladder: &Ladder,
) -> Result<()> {
    let size = Decimal::from_str(&ladder.level_size)
        .map_err(|e| invalid(format!("ladder level_size: {e}")))?;
    let levels = Decimal::from(ladder.levels.max(1));
    let units_to_atoms = |units: Decimal, decimals: u8| -> u64 {
        let atoms = (units * Decimal::from(10u64.pow(decimals as u32))).ceil();
        u64::try_from(atoms).unwrap_or(u64::MAX / 4)
    };
    // Twice the ladder's need, at twice the price.
    let base_amount = units_to_atoms(size * levels * Decimal::TWO, base_decimals);
    let quote_amount = units_to_atoms(size * levels * price * Decimal::from(4), quote_decimals);
    let cheats = surfnet.cheatcodes();
    cheats.fund_token(owner, base, base_amount, Some(base_owner))?;
    cheats.fund_token(owner, quote, quote_amount, Some(quote_owner))?;
    Ok(())
}

/// Pins the surfnet's clock before each step of a simulated market, so a
/// build does not depend on how long the machine takes: blocks made between
/// transactions (a surfnet finalizes asynchronously) would otherwise move the
/// time that orders and markets record. Each step lands at a fixed slot and
/// time, a thousand slots after the last.
pub struct ClockPin {
    slot: u64,
    unix_ms: u64,
    step: std::sync::atomic::AtomicU64,
}

impl ClockPin {
    /// Steps count from the source snapshot's slot and time.
    pub fn new(fork_slot: u64, created_at_unix: u64) -> Self {
        Self {
            slot: fork_slot,
            unix_ms: created_at_unix * 1_000,
            step: 0.into(),
        }
    }

    async fn next(&self, surfnet: &Surfnet) -> Result<()> {
        // The previous step's transaction leaves blocks to make, back to back;
        // pinning before they are done would let them move the time after it.
        let rpc = surfnet.rpc_client();
        let mut still = 0;
        let mut slot = rpc.get_slot()?;
        while still < 3 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let now = rpc.get_slot()?;
            still = if now == slot { still + 1 } else { 0 };
            slot = now;
        }
        let n = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        surfnet
            .cheatcodes()
            .time_travel_to_slot_at(self.slot + n * 1_000, self.unix_ms + n * 250_000)?;
        Ok(())
    }
}

async fn pin(clock: Option<&ClockPin>, surfnet: &Surfnet) -> Result<()> {
    match clock {
        Some(c) => c.next(surfnet).await,
        None => Ok(()),
    }
}

impl ClockPin {
    /// Where a snapshot made after the steps should start: past the last one,
    /// so a session's clock is never before the time an order records.
    /// `(slot, unix seconds)`.
    pub fn end(&self) -> (u64, u64) {
        let n = self.step.load(std::sync::atomic::Ordering::SeqCst) + 1;
        (self.slot + n * 1_000, (self.unix_ms + n * 250_000) / 1_000)
    }

    /// Whether any step ran.
    pub fn used(&self) -> bool {
        self.step.load(std::sync::atomic::Ordering::SeqCst) > 0
    }
}

/// A market made by an action.
#[derive(Debug, Clone)]
pub struct Made {
    pub address: Pubkey,
    pub program: Pubkey,
    pub detail: serde_json::Value,
}

/// Phoenix's `RequestSeatAuthorized`: the market's authority seats `trader`.
fn phoenix_seat_ix(market: &Pubkey, authority: &Pubkey, trader: &Pubkey) -> Instruction {
    Instruction {
        program_id: phoenix_v1::PROGRAM_ID,
        accounts: vec![
            account_meta(phoenix_v1::PROGRAM_ID, false, false),
            account_meta(phoenix_v1::ix::log_authority(), false, false),
            account_meta(*market, true, false),
            account_meta(*authority, false, true),
            account_meta(*authority, true, true),
            account_meta(*trader, false, false),
            account_meta(phoenix_v1::ix::seat(market, trader), true, false),
            account_meta(Pubkey::default(), false, false),
        ],
        data: vec![105],
    }
}

/// Phoenix's `NameSuccessor`: the authority hands the market to `successor`.
fn phoenix_name_successor_ix(market: &Pubkey, authority: &Pubkey, successor: &Pubkey) -> Instruction {
    let mut data = vec![102];
    data.extend_from_slice(successor.as_ref());
    Instruction {
        program_id: phoenix_v1::PROGRAM_ID,
        accounts: vec![
            account_meta(phoenix_v1::PROGRAM_ID, false, false),
            account_meta(phoenix_v1::ix::log_authority(), false, false),
            account_meta(*market, true, false),
            account_meta(*authority, false, true),
        ],
        data,
    }
}

/// The Seat Manager's `ClaimMarketAuthority`: it takes the market as its
/// named successor, as it holds every mainnet Phoenix v1 market, and seats any
/// trader that asks (`ClaimSeat`).
fn seat_manager_claim_authority_ix(market: &Pubkey, payer: &Pubkey) -> Instruction {
    Instruction {
        program_id: phoenix_v1::SEAT_MANAGER_ID,
        accounts: vec![
            account_meta(phoenix_v1::PROGRAM_ID, false, false),
            account_meta(phoenix_v1::ix::log_authority(), false, false),
            account_meta(*market, true, false),
            account_meta(phoenix_v1::ix::seat_manager(market), true, false),
            account_meta(*payer, true, true),
            account_meta(phoenix_v1::ix::seat_deposit_collector(market), true, false),
            account_meta(Pubkey::default(), false, false),
        ],
        data: vec![0],
    }
}

/// Phoenix's `ChangeSeatStatus` to approved.
fn phoenix_approve_ix(market: &Pubkey, authority: &Pubkey, trader: &Pubkey) -> Instruction {
    Instruction {
        program_id: phoenix_v1::PROGRAM_ID,
        accounts: vec![
            account_meta(phoenix_v1::PROGRAM_ID, false, false),
            account_meta(phoenix_v1::ix::log_authority(), false, false),
            account_meta(*market, true, false),
            account_meta(*authority, false, true),
            account_meta(phoenix_v1::ix::seat(market, trader), true, false),
        ],
        data: vec![104, 1],
    }
}

/// Phoenix's `ChangeMarketStatus` to active.
fn phoenix_activate_ix(market: &Pubkey, authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: phoenix_v1::PROGRAM_ID,
        accounts: vec![
            account_meta(phoenix_v1::PROGRAM_ID, false, false),
            account_meta(phoenix_v1::ix::log_authority(), false, false),
            account_meta(*market, true, false),
            account_meta(*authority, false, true),
        ],
        data: vec![103, 1],
    }
}

/// Create a Phoenix v1 market on `base`/`quote`, make its creator a trader and
/// post the ladder around `price`.
#[allow(clippy::too_many_arguments)]
pub async fn phoenix_v1_market(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    recipe: &str,
    spec: &PhoenixMarketSpec,
    base: Pubkey,
    quote: Pubkey,
    price: Decimal,
    clock: Option<&ClockPin>,
) -> Result<Made> {
    let (base_owner, base_decimals) = mint_facts(rpc, &base)?;
    let (quote_owner, quote_decimals) = mint_facts(rpc, &quote)?;
    if base_owner != token_program() || quote_owner != token_program() {
        return Err(invalid(
            "Phoenix v1 trades classic SPL mints only, not Token-2022".into(),
        ));
    }
    let base_unit = 10u64.pow(base_decimals as u32);
    let quote_unit = 10u64.pow(quote_decimals as u32);
    if spec.base_lot_atoms == 0
        || spec.quote_lot_atoms == 0
        || !base_unit.is_multiple_of(spec.base_lot_atoms)
        || !quote_unit.is_multiple_of(spec.quote_lot_atoms)
        || !spec
            .tick_quote_atoms_per_base_unit
            .is_multiple_of(spec.quote_lot_atoms)
    {
        return Err(invalid(format!(
            "{}: lots must divide a unit and the tick must be whole quote lots",
            spec.name
        )));
    }
    let base_lots_per_unit = base_unit / spec.base_lot_atoms;
    let quote_lots_per_unit = quote_unit / spec.quote_lot_atoms;
    let tick_lots = spec.tick_quote_atoms_per_base_unit / spec.quote_lot_atoms;
    if !tick_lots.is_multiple_of(base_lots_per_unit) {
        return Err(invalid(format!(
            "{}: the tick ({tick_lots} quote lots per base unit) must be a multiple of the {base_lots_per_unit} base lots per unit",
            spec.name
        )));
    }

    let creator = derived_keypair(recipe, "creator");
    let market = derived_keypair(recipe, &format!("phoenix-v1/{}", spec.name));
    fund_creator(surfnet, &creator.pubkey())?;
    let len = phoenix_market_len(spec.bids_size, spec.asks_size, spec.num_seats);
    let lamports = rpc.get_minimum_balance_for_rent_exemption(len)?;
    let base_vault = phoenix_v1::ix::vault(&market.pubkey(), &base);
    let quote_vault = phoenix_v1::ix::vault(&market.pubkey(), &quote);
    let init = Instruction {
        program_id: phoenix_v1::PROGRAM_ID,
        accounts: vec![
            account_meta(phoenix_v1::PROGRAM_ID, false, false),
            account_meta(phoenix_v1::ix::log_authority(), false, false),
            account_meta(market.pubkey(), true, false),
            account_meta(creator.pubkey(), true, true),
            account_meta(base, false, false),
            account_meta(quote, false, false),
            account_meta(base_vault, true, false),
            account_meta(quote_vault, true, false),
            account_meta(Pubkey::default(), false, false),
            account_meta(token_program(), false, false),
        ],
        data: phoenix_initialize_data(
            spec.bids_size,
            spec.asks_size,
            spec.num_seats,
            quote_lots_per_unit,
            tick_lots,
            base_lots_per_unit,
            spec.taker_fee_bps,
            &creator.pubkey(),
        ),
    };
    pin(clock, surfnet).await?;
    send(
        rpc,
        &creator,
        &[&market],
        vec![
            create_account(
                &creator.pubkey(),
                &market.pubkey(),
                lamports,
                len,
                &phoenix_v1::PROGRAM_ID,
            ),
            init,
            phoenix_activate_ix(&market.pubkey(), &creator.pubkey()),
            phoenix_seat_ix(&market.pubkey(), &creator.pubkey(), &creator.pubkey()),
            phoenix_approve_ix(&market.pubkey(), &creator.pubkey(), &creator.pubkey()),
        ],
        "create phoenix v1 market",
    )?;

    fund_maker(
        surfnet,
        &creator.pubkey(),
        (&base, &base_owner, base_decimals),
        (&quote, &quote_owner, quote_decimals),
        price,
        &spec.ladder,
    )?;
    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(surfnet.rpc_url()));
    let trader =
        PhoenixV1Trader::connect(chain, market.pubkey(), "BASE", "QUOTE", creator.pubkey())
            .await
            .map_err(|e| invalid(format!("connecting to the new market: {e}")))?;
    let instrument = &trader.market().instrument;
    let (bids, asks) = ladder_orders(
        price,
        &spec.ladder,
        instrument.tick_size,
        instrument.lot_size,
    )?;
    for (side, orders) in [(Side::Buy, &bids), (Side::Sell, &asks)] {
        for (price, quantity) in orders {
            pin(clock, surfnet).await?;
            trader
                .submit(
                    &PhoenixV1Order::PostOnly {
                        side,
                        price: *price,
                        quantity: *quantity,
                        amend_if_crossing: false,
                        last_valid_slot: None,
                    },
                    &creator,
                )
                .await
                .map_err(|e| invalid(format!("seeding {side:?} {quantity} at {price}: {e}")))?;
        }
    }
    // The handover mainnet markets have: the Seat Manager becomes the market's
    // authority, so a bot claims its seat from it with the kit's code.
    pin(clock, surfnet).await?;
    send(
        rpc,
        &creator,
        &[],
        vec![
            transfer(
                &creator.pubkey(),
                &phoenix_v1::ix::seat_deposit_collector(&market.pubkey()),
                SEAT_DEPOSITS,
            ),
            phoenix_name_successor_ix(
                &market.pubkey(),
                &creator.pubkey(),
                &phoenix_v1::ix::seat_manager(&market.pubkey()),
            ),
            seat_manager_claim_authority_ix(&market.pubkey(), &creator.pubkey()),
        ],
        "hand the phoenix v1 market to the seat manager",
    )?;
    info!(name = %spec.name, market = %market.pubkey(), "simulated phoenix v1 market created");
    Ok(Made {
        address: market.pubkey(),
        program: phoenix_v1::PROGRAM_ID,
        detail: serde_json::json!({
            "base_mint": base.to_string(),
            "quote_mint": quote.to_string(),
            "price": price.to_string(),
            "levels": spec.ladder.levels,
            "market_authority": creator.pubkey().to_string(),
        }),
    })
}

/// Create an OpenBook v2 market on `base`/`quote` and post the ladder around
/// `price`.
#[allow(clippy::too_many_arguments)]
pub async fn openbook_v2_market(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    recipe: &str,
    spec: &OpenbookMarketSpec,
    base: Pubkey,
    quote: Pubkey,
    price: Decimal,
    clock: Option<&ClockPin>,
) -> Result<Made> {
    let (base_owner, base_decimals) = mint_facts(rpc, &base)?;
    let (quote_owner, quote_decimals) = mint_facts(rpc, &quote)?;
    if base_owner != token_program() || quote_owner != token_program() {
        return Err(invalid(
            "the deployed OpenBook v2 trades classic SPL mints only, not Token-2022".into(),
        ));
    }
    if spec.quote_lot_atoms != 1 {
        // The kit's order builder leaves a quote limit of i64::MAX, which the
        // program rejects for any larger quote lot (InvalidInputLotsSize).
        return Err(invalid(format!(
            "{}: an OpenBook v2 market for the kit needs quote_lot_atoms = 1",
            spec.name
        )));
    }
    let creator = derived_keypair(recipe, "creator");
    let label = |part: &str| derived_keypair(recipe, &format!("openbook-v2/{}/{part}", spec.name));
    let (market, bids, asks, heap) = (
        label("market"),
        label("bids"),
        label("asks"),
        label("event-heap"),
    );
    fund_creator(surfnet, &creator.pubkey())?;
    let authority = openbook::ix::market_authority(&market.pubkey());
    let ata = |mint: &Pubkey, program: &Pubkey| {
        surfnet
            .cheatcodes()
            .get_ata(&authority, mint, Some(program))
    };
    let (base_vault, quote_vault) = (ata(&base, &base_owner), ata(&quote, &quote_owner));
    let program = openbook::PROGRAM_ID;
    let accounts = vec![
        account_meta(market.pubkey(), true, true),
        account_meta(authority, false, false),
        account_meta(bids.pubkey(), true, false),
        account_meta(asks.pubkey(), true, false),
        account_meta(heap.pubkey(), true, false),
        account_meta(creator.pubkey(), true, true),
        account_meta(base_vault, true, false),
        account_meta(quote_vault, true, false),
        account_meta(base, false, false),
        account_meta(quote, false, false),
        account_meta(Pubkey::default(), false, false),
        account_meta(base_owner, false, false),
        account_meta(
            mtx_kit::chain::token::associated_token_program(),
            false,
            false,
        ),
        account_meta(program, false, false), // oracle a: none
        account_meta(program, false, false), // oracle b: none
        account_meta(creator.pubkey(), false, false), // collect fee admin
        account_meta(program, false, false), // open orders admin: none
        account_meta(program, false, false), // consume events admin: none
        account_meta(program, false, false), // close market admin: none
        account_meta(openbook::ix::event_authority(), false, false),
        account_meta(program, false, false),
    ];
    let create = Instruction {
        program_id: program,
        accounts,
        data: openbook_create_data(
            &spec.name.chars().take(16).collect::<String>(),
            spec.quote_lot_atoms,
            spec.base_lot_atoms,
            spec.maker_fee,
            spec.taker_fee,
        ),
    };
    let rent = |len| rpc.get_minimum_balance_for_rent_exemption(len);
    pin(clock, surfnet).await?;
    send(
        rpc,
        &creator,
        &[&market, &bids, &asks, &heap],
        vec![
            create_account(
                &creator.pubkey(),
                &bids.pubkey(),
                rent(OPENBOOK_BOOKSIDE_LEN)?,
                OPENBOOK_BOOKSIDE_LEN,
                &program,
            ),
            create_account(
                &creator.pubkey(),
                &asks.pubkey(),
                rent(OPENBOOK_BOOKSIDE_LEN)?,
                OPENBOOK_BOOKSIDE_LEN,
                &program,
            ),
            create_account(
                &creator.pubkey(),
                &heap.pubkey(),
                rent(OPENBOOK_EVENT_HEAP_LEN)?,
                OPENBOOK_EVENT_HEAP_LEN,
                &program,
            ),
            create,
        ],
        "create openbook v2 market",
    )?;

    fund_maker(
        surfnet,
        &creator.pubkey(),
        (&base, &base_owner, base_decimals),
        (&quote, &quote_owner, quote_decimals),
        price,
        &spec.ladder,
    )?;
    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(surfnet.rpc_url()));
    let trader =
        OpenBookV2Trader::connect(chain, market.pubkey(), "BASE", "QUOTE", creator.pubkey())
            .await
            .map_err(|e| invalid(format!("connecting to the new market: {e}")))?;
    let plan = trader
        .plan_open_account()
        .map_err(|e| invalid(format!("open orders account: {e}")))?;
    pin(clock, surfnet).await?;
    trader
        .send(&plan, &creator)
        .await
        .map_err(|e| invalid(format!("open orders account: {e}")))?;
    let instrument = &trader.market().instrument;
    let (bid_orders, ask_orders) = ladder_orders(
        price,
        &spec.ladder,
        instrument.tick_size,
        instrument.lot_size,
    )?;
    for (side, orders) in [(Side::Buy, &bid_orders), (Side::Sell, &ask_orders)] {
        for (price, quantity) in orders {
            pin(clock, surfnet).await?;
            trader
                .submit(
                    &OpenBookV2Order::PostOnly {
                        side,
                        price: *price,
                        quantity: *quantity,
                        expiry_timestamp: 0,
                    },
                    &creator,
                )
                .await
                .map_err(|e| invalid(format!("seeding {side:?} {quantity} at {price}: {e}")))?;
        }
    }
    info!(name = %spec.name, market = %market.pubkey(), "simulated openbook v2 market created");
    Ok(Made {
        address: market.pubkey(),
        program,
        detail: serde_json::json!({
            "base_mint": base.to_string(),
            "quote_mint": quote.to_string(),
            "price": price.to_string(),
            "levels": spec.ladder.levels,
            "token_program": base_owner.to_string(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypairs_derive_from_the_recipe_and_label() {
        let a = derived_keypair("r", "x").pubkey();
        assert_eq!(a, derived_keypair("r", "x").pubkey());
        assert_ne!(a, derived_keypair("r", "y").pubkey());
        assert_ne!(a, derived_keypair("s", "x").pubkey());
    }

    #[test]
    fn a_mint_is_82_bytes_with_its_decimals_and_no_authority() {
        let data = mint_data(6);
        assert_eq!(&data[0..36], &[0; 36], "no mint authority");
        assert_eq!((data[44], data[45]), (6, 1));
        assert_eq!(&data[46..50], &[0; 4], "no freeze authority");
    }

    #[test]
    fn phoenix_market_size_matches_the_decoder() {
        // The mainnet SOL/USDC market: 4,096 orders a side, 8,321 seats.
        assert_eq!(phoenix_market_len(4096, 4096, 8321), 1_723_488);
    }

    #[test]
    fn phoenix_initialize_data_is_borsh() {
        let data = phoenix_initialize_data(
            4096,
            4096,
            8321,
            1_000_000,
            1_000,
            1_000,
            5,
            &Pubkey::default(),
        );
        assert_eq!(data[0], 100);
        assert_eq!(data.len(), 1 + 6 * 8 + 2 + 32 + 1);
        assert_eq!(u64::from_le_bytes(data[41..49].try_into().unwrap()), 1_000);
    }

    #[test]
    fn openbook_create_data_starts_with_the_anchor_discriminator() {
        let data = openbook_create_data("TEST/USDC", 1, 1_000_000, 0, 0);
        assert_eq!(data[..8], Sha256::digest(b"global:create_market")[..8]);
        assert_eq!(u32::from_le_bytes(data[8..12].try_into().unwrap()), 9);
        assert_eq!(data.len(), 8 + 4 + 9 + 4 + 1 + 5 * 8);
    }

    #[test]
    fn a_ladder_never_crosses_and_sits_on_the_grid() {
        let ladder = Ladder {
            levels: 3,
            half_spread_bps: 10,
            step_bps: 10,
            level_size: "2.5".into(),
        };
        let (tick, lot) = (Decimal::new(1, 2), Decimal::new(1, 1));
        let (bids, asks) = ladder_orders(Decimal::new(1234, 1), &ladder, tick, lot).unwrap();
        assert_eq!((bids.len(), asks.len()), (3, 3));
        assert!(bids[0].0 < asks[0].0);
        assert!(bids.windows(2).all(|w| w[0].0 > w[1].0));
        assert!(asks.windows(2).all(|w| w[0].0 < w[1].0));
        for (price, quantity) in bids.iter().chain(&asks) {
            assert_eq!(*price % tick, Decimal::ZERO);
            assert_eq!(*quantity, Decimal::new(25, 1));
        }
        assert_eq!(
            bids[0].0,
            Decimal::new(12327, 2),
            "123.4 less 10 bp, rounded down"
        );
    }

    #[test]
    fn a_ladder_smaller_than_a_lot_is_refused() {
        let ladder = Ladder {
            levels: 1,
            half_spread_bps: 1,
            step_bps: 1,
            level_size: "0.001".into(),
        };
        assert!(
            ladder_orders(
                Decimal::ONE,
                &ladder,
                Decimal::new(1, 2),
                Decimal::new(1, 1)
            )
            .is_err()
        );
    }
}
