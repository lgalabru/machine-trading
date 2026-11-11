# Guide: writing a bot

Status: guide

A bot is a strategy that trades one session of a task: a scripted market,
replayed on an offline copy of Solana mainnet. You write the strategy in
Rust, on `mtx-kit`, and build it into a WebAssembly component, `bot.wasm`.
The grader does everything else: it reads the market before every step,
calls your strategy, and sends its orders without breaking the
[mandate](../../mandate.md). This guide builds a first bot, explains what
the grader hands you and what it does with your orders, shows how to test a
strategy without a chain, and ends with the rules every bot runs under.

It uses the guides' example: SPCX trades at **$100**, on Phoenix, and the
mandate caps the position at **70 SPCX**. The rules at the end are what
`mtx grade` enforces, from its source (`crates/grader/src/session.rs`,
`guard.rs`) and `mtx-kit` (`crates/kit`).

:::glance
- **You write:** a `Strategy`: what to do at each step
- **The grader does:** the market reads, sending your orders, signed with the session's key
- **Your orders are:** intentions, such as "be long 70 SPCX", cut to the mandate's cap
- **You test:** a strategy as plain Rust, with markets you make up
- **You practise:** `mtx bot practice`, the grader itself, on practice scenarios
- **A session breaks on:** a mandate breach, or ending liquidatable
:::

## Quick start

You need Rust with the `wasm32-wasip2` target (`rustup target add
wasm32-wasip2`), [`just`](https://just.systems), and `mtx` (`just install
mtx` in the repository); no Docker.

```sh
mtx bot new ~/my-bot          # a strategy that watches the market and never trades
cd ~/my-bot
mtx bot practice              # build it, and grade it on a task's practice scenarios
```

The strategy is in `src/lib.rs`, and `just build` (the bot's `Justfile`)
builds it into `bot.wasm`. `mtx bot practice` builds it, runs the same
grader the benchmark uses on the task's practice scenarios, and prints one
line a session, then the grade:

```
scenario                     your PnL    reference        bound
practice-1                      $0.00      +$73.77        $1028

reward 0.000   (your PnL $0.00 over the hindsight bound $1028)
skill  0.00    (your PnL over the reference bot's: 1 matches it)
```

The starter doesn't trade, so it scores 0. The rest is yours.

## Your first strategy

A strategy that buys a dip and sells at the next high:

```rust
use mtx_kit::prelude::*;

#[derive(Default)]
struct FadeTheDip {
    peak: f64,
}

impl Strategy for FadeTheDip {
    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.peak = self.peak.max(market.mark);
        if market.mark < self.peak * 0.97 {
            orders.target_position(market.cap); // be long the whole cap
        } else if market.mark >= self.peak {
            orders.target_position(0.0); // back to flat
        }
        Ok(())
    }
}

mtx_kit::export!(FadeTheDip);
```

`export!` makes the strategy the component; it needs a `Default` strategy,
which the grader starts from. `step` never sends anything itself. It says
where it wants to be, and the grader works out the orders after it returns:

| Step | SPCX | `step` asks for | The grader sends | Position after |
| --- | --- | --- | --- | --- |
| 10 | $100.00 | flat | nothing: already flat | 0 |
| 40 | $96.80 | `target_position(70)` | 7 market buys of 10 SPCX, at no more than $98.74 | 70 |
| 41 | $96.50 | `target_position(70)` | nothing: already there | 70 |
| 60 | $100.10 | `target_position(0)` | 7 market sells of 10 SPCX, at no less than $98.10 | 0 |

The buys and sells come in slices of 10 SPCX, because one order that crosses
much of the book can exceed a transaction's compute budget, and each one may
fill up to 2% from the mark ($96.80 × 1.02 = $98.74). Both are the defaults
of `Execution`, which you can change.

## What the grader does

Over one session:

| When | The grader | Your hook |
| --- | --- | --- |
| At start | Loads `bot.wasm`, reads the session and its mandate. | `start(&Session)`, optional: read anything you need once. |
| Every step | Reads the market: the mark, the oracle, the index, the best bid and ask, and your position. Updates its PnL and drawdown estimates. | `step(&Market, &mut Orders)`: decide, and fill in the sheet. |
| After `step` | Turns the sheet into orders cut to the position cap, sends them in order, and moves the world on. | |
| At the end | Ends the session after its last step. | `end(Option<&Market>)`, optional. |

If `step` returns an error, the grader logs it and sends nothing that step;
the session goes on. If a read fails, it skips the step. `mtx_kit::log`
writes a line to the session's log.

`Session` holds what doesn't change: the clock (`steps`, `start_slot`,
`end_slot`), the `symbol`, the `mandate` with its `cap` and `max_drawdown`,
the mark and oracle at the start, the venue's metadata (`venue`: Phoenix's
`/exchange` response, which `mtx_kit::venue::PhoenixVenue` reads), the
trading key's public key (`authority`), the task's variables
(`var("MTX_REOPEN_SLOT")` on an off-hours task) and a `seed`.

## The market

`Market` is a plain value, read before each step:

| Field | What it is |
| --- | --- |
| `k`, `slot`, `last` | The step (from 0), the chain's slot, and whether it's the last step. |
| `steps_left`, `slots_left` | How much of the session is left. |
| `mark`, `oracle` | Phoenix's mark price for SPCX, which values your account, and its oracle price. |
| `index`, `best_bid`, `best_ask` | Phoenix's index price, and the book's best bid and ask, if it has them: the same read as the mark. |
| `position` | Your position (`size` in SPCX, positive long, and `entry_usd`). |
| `cap`, `max_drawdown` | The mandate's position cap (70 SPCX) and drawdown limit. |
| `resting_bids`, `resting_asks` | What you have resting, in SPCX: every bid and ask the grader placed since your last cancel or quote, filled or not. |
| `pnl`, `peak_pnl`, `drawdown` | Your PnL since the first step, marked to market, its high and how far below it you are. An estimate: it leaves out fees and funding, which the grader counts. |

And three helpers: `room_to_buy()` and `room_to_sell()`, how much more you
can trade each way before the cap if your resting orders all fill, and
`drawdown_room()`, how far you are from the drawdown limit on the estimate.
The room is the grader's own rule (`Allowance::room` in `mtx-core`): what it
allows, the audit accepts.

## The order sheet

`Orders` records what you want. Everything on it goes out after `step`
returns, in order:

| Call | What the grader sends |
| --- | --- |
| `target_position(units)` | Market orders toward `units`, in slices, as `Execution::default()` says. |
| `target_position_with(units, execution)` | The same, paced your way: `max_per_step`, `slice`, `slippage_bps`, `min_trade`. |
| `quote(bid, ask, size)` | Cancels your resting orders, then rests a bid and an ask of `size`. |
| `buy(size, limit)`, `sell(size, limit)` | A market order, filling no worse than `limit`. |
| `bid(price, size)`, `ask(price, size)` | A resting order. |
| `cancel_all()` | Cancels every resting order. |

**The cap is kept for you.** Before anything is sent, the grader cuts the
sheet so that no fill can take your position past the mandate's cap: it
counts every order you have resting, and every earlier order on the sheet,
as filled. A cancel, or a quote, which cancels first, clears what was
resting:

| You're at | You ask for | What goes out |
| --- | --- | --- |
| 0 SPCX | `target_position(500)` | buys up to 70, and a warning in the log |
| +60 SPCX | `quote(99.0, 101.0, 20)` | a bid of 10 (60 + 10 = 70) and an ask of 20 |
| 0 SPCX | `bid(99.0, 50)` and `bid(98.0, 50)` | bids of 50 and 20 |
| 0 SPCX, bids of 40 resting | `buy(50, 101.0)` | a market buy of 30 |
| 0 SPCX | `buy(50, …)`, then `buy(50, …)` | 50, then 20 |

Each order is its own transaction. The grader sends them in order and stops
at the first that fails: the rest of the sheet waits for the next step.

For what the market doesn't carry, `mtx_kit::rpc(method, params)` reads the
chain: synchronously, as it is at this step, for any read in
`mtx_kit::RPC_METHODS` (accounts, blocks, transactions, signatures,
simulations). `rpc_account(address)` and `rpc_accounts(&[…])` return
accounts' data, which `phoenix_rise::accounts` decodes (re-exported as
`mtx_kit::venue::phoenix_rise`). It can't send: what you trade goes on the
sheet.

## Testing without a chain

A market is a plain value, so a strategy can be tested like any other Rust
code. `step_offline` runs one `step` on a market you make up and returns the
sheet; `plan` shows what the grader would send:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buys_the_dip_up_to_the_cap() {
        let mut bot = FadeTheDip { peak: 100.0 };
        let market = Market { mark: 96.8, cap: 70.0, ..Market::default() };
        let intents = step_offline(&mut bot, &market).unwrap();
        let (orders, _notes) = plan(&intents, &market);
        assert_eq!(orders.len(), 7);
        assert_eq!(orders[0], Action::Market { side: Side::Bid, size: 10.0, limit: 98.74 });
    }
}
```

`cargo test` runs it in a moment, against no chain. Practise for what tests
can't show: fills, slippage, and the market moving.

## Practising, checking, packing

| Command | What it does |
| --- | --- |
| `mtx bot practice` | Builds the bot, then grades it on the task's practice scenarios. |
| `mtx bot practice --task <name>` | Another task in `bench/tasks`, such as the market-making one. |
| `mtx bot practice more/*.json` | Grades on scenarios `mtx scenario generate` drew. |
| `mtx bot practice --json` | The grader's full report instead of the summary. |
| `mtx bot check` | Checks the bot directory: a `bot.wasm` that is a component, built from the current source. |
| `mtx bot pack -o my-bot.tar.gz` | Packages the source, without builds or logs, with a manifest of the task and the `mtx` it was made with. |

A component is the same file on any machine: what practises on your laptop
is what the benchmark grades.

## The rules underneath

### The component

The grader loads `bot.wasm`, a component of the WIT world `mtx:strategy`
(`crates/kit/wit/strategy.wit`), with `wasmtime`. `mtx_kit::export!` writes
the binding; another language that builds components can implement the
world directly.

- **No files, no environment, no network, and a clock that stands still.**
  Everything the strategy knows comes through `start`, `step` and `rpc`.
  Randomness is seeded from the session: the same session plays the same
  every run.
- **A budget.** Each call to `start`, `step` or `end` may run ten billion
  WebAssembly instructions, and the session two billion a step in all
  (about 0.1 s of computing a step; the references spend at most 75
  million). Memory may grow to 1 GiB. Past either, the call traps. A step
  may make 256 `rpc` calls; past that, `rpc` returns an error.
- **A component that doesn't load** (not a component, or importing what
  the grader doesn't provide) plays the session without trading. A trap, from a panic or the
  budget, ends the strategy: Wasmtime won't enter it again, so it isn't
  called for the rest of the session, and what it left resting stays.
  Return an error instead of panicking: an error only skips the step.
- **Reads only.** `rpc` takes the methods in `mtx_kit::RPC_METHODS`; anything
  else is an error that doesn't reach the chain. Reads are at `processed`
  unless they name a commitment.
- The world waits for the strategy however long it computes: there is no
  wall clock to beat, and a session grades the same on any machine.

The benchmark grades components only (`mtx grade --components-only`). A
bot directory without a `bot.wasm` is a process bot, below: `mtx bot
practice` runs it, and the benchmark's reference bots are processes, but
the benchmark doesn't grade one.

## Process bots

A strategy can also run as a process: `mtx_kit::run(strategy)` from `main`,
with `mtx-kit`'s default features, is a whole bot that speaks the grader's
turn protocol, reads the market and sends its orders itself. The benchmark's
reference bots run this way, and the rules below are theirs. Below the run
loop, `Turns`, `BotEnv` and `Trader` work directly, or a bot in another
language.

### The bot directory

The grader runs `just run`, from the bot's `Justfile`, in the bot's
directory; `just build` builds it. The bots in `bots/` all look like this:

```just
build:
    cargo build --release --bin mtx-maker   # then copies the binary to bin/bot

run:
    @exec ./bin/bot
```

- `run` must start the bot without building or downloading anything: the
  grader has no network, and builds nothing.
- The bot must run on Debian bookworm (slim) with nothing but its own
  directory: libc, bash, coreutils and `just`, no compiler, interpreter or
  SDK. A native `bin/bot` meets this.
- A directory without a `Justfile` may have a `run.sh` instead, which the
  grader runs with `bash`: the bots kept from earlier releases do.
- The benchmark's grader receives the directory read-only, without its
  `target/`.

### The environment

The grader clears the environment, then sets exactly:

| Variable | What it is |
| --- | --- |
| `MTX_RPC_URL` | The chain's HTTP JSON-RPC endpoint. |
| `MTX_KEYPAIR` | The trading authority's keypair file (a JSON array of 64 bytes). |
| `MTX_EXCHANGE` | Phoenix's `/exchange` metadata, matching the chain. |
| `MTX_SYMBOL` | The market: `SPCX`. |
| `MTX_END_SLOT` | The slot at which the session ends and the account is valued. |
| `MTX_MAX_POSITION`, `MTX_MAX_DRAWDOWN` | The mandate's position cap and drawdown limit. |
| `MTX_MANDATE` | A file with the session's [mandate](../../mandate.md), its term set to the session's last slot. |
| `MTX_TURN_FD` | The turn socket's file descriptor: `3`. |
| `HOME` | A private, writable directory. |
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`. |
| `RUST_LOG` | The log level, `info` by default. |

An off-hours scenario, whose underlying's market is closed, also sets
`MTX_REOPEN_SLOT`: the slot that market reopens at.

What the account holds at the start is the task's: on `flash-crash-spcx`,
SOL for fees and 10,000 USDC of collateral.

### Turns

The world moves in steps of two slots and waits for the bot between them.
The protocol, `mtx-turn-1`, is JSON lines over the socket on `MTX_TURN_FD`:

| From | Message | Then |
| --- | --- | --- |
| Grader | `{"t":"hello","protocol":"mtx-turn-1","steps":…,"slots_per_step":2,"start_slot":…,"end_slot":…,"turn_wall_ms":5000}` | The bot answers `{"t":"ready"}`. |
| Grader | `{"t":"step","k":…,"slot":…,"last":…}`: the chain holds the state after step `k`. | The bot reads, sends, and answers `{"t":"done","k":…}`. Nothing moves until it does. |
| Grader | `{"t":"end"}` | The grader stops the bot. |

Three limits:

| Limit | What happens past it |
| --- | --- |
| 10 s for `ready` | The world doesn't wait for the bot at all: it moves at 250 ms a slot. |
| 5 s for one turn | The world moves on without the bot. |
| 500 ms a step, summed over the session | The same. |

A transaction sent while the world moves is refused, which isn't a breach: it
just misses the turn.

### The chain

`MTX_RPC_URL` serves standard Solana JSON-RPC over HTTP, with no websocket:
read what you need each turn. A transaction is final once it executes; reads
are at `processed`, and an executed transaction reports `finalized`, so
clients that wait for confirmation return within the turn.

Only these methods are allowed, and **a single call to any other**, including
the sandbox's `surfnet_*` cheatcodes and `requestAirdrop`, **disqualifies the
session**:

`getAccountInfo`, `getBalance`, `getBlock`, `getBlockCommitment`,
`getBlockHeight`, `getBlockProduction`, `getBlockTime`, `getBlocks`,
`getBlocksWithLimit`, `getClusterNodes`, `getEpochInfo`, `getEpochSchedule`,
`getFeeForMessage`, `getFirstAvailableBlock`, `getGenesisHash`, `getHealth`,
`getHighestSnapshotSlot`, `getIdentity`, `getInflationGovernor`,
`getInflationRate`, `getInflationReward`, `getLargestAccounts`,
`getLatestBlockhash`, `getLeaderSchedule`, `getMaxRetransmitSlot`,
`getMaxShredInsertSlot`, `getMinimumBalanceForRentExemption`,
`getMultipleAccounts`, `getProgramAccounts`, `getRecentPerformanceSamples`,
`getRecentPrioritizationFees`, `getSignatureStatuses`,
`getSignaturesForAddress`, `getSlot`, `getSlotLeader`, `getSlotLeaders`,
`getStakeMinimumDelegation`, `getSupply`, `getTokenAccountBalance`,
`getTokenAccountsByDelegate`, `getTokenAccountsByOwner`,
`getTokenLargestAccounts`, `getTokenSupply`, `getTransaction`,
`getTransactionCount`, `getVersion`, `getVoteAccounts`, `isBlockhashValid`,
`minimumLedgerSlot`, `sendTransaction`, `simulateTransaction`.

### The sandbox

- No network but the RPC endpoint.
- When the grader runs as root, the bot runs as an unprivileged user.
- The bot runs in its own process group. At the end, the grader sends it
  `SIGTERM`, and `SIGKILL` 2 seconds later.
- Standard output and error go to the session's log.

### What breaks a session

The grader disqualifies a session, whatever its PnL, when the bot:

1. called a method outside the list above (a process bot: a component can't);
2. broke its [mandate](../../mandate.md#rules), which the grader checks
   after every step: a market outside its universe, the position past its
   cap, the account past its drawdown limit;
3. ended the session with its account liquidatable;
4. wrote a file the grader reports its grade in.

On the benchmark, **one disqualified session scores the whole grade 0**.
Each task's README gives its score, its account, its mandate and its market.

## Glossary

Every term this guide uses is in the [glossary](../glossary.md). In the text,
an underlined term shows its definition when you hover it or tap it.
