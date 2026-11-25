# Design record: market-making-spcx

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

## The market, measured (0.15.0)

From 0.15.0 the family (`market-making-measured`, `crates/grader/src/making/measured.rs`)
is built from a **market profile**: a real market's taker flow in US
regular hours, measured on luthor's Phoenix tape (`phoenix-tape.20261125.fc443f1e`:
543,438 rows, 3.06 days from 2026-11-21 to 2026-11-25, every market;
`calibration/fitter/tape/study.py`). The profile is a constructor
argument (`MeasuredFamily::new(MarketProfile, flow_mult)`): the task draws
SPCX's; `market-making-measured-ant` draws ANT's on the same book, a
one-line change if the task moves (RFD 0007's open question).

| Number | SPCX | ANT | Source |
| --- | --- | --- | --- |
| Non-pinger orders a minute, regular hours | 0.33 | 11.6 | tape, `markets.<m>.flow.regular` |
| Median order, log-sd, p90 | $1,000, 1.315, $3,483 | $100, 1.522, $1,001 | same |
| Same side next | 0.47 | 0.56 | same (reported, not fitted) |
| Takers' 60 s markout, order-weighted | +1.35 bp (469 orders; the $1k-10k bucket's 95% interval is -3.1 to 4.1) | +4.43 bp (25,187 orders; $10k+ +13.7) | tape, `markets.<m>.markouts` |
| What takers paid against the mid | not measurable (pinger mids: -2.1 bp at $1k-10k) | 2.1 to 3.3 bp, 6.3 at $10k+ | same |
| Worst fill from the mid, p90 ($100-10k) | 15.5 to 17.4 bp | 11.4 to 16.5 bp | tape, `markets.<m>.impact` |
| Touch half-spread | 0.4 bp (one tick, 125.64 by 125.65) | 2.7 bp | luthor `quotes`, 1 minute of top-of-book reads, 2026-11-25 ~01:40 UTC |
| Volatility | 1.55 bp a second, session log-sd 0.54 | (SPCX's) | `calibration/market/spcx.20261112.d1f48be2.toml`, `fair_value` |
| Jumps of 1.5%+ | none in 3 days | 4 | tape: 93 on 110 markets in 16 distinct hours; median 2.0%, p90 4.17% |
| Fees | maker 0.5 bp, taker 3.5 bp | | snapshot `exchange.json`, calibration `venues.phoenix_perp` |

How the family turns them into a session:

- **Takers** are the actor library's: eight noise wallets and four
  informed wallets (`actors.noise`, `actors.informed`), `scale` filled by
  the generator from the profile: the rate, and the median order in SPCX at
  the snapshot's $143.05. Informed wallets see the path 240 slots (60 s)
  ahead and are right 85% of the time (a wrong view lasts its minute);
  their share of the flow is fitted so the average taker's one-minute
  markout is the profile's: `share = markout / ((2 * 0.85 - 1) * 0.8 *
  12 bp)`, 0.2 for SPCX and 0.656 for ANT. A simulation of the library's
  rules on calm sessions gives 1.19 bp (SPCX, target 1.35) and 4.01 bp (ANT,
  target 4.43) (`simulated_markouts_match_the_profile`). Informed orders
  are 1.5 times larger, so big orders are the more informed, as on ANT.
  Takers reach the profile's p90 worst fill through the price.
- **The arbitrageur** (`actors.arbitrageur`) looks after each step's move,
  before the bot: it takes any quote more than its edge from Phoenix's
  index, 3.5 to 5 bp (the taker fee and a little more), latency 1 to 2
  slots. luthor's paper makers lost -1 to -3 bp a fill on mainnet to
  exchanges leading Phoenix by 1 to 2 slots: this is that channel.
- **The competitor** is reactive (`competition.reactive`). SPCX's touch was
  one tick: the grader's maker quotes one to two ticks (0.7 to 1.4 bp),
  0.5 to 2 SPCX, with the snapshot's makers widened to 3 to 8 bp behind it
  (`makers_bps`: SPCX's worst fills, median 0.5 bp and p90 15.5 bp, say
  thin size at a tight touch with depth behind). ANT's 2.7 bp touch: the
  grader's maker at 0.75 to 1.3 times it, 2 to 5 SPCX, the makers there too.
- **Quoting obligations** are reported, never scored: both sides within 10
  bp, at a tenth of a median order, half the steps.

### Stated departures

| Departure | Measured | In the task | Why |
| --- | --- | --- | --- |
| Flow multiplier | SPCX 0.33 orders a minute | x35: 11.7 a minute | a 2-3.3 minute session would hold one order; x35 is ANT's pace. Longer sessions instead would multiply the verifier's hour |
| Jumps | 0.0115 per market-hour (93 / 110 markets / 73 h), clustered; none on SPCX | one jump in 12 of 72 sessions | stepping back must be tested; a sixth, not 0.13's half |
| Trends | not measured | 24 of 72 sessions drift 0.002-0.006% a step | inventory risk with informed flow leaning through it |
| Touch | 0.4 bp, half of a one-tick spread | 0.7-1.4 bp, one or two ticks from the level | the level sits on a tick; one minute of quotes |
| Informedness | a mix, unobserved (the tape has no taker addresses) | noise plus informed wallets at hit rate 0.85 | fits the average; a mix is what makes "who is informed" knowable |
| Library clock | the library counts 0.4 s a slot (`library::SLOT_SECONDS`) | rates handed over x0.625 | the bench's clock is 0.25 s a slot |
| Not modelled | transaction-layer races, price impact | none | impact is refused with a competitor and perp actors don't send through the transaction layer: new engine work |

## The central decision

Where to quote, and when to step back, against flow that is mostly noise
and sometimes knows the next jump.

| Option | Why it looks defensible | Why it isn't |
| --- | --- | --- |
| A tick inside, symmetric | most fills | informed bursts run it over before jumps |
| Wide | safe | nothing fills, and the competitor takes the flow |
| Join the competitor | queue priority | its price moves when it undercuts |
| Trade with every burst | informed flow predicts jumps | the mandate allows taking only to reduce (0.13.0): a burst can be leaned into with a quote, not bought |

**The calculation chain:** read the oracle and the book each turn → place
the competitor's quotes and stay just inside them → skew by inventory → read
the tape (its own fills, the trade ring, `getBlock`) for one-sided flow →
widen, shrink or pull a side when it turns → shed inventory with a take if it's worth the spread → requote when the quotes would move.

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
| taking to open | refused: the mandate allows taking only to reduce |

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

- Informed, fake-out and noise orders draw their size, limit and account
  from the same distributions, so no single order says which it is: only a
  burst's one-sided volume and timing do, and fake-outs share those.
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

- Every correct path runs through standard RPC and `mtx-kit`: the
  reference does.
- The instruction states the bound (spread plus hindsight), the valuation at
  the scenario's oracle (`Trader::oracle_usd` reads it), and the flow's
  ranges and odds.
- **Penalty twins:** as in flash-crash: at the cap passes, over it zeroes; a
  send outside the turn is refused, not a violation. A take that would open,
  add to or flip the position is refused before it is sent and the bot is
  told: no penalty. Only a taker fill the grader couldn't refuse beforehand
  is a breach.
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
| 0.10.0 | informed orders still ran 2–4× noise sizes and limits, so each stood out as it traded: Opus took the cap with every burst (14–20×), Fable too but chased fake-outs (1.3–12×) | 0.11.0: informed orders drawn exactly like noise (size, limit, account), only their side and timing differ; fake-outs drawn exactly like bursts, 0–2 a session; renamed `market-making-spcx`; the 0.10.0 bots join the exploit checks |
| 0.12.0 | every Astra bot lost on its quotes ($77 to $351 over 72 sessions) and made $1,794 to $2,615 buying 16 to 19 SPCX ahead of each jump, on bursts read from the trade ring, from `getBlock`, or from the takers' own accounts: 16–25× the reference. Within the rules, and not market making | 0.13.0: the mandate allows taking only to reduce ([ADR 0003](../../../docs/adr/0003-free-strategies-bounded-by-the-mandate.md)); a reference that never takes and stops quoting the side a burst hit; a flow oracle that leans with a quote; three Astra bots join the exploit checks |
