# The skill ladder

Fixed bots, from harmful to ideal, graded on fresh hidden sets. A task whose
scenarios are right ranks them the way a trader would, on every set; a
scenario change that reorders them fails, whatever it does to the models'
scores. `just ladder <task>` (a task's folder name, as in `just ladder flash-crash-spcx`)
draws `sets` hidden sets, each from a secret of its own, grades every rung on
each with `mtx grade --draw`, and checks the task's `checks/ladder.json`. A
task's rungs, ladder bots and exploit bots live in its folder
(`bench/tasks/<task>/checks/`); only the blind rungs and the rung binaries
(`rungs/`) are shared, here:

- **The order rule**, on every set, and how often it would hold on a set
  like it: a bootstrap over each set's sessions, at least `min_rule_odds`.
- **The exploit bots' limits** (`bench/tasks/<task>/checks/exploits`), over every set: they must
  lose some sessions, stay within a multiple of the reference, and never
  reach the bound.
- **Clean runs**: no rung is refused or disqualified.

Skill is a rung's total PnL over the reference's on the same set: the
reference is 1, doing nothing 0, and `hindsight` is the bound.

- **The probe gate** (a task's `probes` section, see `../probes`): with it,
  every rung is graded with `--scoring v2` against the reference's own run on
  the same sessions, and the ladder checks where the probe bots land on the v2
  score: gamblers and naive holds at most 0.02 over doing nothing, an occasional
  breacher and a crashing bot under their twins, levered references held to
  their multiple, honest bots from Sharpe 0.2 over doing nothing. `summary.json`
  carries each rule's verdict (`pass`, `fail`, `inconclusive`, `no_power`).

## Flash-crash

| Rung | What it is | Where it should land |
| --- | --- | --- |
| `nop` | the starter: never trades | 0 |
| `blind-long`, `blind-short` | the full cap one way at the open, held | a coin flip, below the reference: the family mirrors crashes and squeezes |
| `dip-buyer` | the full cap long once the mark is 3% down | below the reference: squeezes and extensions punish it |
| `momentum` | the full cap with the move once it reaches 4%, held | below the reference: it rides the shock and never fades it |
| `reference` | rides each move while it runs, fades it once it stops | 1 |
| `regime-oracle` | the reference's trading, told the draw's shape (`mtx grade --tell-regime`), not its path: holds a shock to its extreme, takes the cap from the extreme toward where the aftermath ends, and trades fizzles as the reference does | above the reference: what knowing the regime adds to good trading |
| `flash-crash-opus`, `flash-crash-fable` (0.6.0), `*-0.10` (0.10.0) | the frozen exploit bots | below the regime oracle, losing a quarter of sessions or more |
| `hindsight` | the bound | top |

## Market-making

| Rung | What it is | Where it should land |
| --- | --- | --- |
| `nop` | the starter | 0 |
| `naive-quoter` | one tick inside the competitor around the oracle, symmetric | below the reference |
| `skew-quoter` | the same, leaning against its inventory | below the reference |
| `reference` | `reference/` (market-making-spcx): inside the competitor, skewed, leaning with the drift, backing off one-sided flow | 1 |
| `flow-oracle` | told which taker orders are informed (`mtx grade --tell-regime`), each only once it has traded: quotes like the skewed quoter, and on an informed order takes the cap its way until the jump lands | above the reference: what reading the tape perfectly is worth |
| `market-making-opus`, `market-making-fable` (0.7.0), `*-0.10` (0.10.0) | the frozen exploit bots | below the flow oracle, losing in a tenth of sessions or more |
| `hindsight` | the bound (spread plus hindsight) | top |

## Funding-carry

| Rung | What it is | Where it should land |
| --- | --- | --- |
| `nop` | the starter | 0 |
| `blind-long`, `blind-short` | the full cap one way at the open, held | below the reference |
| `fader` | fades every premium at once, at the cap | below the reference: on discovery days the index follows the book, and the move runs several times the premium |
| `follower` | follows every premium at once | below the reference: on positioning days the premium bleeds out against it |
| `practice` | a gradient-boosted model fitted on the practice family | below the reference: the hidden family's noise is wider |
| `nosession` | the model fitted on the hidden family, without the session's own history or noise | below the reference |
| `gbm` | the model fitted on the hidden family, with them | below the reference |
| `reference` | `reference/` (funding-carry-spcx): tree models of the next seconds' return after funding, from the episode, the session's history and its funding parameters | 1 |
| `told` | the reference told each episode's kind: follows informed premiums until the index catches up, fades crowded ones while they last | above the reference and every frozen bot |
| `funding-carry-sol-1`, `-2` | frozen GPT-6.1 Sol bots (`bench/tasks/<task>/checks/exploits`) | below `told` |
| `hindsight` | the bound (the book's variation plus the funding the cap could collect) | top |

All but the blind rungs and the frozen bots are `checks/bots/carry-gbm` or
`reference/` (funding-carry-spcx) run with `MTX_RUNG` set (`carry-*/run.sh`).

## Weekend-gap

| Rung | What it is | Where it should land |
| --- | --- | --- |
| `nop` | the starter | 0 |
| `follow`, `fade` | the cap with (or against) the perp's move from the last external price, from 40 slots before the reopen, held | below the reference: the open keeps the informed part of the move and takes back the rest |
| `basis` | Phoenix's index as fair value: short the perp's premium over it, long its discount | below the reference: in a closed period the index is the perp's own price, lagged |
| `fitted` | a ridge regression on generic features, fitted on 2,000 generated sessions | below the reference |
| `reference` | `reference/` (weekend-gap-spcx): the structural model plus a gradient-boosted 64-slot forecast trained on generated sessions; trades the closed period, sized for the loss limit into the reopen | 1 |
| `g0-reference` | the first generation's reference: the structural model, the gap only | below the reference |
| `told` | the reference's forecaster retrained with the regime (`--tell-regime`): who is the informed trader and the worker, the half-life, where SPCX opens, and the exact permanent/transitory split; holds the known gap outright near the reopen | above the reference and every frozen bot |
| `path-oracle` | told the whole path; plans its positions by dynamic programming after costs | above `told`, below the bound |
| `weekend-gap-sol-*` | the frozen Sol bots (`bench/tasks/<task>/checks/exploits`) | below `told` |
| `hindsight` | the bound | top |

All the weekend rungs but the frozen bots are `reference/` (weekend-gap-spcx) run with
`MTX_RUNG` set (`weekend-*/run.sh`).

The flow oracle is market-making's ceiling for the exploit bots, as the
regime oracle is flash-crash's. It isn't told a regime (calm, jumps, trend:
little use without the jump times) or the jumps, only what a perfect reader
of the tape would know, when it would know it. An exploit bot above it reads
something no honest trader could: a scenario bug, not skill.

## Shared exploit bots

Bots that attack how a family scores, not how it trades, live once in
`bench/checks/exploits/` and are listed by any task they apply to. A rung may
carry an `env` (variables its bot starts with: the grader clears a bot's
environment, so the ladder starts it through a generated wrapper) and a
`push` (1: pushes the mark above the index, -1: below, 0: either way), which
makes the ladder read how far the rung moved the mark beyond `nop`'s and call
a bot that moved nothing `no_power`, not a pass. A `calibrate` rung must also
move it at least a median premium episode.

| Rung | What it is | Where it should land |
| --- | --- | --- |
| `mark-pusher-up`, `-down` | `exploits/mark-pusher`: holds the side that receives funding and quotes and trades to push Phoenix's mark away from the index (`MTX_PUSHER_MODE` `short-push-up`, `long-push-down`) | below `nop`: valuing funding on the book, not the mark, makes the push cost money |
| `mark-pusher-wash` | the same bot printing trades between its own two Phoenix accounts (`wash`) | below `nop` |

A task lists them with `"env": {"MTX_PUSHER_MODE": ..., "MTX_PUSHER_INTENSITY": ...}`
and the rules `mark-pusher-up < nop` (a rung's total against `nop`'s, which is
0 in these families).

`just ladder-test` runs this script's tests.

## Bots

The rungs written for the ladder are in `rungs/` (one crate, outside the
repository's workspace, like the exploit bots), built into `bin/` by
`just ladder`. Each rung directory holds a `run.sh` that starts its binary.
They take turns like any bot. The exploit bots predate turns and run the
old way, on a world that moves at 250 ms a slot whatever they do: slower,
and not replayable, but the same market. None of these bots ships in a task
image.
