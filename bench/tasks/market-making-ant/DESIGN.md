# Design record: market-making-ant

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

From 0.15.0 the task is `market-making-ant` (it was `market-making-spcx`):
the owner moved it to ANT, the Phoenix market with real flow, on a new
snapshot (`snapshots/phoenix-ant-2026-11-25`, ANT at $79.99, a $0.01 tick,
1.25 bp). The family (`market-making-ant`, `crates/grader/src/making/measured.rs`)
is built from a **market profile**: a market's taker flow in US regular
hours, measured on luthor's Phoenix tape (`phoenix-tape.20261125.fc443f1e`:
543,438 rows, 3.06 days from 2026-11-21 to 2026-11-25, every market;
`calibration/fitter/tape/study.py`). The profile and the snapshot market
are constructor arguments (`MeasuredFamily::new(profile, flow_mult,
market)`); `market-making-measured-spcx` draws SPCX's profile on SPCX's
snapshot, kept to compare.

| Number | ANT (the task) | SPCX (compared) | Source |
| --- | --- | --- | --- |
| Non-pinger orders a minute, regular hours | 11.6 | 0.33 | tape, `markets.<m>.flow.regular` |
| Median order, log-sd, p90 | $100, 1.522, $1,001 | $1,000, 1.315, $3,483 | same |
| Same side next | 0.56 | 0.47 | same (reported, not fitted) |
| Takers' 60 s markout, order-weighted | +4.43 bp (25,187 orders; $5-100 +5.4, $100-1k +3.4, $1k-10k +3.5, $10k+ +13.7) | +1.35 bp (469 orders; $1k-10k 95% interval -3.1 to 4.1) | tape, `markets.<m>.markouts` |
| What takers paid against the mid | 2.1 to 3.3 bp, 6.3 at $10k+ | not measurable (pinger mids) | same |
| Worst fill from the mid, p90 | 11.4 ($100-1k), 16.5 ($1k-10k) bp | 15.5 to 17.4 bp | tape, `markets.<m>.impact` |
| Touch half-spread | 2.7 bp, mean of 106 minutes (0.6 to 12) | 0.4 bp (one tick) | luthor `quotes` table, from 2026-11-25 ~01:40 UTC |
| Volatility | none measured: SPCX's used | 1.55 bp a second, session log-sd 0.54 | `calibration/market/spcx.20261112.d1f48be2.toml` |
| Jumps of 1.5%+ | 4 in 3 days | none | tape: 93 on 110 markets in 16 distinct hours; median 2.0%, p90 4.17% |
| Fees | maker 0.5 bp, taker 3.5 bp | same | snapshot `exchange.json` |

How the family turns them into a session:

- **Takers** are the actor library's: eight noise wallets and four informed
  wallets (`actors.noise`, `actors.informed`), `scale` filled by the
  generator from the profile (the rate, and the median order in base units
  at the snapshot's mark). An informed wallet sees the path 240 slots (60 s)
  ahead, trades while the move it sees is over 7 bp (twice the 3.5 bp
  taker fee), and is right 85% of the time (a wrong view lasts its minute).
  The informed share of orders is fitted on 200 calm sessions of the family
  so the average taker's one-minute markout is the profile's
  (`MeasuredFamily::fit_informed`): 0.416 for ANT (0.128 for SPCX), the
  informed wallets trading at 0.831 of the flow's pace while they see a move.
  A simulation of the library's rules on 2,000 sessions' calm ones gives
  4.40 bp for ANT (target 4.43) and 1.47 bp for SPCX (target 1.35)
  (`simulated_markouts_match_the_profile`). Informed orders are 1.5 times
  larger, so orders over twice the median are the more informed. Takers
  reach the profile's p90 worst fill (16.5 bp on ANT) through the price.
- **The arbitrageur** (`actors.arbitrageur`) looks after each step's move,
  before the bot: it takes any quote more than its edge from Phoenix's
  index, 3.5 to 5 bp (the taker fee and a little more), latency 1 to 2
  slots. luthor's paper makers lost -1 to -3 bp a fill on mainnet to
  exchanges leading Phoenix by 1 to 2 slots: this is that channel.
- **The competitor** is reactive (`competition.reactive`), at 0.75 to 1.3
  times ANT's measured touch (2.0 to 3.5 bp), 2 to 5 ANT, floor half its
  half-spread; Phoenix's makers widened to the same. (A touch under two
  ticks, as SPCX's, gets a thin grader's maker at one to two ticks with the
  snapshot's makers 3 to 8 bp behind.)
- **Quoting obligations** are reported, never scored: both sides within 10
  bp (or three half-spreads), a tenth of a median order, half the steps.
- **Mandate:** 35 ANT (about $2,800, 0.28x collateral, as SPCX's 20 was),
  $250 drawdown, taking only to reduce.

### Stated departures

| Departure | Measured | In the task | Why |
| --- | --- | --- | --- |
| Jumps | 0.0115 per market-hour (93 / 110 markets / 73 h), clustered; ANT 4 in 3 days | one jump in 12 of 72 sessions | stepping back must be tested; a sixth, not 0.14's half |
| Trends | not measured | 24 of 72 sessions drift 0.002-0.006% a step | inventory risk with informed flow leaning through it |
| Volatility | no ANT calibration | SPCX's 1.55 bp a second | the calibration pipeline has no ANT file yet |
| Informedness | unobserved per taker (the tape has no addresses) | noise plus informed wallets (hit rate 0.85, 7 bp edge) | fits the average and the size gradient's sign; a mix is what makes "who is informed" knowable |
| Library clock | the library counts 0.4 s a slot (`library::SLOT_SECONDS`) | rates handed over x0.625 | the bench's clock is 0.25 s a slot |
| Not modelled | transaction-layer races, price impact | none | impact is refused with a competitor and perp actors don't send through the transaction layer: new engine work |
| (SPCX, not the task) flow | 0.33 orders a minute | x35 in `market-making-measured-spcx` | a session would hold one order |

## The central decision

Where to quote and how much inventory to carry, against flow that is a
little informed on average, an arbitrageur that takes stale quotes, and a
competitor at the measured touch. The average taker gets back about what it
paid within a minute, so a maker earns only by turning inventory over fast,
quoting where it isn't picked off, and stepping out of the way of informed
flow.

## Anchors

- **Reference** (`reference/`, unchanged from 0.14.0): one tick inside the
  competing makers around the oracle, a linear skew against inventory, a
  lean with the drift, and a pause on the side one-sided fills hit. Never
  takes.
- **Informed bot** (`checks/bots/informed/`): the reference, told the
  latent state (`MTX_REGIME`: the informed and arbitrageur wallets, the
  informed share, horizon, edge and hit rate, the flow's pace, the
  arbitrageur's latency and edge). Each step it reads the informed wallets'
  trader accounts (one `getMultipleAccounts`) and takes each position change
  as a trade; a trade's side, weighted by what is left of its 240-slot
  horizon and by `2 h - 1`, sums into a signal in [-1, 1]. It leans its
  reservation price by 4 bp a unit of signal, skews toward holding half the
  cap the signal's way (built with quotes), pulls the side the informed hit
  past a signal of 0.3, and sheds inventory held against a signal of 0.8 or
  more (two wallets agreeing) with a reduce-only market order.
- Neither knows a path, a jump or when anyone trades.

## Detectability

Measured on this branch, never on the ANT snapshot: every grade here ran
before the task moved to ANT, on SPCX's book, and the strength band has
not been measured on the task as it ships. Sessions of 24 (12 calm, 8
trend, 4 jump), reference and informed bot on the same sessions, PnL in
USD:

| Set | Reference (sum, Sharpe a session) | Informed (sum, Sharpe) |
| --- | --- | --- |
| SPCX profile, informed takers without an edge (seed e) | +10.71, 0.23 | +9.23, 0.21 |
| SPCX profile, 7 bp edge (seed f) | -21.26, -0.26 | -5.25, -0.09 |
| ANT profile on SPCX's book, informed takers without an edge (seed e) | -31.25, -0.97 | -10.98, -0.54 |

Jump sessions decide the sums (a 3% jump through 3 to 4 units of
inventory costs $10 to $20, against a calm session's dollar or two). On
ANT's flow the reference loses in every regime, as luthor's paper makers
do on mainnet; the informed bot loses less. The band (reference at 0.2 to
0.7 of the informed bot, per-session Sharpe 0.25) is not met: see the open
questions in the branch's report.

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
| 0.14.0 | — | 0.15.0: renamed `market-making-ant`, on Phoenix's ANT; takers from the actor library at ANT's measured pace, sizes and informedness (RFD 0007); an arbitrageur; the reactive competitor at the measured touch; jumps in a sixth of the sessions; scoring v2 with an informed bot told the informed wallets; the flow oracle and two SPCX-only Astra exploit bots leave the ladder |
