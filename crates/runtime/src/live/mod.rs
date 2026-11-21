//! The runtime that runs strategies against a live chain
//! (`docs/rfd/0004-strategies-on-mainnet.md`), fed by what the chain does
//! as it happens ([`mtx_core::feed`]). It lives in the process that feeds
//! it (a validator, through `mtx-geyser`; surfpool, in tests) on a thread
//! of its own.
//!
//! It keeps the watched accounts ([`feed::AccountCache`]), and at every
//! slot event reads each configured market from them
//! ([`perp::PerpReader`]) and appends the reading to its journal. With a
//! [`session::SessionConfig`], it also plays sessions a grader hands it:
//! it hosts the strategy component, reads its market from the cache, and
//! sends its orders as the bot, its steps coming from the grader's turns.
//! With a [`strategies::ControlConfig`], it runs strategies on its own:
//! each stepped on the chain's slots, controlled through a Unix socket
//! (`mtx live`), and kept on disk across restarts. Their orders go to the
//! leaders' TPU ([`tpu`]) or, in a shadow run, nowhere ([`shadow`]).

pub mod blockhash;
pub mod bot;
pub mod events;
pub mod feed;
pub mod journal;
pub mod perp;
pub mod session;
pub mod shadow;
pub mod strategies;
pub mod tape;
pub mod telemetry;
pub mod tpu;

use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};

use mtx_core::feed::{AccountWrite, Address, FeedEvent};
use mtx_kit::chain::{ChainClient, RpcChainClient};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use self::{
    feed::{AccountCache, FeedReceiver, FeedSender, Watch},
    perp::{PerpReader, PerpReading},
};

/// How long the backfill's reads may take: a node that doesn't answer in
/// this long isn't one to start trading on.
const BACKFILL_TIMEOUT: Duration = Duration::from_secs(30);

/// The most accounts one `getMultipleAccounts` call may name: the RPC's own
/// limit.
const ACCOUNTS_PER_READ: usize = 100;

/// The most markets one runtime reads: each is read at every slot event,
/// on one thread.
pub const MARKETS_MAX: usize = 64;

/// What the runtime is told to do: the `mtx` member of its host's
/// configuration file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where the journal is written, one JSON object a line. Truncated at
    /// start.
    pub journal: PathBuf,
    /// A JSON-RPC endpoint of the node the feed comes from, to read the
    /// watched accounts once at start: a feed only tells of writes, and an
    /// account that isn't written is never told. Without it, a market
    /// reads as an error until every account it needs was written.
    #[serde(default)]
    pub backfill_rpc_url: Option<String>,
    /// Journal every market after every write of a watched account too,
    /// under the status `write`: every state the chain passed through, not
    /// only the one each slot event found. For checking the runtime against
    /// another reader; on a busy chain it is a line a transaction.
    #[serde(default)]
    pub journal_writes: bool,
    pub markets: Vec<MarketConfig>,
    /// Play sessions for a grader, whose strategy the runtime hosts
    /// ([`session`]): the bench's step 3 of `docs/rfd/0004`.
    #[serde(default)]
    pub session: Option<session::SessionConfig>,
    /// Run strategies on its own, stepped on the chain's slots and
    /// controlled through a socket ([`strategies`]): step 4.
    #[serde(default)]
    pub control: Option<strategies::ControlConfig>,
    /// Logs, traces and metrics ([`telemetry`]): without it, the
    /// runtime's events go nowhere.
    #[serde(default)]
    pub telemetry: Option<telemetry::TelemetryConfig>,
}

/// One account on one market, to read.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketConfig {
    /// A file holding Phoenix's `/exchange` response: the market's
    /// accounts and its tick and lot sizes.
    pub exchange: PathBuf,
    /// The market, as Phoenix names it (`SPCX`).
    pub symbol: String,
    /// The key the account trades under, base58.
    pub authority: String,
}

impl Config {
    /// The `mtx` member of the JSON file at `path`: a geyser plugin's
    /// configuration file, which also names the library.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut file: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mtx = file
            .get_mut("mtx")
            .map(serde_json::Value::take)
            .ok_or_else(|| format!("{}: no `mtx` member", path.display()))?;
        serde_json::from_value(mtx).map_err(|e| format!("{}: mtx: {e}", path.display()))
    }

    fn readers(&self) -> Result<Vec<PerpReader>, String> {
        if self.markets.len() > MARKETS_MAX {
            return Err(format!(
                "{} markets, and a runtime reads at most {MARKETS_MAX}",
                self.markets.len()
            ));
        }
        self.markets
            .iter()
            .map(|market| {
                let path = &market.exchange;
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                let exchange =
                    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
                let authority: Pubkey = market
                    .authority
                    .parse()
                    .map_err(|_| format!("{} is not a pubkey", market.authority))?;
                PerpReader::new(exchange, &market.symbol, &authority)
            })
            .collect()
    }
}

/// One line of the journal: a market as the runtime read it when a slot
/// reached a status.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reading<'a> {
    pub slot: u64,
    pub status: &'static str,
    pub symbol: &'a str,
    /// The reading, or why there is none.
    #[serde(flatten)]
    pub read: Read,
    /// Events the feed has dropped so far: above 0, the cache may be stale.
    pub dropped: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Read {
    Ok(PerpReading),
    Failed { error: String },
}

/// A running runtime: its thread ends when every [`FeedSender`] is gone.
#[derive(Debug)]
pub struct Runtime {
    thread: JoinHandle<()>,
    /// Flushed once the runtime's thread has ended, so its last events go
    /// out.
    telemetry: Option<telemetry::Telemetry>,
}

impl Runtime {
    /// Start the runtime `config` describes. The sender is the feed's
    /// input: hand it to whatever sees the chain.
    pub fn start(config: Config) -> Result<(Self, FeedSender), String> {
        let telemetry = config.telemetry.as_ref().map(telemetry::init).transpose()?;
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            markets = config.markets.len(),
            control = config.control.is_some(),
            session = config.session.is_some(),
            "the runtime starts"
        );
        let readers = config.readers()?;
        let journal = std::fs::File::create(&config.journal)
            .map_err(|e| format!("{}: {e}", config.journal.display()))?;
        let addresses: Vec<Address> = readers.iter().flat_map(PerpReader::addresses).collect();
        let watch = Arc::new(Watch::new(addresses.iter().copied()));
        let (sender, receiver) = feed::channel(watch.clone());
        let cache = Arc::new(Mutex::new(AccountCache::default()));
        // Set once the cache holds what the backfill found: a session that
        // starts before would read a market the feed hasn't told of yet.
        let ready = Arc::new(session::Ready::default());
        if let Some(sessions) = config.session.clone() {
            session::spawn(
                sessions,
                readers.clone(),
                cache.clone(),
                receiver.progress(),
                ready.clone(),
            )?;
        }
        let latest_blockhash = Arc::new(blockhash::LatestBlockhash::default());
        // The latest slot the feed reported, for the strategies' steps.
        let (slots, slots_seen) = tokio::sync::watch::channel(0u64);
        let mut slot_events = None;
        if let Some(control) = config.control.clone() {
            let (events, received) = match control.tpu {
                Some(_) => {
                    let (events, received) = tpu::slot_events();
                    (Some(events), Some(received))
                }
                None => (None, None),
            };
            slot_events = events;
            strategies::spawn(
                control,
                strategies::Inputs {
                    shared: strategies::Shared {
                        cache: cache.clone(),
                        watch,
                        progress: receiver.progress(),
                        events: events::Events::default(),
                        blockhash: latest_blockhash.clone(),
                        readings: Arc::default(),
                    },
                    slots: slots_seen,
                    slot_events: received,
                },
            )?;
        }
        let thread = std::thread::Builder::new()
            .name("mtx-live".into())
            .spawn(move || {
                if let Some(url) = &config.backfill_rpc_url {
                    let mut backfilled = AccountCache::default();
                    match backfill_blocking(url, &addresses, &mut backfilled) {
                        Ok(found) => tracing::info!(found, of = addresses.len(), "backfilled"),
                        Err(error) => tracing::warn!(%error, "the backfill failed"),
                    }
                    lock(&cache).merge(backfilled);
                }
                ready.set();
                run(
                    &receiver,
                    &readers,
                    &cache,
                    BufWriter::new(journal),
                    config.journal_writes,
                    &Slots {
                        newest: slots,
                        events: slot_events,
                        blockhash: latest_blockhash,
                    },
                );
            })
            .map_err(|e| format!("the runtime's thread: {e}"))?;
        Ok((Self { thread, telemetry }, sender))
    }

    /// Wait for the runtime to end, once its senders are dropped. Its
    /// journal is complete when this returns.
    pub fn join(self) {
        if self.thread.join().is_err() {
            tracing::error!("the runtime's thread panicked");
        }
        tracing::info!("the runtime stopped");
        drop(self.telemetry);
    }
}

/// The status a journal line carries when a write, not a slot event,
/// made it ([`Config::journal_writes`]).
pub const WRITE: &str = "write";

/// The runtime's loop: keep the accounts, and journal every market at
/// every slot event (and after every write, if `journal_writes`), until
/// the feed ends.
fn run(
    events: &FeedReceiver,
    readers: &[PerpReader],
    cache: &Mutex<AccountCache>,
    mut journal: impl Write,
    journal_writes: bool,
    slots: &Slots,
) {
    while let Some(event) = events.next() {
        let mut cache = lock(cache);
        let (slot, status) = match event {
            FeedEvent::Account(write) => {
                let slot = write.slot;
                let kept = cache.apply(write);
                events.applied();
                if !(kept && journal_writes) {
                    continue;
                }
                (slot, WRITE)
            }
            FeedEvent::Slot { slot, status, .. } => {
                events.applied();
                events.applied_slot(slot);
                // Strategies step on the newest slot the node has a bank
                // for (`feed::banked`).
                if feed::banked(status) {
                    events.slot_banked(slot);
                    slots.newest.send_if_modified(|seen| {
                        let newer = slot > *seen;
                        *seen = (*seen).max(slot);
                        newer
                    });
                }
                if let Some(events) = &slots.events {
                    events.offer(slot, status);
                }
                (slot, status.as_str())
            }
            FeedEvent::Block {
                slot, blockhash, ..
            } => {
                events.applied();
                match blockhash.parse() {
                    Ok(hash) => slots.blockhash.produced(slot, hash),
                    Err(error) => tracing::warn!(slot, %error, "a block's hash doesn't read"),
                }
                continue;
            }
        };
        if let Err(error) = record(
            &mut journal,
            readers,
            &cache,
            slot,
            status,
            events.dropped(),
        ) {
            // A journal that can't be written is a runtime that can't be
            // audited: stop rather than run unseen.
            tracing::error!(%error, "the journal can't be written; the runtime stops");
            return;
        }
    }
}

/// Where the feed's loop reports slots: the newest, for the strategies'
/// steps, and each start and end, for the TPU's leader tracker.
struct Slots {
    newest: tokio::sync::watch::Sender<u64>,
    events: Option<tpu::SlotEvents>,
    /// Where each block's hash goes, for the strategies' transactions.
    blockhash: Arc<blockhash::LatestBlockhash>,
}

impl Slots {
    #[cfg(test)]
    fn none() -> Self {
        Self {
            newest: tokio::sync::watch::channel(0).0,
            events: None,
            blockhash: Arc::default(),
        }
    }
}

/// The cache, locked. A thread that panicked holding it left it as its
/// last whole write: each write replaces one account at once.
fn lock(cache: &Mutex<AccountCache>) -> std::sync::MutexGuard<'_, AccountCache> {
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn record(
    journal: &mut impl Write,
    readers: &[PerpReader],
    cache: &AccountCache,
    slot: u64,
    status: &'static str,
    dropped: u64,
) -> std::io::Result<()> {
    for reader in readers {
        let reading = Reading {
            slot,
            status,
            symbol: reader.symbol(),
            read: match reader.read(cache, slot) {
                Ok(reading) => Read::Ok(reading),
                Err(error) => Read::Failed { error },
            },
            dropped,
        };
        serde_json::to_writer(&mut *journal, &reading)?;
        journal.write_all(b"\n")?;
    }
    // Once an event, not once a line: a reader of the file sees whole
    // events.
    journal.flush()
}

/// Read `addresses` from `chain` into `cache`, each as a write at the slot
/// the node was at before the read: a write the feed tells of from that
/// slot on replaces it, and an older one doesn't. Returns how many exist.
pub async fn backfill(
    chain: &dyn ChainClient,
    addresses: &[Address],
    cache: &mut AccountCache,
) -> Result<usize, String> {
    let slot = chain.get_slot().await.map_err(|e| e.to_string())?;
    let mut found = 0;
    for addresses in addresses.chunks(ACCOUNTS_PER_READ) {
        let keys: Vec<Pubkey> = addresses
            .iter()
            .copied()
            .map(Pubkey::new_from_array)
            .collect();
        let accounts = chain
            .get_multiple_accounts(&keys)
            .await
            .map_err(|e| e.to_string())?;
        assert_eq!(accounts.len(), keys.len(), "one answer an account asked");
        for (address, account) in addresses.iter().zip(accounts) {
            let Some(account) = account else {
                continue;
            };
            found += 1;
            cache.apply(AccountWrite {
                slot,
                address: *address,
                owner: account.owner.to_bytes(),
                lamports: account.lamports,
                data: account.data,
                write_version: 0,
            });
        }
    }
    Ok(found)
}

fn backfill_blocking(
    url: &str,
    addresses: &[Address],
    cache: &mut AccountCache,
) -> Result<usize, String> {
    let chain = RpcChainClient::with_timeout(url, BACKFILL_TIMEOUT);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(backfill(&chain, addresses, cache))
}

#[cfg(test)]
mod tests {
    use mtx_core::feed::SlotStatus;
    use mtx_kit::chain::mock::MockChainClient;

    use super::*;

    /// Phoenix's metadata for one market, as much of it as a reader needs.
    fn exchange() -> mtx_kit::phoenix_rise::types::exchange::ExchangeResponse {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let text =
            std::fs::read_to_string(root.join("snapshots/phoenix-spcx-2026-10-29/exchange.json"))
                .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn reader() -> PerpReader {
        PerpReader::new(exchange(), "SPCX", &Pubkey::new_from_array([7; 32])).unwrap()
    }

    fn slot(slot: u64) -> FeedEvent {
        FeedEvent::Slot {
            slot,
            parent: Some(slot - 1),
            status: SlotStatus::Processed,
        }
    }

    fn lines(journal: &[u8]) -> Vec<serde_json::Value> {
        std::str::from_utf8(journal)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn a_market_whose_accounts_were_never_seen_is_an_error_not_a_guess() {
        let readers = [reader()];
        let (sender, receiver) = feed::channel(Arc::new(Watch::default()));
        sender.send(slot(10));
        // An account that isn't Phoenix's, at the asset map's address: seen,
        // and still no reading.
        sender.send(FeedEvent::Account(AccountWrite {
            slot: 11,
            address: readers[0].addresses()[0],
            owner: [0; 32],
            lamports: 1,
            data: vec![0; 8],
            write_version: 0,
        }));
        sender.send(slot(11));
        drop(sender);
        let mut journal = Vec::new();
        run(
            &receiver,
            &readers,
            &Mutex::default(),
            &mut journal,
            true,
            &Slots::none(),
        );
        let lines = lines(&journal);
        let seen: Vec<_> = lines
            .iter()
            .map(|line| {
                (
                    line["slot"].as_u64().unwrap(),
                    line["status"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(seen, [(10, "processed"), (11, WRITE), (11, "processed")]);
        for line in &lines {
            assert_eq!(line["symbol"], "SPCX");
            assert_eq!(line["dropped"], 0);
            assert!(line.get("mark").is_none(), "{line}");
        }
        assert!(lines[0]["error"].as_str().unwrap().contains("wasn't seen"));
        // The asset map is seen now, and the next account a read needs isn't.
        assert!(
            lines[2]["error"]
                .as_str()
                .unwrap()
                .contains("the global trader index")
        );
    }

    #[test]
    fn writes_are_journaled_only_when_asked() {
        let readers = [reader()];
        let (sender, receiver) = feed::channel(Arc::new(Watch::default()));
        sender.send(FeedEvent::Account(AccountWrite {
            slot: 5,
            address: [1; 32],
            owner: [0; 32],
            lamports: 1,
            data: Vec::new(),
            write_version: 0,
        }));
        sender.send(FeedEvent::Block {
            slot: 5,
            blockhash: "hash".into(),
            block_time_unix: None,
            block_height: None,
        });
        drop(sender);
        let mut journal = Vec::new();
        run(
            &receiver,
            &readers,
            &Mutex::default(),
            &mut journal,
            false,
            &Slots::none(),
        );
        assert!(journal.is_empty());
    }

    #[tokio::test]
    async fn a_backfill_reads_what_exists_as_of_the_nodes_slot() {
        let (held, missing) = ([1; 32], [2; 32]);
        let chain = MockChainClient::new();
        chain.set_slot(40).set_account(
            Pubkey::new_from_array(held),
            solana_account::Account {
                lamports: 9,
                data: vec![1, 2, 3],
                owner: Pubkey::new_from_array([5; 32]),
                executable: false,
                rent_epoch: 0,
            },
        );
        let mut cache = AccountCache::default();
        assert_eq!(backfill(&chain, &[held, missing], &mut cache).await, Ok(1));
        assert_eq!(cache.data(&held), Some(&[1, 2, 3][..]));
        assert_eq!(cache.slot(&held), Some(40));
        assert_eq!(cache.data(&missing), None);
        // A write the feed tells of from an earlier slot is older than the
        // backfill, and one from its slot on replaces it.
        let write = |slot, byte| AccountWrite {
            slot,
            address: held,
            owner: [5; 32],
            lamports: 9,
            data: vec![byte],
            write_version: 0,
        };
        assert!(!cache.apply(write(39, 7)));
        assert!(cache.apply(write(40, 8)));
        assert_eq!(cache.data(&held), Some(&[8][..]));
    }

    #[test]
    fn a_configuration_is_the_files_mtx_member_and_nothing_unknown() {
        let dir = std::env::temp_dir().join(format!("mtx-live-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plugin.json");
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            Config::load(&path)
        };
        let config = load(
            r#"{ "libpath": "libmtx_geyser.so",
                 "mtx": { "journal": "j.jsonl",
                          "markets": [ { "exchange": "e.json", "symbol": "SPCX", "authority": "k" } ] } }"#,
        )
        .unwrap();
        assert_eq!(config.markets.len(), 1);
        assert_eq!(config.backfill_rpc_url, None);
        assert!(!config.journal_writes);
        assert!(
            load(r#"{ "libpath": "x" }"#)
                .unwrap_err()
                .contains("no `mtx` member")
        );
        assert!(
            load(r#"{ "mtx": { "journal": "j", "markets": [], "jornal_writes": true } }"#)
                .unwrap_err()
                .contains("unknown field")
        );
        assert!(Config::load(&dir.join("missing.json")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_runtime_refuses_more_markets_than_it_reads() {
        let market = MarketConfig {
            exchange: "e.json".into(),
            symbol: "SPCX".into(),
            authority: "k".into(),
        };
        let config = Config {
            journal: "j".into(),
            backfill_rpc_url: None,
            journal_writes: false,
            markets: vec![market; MARKETS_MAX + 1],
            session: None,
            control: None,
            telemetry: None,
        };
        assert!(config.readers().unwrap_err().contains("at most"));
    }
}
