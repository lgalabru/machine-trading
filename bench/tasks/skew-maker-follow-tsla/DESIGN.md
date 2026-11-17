# Design record: skew-maker-follow-tsla

Card C1 of batch 6 (`TASK_CARDS_BATCH6.md`), with its feasibility model in
`FEASIBILITY_BATCH6_MODEL.md` (`feas_b6/c1_skew.py`). The README says what
the task is; this says why it's built this way, and what each version
learned.

## Thesis

A good bot profits here because it reads, from how three rival makers quote,
which one leans ahead of the price, and follows that maker's lean.

- **Wins:** taking the cap the informed maker's way once its lean shows on
  two quotes, and holding through the move it leans ahead of.
- **Loses:** following every lean (the mean skew), the most-leaned maker,
  or one maker picked blind; quoting neutral into the opening jumps.
- **Regimes:** one of three makers leans ahead of moves over its edge,
  right 85% of the time; two lean on random views at the same rhythm. Paths
  are mirrored in pairs.
- **Mandate:** TSLA only, a 2 TSLA cap, a $150 loss limit, valued at the
  index (what is left at the end closes at the closing index less 5 bp).

## The market

- **Path.** TSLA's calibration from 09:31 New York on a drawn trading day,
  3,000 slots, at the opening hour's measured regime (#111;
  `JUMP_REGIME_MEASUREMENT.md` Table 2): `vol_mult` 2.2, 3.9 jumps an hour,
  sizes median 0.382% and p90 0.652%. Mirrored pairs.
- **Skewers** (the library's `skewers`, `docs/actors.md`). Three, each one
  wallet, in a drawn seat order, each drawn from one set of ranges:
  half-width 4 to 8 bp, lean 0.6 to 1.0 of the half-width, each side's
  distance off by a normal draw of 0.3 of the lean, a quote every 8 to 14
  slots (drawn within 40% each time) and at once when the view changes,
  sizes 1 to 3 trade sizes per side. The informed one sees the path 60 to
  150 slots ahead and leans while the move is over 30 to 40 bp; the two
  decoys lean on random views held as long as the informed one's runs on
  the session's path, as often, and quote flat between.
- **Flow.** Noise at the opening hour's pace (calibration x 1.28 x 4), two
  chasers (trigger 15 to 25 bp) and the arbitrageur (main's defaults). No
  informed taker: the quotes carry all the information.
- **Measured on the engine (48 sessions):** the snapshot's spline makers
  hold the touch at a median half-width of 2.1 bp, so the skewers (median
  6.3 bp) quote behind it and a bot's take costs the splines' half-spread
  plus the 3.5 bp fee. The informed skewer full-leans on about 3% of steps.
- **The arbitrageur and stale quotes.** Every quote a move left more than
  5 bp through the new index was taken by the next step: 40 of 40 on the
  stepped-back side, 28 of 28 on the favoured side, 84 of 84 flat (moves of
  15 bp or more within a step, 48 sessions). A leaning maker's stepped-back
  quote is no shelter from a large jump.

## What the informed bot is told

The roster, identity only: `{"roster": ["skewer-informed", "skewer-decoy"]}`,
which the session resolves to the informed skewer's wallet and the two
decoys'. Every session seats one of each kind and two decoys. Not any view,
when one leans, a jump's time or sign, nor `hit_rate`. The book names a
resting order's trader by a number; the bot names each number by the slots
its quotes were placed in, read off the told wallets' signatures
(`getSignaturesForAddress` at `confirmed`), and follows the informed
number's lean once its votes say so (about 100 slots in).

## `hit_rate` (16d)

0.85, main's default: the informed skewer reads the move the wrong way for a
whole horizon at a time, 15% of the time. Every following rung reads a
maker's lean as the mean skew of its last two quotes against the index each
was placed at, takes the cap at 0.6 of a full lean, rests the favoured side
from 0.35, never turns a position over in one step, and closes 40 steps
after the lean has gone: a lean is followed over its horizon, never on one
quote.

## The anchors (Gate 0 prototypes)

- **The reference (0.5)** groups the book's resting orders by trader number
  (a trader with both sides resting is a maker), reads each maker's lean as
  above, and keeps the PnL of following each maker's lean (a unit, marked at
  the index each step). Once one maker's following PnL is positive and leads
  every other's by 30 bp, it follows that maker as the informed bot does.
- **The informed bot (1)**: above.

## Ladder rungs (non-reading)

| Rung | What it does |
| --- | --- |
| `nop` | the starter |
| `mean-skew` | follows the mean lean of every maker (take 0.3, lean 0.15: the best of 0.1 to 0.3 on the replay) |
| `most-skewed` | follows whichever maker leans furthest |
| `seat-a` | always follows the maker with the lowest trader number |
| `twap` | buys the cap evenly over the session and holds |
| `neutral` | quotes both sides 5 bp from the index, 0.25 TSLA |

## Gate 0

See below (filled in from the run).

## Departures from the card

| Card | Built | Why |
| --- | --- | --- |
| `edge_bps` 20 to 30 | 30 to 40 | at 20 to 30 the informed skewer's view flips in diffusion often enough that following it nets about costs (replay: Sharpe 0.28 at best following); the feasibility model's 30-40 row was the card's alternative |
| Snapshot makers widened to `makers_bps` so the rivals set the touch | not widened (plain perp session; widening exists only with `making`) | the card keeps C1 a plain perp session; the skewers quote behind the splines' 2.1 bp half-width, so the maker lean fills rarely and the take expression carries the edge, as the feasibility verdict asked |
| Decoys hold views 150 to 400 slots | holds from half the informed skewer's mean run to its longest, spaced to its duty on the session's path | matches timing to the informed one per session, as batch 5's rebalancers |
| Cap measured from the near-touch book | 2 TSLA | about the splines' depth within 5 bp (calibration default `depth_base`) |
