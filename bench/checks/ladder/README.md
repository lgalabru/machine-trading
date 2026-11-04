# The skill ladder

Fixed bots, from harmful to ideal, graded on fresh hidden sets. A task whose
scenarios are right ranks them the way a trader would, on every set; a
scenario change that reorders them fails, whatever it does to the models'
scores. `just ladder <task>` (`flash-crash` or `market-making`) draws `sets`
hidden sets, each from a secret of its own, grades every rung on each with
`mtx grade --draw`, and checks the task's `<task>.json`:

- **The order rule**, on every set, and how often it would hold on a set
  like it: a bootstrap over each set's sessions, at least `min_rule_odds`.
- **The exploit bots' limits** (`../exploits`), over every set: they must
  lose some sessions, stay within a multiple of the reference, and never
  reach the bound.
- **Clean runs**: no rung is refused or disqualified.

Skill is a rung's total PnL over the reference's on the same set: the
reference is 1, doing nothing 0, and `hindsight` is the bound.

## Flash-crash

| Rung | What it is | Where it should land |
| --- | --- | --- |
| `nop` | the starter: never trades | 0 |
| `blind-long`, `blind-short` | the full cap one way at the open, held | a coin flip, below the reference: the family mirrors crashes and squeezes |
| `dip-buyer` | the full cap long once the mark is 3% down | below the reference: squeezes and extensions punish it |
| `momentum` | the full cap with the move once it reaches 4%, held | below the reference: it rides the shock and never fades it |
| `reference` | rides each move while it runs, fades it once it stops | 1 |
| `regime-oracle` | told the draw's shape (`mtx grade --tell-regime`), not its path: takes the cap from the extreme toward where the session ends | above the reference: what reading the regime well could still earn |
| `flash-crash-opus`, `flash-crash-fable` | the frozen 0.6.0 exploit bots | below the regime oracle, losing a quarter of sessions or more |
| `hindsight` | the bound | top |

## Market-making

| Rung | What it is | Where it should land |
| --- | --- | --- |
| `nop` | the starter | 0 |
| `naive-quoter` | one tick inside the competitor around the oracle, symmetric | below the reference |
| `skew-quoter` | the same, leaning against its inventory | below the reference |
| `reference` | `bots/maker`: inside the competitor, skewed, leaning with the drift, backing off one-sided flow | 1 |
| `flow-oracle` | told which taker orders are informed (`mtx grade --tell-regime`), each only once it has traded: quotes like the skewed quoter, and on an informed order takes the cap its way until the jump lands | above the reference: what reading the tape perfectly is worth |
| `market-making-opus`, `market-making-fable` | the frozen 0.7.0 exploit bots | below the flow oracle, losing in a tenth of sessions or more |
| `hindsight` | the bound (spread plus hindsight) | top |

The flow oracle is market-making's ceiling for the exploit bots, as the
regime oracle is flash-crash's. It isn't told a regime (calm, jumps, trend:
little use without the jump times) or the jumps, only what a perfect reader
of the tape would know, when it would know it. An exploit bot above it reads
something no honest trader could: a scenario bug, not skill.

## Bots

The rungs written for the ladder are in `rungs/` (one crate, outside the
repository's workspace, like the exploit bots), built into `bin/` by
`just ladder`. Each rung directory holds a `run.sh` that starts its binary.
They take turns like any bot. The exploit bots predate turns and run the
old way, on a world that moves at 250 ms a slot whatever they do: slower,
and not replayable, but the same market. None of these bots ships in a task
image.
