# Design record: rfq-client-tiering-spcx

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it prices each client to its reach and declines the clients whose requests turn out to be informed, learning who they are from a handful of requests per client.

- **Wins:** quoting benign clients just inside their reach, flagging informed clients early from markouts and from who takes a quote no uninformed client would, and reading the flagged clients' sides as the coming move
- **Loses:** quoting everyone pays every informed fill; quoting wider loses the benign flow entirely past the reach; tiering by size or leaning against recent request flow catches little of the informed flow
- **Regimes:** 10 to 16 clients, 15% to 45% of them informed (which ones redrawn every session), seeing 30 to 90 slots ahead; reach 0.2 to 0.5 bp per trade size, times the square root of the size
- **Mandate:** 0.001 SPCX position cap, $400 loss limit, 72 hidden sessions per grade.

## Expert research

<!-- TODO: why this is a real expert task, the expert's method, where non-experts go wrong, with sources. -->

## The central decision

<!-- TODO: the one decision the task is about, and the options that look defensible. -->

## Variants

<!-- TODO: each regime and the best response to it; a table of which habit fails where. -->

## Levers

<!-- TODO: what the market does, what a good bot does, and how an honest operator detects each lever. -->

## No tells

<!-- TODO: the audit: nothing in the path's shape gives away what a trader couldn't see. -->

## Detectability

<!-- TODO: how early a good bot can tell the regime, measured on generated sets. -->

## Fairness

<!-- TODO: what the agent is told, what it can learn from practice, and that the reference stays inside the mandate. -->

## Failure modes, priced

<!-- TODO: each way a bot can lose, and what it costs. -->

## Predictions

<!-- TODO: what you expect each model tier to score, before running them. -->

## Gate 0

Prototype only: the reference, the informed bot and the careful rungs as
components (RFQs reach components only; the kit's process run loop sees
none), graded with `mtx grade --draw rfq-tiering` on fresh 72-session sets,
each from a secret drawn on the box and never printed.
`hidden/gate0/engine.py` reads the reports; `hidden/gate0/model.py` is the
fast model the family was tuned on (`gate0-seed-*.txt`).

- **The informed bot** is told the roster (`wallets.rfq-informed`: the
  informed clients' ids, an identity fact) and the clients' process
  (`lead_slots`, `correlation`, `tolerance_bps`), all in `LATENT_FIELDS`.
  Not the path, when anyone asks, or which way. It declines the informed
  clients, quotes the rest just inside their reach, and for one lead after
  an informed request declines uninformed requests on that request's side.
- **The careful rungs** are told the tolerance (so they know the reach
  exactly) and not who is who: quote-all, quote-wide (3 bp past the reach),
  fixed-width (1.5 bp), size-tier (only requests of 20 trade sizes or more),
  side-skew (decline the side the last 80 slots' requests lean to by 0.1).
  Every rung except side-skew reads nothing of the session; side-skew reads
  the request flow, not who sent it.

Each set is 72 fresh sessions; "best careful" counts side-skew, "best blind"
leaves it out. The order is nop, quote-all and quote-wide, and every careful
rung below the reference, below the informed bot, on 2,000 bootstrapped
72-session sets. The reference's share weighs losses 1.5 times, as v2 does.

| Set | Informed / best careful | Informed / best blind | Order holds | Reference, weighted share of informed | Informed / reference | Reference per-session Sharpe |
| --- | --- | --- | --- | --- | --- | --- |
| v0 set 1 | 8.63 (side-skew) | 17.1 (size-tier) | 0.998 | 0.46 | 2.03 | 0.66 |
| v0 set 2 | 2.63 (side-skew) | 3.98 (quote-all) | **0.517** | 0.35 | 2.55 | 0.59 |
| v1 set 1 | 3.71 (side-skew) | 9.88 (quote-all) | 1.000 | 0.62 | 1.61 | 1.01 |
| v1 set 2 | 2.09 (size-tier) | 2.09 (size-tier) | 0.875 | 0.63 | 1.53 | 0.79 |
| v1 set 3 | 2.91 (side-skew) | 3.70 (quote-all) | 0.961 | 0.51 | 1.87 | 0.80 |
| v1 set 4 | 2.76 (side-skew) | 3.79 (quote-all) | 0.925 | 0.45 | 2.04 | 0.63 |
| v1 pooled (288) | 2.80 (side-skew) | 3.81 (quote-all) | 0.984 | 0.56 | 1.73 | 0.80 |

- **v0** (two probes per client, flag at odds 0.5) failed the order on its
  second set: side-skew came within 3% of the reference. Its reference sat
  at 0.35 to 0.46 of the informed bot.
- **v1**, the one rework: one probe per client (most informed clients take
  the first, and every probe costs a benign client its request), and a
  markout flag at odds 0.7 (in the model, flagging at 0.5 declined more
  benign clients on noise than it caught informed ones).
  It passes on all four fresh sets: the informed bot makes at least 2x the
  best careful rung, and the order holds on at least 0.875 of bootstraps.
  The reference sits at 0.45 to 0.63 of the informed bot: in the 0.2 to 0.7
  band, above the card's 0.5 target, so the informed bot is 1.5 to 2.0
  times the reference.
- **Thin spots.** The informed bot's margin over the best careful rung is
  2.09 on v1 set 2 (its bootstrap clears 2x in only 41% of resamples), a
  set where few sessions had many informed clients and quoting everyone
  paid. Side-skew, which reads the flow but not who sent it, is the rung to
  watch: 0.16 to 0.41 of the informed bot.

The fast model (`gate0-seed-51..53`, 1,000 sessions each) agrees: 3.0 to 3.4x
the best careful rung, order 0.99, the reference at 0.54 to 0.58.

## Departures from the card

| Card | Built | Why |
| --- | --- | --- |
| `tolerance_bps` 0.5 to 1.5 | 0.2 to 0.5 | at 0.5 to 1.5 the reach on a 20-trade-size request (2 to 7 bp) is as large as what an informed client sees over the lead on SPCX's calibrated volatility (about 5 to 8 bp), so an informed fill costs the desk little and quoting everyone at the reach earned about as much as the informed bot in the model |
| Sizes 1 to 200 trade sizes | lognormal, median 20, log-sd 0.5 (about 9 to 45) | with two decades of sizes the reach (which grows with the square root) outruns the markout (which doesn't) on the large requests, and a rung quoting only large requests earned most of the informed bot |
| `lead_slots` 40 to 120 | 30 to 90 | with a longer lead the informed clients' requests cluster on one side long enough for the side-skew rung, which reads only the flow, to come within 1.2 to 1.9x of the informed bot in the model |
| 0 to 2 jumps of 1 to 3% | SPCX's calibrated jumps (0.67 an hour, median 0.52%) | the card's jumps come about 13 times as often as the calibration's and are 2 to 6 times as large; one informed request ahead of a 2% jump outweighed a session of reach, and inventory riding through them swamped every rung's edge |
| Desk inventory within about 40 SPCX | every prototype keeps it within 1 SPCX of the start | unhedged inventory rides at the index; at 2 SPCX the inventory's ride was most of a session's variance and the ladder's order held less often (the mandate's own bound on desk inventory is not checked yet) |
| Perp position 0 | a 0.001 SPCX cap | the mandate refuses a zero cap |
| The reference: markouts on fills, shrunk to a prior | markouts on every request, plus one wide-quote probe per client and its flagged clients' flow | markouts alone, on 3 to 10 requests a client, earned 0 to 0.2 of the informed bot in the model. A quote just past the widest reach is taken by no uninformed client and by most informed ones: a client's hit on a wide quote is the desk's sharpest read of who it is |

## Generations

- **v0 (Gate 0 prototype).** The family above, the prototype bots, and
  Gate 0 on the engine (see "Gate 0").
