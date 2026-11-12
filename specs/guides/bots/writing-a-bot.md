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

The starter doesn't trade, so it scores 0. The rest is yours. To watch a
strategy play one session, step by step, run `mtx bot run --debug`; the
[step-by-step guide](first-strategy.md) starts there.

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
the session goes on. If a read fails, it skips the step. What the strategy
prints (`println!`, `eprintln!`, or `mtx_kit::log`) goes to the session's
log, up to 1 MiB a call; `mtx bot run` shows it.

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
| `sends` | What became of each order your last sheet planned: its line on the sheet, the venue, your client id, the order as planned, and its state: `Landed` with its signature, `Failed` with the reason (the venue's, or a post-only order that would have crossed) and, for a transaction that ran on chain and failed, its signature, or `NotSent` with why not. Slot, fee and tip are there for sessions that know them. |
| `notes` | What the grader cut or dropped from your last sheet, and why: the same notes it logs. |
| `venues` | Each of the session's markets (`Session::venues`, yours first, each with a unique id and its account's address): its best bid and ask, its book and its trades since the last step where the session reports them (`Session::reports`; a reported tape that's empty is a step without trades), your position there, and, as venues arrive, your resting orders, liquidity positions, lending account and the venue's own numbers (`stats`). |
| `fills` | Your own trades since the last step, from the venues' tapes: empty where the scenario reports no tape, as SPCX's sessions don't. |
| `tape`, `depth`, `unix_timestamp` | The step's trades and books on every venue, and the world's clock, when the scenario reports them. |
| `rfqs` | Requests for quotes open at this step, one-sided or two-way: none in today's tasks. |
| `fees` | The fee market (recent priority fees, the tip floor, the next leaders), where a session models one: none does yet. |

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

**Each line can say more.** A call adds a line to the sheet; these change
the line added last:

| Call | What it does |
| --- | --- |
| `.post_only()` | A `bid`, `ask` or `quote` that takes nothing. One that would cross the book as it is when it's sent (the world is paused, so that includes your own earlier fills) isn't sent: it comes back `Failed("post-only would cross: not placed")` and doesn't count as resting. |
| `.ioc()` | A `bid` or `ask` that fills what it can now, at no worse than its price, and rests nothing: a market order bounded by the price. |
| `.reduce_only()` | The line's orders are cut so that no fill can grow or flip your position, whatever else fills: the sheet's earlier orders that shrink it, and what rests on its side, count as filled; earlier orders that grow it count as not filled. Flat, `bid(…).ioc()` then `sell(5, …).reduce_only()` sells nothing: the buy may not fill. The orders also carry Phoenix's reduce-only flag, so a resting one can't open a position however the book moves. |
| `.expires(slot)` | A `bid`, `ask` or `quote` rests until `slot` (Phoenix's last valid slot); a response to a request for a quote can be taken until then. |
| `.participation(share)` | A target trades at most `share` of the venue's volume since the last step. The volume is the venue's tape's: in a session without one, there is none to take a share of, and the target doesn't trade. |
| `.at(venue)` | The line is for another of `Session::venues` (`openbook:spot:SOL-USDC`). A session trades only its own market for now: a line for another comes back `NotSent`. |
| `.client_id(id)` | Your own id for the line's orders: in their `sends`, and what `cancel(id)` names. |
| `.tx(TxOptions { … })` | A priority fee, a tip, a compute limit, a bundle id, a route, retries until a slot. Accepted and counted, and changes nothing: no session applies them yet (`Session::applies_tx_options`). |

`orders.bid(99.0, 5.0).post_only()` rests a post-only bid;
`orders.sell(70.0, 98.0).reduce_only()` sells what you hold, and no more.
`orders.respond(id, bid, ask)` answers a request for a quote in `rfqs`; no
task sends requests yet, so it comes back `NotSent`.

The sheet also takes what the next venues will execute: `cancel(id)`,
`swap(side, amount, unit, limit)`, `add_liquidity(base, quote, range)`,
`remove_liquidity(position, share)`, `deposit`, `withdraw`, `borrow`,
`repay` and `liquidate`, each with `.at(venue)`. A session without an
executor for one answers it `NotSent("unsupported in this session")`; on
SPCX today that is all of them. A line the session won't execute counts
for nothing: it doesn't make room under the cap for the lines after it.
Where a venue executes them, a swap is cut to the cap like a market
order, `remove_liquidity(position, 0.0)` collects a position's fees only
(`Intent::RemoveLiquidity` also takes a minimum of each asset out),
and a `TxOptions` without `cu_limit` asks for the simulation's units plus
a margin; `retry_until_slot` sends the same transaction again each slot,
and signs it again with a new blockhash only once the old one has expired
and the transaction hasn't landed.

For what the sheet can't say, a component can build a transaction itself
and have the grader sign it: `mtx_kit::send_raw`, the world's `send`
import, from `step`. A task opts in, and lists the programs raw
transactions may run, directly or through another program, in the session
variable `MTX_RAW_PROGRAMS` (comma-separated); every other session refuses
them all. An instruction names its accounts by address, as the authority,
as one of the transaction's fresh signers (`Fresh(n)`: keys the grader
makes for that transaction alone, for accounts that must sign their own
creation, whose addresses `send` returns), or as one of its derived
addresses (`Derived(n)`: a PDA from a program and seeds, or an associated
token account, which may name a fresh signer, as a new position's
accounts do). `mtx_kit::raw` has the types, and `RawTransaction::check`
the rules of their shape.

The grader simulates each one at once, against the paused chain, and
refuses it, with the reason, if it fails, runs a program off the list, or
writes a Phoenix trader account other than the bot's main one (a
subaccount it registers or funds, or another trader's), or would leave the
account breaking its mandate: over its position cap, holding another
market, or past its drawdown limit. A step may have 16 raw sends
simulated; the rest are refused, and so is a copy of a send the step
already took (it would be the same transaction). A task may allow only the
programs whose effects the grader measures: Phoenix, the system, token,
associated token and compute budget programs. A refused send queues
nothing. A taken one goes out after the sheet, in the order taken, once it
has been checked again against the chain the sheet left; the next step's
`sends` say what became of it (`SendSource::Raw(id)`, with its signature,
slot and fee, or `Delayed` where the task's faults held it: it may still
land, so don't send it again; until it lands, new sends are checked as if
it had, and two held at once refuse a third). Raw sends in one step don't see each other when they're
taken, so a transaction that needs an account another creates goes in the
same transaction or a later step. The planner doesn't count orders a raw
transaction rests on the book.

A line with an amount or a price that isn't a number (NaN) isn't sent,
and a note says so.

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
| `mtx bot run` | Builds the bot and plays one practice session, showing what it printed. `--debug` stops at every step: the market, what it printed, its sheet, the orders sent (`--auto 200`, `--from 40`). It plays `practice-101` (5 steps) unless `--scenario` names another: `practice-102` (10 steps), one of the task's practice set, or a file. |
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

The grader loads `bot.wasm`, a component of the WIT world
`mtx:strategy@0.2.0` (`crates/kit/wit/strategy.wit`), with `wasmtime`.
`mtx_kit::export!` writes the binding; another language that builds
components can implement the world directly. A component built on 0.1.0
(`crates/kit/wit/0.1.0/strategy.wit`) still loads, and plays as it did: it
gets 0.1.0's market, and its sheet's lines have no options.

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
