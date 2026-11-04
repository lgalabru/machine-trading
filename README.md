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
starts with a benchmark: agents write trading bots, and the bots are graded
on market scenarios replayed against an offline copy of Solana mainnet. Real
programs, real order books, and real fees run in a sandbox no bot can cheat.

The benchmark comes first because everything else needs it. A trading
harness is only as good as the scores that tell you it works, and a trading
model can only be trained against rewards that can be verified.

## 🧭 Where this is going

| Area | What it is | Status |
| --- | --- | --- |
| 📊 **Bench** | Harbor tasks where an agent writes a bot that's graded on hidden market scenarios. Each task is both an eval and an RL environment. | Shipping: [`bench/`](bench/) |
| 🛠️ **Harness** | An agent harness for trading on Solana: tools for market data, order entry and risk checks over [`luce`](crates/luce), plus guardrails, running against the same sandboxes. | Next |
| 🧠 **Models** | Trading models trained on the bench: RL against task rewards, SFT on agent trajectories, evaluated on held-out scenarios. | Later |

Every area builds on the same core: the `mtx` grader and simulator
([`crates/`](crates/)), reference bots ([`bots/`](bots/)), pinned mainnet
state ([`snapshots/`](snapshots/)), and the agent skills
([`skills/`](skills/)) that encode how to build all of the above.

## 📊 Bench

The [`solana-foundation/machine-trading`](https://hub.harborframework.com/datasets/solana-foundation/machine-trading)
dataset on Harbor Hub. Run it with [Harbor](https://harborframework.com):

```sh
# The whole dataset, three attempts per task, with Claude Code
harbor run -d solana-foundation/machine-trading -a claude-code -m anthropic/claude-sonnet-5-5 -k 3

# One task
harbor run -d solana-foundation/machine-trading -i solana-foundation/phoenix-flash-crash -a claude-code -m anthropic/claude-sonnet-5-5

# From a checkout: the reference solution, skill 1 by construction. The
# verifier draws its hidden set from the batch's secret; it refuses to grade
# without one.
export MTX_EVAL_SEED="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
harbor run -p bench/tasks/phoenix-flash-crash -a oracle
```

In every task the agent writes a bot to `/app/bot`. The verifier then
receives only that bot, with no network. It draws the hidden set from the
batch's secret (no task ships one), grades the reference bot on it, and for
each hidden session:

1. Restores a pinned mainnet snapshot into a fresh, offline
   [Surfpool](https://github.com/solana-foundation/surfpool) surfnet, at
   the slot and time it was taken, serving RPC only at a secret path.
2. Funds a new trader, stops the clock, registers the scenario, and locks
   every cheatcode for good.
3. Starts the bot as an unprivileged user, behind an RPC proxy that forwards
   standard Solana methods only.
4. Moves the world one step at a time, and gives the bot its turn between
   steps: nothing moves while the bot thinks, so grades replay.
5. After the last step, stops the bot and values its account on-chain.

The reward is the bot's total PnL over the hidden set as a fraction of a
**hindsight bound**: what the full position cap held the right way through
every move would make, at no cost. No bot reaches it, so the score keeps
headroom as agents improve. `skill`, the bot's PnL over the reference's on
the same sessions, is the headline for comparing bots. Each scenario carries a [mandate](specs/mandate.md), which gates
risk like a trading desk's: one market, a position cap, and a loss limit the
session may not draw down past. Breaking it, a refused RPC call, or ending
liquidatable scores 0, and worst session, tail
loss, drawdown, profitable share and Sharpe are reported.

### Tasks

Each task is a family of scenarios built from
[Surfpool's protocol templates](https://github.com/solana-foundation/surfpool/tree/main/crates/core/src/scenarios).
Phoenix comes first; other protocols follow.

#### 📉 Flash crash: `phoenix-flash-crash` ✅

SPCX (SpaceX) perps crash or squeeze in one to three legs of 0.6–8%, 50/50, among fizzles that give their move back. Most shocks revert, some stall,
and some keep going. A good bot reads which way the shock is going, takes
the other side of the forced flow, and sizes so the shocks that don't
revert can't take it past its loss limit. Blind bets at the open, and blind
dip-buying, score 0. [Task →](bench/tasks/phoenix-flash-crash)

#### ⚖️ Market making: `phoenix-market-making` 🧪

Quote both sides of the SPCX perp against Phoenix's own makers, widened to
a spread drawn per session, while the price walks, trends or jumps.
Grader-run takers trade against the best quotes: noise either side, and
informed bursts ahead of each jump. A good bot earns the spread and skews
or steps back before the informed flow runs it over. Valued at the oracle,
not the mark its own quotes can push. [Task →](bench/tasks/phoenix-market-making)

#### 💸 Funding squeeze: `phoenix-funding-squeeze` 🧪

Funding rates spike against the crowded side. Harvest the carry, or get out
before it bleeds the account. *Templates: market funding.*

#### 🧯 Margin squeeze: `phoenix-margin-squeeze` 🧪

The exchange tightens risk factors mid-session. Deleverage in time, or get
liquidated. *Templates: market risk factors, max liquidation size.*

#### 🚦 Halts: `phoenix-halt` 🧪

The market goes cancel-only, halts, then resumes. Manage orders and exposure
through every state change. *Templates: market status, exchange status.*

#### 🧾 Fee regime: `phoenix-fee-regime` 🧪

Maker and taker fees change, and open interest hits its cap. Route between
resting and crossing orders as the economics shift. *Templates: market fees,
open interest cap.*

#### 🔮 Oracle divergence: `oracle-divergence` 🧪

A Pyth or Switchboard feed drifts away from the venue's mark. Trade the
convergence, and don't get caught by a feed that's simply wrong.
*Templates: Pyth, Switchboard, Phoenix oracle price.*

#### 🔀 Spot–perp basis: `spot-perp-basis` 🧪

Phoenix perps dislocate from spot on Jupiter, Raydium and Orca. Capture the
basis across venues, net of fees and slippage. *Templates: Jupiter, Raydium,
Whirlpool, Phoenix market move.*

#### 🏦 Leverage loop: `kamino-leverage-loop` 🧪

Run a looped lending position while borrow rates spike and reserve limits
change. Unwind before the loop turns on you. *Templates: Kamino reserves.*

#### 🚀 Token graduation: `pump-graduation` 🧪

Trade a token through its pump.fun bonding-curve graduation to the AMM,
where liquidity and price discovery change in a single slot. *Templates:
pump, pump-amm.*

✅ published · 🧪 planned

## 🗂️ Repository layout

```
machine-trading/
├── bench/                 📊 the benchmark
│   ├── tasks/             Harbor dataset: dataset.toml + one directory per task
│   ├── checks/            cheating bots every task must score 0
│   └── image/             the images every task builds on: dev (agents) and runtime (verifiers)
├── crates/                🦀 shared Rust
│   ├── core/              mtx-core: offline worlds, scenario families, RPC guard, sessions, grading
│   ├── cli/               mtx: the grader, scenario generator, and agent-authored scenarios
│   ├── luce/              luce: the trading library (exact Decimal model, Solana runtime, Phoenix)
│   └── botkit/            mtx-botkit: the SDK bots build on, over luce (both ship in the task image)
├── bots/                  🤖 starter, reference (the oracle), and baselines
├── snapshots/             📸 pinned mainnet state (surfnet.json is gitignored)
├── specs/                 📜 the mandate: what an agent may trade, and its limits
├── skills/                🧠 agent skills: how to design great trading tasks
└── docs/                📐 architecture decision records (docs/adr) and assets
```

The harness and models will get top-level directories next to `bench/`,
sharing `crates/`, `bots/` and `snapshots/`.

## 🛠️ Hack on it

### Prerequisites

- Rust 1.97.1 (pinned in `rust-toolchain`), [`just`](https://github.com/casey/just),
  Docker with buildx, and [Harbor](https://harborframework.com) (`uv tool install harbor`).
- Nothing else: Cargo fetches Surfpool from the branch of
  [surfpool#902](https://github.com/solana-foundation/surfpool/pull/902)
  (cheatcode locks, SDK snapshots, clock restore) until it ships in a release.
- The snapshot's `surfnet.json`, which is too big for git:
  `just snapshot-pull` copies it out of the published task image. To fork a
  fresh one instead, put a mainnet RPC URL in `.env` as `MTX_FORK_RPC_URL`
  and run `just snapshot-prepare`.

### Everyday loop

```sh
just bots                                          # build every bot's bin/bot
just grade bots/reference bench/tasks/phoenix-flash-crash/environment/practice/*.json
just test lint                                     # unit tests and clippy
```

`mtx grade` runs exactly what the verifier runs. Bot logs land in
`target/logs/`. Don't run heavy builds while grading: slots advance on the
wall clock, and a starved machine gives bots fewer looks per slot.

### Review a bench

```sh
just bench                          # results go to jobs/bench/<batch>/
mtx bench review jobs/bench/<batch> # or `just review jobs/bench/<batch>`
```

`mtx bench review` opens a page over everything under the path: a batch, a
Harbor job or one trial. It shows a leaderboard per task (skill, reward, PnL,
disqualifications, timeouts, agent time and cost by model, and skill by
trial). Each trial has its sessions against the reference's, the agent's
trajectory (every message, tool call and output, with an activity strip),
the bot it wrote, and the verifier's logs. Each session is a timeline: price
(mark, index, the planned level, the book's best bid and ask, the bot's
orders), position and PnL against the reference's, the drawdown, and lanes
of events. The lanes show the scenario's moves, the account's fills, every
transaction the bot sent (decoded, with failures and duplicates), refused
calls, overrun turns, and for market making each taker order with the
informed ones marked. Session timelines come from the detail files the
verifier writes to `verifier/sessions/` (`mtx grade --details`) once every
bot has stopped: trials graded before those files existed show their
sessions as a table only.

The header is the task at a glance: a watchlist of models with their skill,
and the picked model's cumulative PnL over the hidden sessions against the
reference's. Hover the curve to read it session by session.

### Publish and compare releases

```sh
mtx bench publish jobs/bench/<batch> 0.9.0  # or `just publish …`; writes bench/results/0.9.0/
mtx bench review 0.9.0                      # a published release, by version
mtx bench compare 0.9.0...0.8.0             # skill by task and model, and the change
mtx bench compare 0.9.0...0.8.0 --open      # the same in the page
```

A published release is small enough to commit (a few hundred KB a trial):
each trial's result, the verifier's report, reward and logs, the session
detail files, the bot's source without build output, and the agent's
trajectory, gzipped. It never holds the batch's `.seed`: publishing refuses
a release where any file contains the seed, an Anthropic API key, or the
shell's `ANTHROPIC_API_KEY`. Every trial must have finished.

The review page serves every published release next to the path it was
given: switch between them in the header, or compare any two. Comparisons
use skill (PnL over the reference's on the same hidden sessions), since each
batch draws its own hidden set; a task's version can differ between
releases, and the comparison shows both.

### Author and play scenarios

`mtx scenario new` has a coding agent write a
[Surfpool scenario](https://github.com/solana-foundation/surfpool/tree/main/crates/core/src/scenarios)
from a request. The agent runs over [ACP](https://agentclientprotocol.com)
with the Surfpool MCP server attached, pointed at a surfnet restored from the
snapshot, so it reads the same prices the scenario starts from. mtx checks
what it writes (known templates and properties, Phoenix steps within 4%,
an uncross after each move) and sends problems back until it passes.

```sh
mtx scenario new "simulate a crash for SPCX" -a claude-code   # writes scenario.json
mtx scenario play "simulate a crash for SPCX" -a claude-code  # authors it, then plays it
mtx scenario play scenario.json                               # replays a saved one
```

`play` serves a page with a chart per venue and dimension, every override
on the timeline in the color of the instrument it moves. Phoenix prices are
sampled slot by slot: one market charts its mark, bid–ask and planned path
in dollars; several share one chart as a change from each one's start.
Overrides of anything else (funding, fees, another venue's rates) get a
chart of their own, which marks when each lands; press Play, or pass
`--autoplay`. Pause stops the surfnet's clock, Reset restores the snapshot on
a fresh surfnet and stages the scenario again, and Stop shuts the surfnet
down and ends the command. Play opens the Workbench (also `` ` ``), a bottom
panel with the surfnet's RPC and websocket URLs and commands to reach them
on the left, and on the right a live websocket stream of slots, logs and the
accounts the overrides write, each update tagged with the override behind
it. Agents are `claude-code` and `codex`; their ACP adapters
(`claude-agent-acp`, `codex-acp`) come from `PATH`, or through `npx`
otherwise. `surfpool` must be a build with the Phoenix templates. Both
commands show progress lines; `-v` shows the logs, the agent's replies and
its adapter's output instead.

### Change a task

```sh
just ladder <family>                   # the skill ladder on fresh hidden sets
just image && just harbor-checks       # oracle skill 1; nop, cheater, sneaky 0
just leak-audit                        # the agent's image holds no answer
harbor check bench/tasks/<task>        # LLM review against Harbor's rubric
```

After changing `bots/reference`, run `just solution` and recalibrate the
practice scenarios (`just calibrate bots/reference FILES`). Hidden sets need
no calibration: the verifier grades the reference on them every time.

### Add a task

Start with the [`trading-task-design`](skills/trading-task-design/SKILL.md)
skill. Claude Code loads it in this repo. It walks through the thesis
("a good bot profits here because it ___"), the scenario family, the
reference bot, the instruction contract, anti-cheat, market realism, and
calibration against real agents, and it ends with a pre-publish checklist.

### Release

Bump `tag` in the Justfile, build the images and run the checks on them
(`just image && just harbor-checks`), commit, then:

```sh
just release     # push both images (amd64 + arm64, ~40 min on Apple silicon),
                 # pin every task to its digest, lint, sync digests, publish
```

Commit the pins and digests it writes. CI checks every push (fmt, clippy,
tests, lint, calibrated scenarios), and builds the images and runs the
oracle through Harbor weekly.

## Known gaps

- The bot shares the verifier container with the surfnet. A bot that finds
  the surfnet's own port gets nothing from it (it serves only a secret path),
  but it could still compete for CPU; in a lockstep world that slows the
  grade, not the market. A sidecar container that can reach only the proxy
  would close this.
- Sessions can only be graded one at a time against untrusted bots: in
  parallel, a bot could reach another session's surfnet before it is
  locked. `--jobs` is for trusted bots (calibration) until sessions are
  isolated from one another.
- Scenarios move the market along a scripted path, so makers don't react to
  the bot. Each task's README lists its realism limits.
- The generator is public, so the family is known; only each batch's draws
  are secret. A held-out family (a different generator) would test transfer
  ([ADR 0001](docs/adr/0001-public-and-held-out-scenarios.md)).
