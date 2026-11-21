# Driving the live runtime from a sidecar

The live runtime (`docs/rfd/0004-strategies-on-mainnet.md`) runs inside a
validator, as a geyser plugin. Whatever decides *which* strategies it runs
lives beside it, in a process of its own: a person with `mtx live`, or a
sidecar such as the harness that runs it on its own (luthor, a separate,
private repository). This page is what a sidecar can rely on. Everything
here is on the node; a sidecar runs on the same machine.

| | What | Where |
| --- | --- | --- |
| **Commands** | add, pause, resume, mandate, remove, kill, status: one JSON line each way, one command a connection | the control socket, `control.socket`; `mtx_core::control` |
| **Events** | everything that happens, pushed as it happens: a hello, then slots, steps, paper fills and markouts, trades, pauses, commands | the event socket, `control.events`; `mtx_core::events` |
| **Journals** | each strategy's steps, shadow sends and paper fills, in full | `<control.logs>/<name>.{steps,shadow,fills}.jsonl`; `crates/runtime/src/live/journal.rs` |
| **Telemetry** | logs, traces and metrics | OTLP to `telemetry.otlp_endpoint`, a JSON log at `telemetry.log`; `ops/grafana` |

A sidecar depends on `mtx-core` (a git dependency on this repository),
which holds both protocols and a blocking client for each:
`mtx_core::control::request` and `mtx_core::events::subscribe`. It holds
no Solana code and no runtime.

## Turning it on

In the plugin's configuration, beside `control.socket`:

```json
"control": {
  "socket": "/home/sol/mtx/control.sock",
  "events": "/home/sol/mtx/events.sock",
  "state": "/home/sol/mtx/strategies.json",
  "rpc_url": "http://127.0.0.1:8899",
  "logs": "/home/sol/mtx/logs"
}
```

Without `events`, there is no event socket. Both sockets are files the
validator's user owns: a sidecar runs as that user, or in its group.

## The event stream

Connect to the event socket and read lines. Each is a `Line`: `at_ms` (the
runtime's clock) and an `event`.

1. **`hello`** first: the protocol (`mtx_core::events::PROTOCOL`), the feed
   and every strategy's status, as `mtx live status` has them. A sidecar
   starts from it and never needs to poll. It refuses a protocol it
   doesn't know.
2. Then, as they happen:

| Event | When | Carries |
| --- | --- | --- |
| `slot` | the runtime stepped on a bank slot (about 2.5 a second) | slot, slots behind the chain, feed events dropped |
| `step` | a strategy's step ended | strategy, slot, step number, time, orders sent, position, PnL, drawdown, still running |
| `paper_fill` | a shadow strategy's order filled on paper | model (`through`, `touch`), side, price, size, maker or taker, edge at the mid, paper position |
| `markout` | a paper fill, 3, 25 and 150 slots on | model, fill slot, horizon, bps (positive: the market went its way) |
| `trade` | a trade on a market a strategy runs on, as the validator ran it | symbol, slot, taker's side, price, size |
| `paused` | the enforcer paused a strategy | strategy, `mandate` or `feed`, why |
| `command` | a command that changes something was done or refused | the command, ok, error |
| `lagged` | this connection fell behind and lost events | how many: read `status` again |

**It never slows the runtime.** An event is serialized once, only when
someone listens, into a queue of 8192; each connection has its own writer
thread. A reader that falls behind loses the oldest events and gets a
`lagged` line saying how many; one that stops reading for two seconds is
disconnected. At most 16 connections at once.

**Numbers and enums.** The only text is a reason or an error the runtime
wrote itself. Nothing a stranger can put on the chain (a memo, a token's
name) reaches a sidecar through an event.

**Latency.** A `step` event leaves the runtime as the step ends, which is
within a slot of the slot it stepped on; a `trade` at the next slot after
the validator ran it. Both are a local socket write away from the
sidecar.

`mtx live --socket … watch --events /home/sol/mtx/events.sock` prints the
stream for a person (`--json` for the lines as sent).

## Acting

A sidecar acts through the control socket, exactly as `mtx live` does.
Every command it sends shows up on the event stream as a `command` event,
for itself and for anyone else watching, so a person acting by hand and a
sidecar acting on its own see each other.

- A command is applied at the strategy's next step boundary.
- `add` names a component and a keypair by path on the node: a sidecar
  puts both there first. The runtime refuses a component whose SHA-256
  changed since it was added.
- An answer can be lost (the sidecar dies mid-command). Read `status` and
  decide from what the runtime has; never resend blind. The desired state
  (`control.state`) is what the runtime restores after a restart.

## What a sidecar can't do

It has no hand on a strategy's key, its orders or the enforcer. A
strategy's mandate is audited inside the runtime, and a breach pauses it
whatever the sidecar wants. The worst a sidecar can do is add, run, pause
or remove strategies within what each mandate allows.
