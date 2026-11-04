# solana-foundation/machine-trading

Trading tasks for agents on Solana. In each task the agent writes a bot. The
bot is then graded on hidden market scenarios replayed against an offline
copy of Solana mainnet, restored with
[Surfpool](https://github.com/solana-foundation/surfpool).

| Task | Market | A good bot… |
| --- | --- | --- |
| [`phoenix-flash-crash`](phoenix-flash-crash) | Phoenix SPCX perp | fades crashes and squeezes that revert, and survives the ones that don't |
| [`phoenix-market-making`](phoenix-market-making) | Phoenix SPCX perp | earns the spread from noise and steps back before informed flow |

Planned tasks are
[issues labelled `new task`](https://github.com/lgalabru/machine-trading/issues?q=label%3A%22new+task%22).
Every release is in the [changelog](CHANGELOG.md).

## How a bot is judged

### The score is PnL

A bot's grade is its total PnL over the whole hidden set. Scoring the set as
a whole means losing sessions count against winning ones. Two numbers come
out of it:

| | Formula | What it's for |
| --- | --- | --- |
| **`reward`** | `clamp(Σ PnL / Σ bound, 0, 1)` | The Harbor reward and the RL signal. The bound is a hindsight bound that no bot can reach, so the reward doesn't saturate as bots improve. Each task defines its own bound. |
| **`skill`** | `Σ PnL / Σ reference PnL` | The headline for comparing bots. The reference runs on the same sessions, so it scores 1 and doing nothing scores 0. The ratio is unclamped. |

### Risk is a set of limits, not a score

Every session carries a [mandate](../../specs/mandate.md). It works like a
trading desk's limits: one market, a position cap, and a loss limit. The
grader checks the bot against it after every step (two slots):

| Rule | How it's checked |
| --- | --- |
| Trade only the task's market | No position or order on any other market. A second read confirms it, so a requote in flight doesn't count. |
| Position cap | The position, long or short, never exceeds the cap at any step. |
| Loss limit | The account's value never falls further below its running peak than the limit. |
| Standard RPC only | The bot never calls a method the RPC proxy refuses, such as a cheatcode. |
| Solvent at the end | The account doesn't end the session liquidatable. |

Breaking any rule in any session sets the whole grade to 0, for both
`reward` and `skill`. The verifier stops at the first breach. A breach never
scores better than complying.

Within the limits, risk is **reported, not scored**:

| Statistic | Definition |
| --- | --- |
| `worst_pnl_usd` | The worst session's PnL. |
| `tail_pnl_usd` | The mean PnL of the worst sixth of sessions. |
| `max_drawdown_usd` | The deepest any session's account value fell below its running peak. |
| `profitable_share` | The share of sessions that made money. |
| `sharpe` | Mean session PnL over its standard deviation, per session and not annualized. |

These statistics are left out of the score on purpose. Over a few dozen
short sessions they're noisy, and a score that blended them in would reward
whatever weights it picked. The loss limit is the risk rule that's enforced.
A bot that trades recklessly still has to stay inside it in every session.

### When there's no grade

A grade is void, rather than 0, when the baseline it's measured against is
broken: the reference bot breaks its mandate, crashes, or overruns its
turns. Some tasks add void conditions of their own. A void trial gets no
reward. `bench/report.py` lists it as void rather than scoring it 0;
re-run it on the same set.

## How a session runs

The verifier receives only the agent's `/app/bot`, and it has no network.
It draws the hidden set from the batch's secret (`MTX_EVAL_SEED`, which no
task ships) and grades the reference on it first. Then, for each session:

1. It restores a pinned mainnet snapshot into a fresh, offline surfnet, at
   the slot and time the snapshot was taken. The surfnet serves RPC only at
   a secret path.
2. It funds a new trader, stops the clock, registers the scenario, and locks
   every cheatcode for good.
3. It starts the bot as an unprivileged user, behind an RPC proxy that
   forwards standard Solana methods only.
4. It moves the world one step at a time and waits for the bot between
   steps over a turn socket (`mtx-turn-1`). Nothing moves while the bot
   thinks, so grades replay exactly.
5. After the last step, it stops the bot and values its account on-chain.

The agent's own environment has a Rust toolchain, the `mtx-botkit` SDK, a
starter bot that builds but never trades, practice scenarios, the scenario
generator, and `mtx-practice`. `mtx-practice` grades a bot exactly as the
verifier does.

**What's public:** the generator ships to agents, so the family of
scenarios is known. Each batch's draws aren't, and reports carry only the
SHA-256 of the batch's secret. Every trial in a batch is graded on the same
set. Before 0.9.0 the hidden scenarios shipped in each task's `tests/`, so
scores on those versions are a development signal, not a held-out
evaluation.

## Known limits

- **A shared container.** The bot runs in the same container as the
  surfnet. A bot that finds the surfnet's port gets nothing from it, but it
  can compete for CPU, which slows the grade but not the market.
- **One session at a time.** Sessions of untrusted bots run one at a time:
  run in parallel, a bot could reach another session's surfnet before it
  locks. `--jobs` is only for trusted bots.
- **Scripted markets.** Scenarios move the market along a scripted path, and
  makers don't react to the bot. Each task lists its realism limits.
- **A public family.** There's no held-out family that would test transfer
  ([ADR 0001](../../docs/adr/0001-public-and-held-out-scenarios.md)).

## Task READMEs

Every task's README has the same sections, in this order. Anything every
task shares belongs in this file, not in a task's README.

```markdown
# solana-foundation/<task>

One sentence: what the agent's bot does, on which market.

| | |
| --- | --- |
| Market | venue, instrument, snapshot date |
| Sessions | hidden per grade, and how they split by regime; practice scenarios |
| Mandate | position cap, loss limit |
| Bound | what the reward divides by |
| Reference | its reward, and what it does in one line |

## The skill
The thesis ("a good bot profits here because it ___"), what wins, and the
habits that lose.

## The market
The scenario family: what moves, how often, and what the agent can observe.

## Scoring
Only what's specific to this task: how the bound is computed, how the
account is valued, and any extra void or disqualifying conditions.

## Baselines
The check table (oracle, nop, cheater, sneaky), the ladder, and the
reference's numbers on fresh sets.

## Realism limits
What the simulation doesn't model.

## More
Links to DESIGN.md (why the task is built this way) and instruction.md
(the agent's contract).
```
