# Watching the mtx runtime in Grafana

The live runtime (the geyser plugin's, `docs/rfd/0004`) emits its
telemetry the way pay's services do (`crates/runtime/src/live/telemetry.rs`):

- **Traces:** each `tracing` span becomes an OTLP span, for Tempo.
- **Metrics:** each `counter.*`, `histogram.*` or `gauge.*` field of a
  `tracing` event becomes an OTLP metric, for Prometheus or Mimir. The
  event's other fields become the metric's labels: `strategy`, `outcome`,
  `route`, `reason`, `command`.
- **Logs:** every event becomes a line of JSON in a file on the node, and
  an OTLP log record, for Loki (the exporters' own events excepted).

OTLP goes over HTTP to Grafana's otel-lgtm image beside the node
(`docker-compose.yml`), which keeps traces in Tempo, metrics in
Prometheus and logs in Loki, on the node's disk. The resource attributes
are `service.name=mtx-runtime` (Prometheus's `service_name` label),
`service.version` and `deployment.environment`.

## Self-hosted, through an SSH tunnel

Once, as root on the node (it installs Docker and the `mtx-grafana` unit):

```sh
bash /home/sol/mtx/src/ops/grafana/setup-grafana.sh
```

Every port is bound to `127.0.0.1`. From your machine:

```sh
ssh -N -L 3000:127.0.0.1:3000 -i ~/.ssh/id_ed25519_solana ubuntu@213.239.140.203
open http://localhost:3000   # Dashboards → mtx → mtx runtime
```

Data lives in `/var/lib/mtx-grafana`. The dashboard is provisioned from
`mtx-runtime-dashboard.json`; edit it in git, not in the UI.

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

## Grafana Cloud instead

`collector.yaml` is an OpenTelemetry Collector (contrib) config that
listens on `127.0.0.1:4318` and forwards to Grafana Cloud's OTLP gateway,
using `GRAFANA_OTLP_ENDPOINT` and `GRAFANA_OTLP_AUTH` (base64 of
`<instance id>:<token>`). Run it instead of otel-lgtm, not beside it.

## The dashboard

otel-lgtm provisions `mtx-runtime-dashboard.json`; elsewhere, import it
(its data source uid is `prometheus`). Its variables are `service`
(= `service.name`) and `strategy`. The panels:

| Row | Panels |
|---|---|
| Strategies | steps/s, step time p50/p99, sends by outcome, skipped steps, position, PnL, drawdown, running |
| Enforcer and control | pauses by reason, commands by outcome |
| Feed | slots behind the chain, dropped events, slot, strategies |
| Sends | shadow sends, TPU sends by route, TPU send-to-confirmed p50/p99 |

## Metrics

Names are as otel-lgtm's Prometheus stores them: counters keep their name (no `_total`), and
histograms become `_bucket`, `_sum` and `_count`.

| Metric | Kind | Labels | What |
|---|---|---|---|
| `mtx_strategy_steps` | counter | strategy, shadow, answered | steps taken |
| `mtx_strategy_step_ms` | histogram | strategy | from the slot reaching the runtime to the step's sheet sent |
| `mtx_strategy_steps_skipped` | counter | strategy | steps skipped because the previous one still ran |
| `mtx_strategy_sends` | counter | strategy, outcome | each order's fate: landed, failed, not_sent, delayed, expired |
| `mtx_strategy_position` | gauge | strategy | position, base units |
| `mtx_strategy_pnl_usd` | gauge | strategy | PnL, marked to market (an estimate) |
| `mtx_strategy_drawdown_usd` | gauge | strategy | drawdown from the PnL's peak |
| `mtx_strategy_running` | gauge | strategy | 1 stepping, 0 paused or stopped |
| `mtx_enforcer_pauses` | counter | strategy, reason | `mandate` (a breach) or `feed` (stale) |
| `mtx_control_commands` | counter | command, outcome | `mtx live` commands that change something |
| `mtx_feed_slot` | gauge | | the newest bank slot the runtime applied |
| `mtx_feed_slots_behind` | gauge | | slots between the chain's notifications and what the runtime applied |
| `mtx_feed_dropped` | gauge | | feed events dropped since load |
| `mtx_strategies` | gauge | | strategies running or held |
| `mtx_shadow_sends` | counter | strategy, simulated | transactions a shadow strategy wrote down |
| `mtx_tpu_sends` | counter | strategy, route | `tpu`, or `rpc` when no leader connection took it |
| `mtx_tpu_confirm_ms` | histogram | strategy | from a TPU send to its confirmation |

## Traces

| Span | Attributes | Under it |
|---|---|---|
| `strategy.step` | strategy, slot, k, shadow, answered, step_ms | `strategy.market` (reading the cache), `strategy.call` (the wasm `step`), `strategy.order` (each order: venue, action) |
| `control.command` | command | the command's work |

In Tempo, `{ resource.service.name = "mtx-runtime" && name = "strategy.step" && span.strategy = "mm-sol" }`
finds a strategy's steps. A slow one shows which child took the time.

## Logs

In Loki, `{service_name="mtx-runtime"} | strategy = "mm-sol"` shows a
strategy's events; the JSON file on the node holds the same.
