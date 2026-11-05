<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-github-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="docs/assets/banner-github-light.png">
    <img alt="Machine Trading" width="100%" src="docs/assets/banner-github-light.png">
  </picture>
</div>

<p align="center">
  <a href="https://hub.harborframework.com/datasets/solana-foundation/machine-trading"><img alt="Harbor dataset" src="https://img.shields.io/badge/harbor-solana--foundation%2Fmachine--trading-blue"></a>
</p>

**Machine Trading is a workshop for frontier trading agents on Solana.** It
starts with a benchmark. Agents write trading bots, and the bots are graded
on hidden market scenarios replayed against an offline copy of Solana
mainnet. Real programs, real order books and real fees run in a sandbox that
no bot can cheat.

## Getting started

```sh
just install mtx      # build and install the mtx CLI into ~/.cargo/bin
just snapshot-pull    # the mainnet snapshot scenarios replay on (Docker; too big for git)
```

A **scenario** is a scripted market: a sequence of changes (prices that
crash or squeeze, funding, fees, a halt) applied at given slots to an
offline copy of Solana mainnet, restored from a pinned snapshot. Bots are
graded on hidden scenarios; `mtx scenario play` has a coding agent write one
from a request, then plays it on a local surfnet and charts it live:

```sh
mtx scenario play "SPCX crashes 5% in three legs, then rebounds halfway"
```

`mtx bench review` browses a bench's results: each model's skill and costs,
every run, and every scenario step by step. Give it a published release
(`bench/results/<version>`) or a batch you ran:

```sh
mtx bench review 0.10.0
```

The rest (building bots, grading, publishing and comparing releases) is in
[docs/development.md](docs/development.md).

## The bench

```sh
harbor run -d solana-foundation/machine-trading -a claude-code -m anthropic/claude-sonnet-5-5 -k 3
```

| Task | A good bot… |
| --- | --- |
| [📉 `phoenix-flash-crash`](bench/tasks/phoenix-flash-crash) | fades crashes and squeezes on the SPCX perp that revert, and survives the ones that don't |
| [⚖️ `phoenix-market-making`](bench/tasks/phoenix-market-making) | earns the SPCX spread from noise, and steps back before informed flow |

Planned tasks are tracked as
[issues labelled `new task`](https://github.com/lgalabru/machine-trading/issues?q=label%3A%22new+task%22).

**How bots are judged.** The score is PnL: `skill` is a bot's total PnL over
the hidden set, divided by a reference bot's on the same sessions. Risk is a
set of hard limits that work like a trading desk's: one market, a position
cap, and a loss limit. A bot that breaks one in any session scores 0.
Statistics such as tail loss, drawdown and Sharpe are reported alongside
the score. The details are in the [dataset README](bench/tasks/README.md).

## Where this is going

| | | |
| --- | --- | --- |
| 📊 **Bench** | Harbor tasks: each one is both an eval and an RL environment | Shipping |
| 🛠️ **Harness** | An agent harness for trading on Solana, over [`luce`](crates/luce) | Next |
| 🧠 **Models** | Trading models trained on the bench's rewards and trajectories | Later |

## Repository

```
bench/      📊 tasks (the Harbor dataset), checks, images, published results
crates/     🦀 mtx (grader and CLI), mtx-core, luce (trading library), mtx-botkit (bot SDK)
bots/       🤖 starter, reference and baseline bots
snapshots/  📸 pinned mainnet state
specs/      📜 the mandate: what an agent may trade, and its limits
skills/     🧠 agent skills, starting with how to design trading tasks
docs/       📐 development guide and decision records
```

To build, grade, review a bench or add a task, see
[docs/development.md](docs/development.md).
