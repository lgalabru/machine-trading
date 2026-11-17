# Design record: rfq-client-tiering-spcx

**Status: RETIRED (2026-11-17).** With main's realistic `hit_rate` of 0.85 (#114), informed RFQ clients
are wrong on 15% of their requests, which legitimately shrinks what knowing who they are is worth. The
side-skew rung reads the session's net request flow without identity: it is exactly the cheaper skill the
card must beat, and on two fresh sets the informed bot made only 1.64x and 1.65x of it, with the ladder's
order holding on 0.511 and 0.307 of bootstraps. The diagnosis (no engine bug: #113 has no effect here, and
#114 at a hit rate of 1.0 only redraws the requests) is in "Gate 0".

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it prices each client to its reach and declines the clients whose requests turn out to be informed, learning who they are from a handful of requests per client.

- **Wins:** quoting benign clients just inside their reach, flagging informed clients early from markouts and from who takes a quote no uninformed client would, and reading the flagged clients' sides as the coming move
- **Loses:** quoting everyone pays every informed fill; quoting wider loses the benign flow entirely past the reach; tiering by size or leaning against recent request flow catches little of the informed flow
- **Regimes:** 10 to 16 clients, 15% to 45% of them informed (which ones redrawn every session), seeing 45 to 120 slots ahead; reach 0.8 to 1.6 bp a lot of 0.25 SPCX, times the square root of the size in lots
- **Mandate:** answers to requests only (perp cap 0.001 SPCX), $400 loss limit on the desk's value over the idle desk, 72 hidden sessions per grade.

## Expert research

Dealers in request-for-quote markets price each client by its toxicity:
the markout of its past trades, the hit ratio on its requests, and how
those move after the fill (Glosten and Milgrom 1985 on the bid-ask spread
as protection against informed counterparties; Bessembinder, Spatt and
Venkataraman, "A Survey of the Microstructure of Fixed-Income Markets",
JFQA 2020, on dealer RFQ markets; client tiering and "last look" markouts
in FX, Oomen, "Last look", Quantitative Finance 2017). The expert's method:

- **Price to the client's alternative, not to the screen.** A client trades
  with the desk while the desk's price beats what it would pay elsewhere;
  past that it walks. Widening everyone "for safety" loses the benign flow
  outright.
- **Score every counterparty from its own record,** shrunk to a prior,
  because each client has only a handful of trades a session, and decide
  early on thin evidence.
- **Read who hits a wide price.** A client that takes a price no
  liquidity-seeking client would take is telling the desk it knows
  something (the dealer's winner's curse).

The non-expert quotes everyone the same, or leans on the aggregate flow,
and pays every informed fill.

## The central decision

Each request: quote it at the client's reach, quote it wider (a probe
that only an informed client takes), quote part of it, or let it lapse;
and, for the client, keep or stop answering. Quoting everyone pays every
informed fill; declining everyone earns nothing; widening loses the benign
flow past the reach and still pays the informed clients on their big views.

## Variants

| Session | Best response | Quote all | Quote wide | Size-tier | Side-skew |
| --- | --- | --- | --- | --- | --- |
| Few informed clients (15%) | quote nearly everyone, find the few | pays the few | loses all benign flow | gives up the small benign requests | declines benign flow on noise |
| Many informed clients (45%) | flag them early, read their side | pays a third of its fills to them | the informed still take their big views | as quote-all on large requests | catches some, declines much benign flow |
| Long lead, big moves | flag early: each informed fill costs more | loses the most | loses on every big view | as quote-all | best of the careful rungs |

## Levers

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| Informed clients | 15% to 45% of clients see the level 45 to 120 slots ahead and ask its way | declines them | request markouts (the index after each request, signed by its side), and who takes a price past the widest reach |
| The reach | 0.8 to 1.6 bp a lot, times sqrt(lots), one per session | quotes just inside it | its own answers: a declined price was past the reach |
| The index's offset | the index is the level in cents, about 0.5 bp off it | keeps about 1 bp of margin | declines on quotes near the reach, and practice |
| Informed flow | an informed request's side is the coming move's | declines benign requests on that side for a lead | the flagged clients' requests |

## No tells

- Every client is seated and funded in every session; informed and benign
  clients ask on one Poisson schedule, pick sizes from one distribution and
  have one deadline (`rfq.rs`: one `send_requests` path for both).
- Ids are `c<n>-<k>` with `k` the session's request count: nothing about the
  client's role. Which clients are informed is the session's own draw.
- The path is the calibrated model's, with no move laid for the informed;
  practice files carry a neutral description and no roster (#109).
- Known leak, practice only: a practice report's actor tallies count the
  session's informed fills. Hidden sessions report nothing to the bot.

## Detectability

In the Gate 0 model, one probe a client flags most informed clients at
their first request; markouts need several requests (3 to 10 a client a
session), so a markout-only desk earned 0 to 0.2 of the informed bot and
the probing reference 0.45 to 0.55.

## Fairness

- The instruction states every rule the clients follow: the schedule, the
  sizes, the reach formula and its range, what informed clients know, the
  index's rounding and offset, the settlement, and that only components see
  requests.
- The margin under the reach is derivable: the instruction says the index
  is the level in cents and about half a bp off it, and a bot sees every
  declined answer. The informed bot's PnL moves less than 10% for margins
  from 0.5 to 1.4 bp in the model (`index_noise_bp` 0.45): not a precision
  game.
- Practice: six sessions with both anchors' PnL.
- The reference stays inside the mandate (no perp trade; its desk moves at
  most 16 SPCX).

## Failure modes, priced

From the Gate 0 model, per session, share of the informed bot's v2-weighted
PnL (`gate0-seed-81..84`): quote-all −0.6 to −1.1, quote-wide −0.9 to −1.1,
fixed width −0.5 to −0.9, size-tier −0.3 to 0, side-skew −0.2 to 0, the
markout-only desk −0.4 to −0.9, the reference 0.49 to 0.53.

## Predictions

Frontier agents find the reach quickly (practice shows declines) and most
will quote everyone at it, near 0. Those that keep per-client markouts
reach 0.2 to 0.4; few will think of probing with a wide price, the
reference's sharpest read.

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
  fixed-width (2 bp), size-tier (only requests of 5 lots or more),
  side-skew (decline the side the last 80 slots' requests lean to by 0.1).
  Every rung except side-skew reads nothing of the session; side-skew reads
  the request flow, not who sent it. (v0 and v1 used 1.5 bp and 20 trade
  sizes, the model's picks for their family.)

### v2, as built (reach 0.8 to 1.6 bp a lot)

| Set | Informed / best careful | Informed / best blind | Order holds | Reference, weighted share of informed | Informed / reference | Reference per-session Sharpe |
| --- | --- | --- | --- | --- | --- | --- |
| v2 set 1 | 6.44 (side-skew) | none positive | 0.972 | 0.43 | 2.22 | 0.46 |
| v2 set 2 | 4.42 (quote-all) | 4.42 (quote-all) | 0.950 | 0.51 | 1.89 | 0.59 |
| v2 set 3 | 4.23 (side-skew) | 29.2 (size-tier) | 0.988 | 0.65 | 1.60 | 0.67 |
| v2 pooled (216) | 5.15 (side-skew) | 29.3 (size-tier) | 0.984 | 0.53 | 1.88 | 0.56 |

The ladder (`bench/checks/ladder/ladder.py` on the same three sets) holds
every rule on 3 of 3 sets, bootstrap odds 0.994, 0.980 and 0.998 for
naive < reference, careful < reference and reference < informed. Side-skew
sits at 0.35 to 0.38 of the reference (0.01 to 0.15 of the informed bot,
weighted); every blind rung is near 0 or below. The perp front-runner
grades as quote-all: the order sheet cuts its perp trades to the mandate's
0.001 SPCX.

The fast model (`gate0-seed-81..84`, 1,000 sessions each, with the
index's measured 0.45 bp offset and a 1 bp margin) gives 5.3 to 18x the
best careful rung, order 0.905 to 0.977, the reference at 0.49 to 0.53.

### Earlier generations (reach 0.2 to 0.5 bp a 0.021 SPCX trade size)

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

v1's fast model (seeds 51 to 53, 1,000 sessions each; in the branch's history) agreed: 3.0 to 3.4x
the best careful rung, order 0.99, the reference at 0.54 to 0.58.

## Informed clients are sometimes wrong (`hit_rate` 0.85, checklist 16d)

Main's RFQ clients (#114) give an informed client a `hit_rate`: on each
request it acts on what it sees with that probability, and otherwise has
the move backwards, asking and valuing SPCX the other way. The card keeps
the library's default, 0.85 (Michael, 2026-11-17): real informed flow is
right more often than not, never every time, and a desk that reads a client
from one trade is reading noise. The instruction states it as a process
fact.

What it does to the read: a mistaken request is still from an informed
client, so the identity read (who takes a price past the widest reach)
is untouched; the requests' sides and markouts carry about 0.7 of the
signal they did (0.85 − 0.15), so markouts need more requests per client,
and a single informed request is a weaker sign of the coming move. The
reference judges a client over its requests (a probe a client takes, or a
markout record at odds 0.7), never one markout; both anchors read the
identified clients' latest side as the move, which a mistaken request
sometimes gets wrong.

Gate 0 on main after #113 and #114, fresh 72-session sets (`engine-v3-*`):

| Set | Informed / best careful | Informed / best blind | Order holds | Reference, weighted share | Side-skew, weighted share |
| --- | --- | --- | --- | --- | --- |
| `hit_rate` 0.85, set 1 | 1.64 (side-skew) | 3.77 (quote-all) | **0.511** | 0.63 | 0.49 |
| same set, `hit_rate` 1.0 (control) | 2.05 (side-skew) | 13.1 (size-tier) | **0.820** | 0.73 | 0.33 |
| `hit_rate` 0.85, set 2 | 1.65 (side-skew) | 15.2 (size-tier) | **0.307** | 0.47 | 0.50 |

**It fails.** On both sets at 0.85, side-skew (which reads the request
flow, not who sent it) earns as much as the reference or more, and the
informed bot is under 2x it. The paired control shows the mistakes cost
the informed bot about a fifth of its margin, but side-skew is stronger on
main than it was before the merge: 0.35 to 0.38 of the reference then, 0.95
and 1.25 now, and 0.67 on the control with `hit_rate` 1.0. The blind rungs
stay far below.

**Diagnosis (same secret as set 1, `hit_rate` 1.0 wherever it exists, total PnL over 72 sessions):**

| Build | Side-skew | Reference | Informed | Informed / side-skew |
| --- | --- | --- | --- | --- |
| (a) pre-merge base (main 4faade898) | 29.85 | 56.69 | 94.03 | 3.15 |
| (b) base + #113 | 29.85 | 56.69 | 94.03 | 3.15 |
| (c) base + #114 | 43.40 | 64.88 | 89.05 | 2.05 |
| main with both (the control above) | 43.40 | 64.88 | 89.05 | 2.05 |

- #113 changes nothing on this family: (b) matches (a) to the cent.
- At `hit_rate` 1.0, #114 changes no distribution for RFQ clients, only the
  draws: its hit/miss draw takes one number from the clients' stream on
  every informed request, even when it can't miss, so every later request
  (who asks, which side, what size; 5,711 requests in (a), 5,724 in (c)) is
  a new realisation of the same family. The side-skew gain from (a) to (c)
  is that resampling.
- What moves the family is `hit_rate` 0.85 itself. A mistaken informed
  client values SPCX the wrong way, so it declines a quote at the reach and
  costs the desk nothing, and its request misleads a side read. Every desk
  that quotes the flow then pays informed clients less (about 0.7 of the
  toxicity), and knowing who they are is worth that much less. In the model,
  the informed bot over side-skew goes from 9.1x and 6.9x at 1.0 to 2.6x and
  2.5x at 0.85 (seeds 92, 93). In the engine, on set 1, the reference loses
  20% and side-skew gains 15% (`engine-v3-hit*`). Quoting informed clients
  whose request goes against the other informed flow doesn't win it back
  (model).

## Departures from the card

| Card | Built | Why |
| --- | --- | --- |
| `tolerance_bps` 0.5 to 1.5 per Phoenix trade size (0.021 SPCX), sizes 1 to 200 trade sizes | 0.8 to 1.6 bp per lot of 0.25 SPCX; sizes lognormal around 3 lots (0.75 SPCX, about $107), log-sd 0.5 | the reach on a request must sit well above the index's offset from the level the clients value at (0.48 bp sd, rounding included; FEASIBILITY_BATCH3 rank 1), or the task is a precision game: 1.4 to 2.8 bp on the median request, and the informed bot's PnL moves under 10% for margins from 0.5 to 1.4 bp. It must also stay under what a client's informed edge is worth over the lead (about 5 to 9 bp), or quoting everyone pays: with two decades of sizes the reach, which grows with the square root of the size, outruns the markout on large requests. Realism: a client's alternative is crossing Phoenix's book, a 3.5 bp taker fee plus the half-spread (calibration default 9.6 bp), so paying an RFQ desk up to 3 bp past fair is the cheaper route. v1 (0.2 to 0.5 bp a 0.021 SPCX trade size, reach 0.9 to 2.2 bp) passed Gate 0 but sat at the rounding floor |
| `lead_slots` 40 to 120 | 45 to 120 | at 90 to 240 the informed clients' requests cluster on one side long enough for the side-skew rung, which reads only the flow, to reach 0.1 to 0.4 of the informed bot in the model, and the order failed on 2 of 3 model seeds |
| 0 to 2 jumps of 1 to 3% | SPCX's calibrated jumps (0.67 an hour, median 0.52%) | the card's jumps come about 13 times as often as the calibration's and are 2 to 6 times as large; one informed request ahead of a 2% jump outweighed a session of reach, and inventory riding through them swamped every rung's edge |
| Desk inventory within about 40 SPCX | every prototype keeps it within 16 SPCX of the start | unhedged inventory rides at the index; a tight bound (4 SPCX, a few requests) declines benign requests in runs and favoured the side-skew rung in the model (the mandate's own bound on desk inventory is not checked) |
| Perp position 0 | a 0.001 SPCX cap | the mandate refuses a zero cap |
| The reference: markouts on fills, shrunk to a prior | markouts on every request, plus one wide-quote probe per client and its flagged clients' flow | markouts alone, on 3 to 10 requests a client, earned 0 to 0.2 of the informed bot in the model. A quote just past the widest reach is taken by no uninformed client and by most informed ones: a client's hit on a wide quote is the desk's sharpest read of who it is |

## Generations

- **v0 (Gate 0 prototype).** The family above, the prototype bots, and
  Gate 0 on the engine (see "Gate 0").
