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
with no access to anything but what the grader hands it?

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
  export start: func(session: session) -> result<_, string>;
  export step: func(market: market) -> result<list<intent>, string>;
  export end: func(last: option<market>);
}
```

No WASI clocks, network, filesystem or randomness are linked. A strategy
that wants randomness gets a seed in `session`.

### The kit

`mtx-kit` keeps one `Strategy` trait, made synchronous: its async only
served RPC. `mtx_kit::export!(MyBot)` generates the component's exports
with `wit-bindgen`. The same code builds natively, for `mtx bot practice`
and unit tests over `step_offline`, and for `wasm32-wasip2`, which the
grader runs. `Orders::trader()` and `Session::env` go away. A model's
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

### The tasks

The task's instruction says the bot is a component built with `mtx bot
build` (`cargo build --target wasm32-wasip2`). `mtx bot practice` grades it
the way the verifier does. The agent's image keeps its RPC endpoint for
exploring, but grading never gives the strategy one.

### Order of work

1. **Spike, flash-crash.** Port `bots/reference` onto the run loop with the
   synchronous trait, build it as a component, and grade it in a `wasmtime`
   session on the practice set. It needs only `mark`, `position` and
   `market` intents. Check: the same PnL as the process-graded reference on
   every practice scenario, run twice.
2. **The observation interface.** The book, the tape and the fills, and
   what the market-making tasks' `making` worlds put on the tape. Port
   `bots/maker`, which today reads the competitor's quotes from Phoenix's
   spline accounts over RPC: it needs `book`.
3. **Fuel.** Calibrate the step budget against the reference bots and the
   0.11.0 winners' logic, so a reasonable strategy never runs out.
4. **A task version on it**, with the ladder and the harness checks rerun,
   and the RPC guard, bot users and keypair checks retired from that task's
   path.

## Open questions

- **What does the strategy see?** The 0.11.0 market-making winners (Fable
  5.2 at 12×, the #28 GPT runs at 16–24×) read every block's taker orders
  over RPC and trade on one-sided bursts. A tape in `Market` hands them that
  directly: is that the skill the task measures, or does the tape have to be
  delayed, aggregated or noisy? The ladder answers it: rerun the flow oracle
  and the reference against each candidate interface.
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

- **The interface becomes the task.** What the host puts in `Market`
  decides what skill is measured, and a mistake there is a leak by design.
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
