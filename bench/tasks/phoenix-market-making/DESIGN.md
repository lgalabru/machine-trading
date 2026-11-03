# Design record: phoenix-market-making

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Expert research

- **Why it's a real expert task.** Market makers earn the spread from
  uninformed flow and lose to informed flow; the business is telling them
  apart fast enough. Glosten and Milgrom (1985) on adverse selection, Ho and
  Stoll (1981) and Avellaneda and Stoikov (2008) on inventory, Cartea,
  Jaimungal and Penalva, *Algorithmic and High-Frequency Trading* (2015).
- **The expert's method.** Quote around a fair value (here, the oracle),
  skew against inventory, widen or step back when flow turns one-sided, lean
  with a drift, and size so a run of fills can't break the position cap or
  the loss limit.
- **Where non-experts go wrong.** Quoting tight and symmetric (fills the
  most, loses it to jumps and trends), warehousing inventory, ignoring the
  tape, quoting around Phoenix's mark (which a maker's own quotes pull), and
  quoting so wide nothing fills.

## The central decision

Where to quote, and when to step back, against flow that is mostly noise
and sometimes knows the next jump.

| Option | Why it looks defensible | Why it isn't |
| --- | --- | --- |
| A tick inside, symmetric | most fills | informed bursts run it over before jumps |
| Wide | safe | nothing fills, and the competitor takes the flow |
| Join the competitor | queue priority | its price moves when it undercuts |
| Trade with every burst | informed flow predicts jumps | fake-outs, and 40% of jumps come unannounced |

**The calculation chain:** read the oracle and the book each turn → place
the competitor's quotes and stay just inside them → skew by inventory → read
the tape (its own fills, the trade ring, `getBlock`) for one-sided flow →
widen or shrink when it turns → requote when the quotes would move.

**Nothing warns the agent.** A maker that never steps back still fills; the
jumps take it back in the valuation.

## Variants

| Regime | Best response |
| --- | --- |
| calm (~25%) | quote tight, skew, collect |
| jumps (~50%) | step back on one-sided bursts; keep inventory small |
| trend (~25%) | lean with the drift; skew harder |

| Habit | Fails in |
| --- | --- |
| tight and symmetric | jumps (adverse selection), trends (inventory) |
| wide | calm (no fills) |
| trading every burst | fake-outs, unannounced jumps |

## Levers

| Lever | What the market does | What a good bot does | How an honest trader sees it |
| --- | --- | --- | --- |
| the grader's maker | undercuts a better quote by a tick, down to a 2–6 bp floor, eases back out | prices against its floor, doesn't race it to zero | its quotes in the book every turn |
| taker limits | each order fills only within its own limit | quotes inside the limits most orders carry | its own fill rate by quote distance |
| informed bursts (~60% of jumps) | 6–12 orders in the jump's way, within 24 slots ahead | widens or steps back on one-sided flow | the tape: its fills, the trade ring, `getBlock` |
| fake-outs (0–3 a session) | bursts with no jump | doesn't bet the house on a burst | — |
| trends | drift with leaning flow | leans and skews | the oracle's drift |

**Dropped:** static makers and price-blind takers (0.7.0: a tick inside
collected most of the spread, unopposed); a spread-only bound (0.7.0: Opus
traded the bursts for PnL the bound didn't count, and scored past it).

## No tells

- Informed, fake-out and noise orders share one account pool and
  overlapping sizes, so the account or size doesn't say which is which.
- Fake-outs happen in every regime, calm included.
- The grader's maker is onboarded and quotes in every session.

## Detectability

Not yet measured as separations in standard deviations; the ladder stands
in. The flow oracle, told which taker orders are informed (each once it has
traded), made 14–30× the reference on two 72-session sets: what reading the
tape perfectly is worth. The reference's lead over the naive and skewed
quoters is what reading its own fills is worth; it holds in 90% of
resamples at 72 sessions, against 81% at 48.

## Fairness

- Every correct path runs through standard RPC and `mth-botkit`: the
  reference does.
- The instruction states the bound (spread plus hindsight), the valuation at
  the scenario's oracle (`Trader::oracle_usd` reads it), and the flow's
  ranges and odds.
- **Penalty twins:** as in flash-crash: at the cap passes, over it zeroes; a
  send outside the turn is refused, not a violation.
- A session where more than a tenth of the takers' orders fail is voided,
  not scored: the bot didn't see the flow its scenario describes.

## Failure modes, priced

| Failure mode | How it's charged |
| --- | --- |
| run over by informed flow | the jump × the inventory it took |
| warehouses inventory in a trend | the drift × the inventory |
| quotes outside the takers' limits | no fills |
| breach | zero for the whole grade |

## Predictions

Simple quoting roughly breaks even against the grader's maker; the
reference earns a little. Frontier models: most near zero, the best a few
times the reference. The frozen 0.7.0 exploit bots stay profitable but lose
sessions.

## Generations

| Version | What agents did | What changed |
| --- | --- | --- |
| 0.7.0 | quoted a tick inside static makers; traded the informed bursts: reward 1.0, 9–11× the reference, every session won | — (new task) |
| 0.8.0 | — | the grader's maker; per-order taker limits; ~60% of jumps announced; fake-outs; informed sizes closer to noise; bound = spread + hindsight |
| 0.8.0 → 0.9.0 | on 0.8.0's flow a naive quoter beat the reference on fresh sets; the reference took its own unwinding for bursts | a lockstep world (the grader's maker no longer races the bot: grades replay); informed flow that bites; 72 hidden sessions drawn per batch from a secret; the reference graded alongside; `skill`; the ladder, with a flow oracle as the exploit bots' ceiling; a reference that counts only fills that grow its inventory |
