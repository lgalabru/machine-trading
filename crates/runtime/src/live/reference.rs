//! Exchange prices for the markets the runtime trades: what a maker on
//! Phoenix is missing to see a move before the arbitrageurs who trade on
//! it. SOL's price is made on the exchanges; Phoenix follows about half a
//! second later (measured on mtx-alpha: Binance's 1-second returns
//! correlate best with Phoenix's half a second later), and the book
//! maker's fills that came right after Binance moved against them marked
//! out at -1.4 bp, against -0.6 bp for the rest.
//!
//! Each [`ReferenceConfig`] is one exchange's public stream of a pair's
//! best bid and ask, read on a thread of its own. Strategies see it as a
//! read-only venue in `market.venues` ([`References::views`]), named
//! `<source>:spot:<pair>` (`binance:spot:SOLUSDT`), with its age in the
//! view's stats; a quote older than [`STALE_MS`] isn't shown at all. Nothing
//! is ever sent to the exchange: there is no account, only public data.
//!
//! Every quote received is also journaled, one line each, to
//! `<control.logs>/reference-<source>-<pair>.jsonl` (rotated as the other
//! journals): what a study of the lead reads.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::Deserialize;

use super::stopping::Connections;

/// A quote older than this isn't shown: the stream is down or behind, and
/// a strategy acting on it would act on the past.
pub const STALE_MS: u64 = 2_000;

/// How long a reader waits for a message before it checks whether the
/// runtime is stopping, and the longest it waits before connecting again.
const POLL_EVERY: Duration = Duration::from_millis(250);
const RECONNECT_AFTER_MAX: Duration = Duration::from_secs(30);

/// References at most: a few exchanges for a few markets.
pub const REFERENCES_MAX: usize = 16;

/// An exchange the runtime reads prices from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Binance's spot `bookTicker` stream (not from every location: its
    /// terms restrict some, the United States among them).
    Binance,
    /// Coinbase Exchange's public `ticker` channel (`SOL-USD`).
    Coinbase,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Binance => "binance",
            Source::Coinbase => "coinbase",
        }
    }

    /// Where the pair's best bid and ask stream.
    fn url(self, pair: &str) -> String {
        match self {
            Source::Binance => format!(
                "wss://stream.binance.com:9443/ws/{}@bookTicker",
                pair.to_ascii_lowercase()
            ),
            Source::Coinbase => "wss://ws-feed.exchange.coinbase.com".into(),
        }
    }

    /// What to send once connected, if the stream needs a subscription.
    fn subscription(self, pair: &str) -> Option<String> {
        match self {
            Source::Binance => None,
            Source::Coinbase => Some(
                serde_json::json!({
                    "type": "subscribe",
                    "product_ids": [pair.to_ascii_uppercase()],
                    "channels": ["ticker", "heartbeat"],
                })
                .to_string(),
            ),
        }
    }
}

/// One exchange's pair, read as a reference.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceConfig {
    pub source: Source,
    /// The pair as the exchange names it (`SOLUSDT`).
    pub pair: String,
}

impl ReferenceConfig {
    /// The venue a strategy finds it as.
    pub fn id(&self) -> String {
        format!(
            "{}:spot:{}",
            self.source.name(),
            self.pair.to_ascii_uppercase()
        )
    }
}

/// A best bid and ask, and when the runtime received them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quote {
    pub bid: f64,
    pub ask: f64,
    pub received_unix_ms: u64,
}

/// The latest quote of each reference.
#[derive(Debug, Default)]
pub struct Latest {
    quotes: Mutex<BTreeMap<String, Quote>>,
}

impl Latest {
    pub fn set(&self, id: &str, quote: Quote) {
        assert!(
            quote.bid > 0.0 && quote.ask >= quote.bid,
            "a quote isn't crossed: {quote:?}"
        );
        if let Ok(mut quotes) = self.quotes.lock() {
            quotes.insert(id.to_string(), quote);
        }
    }

    /// The stream is alive at `now_unix_ms` without a new quote (a
    /// heartbeat): the last quote still stands, and ages from now.
    pub fn touch(&self, id: &str, now_unix_ms: u64) {
        if let Ok(mut quotes) = self.quotes.lock()
            && let Some(quote) = quotes.get_mut(id)
        {
            quote.received_unix_ms = quote.received_unix_ms.max(now_unix_ms);
        }
    }

    /// The views at `now_unix_ms`: each fresh quote as a venue.
    pub fn views_at(&self, now_unix_ms: u64) -> Vec<mtx_kit::VenueView> {
        let Ok(quotes) = self.quotes.lock() else {
            return Vec::new();
        };
        quotes
            .iter()
            .filter_map(|(id, q)| {
                let age_ms = now_unix_ms.saturating_sub(q.received_unix_ms);
                (age_ms <= STALE_MS).then(|| mtx_kit::VenueView {
                    id: id.clone(),
                    best_bid: Some(q.bid),
                    best_ask: Some(q.ask),
                    stats: vec![("age_ms".into(), age_ms as f64)],
                    ..Default::default()
                })
            })
            .collect()
    }
}

/// The references a strategy reads, as the desk asks for them.
pub struct References(pub Arc<Latest>);

impl crate::desk::References for References {
    fn views(&self) -> Vec<mtx_kit::VenueView> {
        self.0.views_at(now_unix_ms())
    }
}

/// Start a reader for each reference on `connections`' threads: they stop
/// when the runtime's sockets do. Each journals its quotes in `logs`.
pub fn spawn(
    configs: &[ReferenceConfig],
    latest: Arc<Latest>,
    logs: &std::path::Path,
    connections: &Arc<Connections>,
) -> Result<(), String> {
    if configs.len() > REFERENCES_MAX {
        return Err(format!(
            "{} references, at most {REFERENCES_MAX}",
            configs.len()
        ));
    }
    for config in configs {
        let name = format!(
            "reference-{}-{}.jsonl",
            config.source.name(),
            config.pair.to_ascii_uppercase()
        );
        let mut journal = super::journal::Appender::open(logs.join(name))?;
        let (config, latest, stopping) = (config.clone(), latest.clone(), connections.clone());
        connections
            .spawn("mtx-live-reference", move || {
                read(&config, &latest, &mut journal, &stopping)
            })
            .map_err(|e| format!("a reference's thread: {e}"))?;
    }
    Ok(())
}

/// Read one stream until the runtime stops, connecting again (backing off
/// to [`RECONNECT_AFTER_MAX`]) whenever it drops.
fn read(
    config: &ReferenceConfig,
    latest: &Latest,
    journal: &mut super::journal::Appender,
    connections: &Connections,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, reference = config.id(), "no runtime for a reference");
            return;
        }
    };
    let id = config.id();
    let mut wait = Duration::from_secs(1);
    while !connections.stopped() {
        let read = runtime.block_on(stream(config, &id, latest, journal, connections));
        if connections.stopped() {
            return;
        }
        tracing::warn!(reference = id, error = %read.unwrap_err_or_closed(), "a reference's stream dropped");
        let mut slept = Duration::ZERO;
        while slept < wait && !connections.stopped() {
            std::thread::sleep(POLL_EVERY);
            slept += POLL_EVERY;
        }
        wait = (wait * 2).min(RECONNECT_AFTER_MAX);
    }
}

/// One connection: every message into `latest` and the journal, until it
/// drops or the runtime stops (Ok).
async fn stream(
    config: &ReferenceConfig,
    id: &str,
    latest: &Latest,
    journal: &mut super::journal::Appender,
    connections: &Connections,
) -> Result<(), String> {
    use futures_util::StreamExt;
    let (mut socket, _) = tokio_tungstenite::connect_async_tls_with_config(
        config.source.url(&config.pair),
        None,
        false,
        Some(tokio_tungstenite::Connector::Rustls(tls()?)),
    )
    .await
    .map_err(|e| e.to_string())?;
    if let Some(subscription) = config.source.subscription(&config.pair) {
        use futures_util::SinkExt;
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                subscription.into(),
            ))
            .await
            .map_err(|e| e.to_string())?;
    }
    tracing::info!(reference = id, "a reference's stream is open");
    while !connections.stopped() {
        let message = match tokio::time::timeout(POLL_EVERY, socket.next()).await {
            Err(_) => continue,
            Ok(None) => return Err("closed".into()),
            Ok(Some(Err(error))) => return Err(error.to_string()),
            Ok(Some(Ok(message))) => message,
        };
        let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
            continue;
        };
        if is_heartbeat(config.source, &text) {
            latest.touch(id, now_unix_ms());
        } else if let Some((bid, ask)) = parse(config.source, &text) {
            let received_unix_ms = now_unix_ms();
            latest.set(
                id,
                Quote {
                    bid,
                    ask,
                    received_unix_ms,
                },
            );
            journal
                .write(&serde_json::json!({ "unix_ms": received_unix_ms, "bid": bid, "ask": ask }));
        }
    }
    Ok(())
}

/// TLS for a stream: ring as the provider, chosen here rather than for the
/// whole process (the validator's), and the Mozilla roots.
fn tls() -> Result<Arc<rustls::ClientConfig>, String> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// A best bid and ask from a stream's message; `None` for anything else
/// (a ping, an error, a crossed or empty book).
pub fn parse(source: Source, text: &str) -> Option<(f64, f64)> {
    match source {
        Source::Binance => {
            #[derive(Deserialize)]
            struct BookTicker {
                b: String,
                a: String,
            }
            let ticker: BookTicker = serde_json::from_str(text).ok()?;
            quote(&ticker.b, &ticker.a)
        }
        Source::Coinbase => {
            #[derive(Deserialize)]
            struct Ticker {
                r#type: String,
                best_bid: String,
                best_ask: String,
            }
            let ticker: Ticker = serde_json::from_str(text).ok()?;
            (ticker.r#type == "ticker").then_some(())?;
            quote(&ticker.best_bid, &ticker.best_ask)
        }
    }
}

/// Whether a message says only that the stream is alive: Coinbase's
/// ticker sends nothing between trades, its heartbeat every second.
pub fn is_heartbeat(source: Source, text: &str) -> bool {
    match source {
        Source::Binance => false,
        Source::Coinbase => {
            #[derive(Deserialize)]
            struct Typed {
                r#type: String,
            }
            serde_json::from_str::<Typed>(text).is_ok_and(|t| t.r#type == "heartbeat")
        }
    }
}

fn quote(bid: &str, ask: &str) -> Option<(f64, f64)> {
    let (bid, ask) = (bid.parse::<f64>().ok()?, ask.parse::<f64>().ok()?);
    (bid.is_finite() && bid > 0.0 && ask >= bid).then_some((bid, ask))
}

trait ClosedOr {
    fn unwrap_err_or_closed(self) -> String;
}

impl ClosedOr for Result<(), String> {
    fn unwrap_err_or_closed(self) -> String {
        self.err().unwrap_or_else(|| "closed".into())
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn a_binance_book_ticker_reads_and_anything_else_doesnt() {
        let line = r#"{"u":400900217,"s":"SOLUSDT","b":"69.18000000","B":"147.7","a":"69.19000000","A":"776.7"}"#;
        assert_eq!(parse(Source::Binance, line), Some((69.18, 69.19)));
        assert_eq!(parse(Source::Binance, r#"{"result":null,"id":1}"#), None);
        assert_eq!(parse(Source::Binance, r#"{"b":"70.0","a":"69.0"}"#), None);
        assert_eq!(parse(Source::Binance, r#"{"b":"0","a":"69.0"}"#), None);
        assert_eq!(parse(Source::Binance, "not json"), None);
        let coinbase = r#"{"type":"ticker","sequence":1,"product_id":"SOL-USD","price":"69.05","best_bid":"69.04","best_bid_size":"10","best_ask":"69.06","best_ask_size":"3"}"#;
        assert_eq!(parse(Source::Coinbase, coinbase), Some((69.04, 69.06)));
        assert_eq!(
            parse(
                Source::Coinbase,
                r#"{"type":"subscriptions","channels":[]}"#
            ),
            None
        );
    }

    #[test]
    fn the_venue_is_named_as_strategies_find_it() {
        let config = ReferenceConfig {
            source: Source::Binance,
            pair: "solusdt".into(),
        };
        assert_eq!(config.id(), "binance:spot:SOLUSDT");
        assert_eq!(
            Source::Binance.url("SOLUSDT"),
            "wss://stream.binance.com:9443/ws/solusdt@bookTicker"
        );
    }

    /// Against the real stream: a quote arrives within seconds, and the
    /// reader stops when the sockets do. Needs the network.
    #[test]
    #[ignore = "reads Coinbase's public stream"]
    fn a_real_stream_gives_a_quote_and_stops() {
        let latest = Arc::new(Latest::default());
        let connections = Arc::new(Connections::default());
        let config = ReferenceConfig {
            source: Source::Coinbase,
            pair: "SOL-USD".into(),
        };
        let logs = std::env::temp_dir().join(format!("mtx-reference-{}", std::process::id()));
        std::fs::create_dir_all(&logs).unwrap();
        spawn(
            std::slice::from_ref(&config),
            latest.clone(),
            &logs,
            &connections,
        )
        .unwrap();
        let mut seen = Vec::new();
        for _ in 0..100 {
            seen = latest.views_at(now_unix_ms());
            if !seen.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(seen.len(), 1, "no quote within 10 s");
        assert!(seen[0].best_bid.unwrap() > 1.0);
        assert_eq!(connections.stop(), 0);
        let journal =
            std::fs::read_to_string(logs.join("reference-coinbase-SOL-USD.jsonl")).unwrap();
        assert!(journal.lines().count() >= 1);
    }

    /// A heartbeat keeps the last quote showing past its own age, and
    /// does nothing before there is a quote.
    #[test]
    fn a_heartbeat_keeps_the_quote_fresh() {
        let latest = Latest::default();
        latest.touch("coinbase:spot:SOL-USD", 10_000);
        assert!(latest.views_at(10_000).is_empty());
        latest.set(
            "coinbase:spot:SOL-USD",
            Quote {
                bid: 69.0,
                ask: 69.01,
                received_unix_ms: 1_000,
            },
        );
        assert!(latest.views_at(1_000 + STALE_MS + 1).is_empty());
        latest.touch("coinbase:spot:SOL-USD", 5_000);
        assert_eq!(latest.views_at(5_000 + STALE_MS).len(), 1);
        assert!(is_heartbeat(
            Source::Coinbase,
            r#"{"type":"heartbeat","sequence":90,"last_trade_id":20,"product_id":"SOL-USD","time":"2026-11-22T05:45:11.295Z"}"#
        ));
        assert!(!is_heartbeat(
            Source::Coinbase,
            r#"{"type":"ticker","best_bid":"69","best_ask":"69.01"}"#
        ));
    }

    proptest! {
        /// A quote is shown exactly while it is fresh, with its age, and
        /// only its own prices.
        #[test]
        fn a_quote_shows_while_fresh(bid in 1.0f64..500.0, width in 0.0f64..1.0, received in 0u64..1_000_000, later in 0u64..5_000) {
            let latest = Latest::default();
            latest.set("binance:spot:SOLUSDT", Quote { bid, ask: bid + width, received_unix_ms: received });
            let views = latest.views_at(received + later);
            prop_assert_eq!(views.len(), usize::from(later <= STALE_MS));
            if let Some(v) = views.first() {
                prop_assert_eq!(v.best_bid, Some(bid));
                prop_assert_eq!(v.best_ask, Some(bid + width));
                prop_assert_eq!(v.stats.clone(), vec![("age_ms".to_string(), later as f64)]);
                prop_assert!(v.position.is_none() && v.orders.is_empty());
            }
        }
    }
}
