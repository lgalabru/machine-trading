# Design record: market-making-cbrs

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

From 0.15.0 the task is `market-making-cbrs` (it was `market-making-spcx`;
an unreleased draft drew ANT). The owner moved it to CBRS, the Cerebras
perp, a thin, wide book where luthor's mainnet paper maker earns: 287 fills
in 24 h, after Phoenix's 0.5 bp maker fee, marked out +6.6 bp at 1 s, +7.1
at 10 s (95% interval 5.6 to 8.5) and +7.5 at 60 s, quoting inside the
spread under a strict fill model. The snapshot is
`snapshots/phoenix-cbrs-2026-11-25` (cross-margin, CBRS at $84.37, a $0.01
tick, 1.19 bp). The family (`market-making-cbrs`,
`crates/grader/src/making/measured.rs`) is built from a market profile;
`market-making-measured-ant` and `market-making-measured-spcx` draw ANT's
and SPCX's, kept to compare. The tape study is
`phoenix-tape.20261125.3cb49758`: the 543,438 rows of
`phoenix-tape.20261125.fc443f1e`, exported again from luthor's
`history.sqlite` and studied with `--markets CBRS,SPCX,ANT` (its SPCX and
ANT numbers equal the first study's).

| Number | CBRS (the task) | Source |
| --- | --- | --- |
| Orders | 96% the pinger's (about one $3 print a minute) | tape, `markets.CBRS` |
| Non-pinger orders a minute | 0.185 regular, 0.011 off hours, 0 weekend | tape, `flow` |
| Order size | median $298, p90 $308 (nearly one size: likely one bot), p99 $2,685, log-sd 1.295 | tape, `flow.regular` |
| Takers' markouts | 60 s: $5-100 -2.6 bp (47 orders), $100-1k -10.0 (109), $1k-10k -5.0 (12); order-weighted -7.6 at 60 s, -2.1 at 10 s: takers lose | tape, `markouts` |
| Worst fill from the mid, p90 | 28.9 ($5-100), 30.1 ($100-1k), 99.8 ($1k-10k, 12 orders) bp | tape, `impact` |
| Touch half-spread | 13.9 bp, mean of 4.8 hours (289 minute rollups, 3,658 reads), from 0.6 to 17.4 | luthor `quotes` table |
| Volatility | 3.44 bp a second from 60 s returns of the tape's mid (robust 2.05; the wide book's mid bounces between prints, so an upper bound); SPCX by the same method 1.59, ANT 4.44 | tape mid, regular hours |
| Jumps of 1.5%+ | 1 in 3 days (-1.71%, reverted) | tape `jumps` |
| Fees | maker 0.5 bp, taker 3.5 bp | snapshot `exchange.json` |

How the family turns them into a session:

- **Takers** are the actor library's: eight noise wallets at CBRS's
  measured sizes, at 30 times its measured pace (stated), and the pinger, one
  wallet printing $3 once a minute at its measured pace. The family fits an
  informed share and an index-chasing share to the profile's 10 s and 60 s
  markouts (`fit_flow`); CBRS's are negative (takers lose), which no
  mechanism that knows something can make, so both shares are 0 and every
  taker is noise. The tape's negative markout isn't reproduced: here the
  mid moves neither way after a taker's order.
- **The arbitrageur** takes any quote more than its edge (3.5 to 5 bp) from
  Phoenix's index; at a 1-slot latency it looks right after a move, before
  the bot's turn, at 2 slots after it (`after_turn`).
- **The book lags the index** 4 to 8 slots (1 to 2 s), as exchanges lead
  Phoenix (luthor RFD 0001); the competitor quotes around the index and
  rests post-only.
- **The competitor** is reactive, at 0.75 to 1.3 times the measured touch
  (10.4 to 18.1 bp), 2 to 5 CBRS; Phoenix's makers widened to the same.
- **Sessions** are 960 to 1,600 slots (4 to 6.7 minutes), twice the other
  perp tasks', for the thin flow; 40 calm, 28 trend, 4 jump of 72.
- **Quoting obligations** are reported, never scored: both sides within
  three competitor half-spreads, a tenth of a median order, half the steps.
- **Mandate:** 33 CBRS (about $2,800, 0.28x collateral), $250 drawdown,
  taking only to reduce.

### Stated departures

| Departure | Measured | In the task | Why |
| --- | --- | --- | --- |
| Flow multiplier | 0.185 non-pinger orders a minute in regular hours | x30: 5.5 a minute | a session would hold one order |
| Session length | (the other perp tasks: 2 to 3.3 minutes) | 4 to 6.7 minutes | the thin flow |
| Takers' markout | -7.6 bp at 60 s (takers lose) | 0 (noise) | the model's takers know nothing at worst |
| Order sizes | nearly one size ($298 to $308), likely one bot | lognormal around $298, log-sd 1.3 | the library draws lognormal sizes |
| Jumps | 1 in 3 days | one jump in 4 of 72 sessions | a small stress share |
| Volatility | 3.44 bp a second (an upper bound) | SPCX's calibrated walk scaled by 3.44 / 1.55 | CBRS has no calibration file |
| Book lead | Binance leads Phoenix's SOL ~0.5 s, Coinbase ~0.25 s | Phoenix's makers follow the index 4-8 slots later | luthor RFD 0001 |
| Library clock | the library counts 0.4 s a slot | rates handed over x0.625 | the bench's clock is 0.25 s a slot |
| Not modelled | transaction-layer races, price impact | none | engine work |

## The central decision

Where to quote and how much to show, inside a wide spread, against takers
who know nothing, an arbitrageur that takes quotes a move left stale, and a
competitor at the measured touch. The spread pays (luthor's mainnet maker:
+7 bp a fill); the costs are inventory through moves and the quotes left
behind when the index moves.

## Anchors

Both size from the facts the instruction states, not from the band.

- **Reference** (`reference/`): one tick inside the competing makers
  around the oracle (the index), a linear skew against inventory (2.5 bp a
  unit), a lean with the drift, a pause on a side after one-sided fills.
  - **Its inventory limit** is a quarter of the cap, 8.25 CBRS: the most a
    maker that can't tell this session's flow from an informed one holds
    one way (as since 0.13).
  - **Its quote size** keeps an average one-sided run of fills inside that
    limit. The tape says the next CBRS order is on the same side 61% of the
    time, so a run lasts `1 / (1 - 0.61)` = 2.6 orders on average, and the
    size is `0.25 × 33 / 2.6` = 3.2 CBRS (`quote_size`). At the stated
    3.44 bp a second over the 22 seconds a side's fill takes at 5.5 orders
    a minute, that inventory moves about 16 bp, against the 12 bp a fill
    earns inside a 13.9 bp touch after the 0.5 bp fee: a risk the skew
    sheds within a run.
  - It steps off one-sided runs on the tape only until its first 10 fills,
    marked out 10 s on, show they give back no more than half its
    half-spread (`markouts.rs`, `tape.rs`). Never takes.
- **Informed bot** (`checks/bots/informed/`): the reference, told the
  latent state (`MTX_REGIME`): the informed and index-chasing shares (both 0
  on CBRS), the competitor's half-spread, floor and reaction, the book's
  lead, the arbitrageur's latency and edge, the flow's pace. Told every
  taker is noise, it knows an inventory a run builds reverts as the flow
  does, so it holds half the cap (16.5 CBRS) and sizes to it by the same
  rule: `0.5 × 33 / 2.6` = 6.4 CBRS. It never dodges the tape. Where there
  are informed wallets (ANT's profile) it reads their trader accounts and
  leans with them.
- The naive and skew quoters (`bench/checks/ladder/rungs`) quote 1 unit.
- None of them knows a path, a jump or when anyone trades.

## Detectability

Measured on mtx-bench-2 (Linux), CBRS snapshot, `mtx grade --draw
market-making-cbrs`, two sets of 24 sessions (13 calm, 9 trend, 2 jump
each), PnL in USD, Sharpe a session:

| Bot | Set 1 | Set 2 | 48 sessions |
| --- | --- | --- | --- |
| nop | 0 | 0 | 0 |
| naive quoter (1 unit) | +72.75 (0.76) | +31.31 (0.36) | +104.06 |
| skew quoter (1 unit) | +56.70 (1.14) | +42.46 (1.43) | +99.16 |
| reference (3.2 units, a quarter of the cap) | +79.88 (1.16) | +78.70 (1.58) | +158.58 |
| informed (6.4 units, half the cap, before the look rule) | +90.57 (0.78) | +105.51 (2.70) | +196.08 |
| informed, playing the competitor's look rule (shipped) | +99.09 (0.96) | +108.66 (1.69) | +207.75 |
| informed, and skewing twice as steep (withdrawn) | +33.40 | +32.26 | +65.66 |
| informed at three quarters of the cap (9.5 units) | +106.34 | +77.61 | +183.95 |
| informed at the whole cap (12.7 units) | +87.16 | +70.50 | +157.66 |

The reference leads the quoters (+158.58 against +104.06 and +99.16), with
a Sharpe of 1.2 to 1.6. Told the competitor's look rule, the informed bot
quotes a tick inside it for one step in each look period: +207.75, 2.00
times the best non-reading rung (the gate asks 2; it is on the line). The
band is still missed, and the market is left as measured: the reference
makes 0.76 of the informed bot (the band is 0.2 to 0.7). A steeper skew
for the jump stress cost far more spread than it saved. More size doesn't buy the informed bot more: past half the
cap the jump sessions and the walk take back what the spread adds (at the
whole cap it makes less than the reference). Knowing that every taker is
noise is worth about a quarter more PnL on CBRS, not double: what the
reference doesn't know costs it little where nobody informed trades.

Practice is drawn by the date rule the other 0.14 and 0.15 tasks follow
(the snapshot's date: seed 20261125) and calibrated with both anchors
(`mtx calibrate --practice market-making-cbrs --practice-seed 20261125`),
recalibrated when the anchors' sizing changed: reference +24.76, informed
+11.43 over the six. The reference is ahead, so v2 can't scale practice on
this set: one more sign the informed bot isn't dominant enough on CBRS. Seed history: seed
15 (an arbitrary first pick) had the reference ahead (+17.93 against
+12.71), which v2 refuses to scale; seed 17 was then picked for its
outcome, which practice must not be, and is withdrawn.

Tried and withdrawn: anchors that follow the touch (each side steps a tick
inside any rival, down to a 3 bp floor; the informed bot told the
competitor's floor and starting a tick inside it), with a 10 CBRS cap and
the robust 2.05 bp a second. The war for the touch took the spread with it:
over 48 sessions the reference made +6.20 (Sharpe 0.25, 0.04) and the
informed bot lost 65.60, the spread earned falling to about 3 bp a unit.
On CBRS's book the edge is the width; a maker that races the competitor to
its floor gives it away.

## Fairness

- Every correct path runs through standard RPC and `mtx-kit`: the
  reference does.
- The instruction states scoring v2 (its rules written by `mtx task sync`),
  the valuation at the scenario's oracle (`Trader::oracle_usd` reads it),
  every measured number with its source, and every departure.
- The informed bot reads only what anyone reading the chain could, once
  told whose wallets to watch: the informed takers' trader accounts.
- **Penalty twins:** as in flash-crash: at the cap passes, over it zeroes; a
  send outside the turn is refused, not a violation. A take that would open,
  add to or flip the position is refused before it is sent and the bot is
  told: no penalty. Only a taker fill the grader couldn't refuse beforehand
  is a breach.
- A session where more than a tenth of the library actors' actions fail
  fails the grade as infrastructure: the bot didn't see the flow its
  scenario describes.

## Failure modes, priced

| Failure mode | How it's charged |
| --- | --- |
| run over by informed flow | the minute's move × the inventory it took |
| a quote left stale by a move | the arbitrageur takes it at the stale price |
| warehouses inventory in a trend or a jump | the move × the inventory |
| breach | v2's per-session floor and the breach-rate penalty |

## Predictions

On the measured market a maker earns little: Phoenix's ANT takers get back
about what they paid within a minute, and luthor's paper makers lost on
mainnet. Doing nothing scores 0 and a careless quoter scores below it; the
reference loses a little or earns a little; frontier models land near the
reference. Rewards fall from 0.14.0's and don't compare with them.

## Generations

| Version | What agents did | What changed |
| --- | --- | --- |
| 0.7.0 | quoted a tick inside static makers; traded the informed bursts: reward 1.0, 9–11× the reference, every session won | — (new task) |
| 0.8.0 | — | the grader's maker; per-order taker limits; ~60% of jumps announced; fake-outs; informed sizes closer to noise; bound = spread + hindsight |
| 0.8.0 → 0.9.0 | on 0.8.0's flow a naive quoter beat the reference on fresh sets; the reference took its own unwinding for bursts | a lockstep world (the grader's maker no longer races the bot: grades replay); informed flow that bites; 72 hidden sessions drawn per batch from a secret; the reference graded alongside; `skill`; the ladder, with a flow oracle as the exploit bots' ceiling; a reference that counts only fills that grow its inventory |
| 0.10.0 | informed orders still ran 2–4× noise sizes and limits, so each stood out as it traded: Opus took the cap with every burst (14–20×), Fable too but chased fake-outs (1.3–12×) | 0.11.0: informed orders drawn exactly like noise (size, limit, account), only their side and timing differ; fake-outs drawn exactly like bursts, 0–2 a session; renamed `market-making-spcx`; the 0.10.0 bots join the exploit checks |
| 0.12.0 | every Astra bot lost on its quotes ($77 to $351 over 72 sessions) and made $1,794 to $2,615 buying 16 to 19 SPCX ahead of each jump, on bursts read from the trade ring, from `getBlock`, or from the takers' own accounts: 16–25× the reference. Within the rules, and not market making | 0.13.0: the mandate allows taking only to reduce ([ADR 0003](../../../docs/adr/0003-free-strategies-bounded-by-the-mandate.md)); a reference that never takes and stops quoting the side a burst hit; a flow oracle that leans with a quote; three Astra bots join the exploit checks |
| 0.14.0 | — | 0.15.0: renamed `market-making-cbrs`, on Phoenix's ANT; takers from the actor library at ANT's measured pace, sizes and informedness (RFD 0007); an arbitrageur; the reactive competitor at the measured touch; jumps in 4 of 72 sessions; scoring v2 with an informed bot told the informed wallets; the flow oracle and two SPCX-only Astra exploit bots leave the ladder |
