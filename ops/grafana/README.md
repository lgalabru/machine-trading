# Watching the mtx runtime in Grafana

The live runtime (the geyser plugin's, `docs/rfd/0004`) emits its
telemetry the way pay's services do (`crates/runtime/src/live/telemetry.rs`):

- **Traces:** each `tracing` span becomes an OTLP span, for Tempo.
- **Metrics:** each `counter.*`, `histogram.*` or `gauge.*` field of a
  `tracing` event becomes an OTLP metric, for Prometheus or Mimir. The
  event's other fields become the metric's labels: `strategy`, `outcome`,
  `route`, `reason`, `command`.
- **Logs:** every event becomes a line of JSON in a file on the node.

OTLP goes over HTTP to a collector beside the node (`collector.yaml`),
which forwards it to Grafana Cloud or to your own Tempo and Prometheus.
The resource attributes are `service.name=mtx-runtime`, `service.version`
and `deployment.environment`.

## Turn it on

In the plugin's configuration (`/home/sol/mtx/geyser.json`), next to
`control`:

```json
"telemetry": {
  "log": "/home/sol/mtx/runtime.jsonl",
  "otlp_endpoint": "http://127.0.0.1:4318",
  "filter": "info",
  "environment": "mainnet-beta"
}
```

Without `otlp_endpoint`, only the JSON log is written. The plugin reads
this at load, so a change needs a validator restart.

## The collector

`collector.yaml` is an OpenTelemetry Collector (contrib) config. It
listens on `127.0.0.1:4318` and forwards to Grafana Cloud's OTLP gateway,
using `GRAFANA_OTLP_ENDPOINT` and `GRAFANA_OTLP_AUTH` (base64 of
`<instance id>:<token>`). For a self-hosted stack, point the exporter at
Tempo's OTLP endpoint, and at Prometheus started with
`--web.enable-otlp-receiver`.

## The dashboard

Import `mtx-runtime-dashboard.json` (Dashboards → New → Import) and
choose the Prometheus data source. Its variables are `job`
(= `service.name`) and `strategy`. The panels:

| Row | Panels |
|---|---|
| Strategies | steps/s, step time p50/p99, sends by outcome, skipped steps, position, PnL, drawdown, running |
| Enforcer and control | pauses by reason, commands by outcome |
| Feed | slots behind the chain, dropped events, slot, strategies |
| Sends | shadow sends, TPU sends by route, TPU send-to-confirmed p50/p99 |

## Metrics

Names are as Prometheus stores them: OTLP counters gain `_total`, and
histograms become `_bucket`, `_sum` and `_count`.

| Metric | Kind | Labels | What |
|---|---|---|---|
| `mtx_strategy_steps_total` | counter | strategy, shadow, answered | steps taken |
| `mtx_strategy_step_ms` | histogram | strategy | from the slot reaching the runtime to the step's sheet sent |
| `mtx_strategy_steps_skipped_total` | counter | strategy | steps skipped because the previous one still ran |
| `mtx_strategy_sends_total` | counter | strategy, outcome | each order's fate: landed, failed, not_sent, delayed, expired |
| `mtx_strategy_position` | gauge | strategy | position, base units |
| `mtx_strategy_pnl_usd` | gauge | strategy | PnL, marked to market (an estimate) |
| `mtx_strategy_drawdown_usd` | gauge | strategy | drawdown from the PnL's peak |
| `mtx_strategy_running` | gauge | strategy | 1 stepping, 0 paused or stopped |
| `mtx_enforcer_pauses_total` | counter | strategy, reason | `mandate` (a breach) or `feed` (stale) |
| `mtx_control_commands_total` | counter | command, outcome | `mtx live` commands that change something |
| `mtx_feed_slot` | gauge | | the newest bank slot the runtime applied |
| `mtx_feed_slots_behind` | gauge | | slots between the chain's notifications and what the runtime applied |
| `mtx_feed_dropped` | gauge | | feed events dropped since load |
| `mtx_strategies` | gauge | | strategies running or held |
| `mtx_shadow_sends_total` | counter | strategy, simulated | transactions a shadow strategy wrote down |
| `mtx_tpu_sends_total` | counter | strategy, route | `tpu`, or `rpc` when no leader connection took it |
| `mtx_tpu_confirm_ms` | histogram | strategy | from a TPU send to its confirmation |

## Traces

| Span | Attributes | Under it |
|---|---|---|
| `strategy.step` | strategy, slot, k, shadow, answered, step_ms | `strategy.market` (reading the cache), `strategy.call` (the wasm `step`), `strategy.order` (each order: venue, action) |
| `control.command` | command | the command's work |

In Tempo, `{ resource.service.name = "mtx-runtime" && name = "strategy.step" && span.strategy = "mm-sol" }`
finds a strategy's steps. A slow one shows which child took the time.
