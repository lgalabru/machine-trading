//! The live runtime's telemetry, as pay's services emit theirs: `tracing`
//! spans exported as OTLP traces, `counter.*` / `histogram.*` / `gauge.*`
//! fields of `tracing` events exported as OTLP metrics (with the event's
//! other fields as their attributes, a strategy's name among them), and
//! every event as a line of JSON in a log file and as an OTLP log record.
//!
//! OTLP goes over HTTP to a collector (`ops/grafana`: Grafana's otel-lgtm
//! image beside the node), which keeps traces in Tempo, metrics in
//! Prometheus and logs in Loki. Without an endpoint, the log file alone.
//!
//! The runtime lives in a geyser plugin, inside the validator's process:
//! the plugin's `tracing` is its own copy, so the subscriber set here sees
//! the runtime's events and not the validator's, which log through `log`.

use std::{path::PathBuf, time::Duration};

use opentelemetry::{KeyValue, global, trace::TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    metrics::{MeterProviderBuilder, PeriodicReader, SdkMeterProvider},
    trace::{RandomIdGenerator, Sampler, SdkTracerProvider},
};
use opentelemetry_semantic_conventions::{SCHEMA_URL, attribute::SERVICE_VERSION};
use serde::Deserialize;
use tracing_opentelemetry::{MetricsLayer, OpenTelemetryLayer};
use tracing_subscriber::{EnvFilter, Layer as _, layer::SubscriberExt, util::SubscriberInitExt};

/// Where the runtime's telemetry goes: the `telemetry` member of its
/// configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// The collector's OTLP/HTTP base URL (`http://127.0.0.1:4318`): traces
    /// go to `/v1/traces`, metrics to `/v1/metrics`. Without it, nothing is
    /// exported.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// The JSON log file, appended to.
    pub log: PathBuf,
    /// What to record, as `RUST_LOG` says it (`info,mtx_runtime=debug`).
    #[serde(default = "default_filter")]
    pub filter: String,
    /// The `service.name` and `deployment.environment` resources every
    /// trace and metric carries.
    #[serde(default = "default_service")]
    pub service: String,
    #[serde(default = "default_environment")]
    pub environment: String,
}

fn default_filter() -> String {
    "info".into()
}

fn default_service() -> String {
    "mtx-runtime".into()
}

fn default_environment() -> String {
    "mainnet-beta".into()
}

/// The events exported as OTLP logs: the runtime's, not those of the
/// crates that carry them out.
const EXPORTED_LOGS: &str = "info,opentelemetry=off,opentelemetry_sdk=off,opentelemetry_otlp=off,reqwest=off,hyper=off,hyper_util=off,h2=off,rustls=off";

/// How often metrics are exported: a Grafana panel's usual resolution.
const METRICS_EVERY: Duration = Duration::from_secs(10);

/// The exporters, flushed and shut down when the runtime stops.
#[derive(Debug)]
pub struct Telemetry {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(tracer) = self.tracer.take()
            && let Err(error) = tracer.shutdown()
        {
            eprintln!("mtx runtime: OTLP trace shutdown failed: {error:?}");
        }
        if let Some(meter) = self.meter.take()
            && let Err(error) = meter.shutdown()
        {
            eprintln!("mtx runtime: OTLP metric shutdown failed: {error:?}");
        }
        if let Some(logger) = self.logger.take()
            && let Err(error) = logger.shutdown()
        {
            eprintln!("mtx runtime: OTLP log shutdown failed: {error:?}");
        }
    }
}

/// Start the runtime's telemetry. A process sets one global subscriber, so
/// a plugin loaded again in the same process keeps the first one's: that
/// is reported, not an error.
pub fn init(config: &TelemetryConfig) -> Result<Telemetry, String> {
    let filter = EnvFilter::try_new(&config.filter)
        .map_err(|e| format!("telemetry filter {:?}: {e}", config.filter))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.log)
        .map_err(|e| format!("{}: {e}", config.log.display()))?;
    let json = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_writer(Lines::spawn(file)?)
        .boxed();
    let Some(endpoint) = &config.otlp_endpoint else {
        if tracing_subscriber::registry()
            .with(filter)
            .with(json)
            .try_init()
            .is_err()
        {
            eprintln!("mtx runtime: a subscriber is already set; it is kept");
        }
        return Ok(Telemetry {
            tracer: None,
            meter: None,
            logger: None,
        });
    };
    let base = endpoint.trim().trim_end_matches('/');
    let resource = resource(config);
    let tracer = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource.clone())
        .with_batch_exporter(
            opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(format!("{base}/v1/traces"))
                .build()
                .map_err(|e| format!("the OTLP span exporter: {e}"))?,
        )
        .build();
    let meter = MeterProviderBuilder::default()
        .with_resource(resource.clone())
        .with_reader(
            PeriodicReader::builder(
                opentelemetry_otlp::MetricExporter::builder()
                    .with_http()
                    .with_protocol(Protocol::HttpBinary)
                    .with_endpoint(format!("{base}/v1/metrics"))
                    .build()
                    .map_err(|e| format!("the OTLP metric exporter: {e}"))?,
            )
            .with_interval(METRICS_EVERY)
            .build(),
        )
        .build();
    let logger = SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(
            opentelemetry_otlp::LogExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(format!("{base}/v1/logs"))
                .build()
                .map_err(|e| format!("the OTLP log exporter: {e}"))?,
        )
        .build();
    global::set_tracer_provider(tracer.clone());
    global::set_meter_provider(meter.clone());
    if tracing_subscriber::registry()
        .with(filter)
        .with(json)
        .with(MetricsLayer::new(meter.clone()))
        .with(OpenTelemetryLayer::new(tracer.tracer("mtx-runtime")))
        // The exporters' own HTTP client and OpenTelemetry log through
        // tracing too: exported as logs, each export would make more.
        .with(OpenTelemetryTracingBridge::new(&logger).with_filter(EnvFilter::new(EXPORTED_LOGS)))
        .try_init()
        .is_err()
    {
        eprintln!("mtx runtime: a subscriber is already set; it is kept");
    }
    Ok(Telemetry {
        tracer: Some(tracer),
        meter: Some(meter),
        logger: Some(logger),
    })
}

fn resource(config: &TelemetryConfig) -> Resource {
    Resource::builder()
        .with_service_name(config.service.clone())
        .with_schema_url(
            [
                KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
                KeyValue::new("deployment.environment", config.environment.clone()),
            ],
            SCHEMA_URL,
        )
        .build()
}

/// The JSON log's writer, off the threads that log: an event is formatted
/// where it happens and its line handed to a thread that writes and flushes
/// the file. A step logs several events; written in place, each was a
/// system call on the step's path. With [`LINES_QUEUED_MAX`] lines waiting,
/// a line is dropped and counted rather than wait.
struct Lines {
    sender: std::sync::mpsc::SyncSender<Vec<u8>>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Lines the log may fall behind by: seconds of the busiest logging.
const LINES_QUEUED_MAX: usize = 1 << 16;

/// How often the writer flushes when lines keep coming.
const FLUSH_EVERY: Duration = Duration::from_millis(100);

impl Lines {
    fn spawn(file: std::fs::File) -> Result<Self, String> {
        use std::io::Write;
        let (sender, lines) = std::sync::mpsc::sync_channel::<Vec<u8>>(LINES_QUEUED_MAX);
        std::thread::Builder::new()
            .name("mtx-log-writer".into())
            .spawn(move || {
                let mut out = std::io::BufWriter::with_capacity(1 << 16, file);
                let mut flushed = std::time::Instant::now();
                loop {
                    match lines.recv_timeout(FLUSH_EVERY) {
                        Ok(line) => {
                            // A log that can't be written is nothing to stop
                            // a strategy for; the next line tries again.
                            let _ = out.write_all(&line);
                            if flushed.elapsed() >= FLUSH_EVERY {
                                let _ = out.flush();
                                flushed = std::time::Instant::now();
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            let _ = out.flush();
                            flushed = std::time::Instant::now();
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            let _ = out.flush();
                            return;
                        }
                    }
                }
            })
            .map_err(|e| format!("the log's writer: {e}"))?;
        Ok(Self {
            sender,
            dropped: std::sync::Arc::default(),
        })
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Line<'a>;

    fn make_writer(&'a self) -> Line<'a> {
        Line {
            lines: self,
            buffer: Vec::with_capacity(512),
        }
    }
}

/// One event's line, handed over when the event is done with it.
struct Line<'a> {
    lines: &'a Lines,
    buffer: Vec<u8>,
}

impl std::io::Write for Line<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line<'_> {
    fn drop(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let line = std::mem::take(&mut self.buffer);
        if self.lines.sender.try_send(line).is_err() {
            let dropped = self
                .lines
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Once in a while, on stderr: the log itself is what is full.
            if dropped.is_power_of_two() {
                eprintln!(
                    "mtx runtime: the log fell behind; {} lines dropped",
                    dropped + 1
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configuration_needs_only_a_log_file() {
        let config: TelemetryConfig =
            serde_json::from_str(r#"{ "log": "/tmp/mtx-runtime.jsonl" }"#).unwrap();
        assert_eq!(config.otlp_endpoint, None);
        assert_eq!(config.filter, "info");
        assert_eq!(config.service, "mtx-runtime");
        assert_eq!(config.environment, "mainnet-beta");
        assert!(serde_json::from_str::<TelemetryConfig>(r#"{ "log": "x", "otlp": "y" }"#).is_err());
    }
}
