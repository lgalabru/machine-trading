# RFD 0002: Strategies compiled to wasm, called by the grader

State: discussion
Authors: opus, for Ludo Galabru  ·  Started: 2026-11-11

## Problem

A bot is a process the grader starts next to the chain. It gets an RPC
endpoint, a keypair and a socket for the turn protocol, and it can do
anything a Linux process can do with those. Three costs come from that:

- **Defending the process is most of the grader.** The RPC guard
  (`crates/grader/src/guard.rs`), the bot users and session directories
  that keep keypairs out of reach (#18, #22), the cheater, sneaky and
  keypair checks (`just harbor-checks`, `just keypair-probe`), and the leak
  audit all exist because the bot shares a machine and a network with the
  grader. Every new venue or RPC method widens what has to be checked.
- **Grades depend on the machine.** A turn is bounded by wall time
  (`TURN_WALL`, 5 s in `session.rs`), and the world waits for the bot. Under
  load, sessions run late and the grader fails them (`Degraded`), and on
  market-making the reference's PnL on a hidden session differs between two
  trials of one batch (0.11.0: 15 of 20 trials agree on `hidden-1`, 5 don't).
  A bot that ignores the turn protocol makes the grader run in real time: the
  sneaky check's market-making verifier ran past its 4-hour limit.
- **Grading is slow.** A trial's 72 sessions run one at a time, because two
  bot processes would compete for the same CPUs and their turn budgets. A
  market-making verifier takes about 57 minutes: 36 on the bot, 20 on the
  reference.

The run loop (`mtx_kit::run`, 0.11.0) already gives a strategy the shape of
a function: `Strategy::step` reads a `Market`, which is plain data, and fills
an `Orders` sheet that records `Intent`s. `step_offline` runs a strategy that
way with no chain. What keeps a bot a process is the escape hatch around it:
`Orders::trader()`, `Session::env`, and the RPC endpoint any bot can call.

The question: should a strategy be a WebAssembly component the grader calls,
with no access to anything but what the grader hands it, and a synchronous,
read-only RPC the grader answers?

## Sketch

### The interface

A WIT world, versioned with the task:

```wit
package mtx:strategy@0.1.0;

interface types {
  record session { steps: u64, slots-per-step: u64, mandate: string /* JSON */, symbol: string }
  record market {
    k: u64, slot: u64, last: bool, steps-left: u64,
    mark: f64, oracle: f64, position: f64, entry: f64,
    cap: f64, max-drawdown: f64, pnl: f64, peak-pnl: f64, drawdown: f64,
    book: book, tape: list<trade>, fills: list<fill>,
  }
  record book { bids: list<level>, asks: list<level> }
  record level { price: f64, size: f64 }
  record trade { slot: u64, side: side, price: f64, size: f64 }
  record fill  { slot: u64, side: side, price: f64, size: f64 }
  enum side { buy, sell }
  variant intent {
    target(tuple<f64, execution>), quote(tuple<f64, f64, f64>),
    market(tuple<side, f64, f64>), limit(tuple<side, f64, f64>), cancel-all,
  }
}

world strategy {
  use types.{session, market, intent};
  import log: func(line: string);
  /// A read-only JSON-RPC call against the session's chain, answered by the
  /// grader before the call returns: `params` and the result are JSON.
  import rpc: func(method: string, params: string) -> result<string, string>;
  export start: func(session: session) -> result<_, string>;
  export step: func(market: market) -> result<list<intent>, string>;
  export end: func(last: option<market>);
}
```

No WASI clocks, network, filesystem or randomness are linked. A strategy
that wants randomness gets a seed in `session`.

### RPC, synchronous

Strategies keep the chain: today's best market-making bots read every
block's taker orders, and the maker reads the competitor's quotes from
Phoenix's spline accounts. `rpc` is a host call that blocks the strategy
until the grader answers it, from its own in-process client against the
session's surfnet (with `wasmtime`'s async support, the host does real I/O
while the guest sees a plain call).

- **Deterministic.** The world is paused for the whole step: the grader
  holds the clock. Two runs of a step see the same chain, so every read
  answers the same.
- **Read-only.** An allowlist of reads: accounts, blocks, transactions,
  signatures, `simulateTransaction` (which Hawkeye's views use). No
  `sendTransaction`, no `surfnet_*` cheatcodes, nothing that depends on wall
  time or the node's own state (`getRecentPerformanceSamples`, `getHealth`).
  Writes are intents, or, if a strategy needs raw instructions, an `import
  send: func(instructions) -> result` the grader signs: the bot's key never
  enters the sandbox.
- **Bounded.** Each call costs fuel (a fixed charge, and more per KB
  returned), and a step has a cap on calls: a strategy can't stall the world
  with thousands of reads.

In `mtx-kit`, a synchronous `Chain` client (`get_account`,
`get_multiple_accounts`, `get_block`, `get_transaction`,
`get_signatures_for_address`, `simulate`) is backed by the import in wasm and
by a blocking `RpcClient` natively, so the same strategy runs under `mtx bot
practice`. Phoenix's decoding (`phoenix_rise`) is pure code and compiles
into the component. A bot reads what it reads today, without `.await`.

### The kit

`mtx-kit` keeps one `Strategy` trait, made synchronous: its async only
served RPC. `mtx_kit::export!(MyBot)` generates the component's exports
with `wit-bindgen`. The same code builds natively, for `mtx bot practice`
and unit tests over `step_offline`, and for `wasm32-wasip2`, which the
grader runs. `Orders::trader()` and `Session::env` give way to the
synchronous `Chain` client, and `Market` carries the session's keys
(market, trader account) a strategy needs to read the chain. A model's
weights ship inside the component with `include_bytes!`.

### The grader

`mtx-grader` embeds `wasmtime`. A session instantiates the component,
calls `start`, and on each step:

1. builds the `Market` from what it already reads for the step records,
   plus the book, the tape of taker trades and the bot's own fills since the
   last step;
2. calls `step` with a fuel budget and a memory limit;
3. turns the intents into transactions with `plan()` and its own `Trader`,
   signed with the bot's keypair, which never enters the sandbox.

A step that runs out of fuel or traps sends no orders, and the session goes
on: the same rule as an error from `step` today. Grading is a function of
the component, the snapshot and the scenario, so the 72 sessions of a trial
run in parallel.

### Spot sessions

A spot session runs a component the same way, in the same session loop
and through the same host as a perp session (`crates/grader/src/session.rs`,
`crates/grader/src/wasm.rs`), so a spot grade passes `--components-only`.
The grader trades each line of the sheet on the market the line names
(`venue`; the scenario's first venue if it names none) with a seat on that
venue (`crates/grader/src/venue/books.rs`): its Phoenix v1 or OpenBook v2
trader holds the bot's key and sends through the bot's guard. The first
resting order also sends the setup it needs: a Phoenix seat, or an
OpenBook open orders account. The grader sends that setup again on later
orders until it sees the account. Sizes round down to whole lots, and
prices round away from a worse fill. An order no market grid can hold is
refused. Market, limit, post-only and cancel-all orders are executed;
a post-only order's expiry holds on Phoenix, and OpenBook refuses one, as
its orders expire by the clock. Every line's send status comes back in the
next step's `sends`. The market reads as follows:

| Field | In a spot session |
|-------|-------------------|
| `mark`, `oracle`, `index` | the Pyth SOL/USD price the session values the account at |
| `best-bid`, `best-ask` | the book of the scenario's first venue |
| `venues` | each market's best bid and ask; its depth and tape, keyed by market, where the scenario reports them |
| `position.size` | SOL held, in the wallet and on every venue, as the profiles count it |
| `position.entry-usd` | the Pyth price when the session started |
| `pnl`, `peak-pnl`, `drawdown` | the account's value in USD since the start, as the session measures it |
| `cap`, `max-drawdown` | the mandate's position cap, and its drawdown limit (or the account's starting value) |
| `tape`, `depth`, `unix-timestamp`, `fills` | the step's turn data, where the scenario reports it, as a process bot gets it |

`start` gets the mandate in `session.mandate`, as a process bot gets
`MTX_MANDATE`, the markets in `session.venues`, and in `session.venue`
the markets' accounts as JSON:

```json
{ "orders": "phoenix",
  "markets": [ { "venue": "phoenix", "symbol": "SOL-USDC", "program": "PhoeNiX…",
                 "market": "4DoNfF…", "base_mint": "So111…", "quote_mint": "EPjFW…" },
               { "venue": "openbook", … } ] }
```

A component can still read the books itself through `rpc`, with the kit's
`phoenix-v1-venue` and `openbook-v2-venue` features, which build the
venues' decoding and instructions for `wasm32-wasip2` (`bots/spot-smoke`
does this). A 0.1.0 component's lines name no venue, so they go to the
first venue.

The host reads the market and its account through the bot's guard on the
bot's behalf, at a path of its own: those reads count toward none of the
scenario's numbered faults, which fire on the bot's own calls.

### The tasks

The task's instruction says the bot is a component built with `mtx bot
build` (`cargo build --target wasm32-wasip2`). `mtx bot practice` grades it
the way the verifier does. The agent's image keeps its RPC endpoint for
exploring; while graded, the strategy reads the chain only through `rpc`.
The RPC guard stays as that import's allowlist, in process: no port to
defend, no bot users, no keypair files.

### Order of work

1. **Spike, flash-crash.** Port `bots/reference` onto the run loop with the
   synchronous trait, build it as a component, and grade it in a `wasmtime`
   session on the practice set. It needs only `mark`, `position` and
   `market` intents. Check: the same PnL as the process-graded reference on
   every practice scenario, run twice.
2. **RPC.** The `rpc` import, its allowlist and its fuel, and the
   synchronous `Chain` client. Port `bots/maker`, which reads the
   competitor's quotes from Phoenix's spline accounts, and one 0.11.0
   market-making winner, which reads blocks: both should grade as they do
   today.
3. **Fuel.** Calibrate the step budget against the reference bots and the
   0.11.0 winners' logic, so a reasonable strategy never runs out.
4. **A task version on it**, with the ladder and the harness checks rerun,
   and the RPC guard, bot users and keypair checks retired from that task's
   path.

## Open questions

- **What goes in `Market` beyond the step's prices and account?** With
  `rpc`, a strategy can read what it reads today, so a book, a tape of taker
  trades and its own fills are conveniences, not requirements. Each one
  saves agents parsing blocks, and makes the tells the 0.11.0
  market-making winners found (Fable 5.2 at 12×, the #28 GPT runs at
  16–24×) easier to see. The ladder answers whether a convenience changes
  the task: rerun the flow oracle and the reference with and without it.
- **Which reads cost what?** A `getBlock` returns tens of KB, an account
  read a few hundred bytes. The fuel charge per call and per KB, and the
  call cap, need calibrating on the bots that read the most today.
- **Is fuel a fair budget?** Fuel counts wasm instructions, not time, so a
  strategy that's slow because it's careless is bounded the same as one
  that's slow because it thinks. We'd find out by measuring the 0.11.0
  winners' step costs once ported.
- **Python and other languages?** A component can come from any language
  that targets wasip2. `componentize-py` works but ships the interpreter in
  the component (tens of MB). Rust only, at first.
- **Do agents cope with the toolchain?** No tokio, no threads, no sockets.
  The 0.11.0 trajectories show every model building on the run loop when
  the instruction offers it; the spike's instruction can be tried on one
  model before a release.
- **What does a release keep?** The component, so anyone can re-grade a
  published strategy. Can the site run one in the browser over a recorded
  session? Only to show what it would decide at each step: fills depend on
  the chain.

## Risks

- **The interface becomes the task.** What the host puts in `Market`, and
  which RPC methods it answers, decide what skill is measured; a mistake
  there is a leak by design.
  The ladder has to be rerun against every interface change, as it is for
  scenario changes today.
- **Results stop comparing.** A wasm task is a new version: 0.11.0's bots
  call RPC and `Trader` and don't port. The CHANGELOG has to say the
  measurement changed, not only the tasks.
- **`wasmtime` in the grader.** A large dependency, and a new surface: a
  bug in its sandbox is a bug in ours. It is the most used wasm runtime and
  its sandbox is fuzzed continuously, which is more than our guard gets.
- **Expressivity.** Some strategies need more than a step's snapshot, such
  as their own history of the book. They can keep it in their state; the
  memory limit has to allow it.

## What we learned

- 2026-11-11: strategies keep RPC (Ludo). It is a synchronous host import,
  answered while the world is paused, so it stays deterministic; read-only,
  with writes as intents or grader-signed instructions; bounded by fuel and
  a call cap. The observation interface shrinks from a requirement to a set
  of conveniences, and the RPC guard becomes the import's allowlist.

- 2026-11-11: step 1, the spike. `crates/strategy` (`mtx-strategy`) holds
  the WIT world (`wit/strategy.wit`), a synchronous `Strategy` trait and
  `export!`; `bots/reference-wasm` is the flash-crash reference ported to
  it, a 71 KB component built for `wasm32-wasip2` with no extra tools. The
  grader's `crate::wasm` loads a bot directory's `bot.wasm` with `wasmtime`
  50 and runs it in place of the process: the same guarded endpoint, the
  bot's key in the grader's `mtx_kit::Trader`, the run loop's `Market`,
  `plan()` and ledger, orders sent in order until one fails. WASI is linked
  with no files, environment or network, stopped clocks and seeded
  randomness (Rust's std imports them on wasip2), and each call has a fuel
  budget (10^10, uncalibrated). On flash-crash's six practice scenarios the
  component earns the process reference's PnL to the cent, with every one
  of 1,538 steps equal in position, value and mark, and a second run equal
  to the first. A turn takes about as long as the process's (2.7 ms
  against 2.1 ms): the reads, not the strategy, are the cost. `rpc` is
  implemented, an allowlist forwarded to the guarded endpoint, but nothing
  calls it yet: that is step 2, with the maker.
- 2026-11-11: one trait. `mtx-strategy` is folded into `mtx-kit`: the
  strategy (`Strategy`, `Session`, `Market`, `Orders`, `plan()`, `rpc()`)
  builds everywhere; the Solana client, protocols and process plumbing are
  behind a default `chain` feature; `export!` and the WIT world
  (`crates/kit/wit`) behind `component`. The kit without `chain` builds for
  `wasm32-wasip2`; `mtx-core`'s turn client is Unix only. `rpc()` answers in
  a process too, a blocking call to the session's endpoint, and both sides
  check `RPC_METHODS`. Writing it showed reads that named no commitment
  were served at the node's default, 31 slots behind the step: the guard
  now adds `processed`, for process bots as well. The process and component
  references still earn the same PnL, and the reference and maker the same
  as before the guard change.
- 2026-11-11: the tasks move to components. The maker, the carry desk and
  the weekend reference are ported (`bots/maker-wasm`, `bots/carry-wasm`,
  `bots/weekend-wasm`; the last two link the process bots' models rather
  than copying them). Each earns its process reference's PnL to the cent
  on its task's practice set. Porting them named what a component lacked:
  the venue's metadata (`Session::venue`, read without the chain by the
  kit's `venue` feature), its own key (`Session::authority`, for its trader
  account and to tell its own orders on the tape), the task's variables
  (`Session::var("MTX_REOPEN_SLOT")`), and the top of the book: Hawkeye's
  view is a simulated transaction a component can't build, so `Market`
  carries the index and best bid and ask from the read the mark already
  comes from. Two process habits had no counterpart and didn't matter:
  the maker read its position after a lean to count its own fill, and the
  carry desk read it after every slice; counting the order as filled gives
  the same sessions. The starter is now a component, the verifier and
  `mtx-practice` grade with `--components-only`, and the oracle solutions
  are the ported bots. The ladder's anchors and rungs stay processes.
- 2026-11-11: a trap ends a component's session. A probe that panics at
  step 3 is never entered again: Wasmtime answers every later call with
  "cannot enter component instance". The grader now stops calling a
  strategy after its first trap, and the instructions say so; an error
  returned from `step` only skips the step. Re-instantiating after a trap
  would lose the strategy's state silently, so we don't.
- 2026-11-11: step 3, fuel. A loop that spins every step spends 2.1
  billion units in about 108 ms on an M-series core: about 20 billion a
  second. On one practice session each, the references spend at most 75
  million a step (funding-carry, 40.6 billion over 536 steps; the maker
  11.7 million in all, the weekend reference 473 million, flash-crash's
  334 thousand). A call may spend 10 billion, and a session two billion a
  step in all (`FUEL_PER_STEP`): about 0.1 s a step, which bounds a
  72-session hidden set's computing near an hour on a slower core, below
  the verifier's timeout, where the process bots' 0.5 s a step allowed
  three. The spinner runs out at step 134 of 280 and plays no more. Memory
  is capped at 1 GiB. A `bot.wasm` that doesn't load (random bytes, in the
  check) plays the session without trading instead of failing the grade,
  and each session's report carries its fuel and why a component stopped.
- 2026-11-11: the checks. `bench/checks/cheater-wasm` tries, from inside
  the sandbox, the chain's cheatcodes (straight to the host import, past
  the kit's check), the hidden files and the reward file, the environment,
  sockets to the surfnet and the internet, the clock, and memory past the
  cap: every attempt fails (no such file, permission denied, an empty
  environment, a clock that stands still, allocation stops at 768 MiB),
  and the session grades to completion with a reward of 0. A process bot
  under `--components-only` isn't graded: the reward is 0 and
  `not_graded` is 1, so the old cheaters check that.
- 2026-11-11: one crate per reference. The `bots/*-wasm` ports are folded
  back into `bots/reference`, `maker`, `carry` and `weekend`: the strategy
  is `src/lib.rs`, a component on wasm32 (`export!`), and `src/main.rs` is
  `mtx_kit::run(strategy)`, the anchor and the ladder's rungs, which read
  `MTX_RUNG` and `MTX_REGIME` through `Session::var` (both are session
  variables in a process). `just build` builds both. The old process code,
  which read and sent through `Trader` itself, is gone.
- 2026-11-12: the interface, 0.2.0 (`crates/kit/wit/strategy.wit`). What
  the next tasks need, as conveniences on the same step: the session's
  venues (`<venue>:<kind>:<symbol>`, the mandate's names, its own first)
  and a view of each (best bid and ask, the book and the tape when the
  scenario reports them, the position there); lines that name a venue and
  carry a time in force (post-only, immediate-or-cancel), reduce-only, a
  participation cap and transaction options; what became of every order of
  the last sheet (`sends`: landed with a signature, failed with the venue's
  reason, not sent, and, for the transaction layer, delayed and expired),
  the planner's notes, own fills from the tape, and requests for quotes
  with a response line. Intents became records (an action, a venue, the
  options) rather than a variant of tuples. The kit plans every sheet with
  one planner (`plan_sheet`), a ledger per venue; `plan()` is the same for
  lines without options. The grader keeps 0.1.0 (`wit/0.1.0`), tells the
  worlds apart by type-checking the component against each before any of
  its code runs, and plans both with the same planner: the flash-crash,
  maker, carry and weekend references grade the same, built on either.
  Not yet: a session trades only its own market (a line for another comes
  back not sent until spot venues have component executors), no session
  reports a perp tape (so a perp strategy's fills come from its position),
  no session sends requests for quotes, and transaction options are
  counted, not applied, until the transaction layer.
- 2026-11-12: 0.2.0 settled before it freezes, after review. WIT records
  and variants are closed, so what the next venues need is in the world
  now, answered `not-sent` until their executors land: market kinds for
  pools, lending reserves and stake pools; swap, add- and
  remove-liquidity (with a price range), deposit, withdraw, borrow, repay
  and liquidate; client order ids and cancel by id; two-way requests for
  quotes; a route and retries on transaction options; slot, fee and tip on
  send status, and a fee view; liquidity positions, a lending account,
  resting orders and the venue's own numbers in each venue's view; and the
  grader-signed `send` import, reserved, which every session refuses until
  raw sends land. Four fixes to what was there: reduce-only now counts
  only the sheet's orders that shrink the position as filled, so an
  unfilled buy can't let a sell open a short; a post-only order is checked
  against the paused book and, if it would cross, reported failed and kept
  out of the resting count (Phoenix's own post-only skips it silently);
  the session says what its steps report, so an empty tape is a step
  without trades; and turn data names its market (`turns::Trade::market`),
  venues carry their address, and an id is unique in the session (with
  `@<address>` where two pools would share one).
- 2026-11-12: two planner bugs, and the last closed-type gaps. A line the
  session won't execute (a swap on a perp) counted as filled and made
  room under the cap for the lines after it: a session now plans with
  what it executes (`plan_sheet_with`), and such a line counts for
  nothing. A reduce-only line was only cut on the sheet: its orders now
  carry Phoenix's reduce-only flag (a post-only one through the second
  multiple-order packet), so a resting one can't open a position later.
  A target whose `max_per_step` isn't positive panicked in `clamp`; it is
  a note now. And, so 0.2.0 doesn't need a 0.3.0: adding to an existing
  liquidity position and a bin shape; expiries on limit orders, quotes and
  responses to requests for quotes (Phoenix's last valid slot; not yet on
  a post-only order); a liquidation's minimum received; a failure's
  signature; and fresh signers on raw transactions, named by index in the
  instructions and returned by `send`, for accounts that sign their own
  creation (a position's mint).
- 2026-11-12: what raw sends need for a new position. Opening a Raydium
  CLMM or Orca position names accounts derived from the fresh NFT mint: the
  position PDA and the owner's token account for the mint. A raw
  transaction now lists derived addresses (a PDA from a program and seeds,
  or an associated token account), which instructions and later entries
  name by index; WIT types can't be recursive, so an entry names only the
  ones before it, which also rules out cycles and bounds the depth by the
  list's length (32). The grader makes the fresh signers, then resolves
  the list in order. `remove-liquidity` gained minimums out, an expiring
  post-only order goes through Phoenix's second multiple-order packet built
  from its parameters, and a line with an amount that isn't a number gets
  a note.
