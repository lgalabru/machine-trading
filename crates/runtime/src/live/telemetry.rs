//! The live runtime's telemetry, as pay's services emit theirs: `tracing`
//! spans exported as OTLP traces, `counter.*` / `histogram.*` / `gauge.*`
//! fields of `tracing` events exported as OTLP metrics (with the event's
//! other fields as their attributes, a strategy's name among them), and
//! every event as a line of JSON in a log file.
//!
//! OTLP goes over HTTP to a collector (an OpenTelemetry Collector or Grafana
//! Alloy beside the node, `ops/grafana`), which forwards traces to Tempo and
//! metrics to Prometheus or Mimir. Without an endpoint, the log file alone.
//!
//! The runtime lives in a geyser plugin, inside the validator's process:
//! the plugin's `tracing` is its own copy, so the subscriber set here sees
//! the runtime's events and not the validator's, which log through `log`.

use std::{path::PathBuf, time::Duration};

use opentelemetry::{KeyValue, global, trace::TracerProvider as _};
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
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

/// How often metrics are exported: a Grafana panel's usual resolution.
const METRICS_EVERY: Duration = Duration::from_secs(10);

/// The exporters, flushed and shut down when the runtime stops.
#[derive(Debug)]
pub struct Telemetry {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
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
        .with_writer(std::sync::Mutex::new(file))
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
        .with_resource(resource)
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
    global::set_tracer_provider(tracer.clone());
    global::set_meter_provider(meter.clone());
    if tracing_subscriber::registry()
        .with(filter)
        .with(json)
        .with(MetricsLayer::new(meter.clone()))
        .with(OpenTelemetryLayer::new(tracer.tracer("mtx-runtime")))
        .try_init()
        .is_err()
    {
        eprintln!("mtx runtime: a subscriber is already set; it is kept");
    }
    Ok(Telemetry {
        tracer: Some(tracer),
        meter: Some(meter),
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
