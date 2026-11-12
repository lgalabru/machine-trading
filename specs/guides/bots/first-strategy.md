# Guide: your first strategy, step by step

Status: guide

This guide builds two strategies and watches each play a market, one step
at a time. The first trades nothing: it prints what it is handed, so you
can see what a strategy works with. The second buys a dip and sells the
recovery, in under twenty lines, and shows the two ways that ends. Both are
in the repository, as `bots/printer` and `bots/first-strategy`; the
[guide to writing a bot](writing-a-bot.md) has the rest of the details.

They trade SPCX, a perpetual on SpaceX shares, on Phoenix, through the
flash-crash task's practice scenarios: a minute or two of market each, with
a sudden shock somewhere in it. The strategy may hold up to 70 SPCX either
way.

:::glance
- **You write:** a `Strategy`, in Rust, built into `bot.wasm`
- **It prints:** with `println!`, into the session's log
- **You watch it:** `mtx bot run --debug`, a block per step
- **It trades:** by filling in an order sheet the grader sends for it
:::

## 1. Set up

You need Rust with the `wasm32-wasip2` target, [`just`](https://just.systems),
and `mtx` (`just install mtx` in the repository); no Docker.

```sh
rustup target add wasm32-wasip2
mtx bot new ~/my-bot          # a strategy that watches the market and never trades
cd ~/my-bot
```

The strategy is in `src/lib.rs`. `just build` builds it into `bot.wasm`, a
WebAssembly component: the one file the grader loads.

## 2. A strategy that prints

Put this in `src/lib.rs`:

```rust
use mtx_kit::prelude::*;

#[derive(Default)]
struct Printer;

impl Strategy for Printer {
    /// Once, before the first step: what doesn't change in the session.
    fn start(&mut self, session: &Session) -> BotResult {
        println!(
            "{} for {} steps, from slot {} to {}",
            session.symbol, session.steps, session.start_slot, session.end_slot
        );
        println!(
            "the mark starts at {:.2}; I may hold up to {} {} either way",
            session.start_mark, session.cap, session.symbol
        );
        Ok(())
    }

    /// Every step: the market as it is now, and an empty order sheet.
    fn step(&mut self, market: &Market, _orders: &mut Orders) -> BotResult {
        println!(
            "mark {:.2}, oracle {:.2}, holding {}",
            market.mark, market.oracle, market.position.size
        );
        if market.last {
            println!("the last step: the account is valued after it");
        }
        Ok(())
    }
}

mtx_kit::export!(Printer);
```

A strategy has three hooks. `start` runs once, with the `Session`: what
doesn't change. `step` runs at every step, with the `Market` as it is then
and an empty order sheet. `end`, which this one leaves out, runs once the
session is over. `export!` makes the strategy the component.

Run it, step by step:

```sh
mtx bot run --debug
```

`mtx bot run` builds the strategy, plays one practice session, and shows
what it printed. With `--debug` it stops after every step:

```
━━ SPCX · 280 steps of 2 slots · slots 462882345–462882905 ━━━━━━━━━━━━━
 session  mark 143.11   oracle 143.05   cap 70   drawdown limit $1000.00
 printed  │ SPCX for 280 steps, from slot 462882345 to 462882905
          │ the mark starts at 143.11; I may hold up to 70 SPCX either way

━━ step 1 of 280 · slot 462882347 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 market   mark 143.11   oracle 143.05   index 143.05   bid 143.03 · ask 143.06
          position flat   resting 0/0   pnl +$0.00   drawdown $0.00
 printed  │ mark 143.11, oracle 143.05, holding 0
 sheet    empty: no orders
 cost     6.3K fuel · 0 rpc
  enter: next step · c: run to the end · q: stop showing steps
```

Each block is one step:

| Row | What it shows |
| --- | --- |
| `market` | What `step` was handed: Phoenix's mark (which values the account), the oracle, the index, the book's best bid and ask, the position, the orders resting, and an estimate of the session's PnL and drawdown |
| `printed` | What the strategy printed during the call, `println!` and `eprintln!` alike |
| `sheet` | What it asked for |
| `sent` | The orders the grader planned from it, and what became of each: ✓ landed, ✗ failed (with the venue's reason), – not sent (with why); the strategy gets the same in the next step's `market.sends` |
| `cost` | What the call spent: fuel (about one unit per WebAssembly instruction) and `rpc` reads |

Press enter for the next step, `c` to run to the end, `q` to stop showing
steps. `--auto 200` plays on, 200 ms a step; `--from 40` starts showing at
step 40; `--scenario practice-4` plays another practice scenario. The world
waits for the strategy, so stopping changes nothing: the session plays
exactly as it would graded.

## 3. A first strategy: buy the dip

A shock is a sudden move; most come back. This strategy waits for the mark
to fall 1% below where the session started, buys half the cap, and sells it
all once the mark is back:

```rust
use mtx_kit::prelude::*;

/// How far below the start the mark must fall before buying, in percent.
const DIP_PCT: f64 = 1.0;

#[derive(Default)]
struct BuyTheDip {
    start_mark: f64,
}

impl Strategy for BuyTheDip {
    fn start(&mut self, session: &Session) -> BotResult {
        self.start_mark = session.start_mark;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let change_pct = (market.mark / self.start_mark - 1.0) * 100.0;
        let holding = market.position.size;
        if holding == 0.0 && change_pct <= -DIP_PCT {
            println!("down {:.2}% from the start: buying", -change_pct);
            orders.target_position(market.cap / 2.0);
        } else if holding > 0.0 && change_pct >= 0.0 {
            println!("back to the start: selling");
            orders.target_position(0.0);
        }
        Ok(())
    }
}

mtx_kit::export!(BuyTheDip);
```

The strategy keeps state between steps: here, the starting mark, read once
in `start`. It doesn't send orders. `target_position(35.0)` says where it
wants to be, and the grader works out the orders. Practice scenario 4 has a
crash that recovers:

```sh
mtx bot run --debug --scenario practice-4 --from 30
```

```
━━ step 33 of 260 · slot 462882411 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 market   mark 140.90   oracle 140.83   index 140.83   bid 141.00 · ask 140.84
          position flat   resting 0/0   pnl +$0.00   drawdown $0.00
 printed  │ down 1.54% from the start: buying
 sheet    target_position(+35)
 sent     ✓ market buy 10 ≤ 143.72
          ✓ market buy 10 ≤ 143.72
          ✓ market buy 10 ≤ 143.72
          ✓ market buy 5 ≤ 143.72
 cost     5.4K fuel · 0 rpc

━━ step 34 of 260 · slot 462882413 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 market   mark 140.86   oracle 140.83   index 140.83   bid 140.83 · ask 140.86
          position +35 @ 140.85   resting 0/0   pnl −$1.40   drawdown $1.40
 sheet    empty: no orders
 cost     600 fuel · 0 rpc
```

At step 33 the mark is 1.54% below the start, so the strategy asks for 35
SPCX. The grader sends four market orders: one order that crosses much of
the book could exceed a transaction's budget, so `target_position` slices
them, 10 at a time. Each may fill up to 2% past the mark (143.72). At step
34 the position is there, bought at 140.85. On step 34 the strategy is
holding and the mark hasn't recovered, so it asks for nothing. When the
mark is back at the start, it sells. The session ends up **+$124.03**,
where the reference bot, which rides moves and fades them, lost $17.95.

## 4. When the dip doesn't come back

Not every shock reverts. Practice scenario 5 crashes, and keeps going:

```sh
mtx bot run --scenario practice-5
```

```
│ down 1.22% from the start: buying

practice-5  −$232.83   bound $948.78   reference +$171.36
```

The strategy bought the first 1% and then held all the way down, because
it only sells at the start. That is the lesson of a strategy this simple:
it is right about the scenarios it was written for, and has no plan for
the others. A next version might cut the position past a loss, or wait for
the fall to slow before buying.

## 5. Grade it

`mtx bot run` plays one session. `mtx bot practice` grades the strategy the
way the benchmark does, on all of the task's practice scenarios:

```sh
mtx bot practice
```

A strategy is plain Rust, so you can also test it without a chain:
`step_offline` runs one `step` on a market you make up and returns the
sheet ([Testing without a chain](writing-a-bot.md#testing-without-a-chain)).

The benchmark grades the strategy on scenarios it has never seen, and a
mandate it must not break: a position past the cap, or a drawdown past the
limit, scores the whole grade 0. The [guide to writing a
bot](writing-a-bot.md) covers those rules, the order sheet in full, and how
to read anything else from the chain.

## Glossary

Every term this guide uses is in the [glossary](../glossary.md).
