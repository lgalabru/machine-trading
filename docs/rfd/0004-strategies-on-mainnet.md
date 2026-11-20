# RFD 0004: Strategies on mainnet, run by a geyser plugin

State: discussion
Authors: fable, for Ludo Galabru  ·  Started: 2026-11-20

## Problem

A strategy that grades well has nowhere to go. It is a wasm component
(`crates/kit/wit/strategy.wit`, [RFD 0002](0002-wasm-strategies.md)) that
only the grader can call, against a surfnet the grader pauses. To trade it
for real, someone has to rewrite it as a process with its own reads, sends
and risk checks, and what runs is then no longer what was graded.

The question: can the component that was graded run unchanged against
mainnet, under the same mandate, with a host that reads the chain from a
validator's geyser stream? One host should run several strategies, and take
changes while it runs: pause one or all, change a strategy's mandate, add a
strategy.

Short answer: yes. The component boundary already separates what a strategy
decides from how the chain is read and written. What is missing is a host
that isn't a session, and a crate layout that lets two hosts share the
code. Nothing in the WIT world has to change for a first version.

## What we have

- **The strategy is a function.** `start(session)`, `step(market) ->
  intents`, `end`. It imports `log`, a read-only `rpc`, and a host-signed
  `send`. It holds no key and opens no socket.
- **The host is already two layers.** In `crates/grader/src/wasm.rs`,
  `WasmStrategy` is wasmtime, fuel, memory limits and the WIT conversions.
  `WasmBot` is the loop: read a `Market` through a `Reader`, call `step`,
  plan the sheet (`mtx_kit::plan_sheet_under`), check it
  (`taking::TakingGuard`), send each order through a `venue::Seat`. Neither
  layer names surfpool. `wasm.rs`, `venue.rs`, `venue/*`, `taking.rs`,
  `risk.rs` and `orders.rs` have no surfpool import.
- **The mandate and its audit are in `mtx-core`** (`Mandate::audit`,
  `check_sample`, `Observed`). The enforcer is whoever feeds them.
- **The chain is behind traits in `mtx-kit`**: `ChainClient` for reads,
  `TransactionSigner` for keys.
- **Send status already has the mainnet states**: `landed`, `failed`,
  `not-sent`, `delayed`, `expired`, with slot, fee and tip. Transaction
  options (priority fee, tip, route, retries) are in 0.2.0.

What is the grader's alone: scenarios and generators, actors, the snapshot
and its recipe, the RPC guard, keys derived from the grade's secret, the
simulated transaction layer, faults, scoring.

## What a geyser plugin is

Read from `~/Coding/surfpool` (`examples/hello-geyser`,
`crates/core/src/runloops/mod.rs`, and `crates/subgraph` at `v1.0.0`, the
embedded subgraph plugin, gone from `main`).

- A `cdylib` that exports `_create_plugin() -> *mut dyn GeyserPlugin`
  (`agave-geyser-plugin-interface`, 4.3.2 in surfpool). The host reads a
  JSON config with a `libpath`, opens the library, and calls
  `on_load(config_file, is_reload)`.
- The host then calls, on its own threads: `update_account` (pubkey,
  owner, lamports, data, and the transaction that wrote it),
  `notify_transaction` (message, status meta with logs and inner
  instructions), `update_slot_status` (processed, confirmed, rooted),
  `notify_block_metadata` (blockhash, height, time) and `notify_entry`.
- **It only reads.** There is no call back into the validator. A plugin
  that wants to write sends a transaction like anyone else.
- Account notifications are all or nothing: the plugin sees every account
  write on the chain and filters them itself.
- Plugins load, unload and reload at run time: surfpool's `loadPlugin`,
  `unloadPlugin`, `reloadPlugin` and `listPlugins` RPC, and Agave's admin
  RPC has the same four. A reload is an unload and a fresh load, so the
  plugin's memory is lost.
- The subgraph plugin shows the usual shape: do almost nothing in the
  callback, filter by owner, copy the bytes and push them to a channel
  (`ipc-channel` there), with the work done elsewhere. It also shows the
  backfill problem: an account whose meaning arrives later sits in a
  "purgatory" until a transaction names it.

Surfpool loads the same `cdylib` a validator does (`surfpool start -g
plugin.json`). That gives us a test bed: the plugin can run against a
surfnet, including a bench session, before it ever sees mainnet.

## Sketch

### The pieces

Everything runs in the validator's process (decided 2026-11-20: speed
first). Nothing on the path from an account write to a transaction on the
wire leaves the process or makes an RPC call.

```
validator (or surfpool, in tests)
        │  account, transaction, slot, block     callbacks, validator threads
        ▼
  mtx-geyser (cdylib)      filter, copy, try_send. Nothing else.
        │  bounded channel
        ▼
  mtx-runtime, `live` (its own threads, same process)
    feed       account cache, tape, own fills, blockhash, slot clock
    desks      one per strategy: Reader + Seats + signer + mandate
    strategies one wasmtime Store per strategy
    enforcer   pre-trade checks, post-trade audit, pause on breach
    tpu        leader tracking, warm QUIC connections to the next leaders
    control    local socket: pause, resume, mandate, add, remove, status
    journal    every step, sheet, send and fill, appended to disk
        │  signed transactions, QUIC
        ▼
  the leaders' TPU
```

The runtime still takes its events from a `Feed` trait, because tests feed
it recorded events and surfpool feeds it a bench session. A remote stream
is not a target.

### The step

The bench pauses the world for a step. Mainnet doesn't wait.

- **Clock.** A strategy is stepped every `slots_per_step` slots, on the
  slot notification. If its previous step is still running, the step is
  skipped and counted, never queued.
- **Market.** Built from the feed's cache wherever the venue's accounts
  say enough: the oracle, the tape and own fills from
  `notify_transaction` (the kit's `protocols/fills.rs` and Phoenix meter
  already decode the events), fees from the blocks seen, and the spot
  venues' books, which the kit decodes. A Phoenix perp's mark, top of
  book and a maker's position are not in what its public decoders give
  (step 2, in what we learned): until they are, those three come from
  Phoenix's Hawkeye views, simulated on the node's local RPC.
- **`rpc` import.** The same allowlist (`mtx_kit::RPC_METHODS`). Account
  reads are answered from the cache, in memory. A read the cache can't
  answer (`getBlock`, `simulateTransaction`, an account nobody watches)
  goes to the node's local RPC: it works, it is slow, and it is counted
  per step so a strategy's owner can see it. An account read once this way
  joins the watch set. Reads are no longer of a paused chain, so two reads
  in one step may see different slots.
- **Budget.** Fuel stays, per step, as in the bench. A wall deadline is
  added with wasmtime's epoch interruption, since here a slow step costs
  the strategy the market, not the grader its time.
- **Sends.** The planner and the seats are the bench's. The blockhash
  comes from `notify_block_metadata`, the bytes go to the leaders over
  QUIC (see [Sending](#sending-straight-to-the-leaders)). A send's fate is
  read from the stream: our signature in `notify_transaction` is `landed`
  or `failed`, a blockhash too old is `expired`. No polling.
- **Where the time goes.** The component is compiled when it is added,
  not when it is stepped. Each strategy steps on its own thread, so one
  slow strategy doesn't delay another. Signing is in process. The budget
  we should hold ourselves to, from the slot notification to the last
  byte on the wire, is a few milliseconds plus the strategy's own time,
  and step 6 measures it.
- **Session.** `start` gets the mandate, the venues and the authority as
  today. `steps` and `end_slot` come from the mandate's term. A mandate
  without a term needs a decision (see open questions).

### Many strategies

Each strategy has its own wasmtime `Store`, fuel, memory cap, log, journal
and authority (its own key, so its position is its own and the mandate
audits one account). They share the `Engine`, the feed and the cache. A
trap stops that strategy, as in the bench, and nothing else. The watch set
is the union of what the desks need, and grows when a strategy is added.
New accounts are backfilled once from the local RPC, then kept fresh from
the stream.

### Control while running

A geyser reload loses every strategy's state, so control can't go through
the plugin's config. The runtime listens on a Unix socket, and `mtx live`
is its client:

| Command | Effect, at the strategy's next step boundary |
|---|---|
| `mtx live status` | each strategy: state, position, PnL, last step, fuel, skipped steps, sends |
| `mtx live pause <name>` / `--all` | no more `step` calls. By default resting orders are cancelled and the position is kept. `--keep-orders` and `--flatten` are the other two choices |
| `mtx live resume <name>` / `--all` | steps again, with a `Market` that shows what happened meanwhile |
| `mtx live mandate <name> <file>` | validates (`Mandate::validate`), then swaps the enforcer's mandate. See below |
| `mtx live add <name> <bot.wasm> <mandate> --key …` | compiles off the hot path, backfills its accounts, calls `start`, begins paused unless `--run` |
| `mtx live remove <name>` | pause, cancel, `end`, drop |

The desired state (strategies, component hashes, mandates, paused or not)
is written to disk on every change, so a validator restart or a plugin
reload brings the same set back. A restarted strategy gets a fresh `start`
with a position it didn't open. Strategies have to cope with that.

**A mandate update** takes effect in the enforcer at once. `cap` and
`max_drawdown` are in every `Market`, so a strategy sees those change with
no new interface. The rest of the mandate is only in `session`, given at
`start`. Two ways to tell the strategy: restart it (`end`, then `start`,
state lost), or add an optional `mandate-changed` export in a 0.3.0 world.
A first version restarts. When the new mandate is tighter than the
position held, the desk goes reduce-only until the position fits.

### The enforcer

On the bench a breach costs a score. Here the enforcer is the last thing
between a strategy and the principal's money, so it acts.

- Before the trade: the planner cuts the sheet to the cap, the taking
  guard refuses forbidden takes, a raw send is simulated and audited
  (`raw.rs`) before it is signed.
- After the trade: fills and positions from the feed go into `Observed`,
  and `Mandate::audit` and `check_sample` run every step.
- On a breach, or a drawdown past the limit: the strategy is paused, its
  orders cancelled, and, if the mandate's owner chose so, the position is
  closed by the host (`risk.rs` already has the forced close).
- A global kill switch pauses everything. A feed that dropped events, or
  fell behind the tip by more than a few slots, pauses everything too:
  a strategy must not trade on a stale `Market`.

### Sending, straight to the leaders

A send through RPC costs a hop and a queue. The runtime sends to the
leaders' TPU itself, after `~/Coding/bifrost` (`src/tpu_client`), which
does this for browsers:

- **Connections** (`manager.rs`). One `quinn` endpoint, ALPN `solana-tpu`,
  a client certificate from `solana_tls_utils::QuicClientCertificate`,
  0-RTT, a 4 s keep-alive under a 5 s idle timeout. A transaction is one
  unidirectional stream: open, write, finish. It goes to the current
  leader and the next.
- **Leaders** (`tracker/`). Three things tracked apart, so a slow one
  never blocks a fast one: the current slot, estimated from first-shred
  and completed events with outliers dropped (`slots_tracker.rs`); the
  epoch's schedule by slot index, rotated at the epoch boundary
  (`schedule_tracking.rs`); and each identity's TPU address, refreshed
  every minute (`leader_tracker.rs`).

What we change:

- **The slot comes from geyser**, not a websocket: `update_slot_status`
  gives `FirstShredReceived` and `Completed` in the interface we build on.
  The schedule and the cluster's addresses come from the node's local RPC,
  once an epoch and once a minute, off the hot path.
- **Connections are opened ahead.** Bifrost's send path only reuses a
  connection that exists. We connect to the leaders of the next few
  leader windows as the schedule advances, so a send never waits for a
  handshake.
- **Stake.** Leaders give most of their QUIC streams to staked peers. A
  certificate made from no key, as bifrost's is, is an unstaked peer, and
  on mainnet under load its transactions are the first dropped. Built from
  the validator's identity, the certificate gets the validator's stake.
  The geyser interface doesn't hand a plugin the identity, so the operator
  has to give the runtime the keypair's path. This is the largest open
  point of the design (see open questions).
- **Routes.** `mtx_kit::Route` already names `Rpc`, `Staked` and `Jito`.
  The default becomes the direct QUIC send, `Jito` stays a line's choice
  for bundles, and `Rpc` is the fallback when no leader connection is up.
- **Retries.** A transaction not seen landed is sent again to each new
  leader until its blockhash expires, up to the line's `retries`.

The leader maths (slot estimate, schedule index, who leads the next *n*
slots) is pure, and goes in `mtx-core` beside the simulated layer's
vocabulary, where the bench's synthetic leaders
(`crates/grader/src/txlayer/leader.rs`) can share its types.

### Inside the validator

The runtime shares the validator's process, so it must never hurt it.

- A callback filters against a set of pubkeys and owners, copies the
  bytes and does a `try_send` on a bounded channel. On a full channel it
  drops, counts, and marks the feed lossy. It never blocks, allocates
  little, and never calls wasm.
- A panic on a validator thread would take the validator down. Callbacks
  are wrapped in `catch_unwind`. The runtime's threads are its own, with
  its own tokio runtime, and a panic there stops the runtime and pauses
  every strategy, not the node.
- wasmtime's memory is capped per strategy (1 GiB today, to be lowered),
  and the cache, the journal's buffer and every channel have a size.
- The `cdylib` must be built with the validator's Rust toolchain and the
  same `agave-geyser-plugin-interface` version: the trait object crosses
  the boundary with no stable ABI.
- **Keys are in the process.** Signing elsewhere costs a round trip on
  every send. Each strategy's key is loaded from a file the operator
  names, and funded with no more than its mandate lets it lose. The
  desks sign through `TransactionSigner`, so a remote signer stays
  possible for a strategy that would rather be slow than exposed.

### Splitting the grader

`mtx-grader` is 40 modules in one crate. We add two crates, not four, and
put what is vocabulary or pure logic in `mtx-core`.

| Crate | Gains | From |
|---|---|---|
| `mtx-core` | The live vocabulary and its pure logic: a strategy's lifecycle (running, paused, stopped, and why), the control commands and their answers, journal records, feed events, the step clock (which slots are steps, what a skipped step is), leader and slot tracking, forced closes (`ForcedClose`), and the enforcer's verdicts over `Observed` | new, `risk.rs`, and types `wasm.rs` and `session.rs` share (`Account`, `StepTrades`) |
| `mtx-runtime` (new) | **host**: `WasmStrategy`, both WIT worlds and their conversions, fuel, limits, the stopped WASI. **desk**: `WasmBot`, `Desk`, `Reader`, `Seat` and the venues' seats, the taking guard, the raw-send audit. **live**, behind a feature: feed, cache, scheduler, enforcer loop, TPU sender, control socket, journal | `wasm.rs`, `venue.rs`, `venue/*`, `taking.rs`, `raw.rs`; `live` is new |
| `mtx-geyser` (new) | The `cdylib`: `_create_plugin`, the callbacks, the channel. A hundred lines, and the only crate that names `agave-geyser-plugin-interface` | new |
| `mtx-grader` | Keeps scenarios, generators, actors, sessions, snapshots, the guard, the simulated transaction layer, keys, faults, scoring. Depends on `mtx-runtime` without `live` | |

`mtx-core` has to keep building for `wasm32-wasip2` and shipping in the
task image, so nothing with a socket, a thread or wasmtime goes there. That
is the line: what the grader and the live runtime must agree on goes in
`mtx-core`, what touches a chain or a sandbox goes in `mtx-runtime`.

What ties the desk's modules to the grader today is small:
`session::INTERNAL_RPC_TIMEOUT` and `session::grader_f64` in the seats,
`making::MakingFill` in `taking.rs`, `session::StepTrades` and
`spot::Wallet` in `WasmBot`, `actors::library::RfqBoard` and
`txlayer::client::PoolClient` in `Desk`. The first five move with the
code or into `mtx-core`. The last two become traits the grader
implements. The `rpc` and `send` imports go through a trait as well, where
`WasmStrategy` holds a URL today. `raw.rs` uses surfpool's
`simulateBundle`, which mainnet doesn't have: its audit needs a second
backend.

The split is worth doing on its own. It is step 1 below, and a grade must
come out the same to the cent before and after.

### Order of work

1. **Split.** `mtx-runtime` out of `mtx-grader`, and the shared types
   into `mtx-core`, no behaviour change. Check: every reference's practice PnL equal, twice.
2. **Plugin on a surfnet, read only.** `mtx-geyser` and the feed, loaded
   into surfpool during a bench session. The plugin builds a `Market`
   each step and we compare it with the one the grader built. Check: equal
   on every step of flash-crash and market-making practice.
3. **Plugin trades on a surfnet.** The plugin steps the component and
   sends, in a session the grader only scores. Check: the reference earns
   what it earns when the grader hosts it.
4. **Control.** The socket, `mtx live`, persistence, the enforcer's pause.
   Check on a surfnet forked from mainnet, running in real time: add,
   pause, change a mandate, kill, restart.
5. **TPU.** The sender, against `solana-test-validator` and then devnet:
   leader tracking from geyser slots, warm connections, a transfer landed
   without RPC. Check: the slot a send lands in, against the slot it was
   sent in.
6. **Mainnet, shadow.** The plugin in a mainnet node. Steps run, sheets
   are planned and signed, nothing is sent. The journal says what it would
   have done. This measures the time from a slot notification to a signed
   sheet, feed loss, and what the plugin costs the node.
7. **Mainnet, small.** One strategy, a mandate with a tiny cap and
   drawdown, its own key.

## Open questions

- **Whose stake do the sends carry?** Unstaked QUIC is throttled. The
  choices: the validator's identity in the runtime's certificate (fastest,
  and the identity key is then read by code that runs strategies), the
  node's own forwarding through its local TPU (one hop, staked, no key),
  or a peering arrangement with a staked node. Step 5 measures the first
  two on devnet. Who operates the node decides the rest.
- **Which node?** An unstaked RPC node gives the feed and none of the
  stake. A voting validator gives both, and a plugin bug then costs votes.
- **Should a step be a slot, or an event?** The fastest strategy reacts
  to a write, not to the next slot boundary: geyser delivers an oracle
  update or a fill while the slot is still being replayed. The interface
  allows it (`k` counts steps, `slot` may repeat), the bench doesn't model
  it. Leave steps on slots first, and measure in the shadow run how old
  the `Market` is when `step` is called.
- **Which commitment does a step read?** Processed is freshest and can be
  rolled back. A position read at processed and a fill reported only once
  confirmed is one option. Measure on mainnet how often a processed fill
  of ours doesn't confirm.
- **How is a Phoenix perp read without a simulation?** Step 2 answered
  the first half: the accounts alone don't give the mark, the top of the
  book or a maker's position (what we learned, 2026-11-20). Three ways
  on. Ask Phoenix for the view's logic, or for decoders of the splines'
  live quotes and of the active trader buffer, and read natively. Or keep
  Hawkeye, simulated on the node's local RPC: a round trip of a
  millisecond or two a step on the same machine, to be measured in the
  shadow run against a 400 ms slot. Or change what a perp `Market`
  reports to what the accounts hold, which changes what is graded and is
  a new task version. The second costs nothing to start with and keeps
  the graded strategy unchanged.
  The blocker is narrower than the three fields. A maker's position is
  the trader account plus the fills seen since in the stream's
  transactions, which the kit's meter decodes: no buffer needed. The mark
  is a blend of the oracles and the book's mid. So the one thing missing
  is how a spline's live quotes are computed. Native is the fastest by
  far (a decode in memory against a simulation over HTTP, and it lets a
  step answer a write), so: oracle and position natively now, mark and
  top of book through Hawkeye until Phoenix's logic is in hand, and
  Hawkeye kept afterwards as a sampled check on the decoders.
  Narrower again after sampling mainnet (the last entry below): the
  position is in the active trader buffer, which has a public decoder,
  and the top of book follows from the splines except when two of them
  lock or cross.
- **A session with no end.** `steps`, `steps_left`, `slots_left` and `last`
  assume a term. A long-lived strategy needs either a rolling term (a
  mandate renewed each day, a restart at each renewal) or `u64::MAX`.
  Check how the references use `steps_left`.
- **State across restarts.** A strategy's memory is lost when the host
  restarts. Is a fresh `start` with a position enough, or do strategies
  need a `save`/`restore` pair? Ask the references first: how much do
  they hold that the `Market` doesn't give back?
- **Does a graded strategy survive a chain that doesn't pause?** Sends
  land a slot or more later, some don't land, the book moves during the
  step. The simulated transaction layer (`docs/tx-layer.md`) is the
  bench's model of this. Step 6's journal against the layer's reports is
  the calibration.
- **Who may use the control socket?** File permissions on a Unix socket
  for a first version. A mandate signed by the principal, or held in an
  on-chain account the plugin already sees, is the stronger form.
- **Raw sends.** Off on mainnet until the audit has a backend that isn't
  surfpool's `simulateBundle`.
- **Does Agave's plugin manager behave as surfpool's does?** This RFD read
  surfpool's host, not Agave's. Check the admin RPC's reload, the threads
  callbacks arrive on, and startup notifications, against Agave 4.x.

## Risks

- **Real money.** A planner or seat bug that costs a score on the bench
  costs capital here. The enforcer has to be simple enough to review
  line by line, and independent of the strategy's own view.
- **The validator.** A plugin that blocks, leaks or panics degrades or
  kills the node, and in process there is no wall between them. We accept
  that for speed. What bounds it: callbacks that only copy, limits on
  every buffer, the shadow run's measure of what the plugin costs the
  node, and a node that isn't one anybody else depends on.
- **Two hosts drift.** If the grader and the live runtime read a market
  differently, a grade stops predicting live behaviour. Steps 2 and 3 are
  parity tests and should stay in CI.
- **The bench bends to mainnet.** Sharing the desk means a live need
  (a new send state, a reconnect) can change what is graded. The WIT
  world stays the contract, and the ladder is rerun when the desk changes.
- **Keys.** One key per strategy, in the validator's memory, funded with
  no more than the mandate allows it to lose. If sends carry the
  validator's stake, its identity key is in reach of the same code.
- **Operations.** A mainnet node, monitoring, and someone to answer when
  the enforcer pauses everything at night.

## What we learned

- 2026-11-20: the study. The component boundary, `WasmStrategy`, `WasmBot`,
  the seats and the mandate audit carry over. The session, the guard, the
  keys and the simulated transaction layer don't. Surfpool loads the same
  `cdylib` a validator does, so the plugin can be tested against a bench
  session before mainnet. A geyser reload drops the plugin's memory, so
  control needs its own channel.

- 2026-11-20: in process, and TPU (Ludo). Speed comes first, so the
  runtime lives in the validator and sends to the leaders over QUIC, after
  `~/Coding/bifrost`'s `tpu_client`. That removes the out-of-process and
  remote-stream layouts, puts keys in the process, makes the `Market` a
  native decode of cached accounts, and raises the question of whose
  stake the sends carry. Fewer crates: shared vocabulary and pure logic go
  in `mtx-core`, and one new library, `mtx-runtime`, holds the host, the
  desk and the live runtime, with a thin `mtx-geyser` `cdylib`.
- 2026-11-20: step 1, the split. `mtx-runtime` holds the host (`host.rs`),
  the desk (`desk.rs`), the taking guard (`taking.rs`), the seats
  (`venue/`) and the read helpers (`reads.rs`). What the grader keeps for
  itself reaches the desk through four traits. `host::Reads` answers the
  `rpc` import: `HttpReads` in a grade, the account cache later.
  `host::RawSend` and `desk::RawDesk` take raw sends: the grader's
  `RawSender`, and `NoRawSends` for a host that has none. `desk::Sends` is
  where sends land later than they are sent: the simulated transaction
  layer's pool and engine in a grade, the TPU sender and the geyser stream
  later. `desk::Rfqs` is the clients' board. The grader keeps a `Desk` of
  its own concrete parts and converts it in `wasm::start`. Nothing moved
  to `mtx-core` yet: `Account`, `StepTrades`, `Wallet` and `Fate` are in
  `desk.rs`, since only hosts read them. They go to `mtx-core` when the
  journal and the control client need them without Wasmtime. Checked:
  the workspace's tests; the component tests on the flash-crash reference,
  the interface probe, the spot smoke, the raw probe and the transaction
  layer's probe; and the flash-crash and market-making references graded
  on their practice sets before and after, whose reports are equal in
  every field but wall time.
- 2026-11-20: step 2, the plugin on a surfnet, read only. `mtx-geyser` is
  the `cdylib`: callbacks that drop what isn't watched, copy the rest and
  `try_send` it, each inside `catch_unwind`. `mtx_runtime::live` (feature
  `live`) is the runtime on its own thread: the feed (65,536 events at
  most, dropped and counted past that), the account cache, a backfill of
  the watched accounts from the node's RPC at start, and a journal. The
  feed's events are `mtx_core::feed`. `crates/grader/tests/geyser_parity.rs`
  (`just geyser-parity`, and in CI) loads the plugin into a session's
  surfnet with `loadPlugin` and compares what it journals with what the
  grader handed the strategy at every step. What it showed:
  - **The plugin has to load before the session locks its surfnet.**
    `CheatcodeFilter::all()` locks `loadPlugin` too. `StepObserver` gained
    `before_lock`, called with the chain's RPC and the bot's key.
  - **A slot has a beginning and an end, and the grader reads in
    between.** `created_bank` comes before the slot's scenario moves,
    `processed` after everything the slot ran, the strategy's own orders
    included. The grader's read falls between the moves and the orders,
    where no slot event does. So the test asserts that the grader's read
    is one of the states the plugin saw the slot pass through, and the
    last of them when the strategy sent nothing. `rooted` trails by about
    30 slots.
  - **Flash-crash: equal.** On the reference's 280 steps, the mark, the
    oracle, the position and its entry price, decoded from the perp asset
    map and the trader account (`mtx_kit::venue::perp_prices`,
    `perp_position`), are what Hawkeye's views gave the grader. The entry
    is the position's quote lots per base lot, rounded down, as the view
    reports it.
  - **Market-making: only the oracle.** The view computes the mark from
    the book as it stands; the account holds the mark of the last crank
    (143.05 against the view's 143.20 once takers had moved the book). The
    view counts a maker's fills at once; the trader account still said
    -0.74 SPCX when the view said -0.445. Active traders' state lives in
    Phoenix's active trader buffer, which `phoenix-rise` 0.6.15 doesn't
    decode.
  - **The top of the book isn't in the orderbook account.** The snapshot's
    makers quote through splines, and their live prices aren't the regions'
    offsets from the spline's mid: with the same regions, the view's best
    bid was 2 ticks under the mid early in a session and 6 later, while the
    best ask stayed 1 above. Whatever moves it (the makers' positions
    against their limits is the likely cause) isn't in the public decoders.
  - **A lead for TPU.** The plugin interface the workspace is on (4.3.2)
    can notify a plugin of validators' gossip contact info, which would
    give the leaders' TPU addresses without `getClusterNodes`.
  So a Phoenix perp's `Market` can't be read from accounts with what is
  public today, and the open question above has three ways on. The spot
  venues and Kamino are read from accounts by the kit already
  (`crates/runtime/src/venue`), through `ChainClient::get_account`: a
  `ChainClient` over the cache serves them, and is step 3's first piece.
  Not checked: the plugin on Linux (CI will), and in a real validator.
- 2026-11-20: the perp's inputs, on mainnet. Hawkeye's `view_bbo` names
  every account it reads: the global config, the global trader index, the
  active trader buffer, the perp asset map, the orderbook and the spline
  collection. No trader account: a maker's live position is in the active
  trader buffer, keyed by its node in the trader index. So the function we
  are after has six inputs, and the plugin can watch all six.
  `crates/grader/examples/phoenix_sample.rs` takes samples from a node:
  one simulation returns the view and those accounts as the same bank
  held them. Three things from 87 samples of SOL on mainnet:
  - **The two accounts have public decoders.**
    `cosmic-phoenix-eternal-types` 0.1.4 (Apache-2.0, vendored from
    Phoenix's types for the Cinder terminal,
    `github.com/cosmic-markets/cinder`) reads the trader index and the
    active trader buffer as red-black trees. It gave every maker's
    position in every sample. So a maker's position is native after all,
    from the buffer, and the fill events are a second source.
    `phoenix-rise` is at 0.10.2 on crates.io; we are on 0.6.15.
  - **Mid plus or minus the region's start offset is right when the
    splines don't meet.** Taking each live region's first tick, the best
    bid and ask over the splines and the orderbook equal the view in 58
    samples of 87. On mainnet the makers' mids differ by a tick or two,
    unlike the bench's, where every spline sits on the oracle.
  - **All 29 misses are two splines locked (25) or crossed (4).** One
    maker's best bid is at or above another's best ask. The view then
    shows a book one tick wide, hiding one of the two quotes: the ask in
    most, the bid in some. Which one isn't the smaller, the older or the
    one further from the mark, as far as these samples say. Cinder trims
    such books by a heuristic of its own, on depth.
  What is left to find is that one rule. Next: more samples with every
  region kept, `view_order_quote` for the price a small order would get on
  each side, and the same on a surfnet where a spline can be edited and
  the view asked again.
- 2026-11-20: Phoenix's own spline rule, checked (astra found it). The
  rule was in a crate we already build on:
  `phoenix_rise::accounts::owned::Spline::liquidity_at_slot_with_position_limits`
  walks a spline's live regions tick by tick, mid minus the offset for
  bids and plus for asks, and stops a side when the maker's position
  reaches its limit (`max_long - position` lots of bids, `max_short +
  position` of asks). Run on the full set of 153 mainnet samples, with
  each maker's position from the active trader buffer, it gives the
  view's best bid and ask in 105. The limits bind for no maker there, so
  it agrees with the plain rule above sample for sample. The 48 misses
  are again exactly the samples where two makers lock or cross, and in
  every one the view's book is one tick wide. The program computes the
  same thing itself: the perp asset map's `last_best_bid` and
  `last_best_ask`, written at each crank, are one tick apart in those
  samples too. On the bench, a maker at its long limit would explain the
  bid two ticks under the mid; not yet rerun there. So the published rule
  is right as far as it goes, and what the program adds is how it
  uncrosses. Tried and ruled out on these samples, each with a
  counter-example: hide the smaller quote, the older by sequence number,
  the older by update slot, the one further from the mark, match the two
  sizes off. Next is the surfnet, where one input can be changed at a
  time and the view asked again.
