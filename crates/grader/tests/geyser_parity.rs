//! The geyser plugin against a grade (`docs/rfd/0004-strategies-on-mainnet.md`,
//! step 2): `mtx-geyser` is loaded into a session's surfnet, and what it
//! reads of the market at each step, from the accounts the feed told it
//! of, is what the grader read through Phoenix's Hawkeye views.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use mtx_grader::{
    grade::{self, GradeOptions},
    keys::SessionKeys,
    scenario::Scenario,
    session,
    wasm::{StepObserver, StepTrace},
};
use serde_json::{Value, json};
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::request::RpcRequest;

/// How a session's strategy trades, which decides what can be compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trades {
    /// It only takes, and every move of the session cranks the market: the
    /// mark the perp asset map holds is the one the view reports, and
    /// nothing changes the account between the grader's read and the
    /// strategy's own orders.
    Takes,
    /// It rests quotes the session's takers fill between its turns: in the
    /// slot after the grader's read, too. The takers' trades don't crank,
    /// so the stored mark lags the view's, which blends the book as it
    /// stands.
    Makes,
}

/// What the grader read at a step: the market the strategy was handed.
/// The mark is `None` where the session doesn't compare it (see
/// [`the_plugin_reads_market_making_as_the_grader_does`]).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Read {
    oracle: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    /// The position and its entry.
    position: (f64, f64),
    mark: Option<f64>,
}

/// One step's numbers, as either side read them.
struct Numbers {
    oracle: f64,
    mark: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    position: f64,
    entry_usd: f64,
}

impl Read {
    fn of(trades: Trades, numbers: Numbers) -> Self {
        Self {
            oracle: numbers.oracle,
            best_bid: numbers.best_bid,
            best_ask: numbers.best_ask,
            position: (numbers.position, numbers.entry_usd),
            mark: (trades == Trades::Takes).then_some(numbers.mark),
        }
    }
}

/// Loads the plugin while the session's surfnet still takes cheatcodes,
/// and keeps each step's read.
struct Watcher {
    plugin: PathBuf,
    dir: PathBuf,
    /// Phoenix's metadata for the snapshot, and the session's market.
    exchange: PathBuf,
    symbol: String,
    trades: Trades,
    /// Each step's slot, how many orders the strategy sent in it, and what
    /// the grader read for it.
    steps: Mutex<Vec<(u64, usize, Read)>>,
    /// The runtime's `session` member, where the plugin plays the session.
    session: Option<Value>,
}

impl Watcher {
    fn journal(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }
}

impl StepObserver for Watcher {
    fn before_lock(&self, url: &str, authority: &str) {
        let config = self.dir.join("mtx-geyser.json");
        let manifest = json!({
            "libpath": self.plugin,
            "mtx": {
                "journal": self.journal(),
                "backfill_rpc_url": url,
                "journal_writes": true,
                "markets": [{
                    "exchange": self.exchange,
                    "symbol": self.symbol,
                    "authority": authority,
                }],
                "session": self.session,
            },
        });
        std::fs::write(&config, manifest.to_string()).unwrap();
        let loaded: Value = RpcClient::new(url.to_string())
            .send(
                RpcRequest::Custom {
                    method: "loadPlugin",
                },
                json!([config]),
            )
            .expect("the surfnet loads the plugin");
        assert!(loaded.to_string().contains("mtx-geyser"), "{loaded}");
    }

    fn started(&self, _session: &mtx_kit::Session, _printed: &[String], failed: Option<&str>) {
        assert_eq!(failed, None, "the strategy's start");
    }

    fn step(&self, trace: &StepTrace) {
        let market = &trace.market;
        self.steps.lock().unwrap().push((
            market.slot,
            trace.sent.len(),
            Read::of(
                self.trades,
                Numbers {
                    oracle: market.oracle,
                    mark: market.mark,
                    best_bid: market.best_bid,
                    best_ask: market.best_ask,
                    position: market.position.size,
                    entry_usd: market.position.entry_usd,
                },
            ),
        ));
    }
}

/// How long the plugin may take to write down what the session's last slot
/// fed it, and how often the journal is looked at meanwhile.
const JOURNAL_SETTLES_WITHIN: std::time::Duration = std::time::Duration::from_secs(20);
const JOURNAL_LOOKS_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// Wait until the journal stops growing. A session ends by stopping its
/// surfnet, which never unloads the plugin, so nothing says the runtime
/// has written the last events it was fed: two looks of the same size do.
async fn settled_journal(path: &Path) {
    let size = || std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut last = size();
    let deadline = std::time::Instant::now() + JOURNAL_SETTLES_WITHIN;
    loop {
        tokio::time::sleep(JOURNAL_LOOKS_EVERY).await;
        let now = size();
        if now == last {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the journal is still growing"
        );
        last = now;
    }
}

/// The plugin's journal, in order: each line's slot, status and reading.
fn journal(path: &Path, trades: Trades) -> Vec<(u64, String, Read)> {
    let text = std::fs::read_to_string(path).expect("the plugin's journal");
    text.lines()
        .map(|line| {
            let line: Value = serde_json::from_str(line).expect("a journal line");
            assert_eq!(line["dropped"], 0, "the feed dropped events: {line}");
            let number = |name: &str| {
                line[name]
                    .as_f64()
                    .unwrap_or_else(|| panic!("no {name}: {line}"))
            };
            (
                line["slot"].as_u64().expect("a slot"),
                line["status"].as_str().expect("a status").to_string(),
                Read::of(
                    trades,
                    Numbers {
                        oracle: number("oracle"),
                        mark: number("mark"),
                        best_bid: line["best_bid"].as_f64(),
                        best_ask: line["best_ask"].as_f64(),
                        position: number("position"),
                        entry_usd: number("entry_usd"),
                    },
                ),
            )
        })
        .collect()
}

/// The states the plugin saw the chain pass through in `slot`: as the slot
/// began, after each write in it, and as it ended.
fn states_in(journal: &[(u64, String, Read)], slot: u64) -> Vec<Read> {
    let is = |status: &'static str| {
        move |line: &&(u64, String, Read)| line.0 == slot && line.1 == status
    };
    let Some(began) = journal.iter().position(|line| is("created_bank")(&line)) else {
        return Vec::new();
    };
    // A session's last slot has no end in the journal: the surfnet stops.
    let ended = journal
        .iter()
        .position(|line| is("processed")(&line))
        .unwrap_or(journal.len() - 1);
    assert!(began <= ended, "slot {slot} ended before it began");
    journal[began..=ended].iter().map(|line| line.2).collect()
}

async fn plays_as_the_grader_reads(bot: &str, scenario: &str, trades: Trades) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let plugin = PathBuf::from(
        std::env::var("MTX_TEST_GEYSER_PLUGIN").expect("MTX_TEST_GEYSER_PLUGIN: the built plugin"),
    );
    let scenario = Scenario::load(&root.join(scenario)).unwrap();
    let snapshot = root.join("snapshots/phoenix-spcx-2026-10-29");
    let dir = std::env::temp_dir().join(format!(
        "mtx-geyser-parity-{}-{}",
        if trades == Trades::Takes {
            "takes"
        } else {
            "makes"
        },
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let watcher = Arc::new(Watcher {
        plugin,
        dir: dir.clone(),
        exchange: snapshot.join("exchange.json"),
        symbol: scenario.symbol.clone(),
        trades,
        steps: Mutex::new(Vec::new()),
        session: None,
    });
    grade::grade(GradeOptions {
        snapshot_dir: &snapshot,
        bot_dir: &root.join(bot),
        scenarios: std::slice::from_ref(&scenario),
        log_dir: &dir,
        jobs: 1,
        first_bot_user: 0,
        reference_dir: None,
        informed: None,
        anchor_jobs: 1,
        regimes: None,
        stop_on_disqualify: false,
        detail_dir: None,
        annotations: None,
        components_only: true,
        trusted: false,
        observer: Some(watcher.clone()),
        keys: &SessionKeys::random(),
        v2_sessions: false,
        host: None,
    })
    .await
    .unwrap();
    let steps = watcher.steps.lock().unwrap().clone();
    settled_journal(&watcher.journal()).await;
    let readings = journal(&watcher.journal(), trades);
    assert!(steps.len() > 100, "{} steps", steps.len());
    let mut traded = 0;
    for (slot, sent, graded) in &steps {
        let states = states_in(&readings, *slot);
        // The grader reads between the slot's moves and the strategy's own
        // orders, where no slot event falls: its read is one of the states
        // the slot passed through, and the last of them unless the strategy
        // traded, or rests quotes the takers fill after the read.
        assert!(
            states.contains(graded),
            "slot {slot}: the grader read {graded:?}, the plugin {states:?}"
        );
        if *sent > 0 {
            traded += 1;
        } else if trades == Trades::Takes {
            assert_eq!(states.last(), Some(graded), "slot {slot}");
        }
    }
    assert!(
        traded > 0,
        "the strategy never traded: its position was never read"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `MTX_TEST_GEYSER_PLUGIN` is the built plugin (`cargo build -p
/// mtx-geyser`), and the bots are built (`just build` in each).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built plugin and components"]
async fn the_plugin_reads_a_flash_crash_as_the_grader_does() {
    plays_as_the_grader_reads(
        "bench/tasks/flash-crash-spcx/reference",
        "bench/tasks/flash-crash-spcx/environment/practice/practice-1.json",
        Trades::Takes,
    )
    .await;
}

/// A market maker's fills come between its turns, from the takers: its
/// position is read from the active trader buffer, where the matching
/// engine counts a fill the moment it happens, and the book from the
/// splines as `view_bbo` computes it. The mark isn't compared: the takers'
/// trades don't crank the market, and between cranks the view blends the
/// book as it stands where the perp asset map holds the last crank's mark.
/// On a flash-crash every move cranks (`docs/rfd/0004`, what we learned).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built plugin and components"]
async fn the_plugin_reads_market_making_as_the_grader_does() {
    plays_as_the_grader_reads(
        "bench/tasks/market-making-cbrs/reference",
        "bench/tasks/market-making-cbrs/environment/practice/practice-1.json",
        Trades::Makes,
    )
    .await;
}

/// Plays a session's turns through the plugin's runtime: connects to the
/// socket it listens on and hands it the session's variables.
struct PluginHost {
    socket: PathBuf,
}

impl session::TurnHost for PluginHost {
    fn connect(
        &self,
        env: &mtx_kit::BotEnv,
        vars: &[(String, String)],
    ) -> std::io::Result<std::os::unix::net::UnixStream> {
        let mut stream = std::os::unix::net::UnixStream::connect(&self.socket)?;
        let all: Vec<(String, String)> = env
            .vars()
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .chain(vars.iter().cloned())
            .collect();
        mtx_core::turns::write_variables(&mut stream, &all)?;
        Ok(stream)
    }
}

/// Step 3 of `docs/rfd/0004`: the plugin's runtime hosts a reference in a
/// session the grader only scores. It reads the market from
/// its account cache, and sends through the bot's endpoint, as the grader
/// would for the component. With the same keys, so the same world and the
/// same seed, it makes what the grader makes hosting it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built plugin and components"]
async fn the_plugin_trades_a_flash_crash_as_the_grader_does() {
    trades_as_the_grader_does(
        "bench/tasks/flash-crash-spcx/reference",
        "bench/tasks/flash-crash-spcx/environment/practice/practice-1.json",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built plugin and components"]
async fn the_plugin_trades_market_making_as_the_grader_does() {
    trades_as_the_grader_does(
        "bench/tasks/market-making-cbrs/reference",
        "bench/tasks/market-making-cbrs/environment/practice/practice-1.json",
    )
    .await;
}

async fn trades_as_the_grader_does(bot: &str, scenario: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let plugin = PathBuf::from(
        std::env::var("MTX_TEST_GEYSER_PLUGIN").expect("MTX_TEST_GEYSER_PLUGIN: the built plugin"),
    );
    let bot = root.join(bot);
    let scenario = Scenario::load(&root.join(scenario)).unwrap();
    let snapshot = root.join("snapshots/phoenix-spcx-2026-10-29");
    let dir = std::env::temp_dir().join(format!(
        "mtx-geyser-trades-{}-{}",
        scenario.name,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let keys = SessionKeys::random();
    let grade = |observer: Option<Arc<dyn StepObserver>>,
                 host: Option<Arc<dyn session::TurnHost>>,
                 logs: PathBuf| {
        let (snapshot, bot, scenario, keys) = (&snapshot, &bot, &scenario, &keys);
        async move {
            grade::grade(GradeOptions {
                snapshot_dir: snapshot,
                bot_dir: bot,
                scenarios: std::slice::from_ref(scenario),
                log_dir: &logs,
                jobs: 1,
                first_bot_user: 0,
                reference_dir: None,
                informed: None,
                anchor_jobs: 1,
                regimes: None,
                stop_on_disqualify: false,
                detail_dir: None,
                annotations: None,
                components_only: true,
                trusted: true,
                observer,
                keys,
                v2_sessions: false,
                host,
            })
            .await
            .unwrap()
        }
    };
    let by_grader = grade(None, None, dir.join("grader")).await;
    let socket = dir.join("sessions.sock");
    let watcher = Arc::new(Watcher {
        plugin,
        dir: dir.clone(),
        exchange: snapshot.join("exchange.json"),
        symbol: scenario.symbol.clone(),
        trades: Trades::Takes,
        steps: Mutex::new(Vec::new()),
        session: Some(json!({
            "socket": socket,
            "component": mtx_grader::wasm::component_in(&bot).expect("the built reference"),
            "log": dir.join("strategy.log"),
        })),
    });
    let by_plugin = grade(
        Some(watcher),
        Some(Arc::new(PluginHost { socket })),
        dir.join("plugin"),
    )
    .await;
    let (graded, hosted) = (
        &by_grader.scenarios[0].session,
        &by_plugin.scenarios[0].session,
    );
    assert!(!hosted.turns.legacy, "the runtime answered the hello");
    assert_eq!(hosted.turns.overran, 0, "the runtime answered every turn");
    assert!(graded.peak_position > 0.0, "the reference traded");
    assert!(
        hosted.refused_calls.is_empty(),
        "{:?}",
        hosted.refused_calls
    );
    assert_eq!(
        (hosted.pnl_usd, hosted.peak_position, hosted.end_position),
        (graded.pnl_usd, graded.peak_position, graded.end_position),
        "the plugin's session against the grader's"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
