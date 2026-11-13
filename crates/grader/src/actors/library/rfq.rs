//! RFQ clients, and the desk they ask: the host side of the interface's
//! requests for quotes.
//!
//! Clients send requests to the bot through an [`RfqBoard`]. A component
//! sees the open ones in its market's `rfqs`, and answers on the board's
//! venue (`rfq:<symbol>`) with a response line, which the board's seat
//! ([`RfqSeat`]) takes. At the start of the next step each client looks at
//! the answer to its request and takes it if the price suits it: no worse
//! than its own value of the asset by its reach. The reach is
//! `tolerance_bps` for one market trade size and grows with the square root
//! of the request's size in them, as the cost of sweeping the size on a
//! screen does: a large request pays more for a quote than a small one.
//! The screen itself plays no part.
//!
//! A client values the asset at the scenario's level, unless it is
//! informed: then it sees the level `lead_slots` ahead, and its value moves
//! that way by `correlation`. An informed client asks to trade the way it
//! sees the price going. Which clients are informed is drawn per session,
//! and stays so all session. A request's id names its client (`c<n>-<k>`),
//! so a desk can keep a record per client.
//!
//! A trade settles bilaterally, as a [`Settlement`] does it: in a perp
//! session, a swap in one transaction that the client and the bot both sign
//! ([`Swaps`]), of the asset's spot token from the seller's wallet for USDC
//! from the buyer's, at exactly the quoted price. No book is involved and
//! the bot pays no fee: the client's wallet pays the transaction's.
//!
//! Before a swap, and after one fails, the settlement reads both wallets'
//! token accounts. A wallet that can't settle (its account is missing, not
//! its own, frozen, asks for a memo, or short of what it pays) makes the
//! trade unfunded ([`SwapError::Unfunded`]): the bot's own doing, or a
//! client's that has traded its funds away with the bot, and no fault of
//! the session's. Any other failure is the infrastructure's, and counts
//! against the session like any other actor's.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

use async_trait::async_trait;
use mtx_kit::{Action, OwnFill, Planned, Role, SendState, Sent, Side};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::{AccountMeta, Instruction, Transaction};

use super::{Rng, Scale, Wallet, finite_at_least_zero, finite_positive};
use crate::{
    actors::{Actor, ActorsReport, Moment},
    venue::{Seat, SeatResult},
};

fn six() -> u32 {
    6
}

fn one() -> f64 {
    1.0
}

fn eight() -> u64 {
    8
}

fn half() -> f64 {
    0.5
}

/// RFQ clients' settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RfqClients {
    #[serde(default = "six")]
    pub clients: u32,
    /// The share of clients that are informed, and how far ahead and how
    /// well they see.
    #[serde(default)]
    pub informed_share: f64,
    #[serde(default)]
    pub lead_slots: u64,
    #[serde(default = "one")]
    pub correlation: f64,
    /// Requests per 100 slots, over all clients.
    #[serde(default = "one")]
    pub per_100_slots: f64,
    /// A request's size, in market trade sizes.
    #[serde(default = "one")]
    pub size_mult: f64,
    /// Slots a request stays open for an answer.
    #[serde(default = "eight")]
    pub deadline_slots: u64,
    /// How far past its value a client pays for one market trade size, in
    /// basis points; for a request `n` times that, `sqrt(n)` times as far.
    /// The default is about what sweeping SPCX's spot pools costs (some
    /// 46 bp for $10k, routed, in 2026-10).
    #[serde(default = "one")]
    pub tolerance_bps: f64,
    /// The share of requests that ask for both sides.
    #[serde(default = "half")]
    pub two_way_share: f64,
    /// What the bot's wallet starts with to settle fills: base units of the
    /// asset, and USDC on top of what it holds.
    #[serde(default)]
    pub desk_base: f64,
    #[serde(default)]
    pub desk_usdc: f64,
}

impl Default for RfqClients {
    fn default() -> Self {
        Self {
            clients: six(),
            informed_share: 0.0,
            lead_slots: 0,
            correlation: one(),
            per_100_slots: one(),
            size_mult: one(),
            deadline_slots: eight(),
            tolerance_bps: one(),
            two_way_share: half(),
            desk_base: 0.0,
            desk_usdc: 0.0,
        }
    }
}

impl RfqClients {
    pub fn validate(&self) -> Result<(), String> {
        if self.clients == 0 {
            return Err("clients must be at least 1".into());
        }
        let share = |what: &str, v: f64| {
            if (0.0..=1.0).contains(&v) {
                Ok(())
            } else {
                Err(format!("{what} must be between 0 and 1"))
            }
        };
        share("informed_share", self.informed_share)?;
        share("correlation", self.correlation)?;
        share("two_way_share", self.two_way_share)?;
        if self.informed_share > 0.0 && self.lead_slots == 0 {
            return Err("informed clients need lead_slots".into());
        }
        if self.deadline_slots == 0 {
            return Err("deadline_slots must be at least 1".into());
        }
        finite_positive("per_100_slots", self.per_100_slots)?;
        finite_positive("size_mult", self.size_mult)?;
        finite_at_least_zero("desk_base", self.desk_base)?;
        finite_at_least_zero("desk_usdc", self.desk_usdc)?;
        finite_at_least_zero("tolerance_bps", self.tolerance_bps)
    }
}

/// A request a client sent.
#[derive(Debug, Clone, PartialEq)]
struct Request {
    id: String,
    client: usize,
    /// What it shows: the side it wants, or none for both.
    shown: Option<Side>,
    /// What it wants: buy (`Bid`) or sell.
    wants: Side,
    size: f64,
    /// The last slot a response is taken at, on the chain's clock.
    deadline_slot: u64,
}

/// A response the bot gave.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Response {
    bid: Option<f64>,
    ask: Option<f64>,
    size: Option<f64>,
    expiry_slot: Option<u64>,
}

struct BoardState {
    venue: String,
    /// The chain's slot now, as the clients last saw it.
    slot: u64,
    open: Vec<Request>,
    responses: BTreeMap<String, Response>,
    /// The bot's fills since it last looked.
    fills: Vec<OwnFill>,
}

/// The requests open to the bot, and its answers: shared by the clients
/// and the bot's desk.
#[derive(Clone)]
pub struct RfqBoard(Arc<Mutex<BoardState>>);

impl RfqBoard {
    pub fn new(venue: String) -> Self {
        Self(Arc::new(Mutex::new(BoardState {
            venue,
            slot: 0,
            open: Vec::new(),
            responses: BTreeMap::new(),
            fills: Vec::new(),
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, BoardState> {
        self.0.lock().expect("rfq board lock")
    }

    pub fn venue(&self) -> String {
        self.state().venue.clone()
    }

    /// The requests open at the current slot, as the bot sees them.
    pub fn open(&self) -> Vec<mtx_kit::Rfq> {
        let state = self.state();
        state
            .open
            .iter()
            .filter(|r| r.deadline_slot >= state.slot)
            .map(|r| mtx_kit::Rfq {
                id: r.id.clone(),
                venue: state.venue.clone(),
                side: r.shown,
                size: r.size,
                deadline_slot: r.deadline_slot,
            })
            .collect()
    }

    /// The bot's fills since the last call: it is the maker of each.
    pub fn take_fills(&self) -> Vec<OwnFill> {
        std::mem::take(&mut self.state().fills)
    }

    /// The bot answers request `id`: its latest answer stands.
    fn respond(&self, id: &str, response: Response) -> Result<(), String> {
        let mut state = self.state();
        let slot = state.slot;
        let request = state
            .open
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| format!("no open request {id}"))?;
        if request.deadline_slot < slot {
            return Err(format!(
                "request {id} closed at slot {}",
                request.deadline_slot
            ));
        }
        let price = match request.wants {
            Side::Bid => response.ask,
            Side::Ask => response.bid,
        };
        if [response.bid, response.ask, response.size]
            .into_iter()
            .flatten()
            .any(|v| !(v.is_finite() && v > 0.0))
        {
            return Err("a response's prices and size must be positive".into());
        }
        if request.shown.is_some() && price.is_none() {
            return Err(format!("request {id} needs a price on the side it asks"));
        }
        state.responses.insert(id.to_string(), response);
        Ok(())
    }
}

/// The bot's seat on an RFQ venue: it takes response lines, and nothing
/// else.
pub struct RfqSeat {
    board: RfqBoard,
    market: String,
}

impl RfqSeat {
    pub fn new(board: RfqBoard) -> Self {
        Self {
            market: board.venue(),
            board,
        }
    }
}

#[async_trait]
impl Seat for RfqSeat {
    fn market(&self) -> &str {
        &self.market
    }

    fn executes(&self, action: &Action) -> bool {
        matches!(action, Action::Respond { .. })
    }

    /// A response goes on the board, where its client reads it at the
    /// next step: nothing is sent to the chain until the client takes it.
    async fn send(&self, planned: &Planned) -> Sent {
        let Action::Respond { id, bid, ask, size } = &planned.action else {
            return Sent {
                state: SendState::NotSent("an RFQ desk takes only responses".into()),
                sent: false,
            };
        };
        let response = Response {
            bid: *bid,
            ask: *ask,
            size: *size,
            expiry_slot: planned.expires_slot,
        };
        Sent {
            state: match self.board.respond(id, response) {
                Ok(()) => SendState::Landed(format!("response to {id}")),
                Err(reason) => SendState::Failed {
                    reason,
                    signature: None,
                },
            },
            sent: false,
        }
    }

    async fn take(&self, _: Side, _: f64, _: f64) -> SeatResult {
        Err("an RFQ desk takes only responses".into())
    }

    async fn rest(&self, _: Side, _: f64, _: f64) -> SeatResult {
        Err("an RFQ desk takes only responses".into())
    }

    async fn cancel_all(&self) -> SeatResult {
        Ok(())
    }

    async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
        Ok((None, None))
    }

    async fn position(&self) -> SeatResult<f64> {
        Ok(0.0)
    }

    fn tick(&self) -> f64 {
        f64::NAN
    }
}

/// How an RFQ trade settles between a client's wallet and the bot's.
#[async_trait]
pub trait Settlement: Send + Sync {
    /// `client` buys (`wants` [`Side::Bid`]) or sells `size` base units
    /// from or to the bot at `price` each: the transaction's signature.
    async fn swap(
        &self,
        client: &Pubkey,
        wants: Side,
        size: f64,
        price: f64,
    ) -> Result<String, SwapError>;
}

/// Why an RFQ trade didn't settle.
#[derive(Debug, Clone, PartialEq)]
pub enum SwapError {
    /// A wallet can't settle it: the bot's (`bot`), or the client's.
    Unfunded { bot: bool, reason: String },
    /// Anything else: the RPC, the chain, the trade itself.
    Failed(String),
}

impl std::fmt::Display for SwapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unfunded { bot: true, reason } => write!(f, "the bot's wallet {reason}"),
            Self::Unfunded { bot: false, reason } => write!(f, "the client's wallet {reason}"),
            Self::Failed(error) => f.write_str(error),
        }
    }
}

/// The bot's RFQ desk at the start and the end of a session: its wallet's
/// asset and USDC, and the asset's reference price. The session values the
/// asset at that price; the USDC is in the account's metered value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct DeskReport {
    pub start_base: f64,
    pub start_usdc: f64,
    pub start_index_usd: f64,
    pub end_base: f64,
    pub end_usdc: f64,
    pub end_index_usd: f64,
}

/// A token as a swap moves it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Token {
    pub mint: Pubkey,
    pub program: Pubkey,
    pub decimals: u8,
}

impl Token {
    /// `owner`'s associated account for it.
    pub fn account(&self, owner: &Pubkey) -> Pubkey {
        mtx_kit::token::associated_token_address(owner, &self.mint, &self.program)
    }

    /// The token program's `TransferChecked` of `amount` (its smallest
    /// units) from `from`'s account to `to`'s.
    fn transfer(&self, from: &Pubkey, to: &Pubkey, amount: u64) -> Instruction {
        let mut data = vec![TRANSFER_CHECKED];
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(self.decimals);
        Instruction {
            program_id: self.program,
            accounts: vec![
                AccountMeta::new(self.account(from), false),
                AccountMeta::new_readonly(self.mint, false),
                AccountMeta::new(self.account(to), false),
                AccountMeta::new_readonly(*from, true),
            ],
            data,
        }
    }
}

/// One wallet's side of a swap: its account of `token`, which pays `pays`
/// (in the token's smallest units) or receives.
struct Leg {
    owner: Pubkey,
    token: Token,
    pays: u64,
    bot: bool,
}

/// Why `leg`'s account can't settle, if it can't: `account` is its
/// program and data, as the chain has them.
fn unfit(leg: &Leg, account: Option<(&Pubkey, &[u8])>) -> Option<String> {
    let Some((program, data)) = account else {
        return Some("has no account of the token".into());
    };
    if *program != leg.token.program || data.len() < TOKEN_ACCOUNT_LEN {
        return Some("holds no token account where it should".into());
    }
    if data[..32] != leg.token.mint.to_bytes() || data[32..64] != leg.owner.to_bytes() {
        return Some("doesn't own its account of the token".into());
    }
    if data[108] != INITIALIZED {
        return Some("has its account of the token frozen".into());
    }
    if requires_memo(&data[TOKEN_ACCOUNT_LEN..]) {
        return Some("asks for a memo on transfers in".into());
    }
    let held = u64::from_le_bytes(data[64..72].try_into().expect("8 bytes"));
    (held < leg.pays).then(|| format!("holds {held} of the {} it pays", leg.pays))
}

/// Whether a Token-2022 account's extensions (after the account type)
/// require a memo on incoming transfers: the one its owner can turn on
/// that a plain transfer to it fails.
fn requires_memo(extensions: &[u8]) -> bool {
    let mut at = 1;
    while at + 4 <= extensions.len() {
        let kind = u16::from_le_bytes([extensions[at], extensions[at + 1]]);
        let len = u16::from_le_bytes([extensions[at + 2], extensions[at + 3]]) as usize;
        let value = extensions.get(at + 4..at + 4 + len).unwrap_or_default();
        match kind {
            0 => return false,
            MEMO_TRANSFER => return value.first().is_some_and(|on| *on != 0),
            _ => at += 4 + len,
        }
    }
    false
}

/// What the chain says of `legs`: the first wallet that can't settle, the
/// bot's first.
fn standing(
    rpc: &solana_rpc_client::rpc_client::RpcClient,
    legs: &[Leg],
) -> Result<Option<SwapError>, SwapError> {
    let keys: Vec<Pubkey> = legs.iter().map(|l| l.token.account(&l.owner)).collect();
    let accounts = rpc
        .get_multiple_accounts(&keys)
        .map_err(|e| SwapError::Failed(e.to_string()))?;
    Ok(legs.iter().zip(&accounts).find_map(|(leg, account)| {
        let account = account.as_ref().map(|a| (&a.owner, a.data.as_slice()));
        unfit(leg, account).map(|reason| SwapError::Unfunded {
            bot: leg.bot,
            reason,
        })
    }))
}

const TOKEN_ACCOUNT_LEN: usize = 165;
const INITIALIZED: u8 = 1;
const MEMO_TRANSFER: u16 = 8;
const TRANSFER_CHECKED: u8 = 12;
const COMPUTE_BUDGET: Pubkey =
    solana_pubkey::pubkey!("ComputeBudget111111111111111111111111111111");

/// RFQ trades settled as swaps on the chain: in one transaction, the
/// seller's `base` goes to the buyer and the buyer's USDC (`quote`) to the
/// seller, at exactly the agreed price (to USDC's last unit). The client
/// pays the transaction's fee and the bot none. Both keys are the
/// grader's: the bot agreed to the price in its response.
pub struct Swaps {
    rpc_url: String,
    bot: Keypair,
    clients: Vec<Keypair>,
    base: Token,
    quote: Token,
    /// How many it has sent: each asks for a few compute units fewer, so
    /// two alike swaps in a slot are different transactions.
    sent: AtomicU32,
}

impl Swaps {
    pub fn new(
        rpc_url: String,
        bot: Keypair,
        clients: Vec<Keypair>,
        base: Token,
        quote: Token,
    ) -> Self {
        Self {
            rpc_url,
            bot,
            clients,
            base,
            quote,
            sent: AtomicU32::new(0),
        }
    }

    /// The swap's instructions, with `buyer` and `seller`.
    fn instructions(
        &self,
        buyer: &Pubkey,
        seller: &Pubkey,
        size: f64,
        price: f64,
    ) -> Result<Vec<Instruction>, String> {
        let (base, usdc) = amounts(size, price, self.base.decimals, self.quote.decimals)?;
        let n = self.sent.fetch_add(1, Ordering::Relaxed);
        let mut limit = vec![2u8];
        limit.extend_from_slice(&(SWAP_COMPUTE_UNITS - n % 1024).to_le_bytes());
        Ok(vec![
            Instruction {
                program_id: COMPUTE_BUDGET,
                accounts: Vec::new(),
                data: limit,
            },
            self.base.transfer(seller, buyer, base),
            self.quote.transfer(buyer, seller, usdc),
        ])
    }
}

/// Plenty for two transfers.
const SWAP_COMPUTE_UNITS: u32 = 60_000;

/// `size` base units at `price`, in each token's smallest units: the size
/// rounded down to the base token's, and what it costs to USDC's nearest.
fn amounts(
    size: f64,
    price: f64,
    base_decimals: u8,
    quote_decimals: u8,
) -> Result<(u64, u64), String> {
    let base = (size * 10f64.powi(base_decimals as i32) + 1e-6).floor();
    let quote = (base * price * 10f64.powi(quote_decimals as i32 - base_decimals as i32)).round();
    if !(base >= 1.0 && quote >= 1.0 && quote < u64::MAX as f64) {
        return Err(format!("{size} at {price} is no amount to swap"));
    }
    Ok((base as u64, quote as u64))
}

#[async_trait]
impl Settlement for Swaps {
    async fn swap(
        &self,
        client: &Pubkey,
        wants: Side,
        size: f64,
        price: f64,
    ) -> Result<String, SwapError> {
        let client = self
            .clients
            .iter()
            .find(|k| k.pubkey() == *client)
            .ok_or_else(|| SwapError::Failed(format!("no client {client}")))?
            .insecure_clone();
        let bot = self.bot.insecure_clone();
        let (buyer, seller) = match wants {
            Side::Bid => (client.pubkey(), bot.pubkey()),
            Side::Ask => (bot.pubkey(), client.pubkey()),
        };
        let (base, usdc) = amounts(size, price, self.base.decimals, self.quote.decimals)
            .map_err(SwapError::Failed)?;
        let instructions = self
            .instructions(&buyer, &seller, size, price)
            .map_err(SwapError::Failed)?;
        let bot_key = bot.pubkey();
        let legs: Vec<Leg> = [bot_key, client.pubkey()]
            .into_iter()
            .flat_map(|owner| {
                let pays = |token: Token, payer: Pubkey, amount: u64| Leg {
                    owner,
                    token,
                    pays: if owner == payer { amount } else { 0 },
                    bot: owner == bot_key,
                };
                [pays(self.base, seller, base), pays(self.quote, buyer, usdc)]
            })
            .collect();
        let rpc_url = self.rpc_url.clone();
        tokio::task::spawn_blocking(move || {
            let rpc =
                mtx_kit::rpc_client_with_timeout(rpc_url, crate::session::INTERNAL_RPC_TIMEOUT);
            if let Some(unfunded) = standing(&rpc, &legs)? {
                return Err(unfunded);
            }
            let blockhash = rpc
                .get_latest_blockhash()
                .map_err(|e| SwapError::Failed(e.to_string()))?;
            let tx = Transaction::new_signed_with_payer(
                &instructions,
                Some(&client.pubkey()),
                &[&client, &bot],
                blockhash,
            );
            rpc.send_and_confirm_transaction(&tx)
                .map(|signature| signature.to_string())
                // A wallet that can't settle now, though it could a moment
                // ago, changed in between: that, not the infrastructure,
                // failed the swap.
                .map_err(|e| match standing(&rpc, &legs) {
                    Ok(Some(unfunded)) => unfunded,
                    _ => SwapError::Failed(e.to_string()),
                })
        })
        .await
        .map_err(|e| SwapError::Failed(e.to_string()))?
    }
}

pub(super) struct Clients {
    spec: RfqClients,
    scale: Scale,
    wallets: Vec<Wallet>,
    informed: Vec<bool>,
    board: RfqBoard,
    settlement: Option<Arc<dyn Settlement>>,
    path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    rng: Rng,
    /// The slot into the session it last looked at, and the chain's slot
    /// the session starts at.
    last: Option<u64>,
    base_slot: u64,
    sent: u64,
}

impl Clients {
    pub fn new(
        spec: RfqClients,
        scale: Scale,
        wallets: Vec<Wallet>,
        board: RfqBoard,
        settlement: Option<Arc<dyn Settlement>>,
        path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
        mut rng: Rng,
    ) -> Self {
        // Which clients are informed: a draw of the session's.
        let n = wallets.len();
        let k = ((spec.informed_share * n as f64).round() as usize).min(n);
        let mut order: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            order.swap(i, rng.below(i + 1));
        }
        let mut informed = vec![false; n];
        for &i in &order[..k] {
            informed[i] = true;
        }
        if let Some(first) = wallets.first() {
            first.count("informed_clients", k as u64);
        }
        Self {
            spec,
            scale,
            wallets,
            informed,
            board,
            settlement,
            path,
            rng,
            last: None,
            base_slot: 0,
            sent: 0,
        }
    }

    /// The chain's slot the session's steps count from: deadlines are on
    /// the chain's clock, as the bot sees it.
    pub(super) fn starting_at(mut self, base_slot: u64) -> Self {
        self.base_slot = base_slot;
        self
    }

    /// What `client` thinks the asset is worth `at`.
    fn value(&self, client: usize, now: &Moment) -> f64 {
        if !self.informed[client] {
            return now.level_usd;
        }
        let ahead = (self.path)(now.at + self.spec.lead_slots).level_usd;
        now.level_usd + self.spec.correlation * (ahead - now.level_usd)
    }

    /// How far past its value a client pays for `size`, as a fraction.
    fn reach(&self, size: f64) -> f64 {
        self.spec.tolerance_bps / 10_000.0 * (size / self.scale.trade_size).sqrt()
    }

    fn send_requests(&mut self, now: &Moment) {
        let slots = now.at - self.last.unwrap_or(0);
        self.last = Some(now.at);
        let n = self
            .rng
            .poisson(self.spec.per_100_slots / 100.0 * slots as f64);
        let mut new = Vec::new();
        for _ in 0..n {
            let client = self.rng.below(self.wallets.len());
            let value = self.value(client, now);
            let wants = if self.informed[client] && value != now.level_usd {
                if value > now.level_usd {
                    Side::Bid
                } else {
                    Side::Ask
                }
            } else if self.rng.chance(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            let shown = (!self.rng.chance(self.spec.two_way_share)).then_some(wants);
            let size = self.scale.draw_size(&mut self.rng, self.spec.size_mult);
            self.sent += 1;
            new.push(Request {
                id: format!("c{client}-{}", self.sent),
                client,
                shown,
                wants,
                size,
                deadline_slot: self.base_slot + now.at + self.spec.deadline_slots,
            });
        }
        if let Some(first) = self.wallets.first() {
            first.count("requests", new.len() as u64);
        }
        let mut state = self.board.state();
        state.slot = self.base_slot + now.at;
        state.open.extend(new);
    }

    /// Each answered request's client takes it or not; requests past their
    /// deadline close.
    async fn settle(&mut self, last: &Moment) {
        let slot = self.base_slot + last.at;
        let (answered, expired) = {
            let mut state = self.board.state();
            let mut answered = Vec::new();
            let mut expired = 0;
            let open = std::mem::take(&mut state.open);
            for request in open {
                match state.responses.remove(&request.id) {
                    Some(response) => answered.push((request, response)),
                    None if request.deadline_slot < slot => expired += 1,
                    None => state.open.push(request),
                }
            }
            (answered, expired)
        };
        let count = |what: &str, n: u64| {
            if let Some(first) = self.wallets.first() {
                first.count(what, n);
            }
        };
        count("expired", expired);
        count("answered", answered.len() as u64);
        for (request, response) in answered {
            let filled = self.take(&request, &response, last, slot).await;
            if filled > 0.0 {
                count("fills", 1);
                if self.informed[request.client] {
                    count("informed_fills", 1);
                }
            } else {
                count("declined", 1);
            }
        }
    }

    /// The client takes `response` if it suits it: what filled, in base
    /// units.
    async fn take(&self, request: &Request, response: &Response, now: &Moment, slot: u64) -> f64 {
        if response.expiry_slot.is_some_and(|expiry| expiry < slot) {
            return 0.0;
        }
        let Some(settlement) = self.settlement.as_deref() else {
            return 0.0;
        };
        let wallet = &self.wallets[request.client];
        let size = response.size.map_or(request.size, |s| s.min(request.size));
        let value = self.value(request.client, now);
        let reach = self.reach(size);
        let (price, suits) = match request.wants {
            Side::Bid => match response.ask {
                Some(ask) => (ask, ask <= value * (1.0 + reach)),
                None => return 0.0,
            },
            Side::Ask => match response.bid {
                Some(bid) => (bid, bid >= value * (1.0 - reach)),
                None => return 0.0,
            },
        };
        if !suits {
            return 0.0;
        }
        let swapped = match settlement
            .swap(&wallet.pubkey(), request.wants, size, price)
            .await
        {
            Ok(signature) => Ok(signature),
            // No action: the bot's wallet, or a client's that traded its
            // funds away with the bot, can't settle.
            Err(error @ SwapError::Unfunded { bot, .. }) => {
                tracing::info!(%error, id = request.id, "an RFQ fill didn't settle");
                wallet.count(if bot { "unsettled" } else { "client_unfunded" }, 1);
                return 0.0;
            }
            Err(SwapError::Failed(error)) => {
                tracing::warn!(%error, id = request.id, "an RFQ fill didn't settle");
                Err(error)
            }
        };
        if !wallet.swap(now.at, request.wants, price, size, swapped) {
            return 0.0;
        }
        let venue = self.board.venue();
        self.board.state().fills.push(OwnFill {
            venue,
            slot,
            side: request.wants.opposite(),
            price,
            size,
            role: Role::Maker,
        });
        size
    }
}

#[async_trait]
impl Actor for Clients {
    async fn before_blocks(&mut self, last: &Moment) {
        self.settle(last).await;
    }

    async fn after_blocks(&mut self, now: &Moment) {
        self.send_requests(now);
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::library::{
        Actions, Ledger, LibraryReport, WalletSeat,
        tests::{Book, at, scale},
    };

    fn planned(id: &str, bid: Option<f64>, ask: Option<f64>) -> Planned {
        Planned {
            line: 0,
            venue: Some("rfq:SPCX".into()),
            client_id: None,
            action: Action::Respond {
                id: id.into(),
                bid,
                ask,
                size: None,
            },
            reduce_only: false,
            expires_slot: None,
            tx: None,
        }
    }

    /// A settlement that notes each swap, and settles none if it fails.
    #[derive(Default)]
    struct Noted {
        swaps: Mutex<Vec<(Pubkey, Side, f64, f64)>>,
        fails: Option<SwapError>,
    }

    #[async_trait]
    impl Settlement for Noted {
        async fn swap(
            &self,
            client: &Pubkey,
            wants: Side,
            size: f64,
            price: f64,
        ) -> Result<String, SwapError> {
            if let Some(error) = &self.fails {
                return Err(error.clone());
            }
            self.swaps
                .lock()
                .unwrap()
                .push((*client, wants, size, price));
            Ok("sig".into())
        }
    }

    fn clients(
        spec: RfqClients,
        scale: Scale,
        settlement: Arc<Noted>,
        path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    ) -> (Clients, RfqBoard, Ledger) {
        let ledger = Ledger::new(true);
        let seats = (0..spec.clients)
            .map(|_| WalletSeat {
                pubkey: Pubkey::new_unique(),
                // A one-tick book: the screen plays no part.
                seat: Box::new(Book::new("client", 99.99, 100.00).0),
            })
            .collect();
        let wallets = ledger.wallets("rfq-client", seats);
        let board = RfqBoard::new("rfq:SPCX".into());
        let clients = Clients::new(
            spec,
            scale,
            wallets,
            board.clone(),
            Some(settlement),
            path,
            Rng::new([9; 32]),
        )
        .starting_at(1_000);
        (clients, board, ledger)
    }

    async fn next_request(clients: &mut Clients, board: &RfqBoard, s: &mut u64) -> mtx_kit::Rfq {
        loop {
            *s += 2;
            clients.after_blocks(&at(*s, 100.0)).await;
            if let Some(r) = board.open().into_iter().next() {
                return r;
            }
        }
    }

    #[tokio::test]
    async fn a_quote_on_a_one_tick_book_settles_as_a_swap_at_its_price() {
        let noted = Arc::new(Noted::default());
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> = Arc::new(|s| at(s, 100.0));
        let spec = RfqClients {
            clients: 1,
            per_100_slots: 50.0,
            two_way_share: 0.0,
            tolerance_bps: 5.0,
            ..RfqClients::default()
        };
        let (mut clients, board, ledger) = clients(spec, scale(), noted.clone(), path);
        let seat = RfqSeat::new(board.clone());
        let mut s = 0;
        let request = next_request(&mut clients, &board, &mut s).await;
        assert_eq!(request.venue, "rfq:SPCX");
        assert_eq!(request.deadline_slot, 1_000 + s + 8);
        assert!(request.id.starts_with("c0-"));
        // A quote far from the client's value is declined; nothing trades.
        assert!(
            seat.send(&planned(&request.id, Some(90.0), Some(110.0)))
                .await
                .state
                .landed()
        );
        clients.before_blocks(&at(s, 100.0)).await;
        assert!(noted.swaps.lock().unwrap().is_empty());
        // Unknown ids are refused.
        let refused = seat.send(&planned("c9-9", Some(1.0), None)).await;
        assert!(!refused.state.landed());

        // The next request gets a quote three ticks through the one-tick
        // screen, within the client's reach: it settles at that price.
        let request = next_request(&mut clients, &board, &mut s).await;
        let wants = request.side.unwrap();
        let sent = seat
            .send(&planned(&request.id, Some(99.97), Some(100.03)))
            .await;
        assert!(sent.state.landed() && !sent.sent);
        clients.before_blocks(&at(s, 100.0)).await;
        let price = match wants {
            Side::Bid => 100.03,
            Side::Ask => 99.97,
        };
        let swaps = noted.swaps.lock().unwrap().clone();
        assert_eq!(swaps.len(), 1);
        assert_eq!(
            (swaps[0].1, swaps[0].2, swaps[0].3),
            (wants, request.size, price)
        );
        let fills = board.take_fills();
        assert_eq!(fills.len(), 1);
        assert_eq!(
            (fills[0].side, fills[0].price, fills[0].role),
            (wants.opposite(), price, Role::Maker)
        );
        assert!(board.take_fills().is_empty(), "taken once");
        let report = ledger.report();
        let counts = &report.actors[0].counts;
        assert_eq!((counts["declined"], counts["fills"]), (1, 1));
        assert_eq!(report.tape.last().unwrap().action, "swap");
        assert_eq!(report.tape.last().unwrap().price_usd, Some(price));
        assert!(
            !board.open().iter().any(|r| r.id == request.id),
            "answered: closed"
        );
    }

    /// Twenty answered requests that settle as `fails` says: the session's
    /// report.
    async fn twenty_answered(fails: SwapError) -> (LibraryReport, RfqBoard) {
        let noted = Arc::new(Noted {
            fails: Some(fails),
            ..Noted::default()
        });
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> = Arc::new(|s| at(s, 100.0));
        let spec = RfqClients {
            clients: 1,
            per_100_slots: 50.0,
            two_way_share: 0.0,
            ..RfqClients::default()
        };
        let (mut clients, board, ledger) = clients(spec, scale(), noted, path);
        let seat = RfqSeat::new(board.clone());
        let mut s = 0;
        for _ in 0..20 {
            let request = next_request(&mut clients, &board, &mut s).await;
            seat.send(&planned(&request.id, Some(100.0), Some(100.0)))
                .await;
            clients.before_blocks(&at(s, 100.0)).await;
        }
        (ledger.report(), board)
    }

    #[tokio::test]
    async fn a_swap_the_bot_cant_pay_for_doesnt_fill_or_count_against_the_session() {
        let unfunded = |bot| SwapError::Unfunded {
            bot,
            reason: "holds 0 of the 100 it pays".into(),
        };
        let (report, board) = twenty_answered(unfunded(true)).await;
        assert_eq!(report.actors[0].counts["unsettled"], 20);
        assert_eq!(report.actions, Actions::default(), "no action tried");
        assert!(!report.degraded(), "the bot's failures are its own");
        assert!(board.take_fills().is_empty());
        let (report, _) = twenty_answered(unfunded(false)).await;
        assert_eq!(report.actors[0].counts["client_unfunded"], 20);
        assert!(!report.degraded());
    }

    #[tokio::test]
    async fn a_swap_the_infrastructure_failed_counts_against_the_session() {
        let (report, board) = twenty_answered(SwapError::Failed("timed out".into())).await;
        assert_eq!(report.actions.failed, 20);
        assert!(!report.actors[0].counts.contains_key("unsettled"));
        assert!(report.degraded(), "an RFQ client's own failures void it");
        assert!(board.take_fills().is_empty());
    }

    /// A token account's data: `mint`, `owner`, `amount`, its state, and
    /// Token-2022 extensions after the account type, if any.
    fn token_account(
        mint: &Pubkey,
        owner: &Pubkey,
        amount: u64,
        state: u8,
        extensions: &[(u16, &[u8])],
    ) -> Vec<u8> {
        let mut data = vec![0; TOKEN_ACCOUNT_LEN];
        data[..32].copy_from_slice(&mint.to_bytes());
        data[32..64].copy_from_slice(&owner.to_bytes());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data[108] = state;
        if !extensions.is_empty() {
            data.push(2);
            for (kind, value) in extensions {
                data.extend_from_slice(&kind.to_le_bytes());
                data.extend_from_slice(&(value.len() as u16).to_le_bytes());
                data.extend_from_slice(value);
            }
        }
        data
    }

    #[test]
    fn a_wallet_that_cant_settle_is_told_from_one_that_can() {
        let token = Token {
            mint: Pubkey::new_unique(),
            program: mtx_kit::token::TOKEN_2022_PROGRAM_ID,
            decimals: 6,
        };
        let owner = Pubkey::new_unique();
        let leg = Leg {
            owner,
            token,
            pays: 100,
            bot: true,
        };
        let program = token.program;
        let check = |data: &[u8]| unfit(&leg, Some((&program, data)));
        let plain = token_account(&token.mint, &owner, 100, INITIALIZED, &[]);
        assert_eq!(check(&plain), None);
        // An associated account's own extension, and a memo it doesn't
        // require, are fine.
        let immutable = token_account(&token.mint, &owner, 100, 1, &[(7, &[]), (8, &[0])]);
        assert_eq!(check(&immutable), None);
        let receiving = Leg { pays: 0, ..leg };
        let empty = token_account(&token.mint, &owner, 0, INITIALIZED, &[]);
        assert_eq!(unfit(&receiving, Some((&program, &empty))), None);

        let short = token_account(&token.mint, &owner, 99, INITIALIZED, &[]);
        assert_eq!(check(&short).unwrap(), "holds 99 of the 100 it pays");
        assert!(unfit(&leg, None).is_some(), "closed");
        let other = token_account(&token.mint, &Pubkey::new_unique(), 100, 1, &[]);
        assert!(check(&other).is_some(), "handed to another owner");
        let frozen = token_account(&token.mint, &owner, 100, 2, &[]);
        assert!(check(&frozen).is_some());
        let memo = token_account(&token.mint, &owner, 100, 1, &[(7, &[]), (8, &[1])]);
        assert!(check(&memo).is_some(), "requires a memo on transfers in");
        let classic = mtx_kit::token::TOKEN_PROGRAM_ID;
        assert!(unfit(&leg, Some((&classic, &plain))).is_some());
    }

    #[tokio::test]
    async fn sizes_of_5_to_150_fill_a_60_bp_quote_at_a_sensible_rate() {
        // SPCX's calibration: trades of about 0.02 SPCX; requests of 5 to
        // 150 SPCX, about 27 in the middle, at $143.
        let noted = Arc::new(Noted::default());
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> = Arc::new(|s| at(s, 143.0));
        let scale = Scale {
            trades_per_minute: 1.5,
            trade_size: 0.02,
            trade_size_sigma: 1.0,
        };
        let spec = RfqClients {
            clients: 6,
            per_100_slots: 50.0,
            size_mult: 1_350.0,
            ..RfqClients::default()
        };
        let (mut clients, board, _) = clients(spec, scale, noted.clone(), path);
        let seat = RfqSeat::new(board.clone());
        let (mut asked, mut sizes) = (0, Vec::new());
        for s in (2..20_000).step_by(2) {
            clients.after_blocks(&at(s, 143.0)).await;
            for request in board.open() {
                asked += 1;
                sizes.push(request.size);
                seat.send(&planned(
                    &request.id,
                    Some(143.0 * 0.994),
                    Some(143.0 * 1.006),
                ))
                .await;
            }
            clients.before_blocks(&at(s, 143.0)).await;
        }
        sizes.sort_by(f64::total_cmp);
        let (p05, p95) = (sizes[sizes.len() / 20], sizes[sizes.len() * 19 / 20]);
        assert!(
            (3.0..8.0).contains(&p05) && (100.0..200.0).contains(&p95),
            "{p05} {p95}"
        );
        let swaps = noted.swaps.lock().unwrap().clone();
        let rate = swaps.len() as f64 / asked as f64;
        assert!((0.08..0.35).contains(&rate), "{} of {asked}", swaps.len());
        // The large ones fill; the small ones find 60 bp too far.
        assert!(swaps.iter().all(|(_, _, size, _)| *size > 60.0));
        assert!(swaps.iter().all(|(_, wants, _, price)| match wants {
            Side::Bid => *price == 143.0 * 1.006,
            Side::Ask => *price == 143.0 * 0.994,
        }));
    }

    #[test]
    fn a_swap_moves_each_token_the_agreed_amount() {
        let (base, usdc) = amounts(27.123456789, 143.07, 6, 6).unwrap();
        assert_eq!((base, usdc), (27_123_456, 3_880_552_850));
        assert!(amounts(0.0000001, 143.0, 6, 6).is_err());
        let token = |n: u8| Token {
            mint: Pubkey::new_unique(),
            program: Pubkey::new_unique(),
            decimals: n,
        };
        let swaps = Swaps::new(
            "http://127.0.0.1:1".into(),
            Keypair::new(),
            Vec::new(),
            token(6),
            token(6),
        );
        let (buyer, seller) = (Pubkey::new_unique(), Pubkey::new_unique());
        let ixs = swaps.instructions(&buyer, &seller, 2.0, 150.5).unwrap();
        assert_eq!(ixs.len(), 3);
        let again = swaps.instructions(&buyer, &seller, 2.0, 150.5).unwrap();
        assert_ne!(ixs[0].data, again[0].data, "two alike swaps differ");
        let (base, quote) = (&ixs[1], &ixs[2]);
        assert_eq!(base.data[0], TRANSFER_CHECKED);
        assert_eq!(
            u64::from_le_bytes(base.data[1..9].try_into().unwrap()),
            2_000_000
        );
        assert_eq!(
            u64::from_le_bytes(quote.data[1..9].try_into().unwrap()),
            301_000_000
        );
        // The base from the seller to the buyer, signed by the seller; the
        // USDC the other way.
        assert_eq!(base.accounts[3].pubkey, seller);
        assert_eq!(quote.accounts[3].pubkey, buyer);
        assert!(base.accounts[3].is_signer && quote.accounts[3].is_signer);
    }

    #[test]
    fn informed_clients_are_drawn_per_session_and_value_ahead() {
        let ledger = Ledger::new(false);
        let seats = (0..10)
            .map(|_| WalletSeat {
                pubkey: Pubkey::new_unique(),
                seat: Box::new(Book::new("c", 99.0, 101.0).0),
            })
            .collect();
        let wallets = ledger.wallets("rfq-client", seats);
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> =
            Arc::new(|s| at(s, if s >= 50 { 102.0 } else { 100.0 }));
        let clients = Clients::new(
            RfqClients {
                clients: 10,
                informed_share: 0.3,
                lead_slots: 40,
                correlation: 0.5,
                ..RfqClients::default()
            },
            scale(),
            wallets,
            RfqBoard::new("rfq:X".into()),
            None,
            path,
            Rng::new([1; 32]),
        );
        assert_eq!(clients.informed.iter().filter(|i| **i).count(), 3);
        let informed = clients.informed.iter().position(|i| *i).unwrap();
        let plain = clients.informed.iter().position(|i| !*i).unwrap();
        assert_eq!(clients.value(informed, &at(20, 100.0)), 101.0);
        assert_eq!(clients.value(plain, &at(20, 100.0)), 100.0);
    }
}
