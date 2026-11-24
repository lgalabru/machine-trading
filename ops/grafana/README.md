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
| `mtx_paper_fills` | counter | strategy, model, side, liquidity | a shadow strategy's paper fills (`maker` or `taker`), under `through` or `touch` |
| `mtx_paper_markout_bps` | histogram | strategy, model, horizon, side, liquidity | a paper fill marked to the mid `1s`, `10s`, `60s` later; positive is good |
| `mtx_paper_fill_edge_bps` | histogram | strategy, model, side, liquidity | how far a paper fill was on the good side of the mid |
| `mtx_paper_position` | gauge | strategy | the paper fills' position, uncapped |
| `mtx_paper_pnl_usd` | gauge | strategy | the paper fills' cash plus position at the mid, before fees |
| `mtx_paper_at_touch` | gauge | strategy, model | 1 when a quote is at or inside the book's best |
| `mtx_tape_trades` | counter | strategy | Phoenix trades on the strategy's market, from the validator's transactions |
| `mtx_tape_missed` | counter | strategy | tape transactions the paper book fell too far behind to read |
| `mtx_tape_undecoded` | counter | strategy | tape transactions whose Phoenix records didn't decode |
| `mtx_read_lock_us` | histogram | symbol | a market read's wait for the account cache's lock |
| `mtx_read_decode_us` | histogram | symbol | a market read's decode, after the lock |
| `mtx_pool_read_us` | histogram | pool | a watched pool's read: assembled from the cache and quoted both ways |
| `mtx_pool_reads` | counter | pool | reads of watched pools: one a write that changed a pool, one a minute otherwise |
| `mtx_pool_desk_read_us` | histogram | | a pool strategy's pools read from the node, a step |
| `mtx_pool_desk_triggers` | counter | | steps a pool strategy was told a trigger |
| `mtx_lending_tell_us` | histogram | | a slot's look at the watched obligations and reserves, when it told any |
| `mtx_lending_events` | counter | | `obligation` and `reserve` events told |
| `mtx_pool_read_failures` | counter | pool | watched pools that couldn't be assembled, or whose accounts weren't found |
| `mtx_journal_dropped` | counter | strategy | step journal lines dropped because the journal's thread fell behind |
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

## Journals, to go back to

Beside each strategy's prints (`logs/<strategy>.log`), the runtime keeps
(`crates/runtime/src/live/journal.rs`):

- `logs/<strategy>.steps.jsonl`, one line per step: the market the
  strategy saw (mark, oracle, index, best bid and ask, position, resting,
  PnL), what it printed, its intents, what the planner cut, and each order
  with what became of it. Replay it through a changed strategy to see what
  the change would have done. Also in Loki as `a strategy decided`.
- `logs/<strategy>.fills.jsonl`, for a shadow strategy: its paper fills
  and their markouts, under two models. The plugin reads every
  transaction that runs Phoenix and keeps its trades (the tape,
  `crates/runtime/src/live/tape.rs`). Under `through`, a real trade past
  a quote's price fills it (price priority puts it first), as does the
  book's far side reaching it: a lower bound. Under `touch`, trades at the
  quote's own price fill it too, as if first in the queue: an upper bound.

Each file is moved aside to `.jsonl.1` past 512 MiB.

```sh
jq -s 'group_by(.horizon_slots) | map({h: .[0].horizon_slots, n: length, mean_bps: (map(.bps) | add / length)})' \
  <(grep '"markout"' ~/mtx/logs/mm-sol.fills.jsonl | grep '"through"')
```
