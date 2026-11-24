# Driving the live runtime from a sidecar

The live runtime (`docs/rfd/0004-strategies-on-mainnet.md`) runs inside a
validator, as a geyser plugin. Whatever decides *which* strategies it runs
lives beside it, in a process of its own: a person with `mtx live`, or a
sidecar such as the harness that runs it on its own (luthor, a separate,
private repository). This page is what a sidecar can rely on. Everything
here is on the node; a sidecar runs on the same machine.

| | What | Where |
| --- | --- | --- |
| **Commands** | add, upgrade, pause, resume, mandate, remove, kill, status: one JSON line each way, as many commands as a connection likes, refusals with a code | the control socket, `control.socket`; `mtx_core::control` |
| **Events** | everything that happens, pushed as it happens: a hello, then slots, steps, paper fills and markouts, trades, pauses, commands | the event socket, `control.events`; `mtx_core::events` |
| **Journals** | each strategy's steps, shadow sends and paper fills, in full | `<control.logs>/<name>.{steps,shadow,fills}.jsonl`; `crates/runtime/src/live/journal.rs` |
| **Telemetry** | logs, traces and metrics | OTLP to `telemetry.otlp_endpoint`, a JSON log at `telemetry.log`; `ops/grafana` |

A sidecar depends on `mtx-core` (a git dependency on this repository),
which holds both protocols and a blocking client for each:
`mtx_core::control::Client` (one connection kept for every command;
`request` for a one-off) and `mtx_core::events::subscribe`. It holds no
Solana code and no runtime. `crates/core/tests/contract.rs` fixes every
line of both protocols as it goes over the wire.

Both sockets speak one protocol version, `mtx_core::events::PROTOCOL`
(2), told in the event stream's hello.

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

Without `events`, there is no event socket.

Beside them, what the runtime's speed depends on:

- `workers` (4): the threads strategies step on, at once across
  strategies; `cores` (none): cores to pin them to, away from the
  validator's busiest.
- `references` (none): exchanges' public prices, each shown to every
  strategy as a read-only venue in `market.venues` (`live::reference`):
  `[{"source": "coinbase", "pair": "SOL-USD"}]`. Sources: `coinbase`
  (Coinbase Exchange's public ticker) and `binance` (its `bookTicker`;
  its terms restrict some locations, the United States among them). Each
  quote received is journaled to `<logs>/reference-<source>-<pair>.jsonl`.
  No account, nothing sent but the subscription.
- `watch` (none): markets watched without trading them, for a sidecar
  that surveys every market (`live::watching`):
  `{"exchange": "/home/sol/mtx/exchange.json", "symbols": [], "every_slots": 25}`.
  No symbols watches every market that trades now. Each market's accounts
  join the cache; a few are read each slot after the steps, so each is
  read once in `every_slots` (at least 5) and told as a `quote`, with its
  trades as `trade` events. `mtx_kit::venue::PhoenixVenue::markets` lists
  them as `mtx_core::market::MarketInfo` (tick, lot, fees).
- `pools` (none): pools watched without trading them, for a sidecar that
  looks for arbitrage across them (`live::pools`):
  `{"pools": ["orca:amm:Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE"], "every_slots": 150, "rungs_bps": [2, 5, 10, 20, 40, 80, 160, 320, 640]}`.
  Orca Whirlpools and Raydium CLMM pools, 64 at most. A pool's account
  joins the cache with the accounts a quote needs (tick arrays, oracle,
  config, bitmap extension); the tick arrays a new price needs are read
  once from the node. After the steps, each pool a write changed since
  its last read, and each pool once in `every_slots` (at least 25), is
  quoted with the kit's exact quotes and told as a `pool`. A rung is the
  exact swap that moves the price by that many bps; a side's ladder ends
  where the pool can't fill a rung. The quotes use the host's clock.
- `lending` (none): lending obligations watched without touching them,
  for a sidecar that looks for liquidations (`live::lending`):
  `{"market": "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF", "obligations": "/home/sol/mtx/obligations.json", "obligations_max": 2048, "every_slots": 1500}`.
  The file names the obligations; `ops/agave/scan-obligations.py` writes
  it from one scan of the node (minutes on a node with no account index),
  the least healthy first. Each joins the cache with the reserves it
  names. After the steps, a reserve whose price changed is told as a
  `reserve`, and an obligation a write changed, or whose turn it is (all
  of them once in `every_slots`, at least 150), as an `obligation`.
  4,096 obligations and 128 reserves at most.
- `tpu.preflight` (false): a live strategy over the TPU sends without
  waiting for its order to land (its `fate` comes on the event stream),
  and without the node's simulation first unless this is on: a round trip
  on every order, against the fee of a transaction that fails on chain.
- The blockhash every transaction is signed on is the one the plugin last
  saw produced, and a strategy's component is compiled once and kept in
  wasmtime's code cache, so a restart or a new mandate doesn't compile it
  again. Both sockets are files the
validator's user owns: a sidecar runs as that user, or in its group.

## The event stream

Connect to the event socket and read lines. Each is a `Line`: `at_us`
(microseconds, the runtime's clock, as the line was handed to the socket:
on the same host, a sidecar reads its own delay off it) and an `event`.

1. **`hello`** first: the protocol (`mtx_core::events::PROTOCOL`), the feed
   and every strategy's status, as `mtx live status` has them. A sidecar
   starts from it and never needs to poll. It refuses a protocol it
   doesn't know.
2. Then, as they happen:

| Event | When | Carries |
| --- | --- | --- |
| `slot` | the runtime stepped on a bank slot (about 2.5 a second) | slot, slots behind the chain, feed events dropped |
| `step` | a strategy's step ended | strategy, slot, step number, orders sent, position, PnL, drawdown, still running, and its `timings` (below) |
| `step_skipped` | a step came while the strategy's last still ran | strategy, slot |
| `stopped` | a strategy can't step again: it trapped, or ran out of fuel or time | strategy, why |
| `fate` | what became of a transaction a live strategy sent without waiting (over the TPU) | strategy, signature, landed, slot, the chain's error, microseconds from the send |
| `paper_fill` | a shadow strategy's order filled on paper | model (`through`, `touch`), side, price, size, maker or taker, edge at the mid, paper position |
| `markout` | a paper fill, 3, 25 and 150 slots on | model, fill slot, horizon, bps (positive: the market went its way) |
| `trade` | a trade on a market a strategy runs on, as the validator ran it | symbol, slot, taker's side, price, size |
| `paused` | the enforcer paused a strategy | strategy, `mandate` or `feed`, why |
| `command` | a command that changes something was done or refused | the command (an enum), and the refusal (`code`, `error`) if it was |
| `lagged` | this connection fell behind and lost events | how many: read `status` again |
| `quote` | a watched market was read (`control.watch`) | symbol, slot, best bid and ask (null on an empty side), mark |
| `obligation` | a watched obligation was written, or it is its turn (`control.lending`) | venue, address, market, slot, the slot of the write read, the slot the program last refreshed it in, its debt at the borrow factors and its liquidation limit in dollars, and its deposits and borrows (mint, whole tokens, value at that refresh) |
| `reserve` | a watched obligation's reserve changed price | venue, address, market, mint, slot, the slot of the write read, dollars a whole token, the collateral's least liquidation bonus in bps |
| `pool` | a watched pool was read (`control.pools`): a write changed it, or it is due again | venue, address, slot, the slot of the write read, both mints, mid, fee in bps, liquidity, a ladder of exact swaps each way (bps moved, whole tokens in and out), microseconds the read took |

**It never slows the runtime.** An event is serialized once, only when
someone listens, into a queue of 8192; each connection has its own writer
thread. A reader that falls behind loses the oldest events and gets a
`lagged` line saying how many; one that stops reading for two seconds is
disconnected. At most 16 connections at once.

**Numbers and enums.** The only text is a reason or an error the runtime
wrote itself. Nothing a stranger can put on the chain (a memo, a token's
name) reaches a sidecar through an event.

**A step's timings**, each stage in microseconds on the runtime's
monotonic clock, from the validator's notice that the node has the slot's
bank to the step's last send:

| Stage | From, to |
| --- | --- |
| `feed_us` | the plugin's callback to the runtime's feed applying the slot |
| `wake_us` | the feed to the strategies' loop starting the step |
| `settle_us` | waiting for the feed to apply the writes before the slot |
| `read_us` | reading the strategy's market from the account cache |
| `call_us` | the strategy's `step`, in its wasm sandbox |
| `send_us` | planning, checking against the mandate, signing and sending its orders |
| `total_us` | the plugin's callback to the last send |

Measured on mainnet (mtx-alpha, Phoenix SOL, two shadow strategies), as
`mtx live latency` prints it:

| | p50 | p90 |
| --- | --- | --- |
| total, the slot's bank to the step's last send | 526 µs | 723 µs |
| feed | 5 µs | 7 µs |
| read (the cache's lock: 5 µs) | 292 µs | 340 µs |
| call | 81 µs | 96 µs |
| send (no orders / orders) | 6 µs | 198 µs |
| an event, runtime to sidecar | 23 µs | 61 µs |
| a command round trip, kept connection | 20 µs | 23 µs |
| a command round trip, one connection each | 145 µs | 201 µs |
| a component's load, from the code cache | 1–2 ms | |

`mtx live --socket … watch --events /home/sol/mtx/events.sock` prints the
stream for a person (`--json` for the lines as sent), and `mtx live
--socket … latency --events … --seconds 60` measures it, stage by stage,
with command round trips.

## Acting

A sidecar acts through the control socket, exactly as `mtx live` does,
and keeps its connection: commands on it are answered in order, in tens
of microseconds. Every command it sends shows up on the event stream as a
`command` event, for itself and for anyone else watching, so a person
acting by hand and a sidecar acting on its own see each other.

A refusal is `{"reply":"error","code":…,"error":…}`: a program branches on
the code, a person reads the error. A code a newer runtime adds reads as
`failed`.

| Code | Why |
| --- | --- |
| `not_a_command` | the line isn't a command this runtime reads |
| `unknown_strategy` | no strategy has that name |
| `exists` | a strategy already has that name |
| `too_many` | the runtime runs as many strategies as it may (32) |
| `invalid` | a mandate that doesn't validate or doesn't allow the market, a step of no slots |
| `unreadable` | a file it names (component, keypair, exchange) can't be read |
| `start_failed` | its accounts couldn't be read from the node, or its component didn't start |
| `orders_not_cancelled` | some of its resting orders couldn't be cancelled |
| `not_persisted` | done, but the desired state couldn't be written: a restart won't remember it |
| `stopped` | the runtime's strategies stopped (the plugin is unloading) |
| `failed` | anything else; the error says what |

- A command is applied at the strategy's next step boundary.
- `add` names a component and a keypair by path on the node: a sidecar
  puts both there first. The runtime refuses a component whose SHA-256
  changed since it was added.
- `add` with `pools` hosts a strategy on pools rather than on a perp: the
  bench's atomic arbitrage bots (`mtx:strategy@0.3.0`), in a shadow run
  only (`live::pooldesk`):
  `"pools": {"venues": ["orca:amm:<address>", "raydium:amm:<address>"], "paper_base": 100.0, "trigger_bps": 10.0}`,
  with `symbol` what it counts in (`SOL`) and no `exchange`. Two to twelve
  pools, one of them SOL against USDC. Each step it is shown a wallet of
  `paper_base` on paper (a shadow key holds nothing, and a strategy sizes
  its routes on what it holds) and each pool's prices, read from the node;
  a pool that moved `trigger_bps` since its last step is the step's
  trigger, with its side and no size. Its atomic groups are built, signed
  and written to `<name>.shadow.jsonl`, and sent nowhere. Without `shadow`
  it is refused (`invalid`): nothing here sends a group to a block builder
  yet.
- `upgrade` swaps a strategy's component for a new build, with no restart
  of anything else: it ends, and starts again on the new component under
  the same name, key and mandate, paused or running as it was. If the new
  one doesn't start, the old one starts again and the refusal is
  `start_failed`.
- The runtime itself is swapped without restarting the validator:
  `ops/agave/reload-plugin.sh` builds the plugin and has the validator
  reload it. Every socket connection is closed on the way: a sidecar
  connects again and reads `hello`.
- An answer can be lost (the sidecar dies mid-command). Read `status` and
  decide from what the runtime has; never resend blind. The desired state
  (`control.state`) is what the runtime restores after a restart.

## What a sidecar can't do

It has no hand on a strategy's key, its orders or the enforcer. A
strategy's mandate is audited inside the runtime, and a breach pauses it
whatever the sidecar wants. The worst a sidecar can do is add, run, pause
or remove strategies within what each mandate allows.
