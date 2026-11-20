# 0003. A strategy is free in form, and bounded only by its mandate

- **Status:** Accepted (2026-11-20).
- **Scope:** every task in `bench/tasks`, and `mtx-kit`.

## Context

In 0.12.0 GPT-6 Astra scored first on all four tasks, 20× the reference on
`market-making-spcx`. We read its twenty bots to see whether it had cheated.

It had not read the hidden scenarios. A batch's hidden set is drawn inside
the verifier from a secret the agent never holds: `just bench` draws 32
random bytes per batch, and only the verifier's container gets them, after
the agent has stopped. `mtx scenario generate` draws
other sessions of the same public family. Of the 1,136 sessions Astra's
bots generated and kept reports for, none is one of the 72 hidden sessions
of its trial.

What it did, on three tasks, was learn the family. It generated 1,200 to
18,000 sessions, fitted gradient-boosted trees with scikit-learn, and
compiled them into the bot as Rust tables of about a megabyte. GPT-6.1 Sol
had done the same before the release, and the best of its bots became
`funding-carry-spcx`'s reference.

On `market-making-spcx` it did something the task was not meant to
measure. All five bots lost money on their quotes, $77 to $351 over 72
sessions. They made $1,794 to $2,615 by reading the burst of informed
orders that comes before a jump, buying 16 to 19 SPCX with market orders,
and holding through the jump. The task allowed it: its bound counted the
moves a bot trades into. But that skill is `flash-crash-spcx`'s, and a
market-making task whose winners do not make a market measures the wrong
thing.

One bot also listed every Phoenix trader account, kept those with more than
50,000 USDC deposited within 100 slots of the session's start, and read
their positions every step. Those are the grader's eight takers. It found
them in 72 of 72 sessions and earned no more than the bots that read the
public trade ring.

Two questions followed. Should we stop agents from sampling the generator
and embedding models? And how do we keep a task measuring its own skill?

## Decision

**We do not constrain what a strategy looks like.** The benchmark exists to
rank models and to produce excellent strategies. A fitted model in a bot is
a strategy, and often a better one.

- The four tasks keep `mtx scenario generate` in the agent's image. An
  agent may sample its family as much as its hour allows, train anything
  on the samples, and use any crate.
- `mtx-kit` helps instead of leaving each agent to rebuild the plumbing:
  `mtx_kit::features` for bars, returns and volatility, `mtx_kit::trees`
  for tree ensembles exported from scikit-learn or LightGBM, and the
  `candle` feature for `candle-core` and `candle-nn`
  ([models in a strategy](../../specs/guides/bots/models.md)).
- The limits a strategy runs under are the sandbox's, the same for every
  shape of strategy: reads only, no files, no network, a fuel budget.

**A task bounds a strategy through its mandate, as a desk bounds a
trader.** A mandate says what the account may do: its instruments, its
position cap, its drawdown limit, and what it may take liquidity for
(`specs/mandate.md`). It says nothing about how the bot decides.

- `market-making-spcx`'s mandate allows taking only to reduce the position
  (`taking: "reduce_only"`), from 0.13.0. A maker builds inventory through
  its quotes and may pay the spread to shed it. A market order that would
  open, add to or flip the position is refused before it is sent and
  reported to the bot. It is not a breach.
- The other three tasks' mandates are unchanged. Taking is their skill.

**A harness artifact is a bug in the task, not a rule for the agent.** We
do not forbid reading other accounts: on a real chain they are public. We
make sure reading them pays nothing a market would not pay.

## Alternatives considered

- **Hide the generator** (the guarded image of
  [ADR 0001](0001-public-and-held-out-scenarios.md), which
  `mtx task new` applies). It stops mass sampling, and with it the
  strategies we want to see. A model fitted to the family is fitted to
  something public and stated in the instruction. It cannot be fitted to
  the hidden set.
- **Leave taking free and strengthen the reference.** The task's design
  expected bots to trade the bursts, with a flow oracle as their ceiling.
  A stronger reference would bring the 20× down. But the score would
  still go to the bot that quotes least.
- **Quoting obligations** (`crates/grader/src/obligations.rs`) without the
  taking rule. A bot can meet a presence duty with one small quote a side
  and still make its money on the bursts.
- **Disguise the grader's accounts.** In an offline sandbox every account
  whose position moves is the grader's, so the same list can be built
  without the collateral or the deposit slot.

## Consequences

- On 12 fresh sessions, three of Astra's market-making bots, rebuilt on
  the current kit, went from $143 and $220 (the two we also graded without
  the rule) to -$23, -$8 and $14. The grader refused 255 to 1,334 of their
  orders. The bot that finds the takers still finds them, and it no longer
  pays.
- `market-making-spcx`'s reference and its ladder's flow oracle no longer
  take with a burst. The reference stops quoting the side a burst hit.
  The oracle sheds what the burst gave it and leans with a quote. The hindsight part of the bound stays, and is further out of
  reach than before, so rewards on this task are lower than in 0.12.0.
  Its scores do not compare across the two versions.
- A score partly measures how well an agent fitted the public family. The
  ladder holds the line that matters: a bot that knows the generator must
  still lose sessions and stay under the oracle that is told the regime
  (`bench/checks/exploits/README.md`). Three of Astra's market-making bots
  are rungs now.
- Tasks made by `mtx task new` still keep the generator from their agent.
  For those, sampling needs another route, such as a larger practice set.
  We have not decided it.
- A new exploit is answered in the family or the mandate, then frozen as a
  rung. It is not answered with a rule about what code a bot may contain.

## References

- [0.12.0 results](../../bench/results/0.12.0/release.json), trials
  `2026-11-15__22-23-49/*` of the results dataset.
- `bench/tasks/market-making-spcx/DESIGN.md`, "Generations".
- `crates/grader/src/taking.rs`, the rule's two layers.
