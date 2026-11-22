//! Sending straight to the leaders (`docs/rfd/0004`, step 5): a strategy's
//! transactions go over QUIC to the TPU of the current leader and the next
//! ones, with no RPC hop.
//!
//! Anza's `solana-tpu-client-next` holds the connections: it connects to
//! the next leaders ahead of their slots, so a send never waits for a
//! handshake, and with a staked identity its certificate gets the stake's
//! share of the leaders' streams. Who leads comes from two places: the
//! slot from the feed (the geyser's `FirstShredReceived` and `Completed`,
//! as the client expects), and the epoch's schedule and the cluster's TPU
//! addresses from the node's own RPC, once an epoch and every few minutes,
//! off the hot path.
//!
//! [`TpuChainClient`] is the chain a strategy's desk trades through: its
//! reads go to the node's RPC as before, its sends to the TPU. A send the
//! TPU client can't take (no leader connection yet) goes through the RPC.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use mtx_core::feed::SlotStatus;
use mtx_kit::chain::{
    ChainClient, ChainError, RpcChainClient, SimulationOutcome, TransactionSummary,
};
use serde::Deserialize;
use solana_account::Account;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_tpu_client_next::{
    ClientBuilder, TransactionSender,
    client_builder::Client,
    node_address_service::{LeaderTpuCacheServiceConfig, NodeAddressService, SlotEvent},
};
use solana_transaction::{Hash, Signature, versioned::VersionedTransaction};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How the runtime sends: the `tpu` member of its `control` configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TpuConfig {
    /// The keypair whose stake the sends carry: the validator's identity,
    /// whose path the operator gives, since the geyser interface doesn't.
    /// Without it the sends are an unstaked peer's, the first a loaded
    /// leader drops.
    #[serde(default)]
    pub identity: Option<PathBuf>,
    /// How many leaders each transaction goes to: the current one and the
    /// next.
    #[serde(default = "default_fanout")]
    pub fanout: usize,
    /// Have the node simulate each transaction before it is sent, and send
    /// nothing that would fail. Off by default: a round trip to the node on
    /// every order, against the fee of a transaction that fails on chain
    /// (5,000 lamports), which a post-only order that came too late is.
    #[serde(default)]
    pub preflight: bool,
}

fn default_fanout() -> usize {
    DEFAULT_FANOUT
}

/// The current leader and the next: a send made at the end of a leader's
/// window still lands.
const DEFAULT_FANOUT: usize = 2;

/// The most leaders a send may go to: past a few, a send only costs
/// bandwidth.
const FANOUT_MAX: usize = 8;

/// Connections kept open: the fanout's leaders and the ones after, with
/// room for leaders that repeat.
const CONNECTIONS_MAX: usize = 32;

/// Slot events waiting between the feed and the leader tracker: a few
/// seconds of slots.
const SLOT_EVENTS_MAX: usize = 1_024;

/// How long a sent transaction is watched for landing before it counts as
/// expired: a blockhash lives 150 slots, about a minute.
const CONFIRM_WITHIN: Duration = Duration::from_secs(75);

/// How often a sent transaction's status is asked for while it's watched.
const CONFIRM_LOOKS_EVERY: Duration = Duration::from_millis(400);

/// Sends watched for their fate at once, across every strategy: each asks
/// the node once every [`CONFIRM_LOOKS_EVERY`] for up to [`CONFIRM_WITHIN`].
/// Thirty-two strategies sending four orders every two slots, none landing,
/// keep about 12,000 in flight; past this bound a send's fate is told as
/// unknown at once rather than asked for.
const CONFIRMS_IN_FLIGHT_MAX: usize = 16_384;

/// The leader tracker's view of the slots: what the feed hands it.
#[derive(Debug, Clone)]
pub struct SlotEvents(mpsc::Sender<SlotEvent>);

impl SlotEvents {
    /// Hand over a slot's status, if the tracker counts it: a slot's first
    /// shred starts it, its last completes it. Never waits: a full channel
    /// drops the event, and the tracker's estimate catches up on the next.
    pub fn offer(&self, slot: u64, status: SlotStatus) {
        let event = match status {
            SlotStatus::FirstShredReceived => SlotEvent::Start(slot),
            SlotStatus::Completed => SlotEvent::End(slot),
            _ => return,
        };
        // Dropping is the documented choice above: the next slot corrects.
        let _ = self.0.try_send(event);
    }
}

/// The channel slot events cross from the feed to the leader tracker.
pub fn slot_events() -> (SlotEvents, mpsc::Receiver<SlotEvent>) {
    let (sender, receiver) = mpsc::channel(SLOT_EVENTS_MAX);
    (SlotEvents(sender), receiver)
}

/// A running TPU client: what sends, and what keeps its connections.
pub struct Tpu {
    sender: TransactionSender,
    /// A permit for each send watched for its fate
    /// ([`CONFIRMS_IN_FLIGHT_MAX`]).
    confirms: Arc<tokio::sync::Semaphore>,
    /// Holds the connection workers and the leader tracker; they stop
    /// with the token.
    _client: Client,
    cancel: CancellationToken,
}

impl Drop for Tpu {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Tpu {
    /// Start tracking leaders from `slots` and the node at `rpc_url`, and
    /// connecting to them. Must run inside a Tokio runtime, whose tasks
    /// keep the connections.
    pub async fn start(
        config: &TpuConfig,
        rpc_url: &str,
        slots: mpsc::Receiver<SlotEvent>,
    ) -> Result<Self, String> {
        if !(1..=FANOUT_MAX).contains(&config.fanout) {
            return Err(format!(
                "a TPU fanout of 1 to {FANOUT_MAX}, not {}",
                config.fanout
            ));
        }
        let identity = match &config.identity {
            Some(path) => Some(super::read_keypair(path)?),
            None => None,
        };
        let cancel = CancellationToken::new();
        let rpc = Arc::new(solana_rpc_client::nonblocking::rpc_client::RpcClient::new(
            rpc_url.to_string(),
        ));
        let leaders = NodeAddressService::run(
            rpc,
            tokio_stream::wrappers::ReceiverStream::new(slots),
            LeaderTpuCacheServiceConfig {
                // Enough to connect ahead to the leaders after the ones sent
                // to.
                lookahead_leaders: u8::try_from(config.fanout + 1).unwrap_or(u8::MAX),
                ..LeaderTpuCacheServiceConfig::default()
            },
            cancel.child_token(),
        )
        .await
        .map_err(|e| format!("the leader tracker: {e}"))?;
        let socket =
            std::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
                .map_err(|e| format!("the TPU client's socket: {e}"))?;
        let (sender, client) = ClientBuilder::new(Box::new(leaders))
            .cancel_token(cancel.child_token())
            .bind_socket(socket)
            .leader_send_fanout(config.fanout)
            .identity(identity.as_ref())
            .max_cache_size(
                std::num::NonZeroUsize::new(CONNECTIONS_MAX).expect("a positive constant"),
            )
            .build()
            .map_err(|e| format!("the TPU client: {e}"))?;
        Ok(Self {
            sender,
            confirms: Arc::new(tokio::sync::Semaphore::new(CONFIRMS_IN_FLIGHT_MAX)),
            _client: client,
            cancel,
        })
    }

    /// Hand `transaction` to the connection workers, without waiting for a
    /// leader: what became of it is read from the chain.
    pub fn send(&self, transaction: &VersionedTransaction) -> Result<Signature, String> {
        let signature = *transaction
            .signatures
            .first()
            .ok_or("an unsigned transaction")?;
        let wire = wincode::serialize(transaction).map_err(|e| e.to_string())?;
        self.sender
            .try_send_transaction(bytes::Bytes::from(wire))
            .map_err(|e| format!("the TPU client: {e}"))?;
        Ok(signature)
    }
}

/// The chain a hosted strategy trades through when the runtime sends over
/// the TPU: every read is the node's RPC's, every send goes to the leaders.
pub struct TpuChainClient {
    rpc: Arc<RpcChainClient>,
    tpu: Arc<Tpu>,
    /// The strategy whose sends these are, for their metrics and fates.
    strategy: String,
    /// The newest blockhash the plugin told of, signed on with no round
    /// trip while it is fresh.
    blockhash: Arc<super::blockhash::LatestBlockhash>,
    /// Where a send's fate is told, once the watcher knows it.
    events: super::events::Events,
}

impl TpuChainClient {
    /// `strategy`'s chain: its name labels the sends' metrics and fates.
    pub fn new(
        rpc: RpcChainClient,
        tpu: Arc<Tpu>,
        strategy: &str,
        blockhash: Arc<super::blockhash::LatestBlockhash>,
        events: super::events::Events,
    ) -> Self {
        Self {
            rpc: Arc::new(rpc),
            tpu,
            strategy: strategy.to_string(),
            blockhash,
            events,
        }
    }

    /// Send over the TPU, or over the RPC if the TPU client can't take it.
    async fn send_once(&self, transaction: &VersionedTransaction) -> Result<Signature, ChainError> {
        match self.tpu.send(transaction) {
            Ok(signature) => {
                tracing::info!(
                    counter.mtx_tpu_sends = 1_u64,
                    strategy = %self.strategy,
                    route = "tpu",
                    %signature,
                    "sent to the leaders"
                );
                Ok(signature)
            }
            Err(error) => {
                tracing::warn!(
                    counter.mtx_tpu_sends = 1_u64,
                    strategy = %self.strategy,
                    route = "rpc",
                    %error,
                    "the TPU didn't take a send; it goes through the RPC"
                );
                self.rpc.send(transaction).await
            }
        }
    }
}

#[async_trait]
impl ChainClient for TpuChainClient {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, ChainError> {
        self.rpc.get_account(pubkey).await
    }

    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, ChainError> {
        self.rpc.get_multiple_accounts(pubkeys).await
    }

    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filters: Vec<solana_rpc_client_api::filter::RpcFilterType>,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError> {
        self.rpc.get_program_accounts(program, filters).await
    }

    async fn get_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
    ) -> Result<Vec<(Pubkey, Account)>, ChainError> {
        self.rpc.get_token_accounts_by_owner(owner, mint).await
    }

    async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, ChainError> {
        self.rpc.get_balance(pubkey).await
    }

    async fn get_slot(&self) -> Result<u64, ChainError> {
        self.rpc.get_slot().await
    }

    async fn get_latest_blockhash(&self) -> Result<Hash, ChainError> {
        match self.blockhash.fresh() {
            Some(hash) => Ok(hash),
            None => self.rpc.get_latest_blockhash().await,
        }
    }

    async fn simulate(
        &self,
        transaction: &VersionedTransaction,
        watch: &[Pubkey],
    ) -> Result<SimulationOutcome, ChainError> {
        self.rpc.simulate(transaction, watch).await
    }

    /// Send over the TPU, then watch for it landing, as `send_and_confirm`
    /// does over RPC: a desk waits for a confirmed fill before it plans
    /// the next line.
    async fn send_and_confirm(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<Signature, ChainError> {
        let signature = self.send_once(transaction).await?;
        match confirm(&self.rpc, &self.strategy, signature, Instant::now()).await {
            Fate::Landed { .. } => Ok(signature),
            Fate::Failed { error, .. } => Err(ChainError::Failed {
                label: "a TPU send".into(),
                signature,
                error,
            }),
            Fate::Unseen => Err(ChainError::Rpc(format!(
                "{signature} wasn't seen landed in {}s",
                CONFIRM_WITHIN.as_secs()
            ))),
            Fate::Unknown(error) => Err(ChainError::Rpc(error)),
        }
    }

    /// Send and return at once; a watcher learns its fate and tells it on
    /// the event stream (`fate`): a live strategy doesn't spend its steps
    /// waiting on its orders.
    async fn send(&self, transaction: &VersionedTransaction) -> Result<Signature, ChainError> {
        let signature = self.send_once(transaction).await?;
        let sent_at = Instant::now();
        let (rpc, strategy, events) =
            (self.rpc.clone(), self.strategy.clone(), self.events.clone());
        let permit = self.tpu.confirms.clone().try_acquire_owned();
        tokio::spawn(async move {
            let fate = match permit {
                Ok(_permit) => confirm(&rpc, &strategy, signature, sent_at).await,
                Err(_) => Fate::Unknown(format!(
                    "not watched: {CONFIRMS_IN_FLIGHT_MAX} sends are watched already"
                )),
            };
            let after_us = u64::try_from(sent_at.elapsed().as_micros()).unwrap_or(u64::MAX);
            let (landed, slot, error) = match fate {
                Fate::Landed { slot } => (true, Some(slot), None),
                Fate::Failed { slot, error } => (false, Some(slot), Some(error.to_string())),
                Fate::Unseen => (false, None, None),
                Fate::Unknown(error) => (false, None, Some(error)),
            };
            events.emit(mtx_core::events::Event::Fate {
                strategy,
                signature: signature.to_string(),
                landed,
                slot,
                error,
                after_us,
            });
        });
        Ok(signature)
    }

    async fn get_transaction(
        &self,
        signature: &Signature,
    ) -> Result<Option<TransactionSummary>, ChainError> {
        self.rpc.get_transaction(signature).await
    }

    async fn signatures_for_address(
        &self,
        address: &Pubkey,
        after_slot: u64,
    ) -> Result<Vec<(Signature, u64)>, ChainError> {
        self.rpc.signatures_for_address(address, after_slot).await
    }
}

/// What became of a sent transaction.
enum Fate {
    Landed {
        slot: u64,
    },
    Failed {
        slot: u64,
        error: solana_transaction::TransactionError,
    },
    /// Not seen landing within [`CONFIRM_WITHIN`].
    Unseen,
    /// The node couldn't be asked.
    Unknown(String),
}

/// Watch `signature` until it lands at `confirmed`, fails, or the watch
/// gives up.
async fn confirm(
    rpc: &RpcChainClient,
    strategy: &str,
    signature: Signature,
    sent_at: Instant,
) -> Fate {
    let deadline = sent_at + CONFIRM_WITHIN;
    loop {
        let status = match rpc.rpc().get_signature_statuses(&[signature]).await {
            Ok(statuses) => statuses.value.into_iter().next().flatten(),
            Err(error) => return Fate::Unknown(error.to_string()),
        };
        // A failure is final once seen; a success counts at `confirmed`,
        // the commitment the RPC client confirms at, so a read that
        // follows sees what the send did.
        match status {
            Some(status) if status.err.is_some() => {
                return Fate::Failed {
                    slot: status.slot,
                    error: status.err.expect("matched a failure"),
                };
            }
            Some(status) if status.satisfies_commitment(CommitmentConfig::confirmed()) => {
                tracing::info!(
                    histogram.mtx_tpu_confirm_ms =
                        u64::try_from(sent_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                    strategy,
                    landed_slot = status.slot,
                    %signature,
                    "a send landed"
                );
                return Fate::Landed { slot: status.slot };
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Fate::Unseen;
        }
        tokio::time::sleep(CONFIRM_LOOKS_EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_slots_start_and_end_reach_the_leader_tracker() {
        let (events, mut received) = slot_events();
        events.offer(7, SlotStatus::FirstShredReceived);
        events.offer(7, SlotStatus::Processed);
        events.offer(7, SlotStatus::Completed);
        assert!(matches!(received.try_recv(), Ok(SlotEvent::Start(7))));
        assert!(matches!(received.try_recv(), Ok(SlotEvent::End(7))));
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_fanout_past_the_limit_is_refused() {
        let (_, slots) = slot_events();
        let config = TpuConfig {
            identity: None,
            fanout: FANOUT_MAX + 1,
            preflight: false,
        };
        let error = Tpu::start(&config, "http://127.0.0.1:1", slots)
            .await
            .err()
            .expect("refused");
        assert!(error.contains("fanout"), "{error}");
    }
}
