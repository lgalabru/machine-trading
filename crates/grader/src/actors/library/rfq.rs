//! RFQ clients, and the desk they ask: the host side of the interface's
//! requests for quotes.
//!
//! Clients send requests to the bot through an [`RfqBoard`]. A component
//! sees the open ones in its market's `rfqs`, and answers on the board's
//! venue (`rfq:<symbol>`) with a response line, which the board's seat
//! ([`RfqSeat`]) takes. At the start of the next step each client looks at
//! the answer to its request and takes it if the price suits it: no worse
//! than its own value of the asset by `tolerance_bps`, and better than the
//! screen (the market's own best price on its side).
//!
//! A client values the asset at the scenario's level, unless it is
//! informed: then it sees the level `lead_slots` ahead, and its value moves
//! that way by `correlation`. An informed client asks to trade the way it
//! sees the price going. Which clients are informed is drawn per session.
//! A request's id names its client (`c<n>-<k>`), so a desk can keep a
//! record per client.
//!
//! A trade settles on the session's market, between the client's wallet and
//! the bot's, as two orders on the chain: the client rests the size at the
//! agreed price, the bot's account takes it, and the client cancels what is
//! left. The price goes onto the market's tick grid, and a price through the
//! screen (an ask under the best bid, say) settles one tick inside it, where
//! the bot's take meets the client's order first.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use mtx_kit::{Action, Planned, SendState, Sent, Side};
use serde::{Deserialize, Serialize};

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

fn twenty() -> f64 {
    20.0
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
    /// How far past its value a client pays, in basis points.
    #[serde(default = "twenty")]
    pub tolerance_bps: f64,
    /// The share of requests that ask for both sides.
    #[serde(default = "half")]
    pub two_way_share: f64,
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
            tolerance_bps: twenty(),
            two_way_share: half(),
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

pub(super) struct Clients {
    spec: RfqClients,
    scale: Scale,
    wallets: Vec<Wallet>,
    informed: Vec<bool>,
    board: RfqBoard,
    bot: Option<Box<dyn Seat>>,
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
        bot: Option<Box<dyn Seat>>,
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
            bot,
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
        let Some(bot) = self.bot.as_deref() else {
            return 0.0;
        };
        let wallet = &self.wallets[request.client];
        let value = self.value(request.client, now);
        let tolerance = self.spec.tolerance_bps / 10_000.0;
        let (price, suits) = match request.wants {
            Side::Bid => match response.ask {
                Some(ask) => (ask, ask <= value * (1.0 + tolerance)),
                None => return 0.0,
            },
            Side::Ask => match response.bid {
                Some(bid) => (bid, bid >= value * (1.0 - tolerance)),
                None => return 0.0,
            },
        };
        if !suits {
            return 0.0;
        }
        let seat = wallet.seat();
        let tick = seat.tick();
        let Ok((best_bid, best_ask)) = seat.top().await else {
            return 0.0;
        };
        let mut price = (price / tick).round() * tick;
        // Through the screen: one tick inside it. Worse than the screen:
        // the client trades there instead.
        match request.wants {
            Side::Bid => {
                if let Some(bid) = best_bid {
                    price = price.max(bid + tick);
                }
                if best_ask.is_some_and(|ask| price >= ask - tick / 2.0) {
                    return 0.0;
                }
            }
            Side::Ask => {
                if let Some(ask) = best_ask {
                    price = price.min(ask - tick);
                }
                if best_bid.is_some_and(|bid| price <= bid + tick / 2.0) {
                    return 0.0;
                }
            }
        }
        let size = response.size.map_or(request.size, |s| s.min(request.size));
        let before = bot.position().await.unwrap_or(0.0);
        if !wallet.rest(now.at, request.wants, price, size).await {
            return 0.0;
        }
        let taken = bot.take(request.wants.opposite(), size, price).await;
        if let Err(error) = &taken {
            tracing::warn!(%error, id = request.id, "the bot's side of an RFQ fill failed");
        }
        wallet.cancel_all(now.at).await;
        // What filled, as the bot's account shows it.
        let after = bot.position().await.unwrap_or(before);
        (after - before).abs()
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
        Ledger, WalletSeat,
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

    #[tokio::test]
    async fn clients_ask_the_bot_and_take_quotes_that_suit_them() {
        let ledger = Ledger::new(true);
        let (client_book, client_log) = Book::new("client", 99.90, 100.10);
        let (bot_book, bot_log) = Book::new("bot", 99.90, 100.10);
        let wallets = ledger.wallets(
            "rfq-client",
            vec![WalletSeat {
                pubkey: solana_pubkey::Pubkey::new_unique(),
                seat: Box::new(client_book),
            }],
        );
        let board = RfqBoard::new("rfq:SPCX".into());
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> = Arc::new(|s| at(s, 100.0));
        let spec = RfqClients {
            clients: 1,
            per_100_slots: 50.0,
            two_way_share: 0.0,
            tolerance_bps: 5.0,
            ..RfqClients::default()
        };
        let mut clients = Clients::new(
            spec,
            scale(),
            wallets,
            board.clone(),
            Some(Box::new(bot_book)),
            path,
            Rng::new([9; 32]),
        )
        .starting_at(1_000);
        let seat = RfqSeat::new(board.clone());
        let mut s = 0;
        let request = loop {
            s += 2;
            clients.after_blocks(&at(s, 100.0)).await;
            if let Some(r) = board.open().into_iter().next() {
                break r;
            }
        };
        assert_eq!(request.venue, "rfq:SPCX");
        assert_eq!(request.deadline_slot, 1_000 + s + 8);
        assert!(request.id.starts_with("c0-"));
        // A quote far from the client's value is declined; nothing trades.
        let (bad_bid, bad_ask) = (Some(90.0), Some(110.0));
        assert!(
            seat.send(&planned(&request.id, bad_bid, bad_ask))
                .await
                .state
                .landed()
        );
        clients.before_blocks(&at(s, 100.0)).await;
        assert!(client_log.lock().unwrap().is_empty());
        // Unknown ids are refused.
        let refused = seat.send(&planned("c9-9", Some(1.0), None)).await;
        assert!(!refused.state.landed());

        // The next request gets a fair quote: it settles as the client's
        // order and the bot's take, at the quote.
        let request = loop {
            s += 2;
            clients.after_blocks(&at(s, 100.0)).await;
            if let Some(r) = board.open().into_iter().next() {
                break r;
            }
        };
        let wants_next = request.side.unwrap();
        let sent = seat
            .send(&planned(&request.id, Some(99.97), Some(100.03)))
            .await;
        assert!(sent.state.landed() && !sent.sent);
        clients.before_blocks(&at(s, 100.0)).await;
        let client_log = client_log.lock().unwrap().clone();
        let bot_log = bot_log.lock().unwrap().clone();
        let (price, client_side, bot_side) = match wants_next {
            Side::Bid => ("100.03", "Bid", "Ask"),
            Side::Ask => ("99.97", "Ask", "Bid"),
        };
        assert_eq!(client_log.len(), 2, "{client_log:?}");
        assert!(client_log[0].starts_with(&format!("client rest {client_side}")));
        assert!(client_log[0].ends_with(price), "{client_log:?}");
        assert_eq!(client_log[1], "client cancel");
        assert!(
            bot_log[0].starts_with(&format!("bot take {bot_side}")) && bot_log[0].ends_with(price)
        );
        let counts = &ledger.report().actors[0].counts;
        assert_eq!((counts["declined"], counts["fills"]), (1, 1));
        assert!(
            !board.open().iter().any(|r| r.id == request.id),
            "answered: closed"
        );
    }

    #[test]
    fn informed_clients_are_drawn_per_session_and_value_ahead() {
        let ledger = Ledger::new(false);
        let seats = (0..10)
            .map(|_| WalletSeat {
                pubkey: solana_pubkey::Pubkey::new_unique(),
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
