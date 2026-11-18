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

## Mandate (checklist item 0)

**As built and gated, C1 is an alpha card, not a desk mandate.** The desk
starts flat with no duty: it may trade within a 2 TSLA cap, and its whole
payoff is a directional position taken on the read (the take expression;
the maker lean rarely fills behind the splines' 2.1 bp touch). Doing
nothing scores exactly 0, which fails no duty. Item 0 asks for a rewrite,
so the gates above (sets 1, 2 and the rework) measure the read's strength,
not a mandate's.

**Reframe: informed execution (item 0's sweet spot, as card 1 and B6).**

- **What the desk must do.** Each session hands the desk an order: buy (or
  sell) Q TSLA by the session's end, Q drawn per session (for example 4 to
  8, sized to the book near the touch), the side drawn with equal odds and
  mirrored in pairs. It must end within 80 to 100% of Q (the tolerance
  band); whatever is left at the end is done by the broker at the closing
  index plus (minus) a haircut. Same position cap (Q) and a loss limit out
  of reach of the moves. Scored against the broker route (`order`, as B6):
  doing nothing scores 0 and pays nothing, so the duty is set by charging
  the unfilled band: below 80% the broker fills the rest at the haircut, and
  the score is cost saved against TWAP over the session (the rung every
  careful non-reader can run).
- **How the read improves performance against the benchmark.** The read
  never sets the side or the size; it sets pace and aggressiveness. When the
  informed maker leans with the order's side (up ahead of a buy), the desk
  crosses and front-loads before the move; when it leans against (down
  ahead of a buy), the desk pauses or rests passively and buys after the
  move, and within the band may finish nearer 80% if the price ran away. A
  right read saves about the move (median jump 38 bp) on the share it
  timed; a wrong read (a decoy's lean, or the informed maker's 15% wrong
  horizons) buys ahead of a fall or waits through a rise, paying the move.
- **Why doing nothing fails the duty.** An unfilled order goes to the
  broker at the haircut, and below the band the shortfall is charged: idle
  is a sure cost, TWAP is the honest non-reading baseline, and the blind
  rungs (follow the mean lean, the most-leaned maker, one maker picked
  blind) time the order on noise and pay crossing costs for it.
- **What changes in the build.** The generator adds an `ExecutionOrder`
  (side, Q, haircut) and start position 0, as B6's family does; the
  mandate becomes the order's side with the cap at Q; the ladder's rungs
  become `twap`, `dump` (all at once), `late`, `mean-skew`, `most-skewed`
  and `seat-a` timing the same order; the reference and the informed bot
  keep their reads and swap the follower for order pacing. The skewers,
  the world and the told roster are unchanged. Gate 0 is re-run from
  scratch on fork/main once actor validation phase 2 (actors at the
  stated rate) and the maker fix (makers re-centre every step) have merged.

An alternative is a market-making duty (quote both sides within a width,
presence and minimum size, inventory within +/-N, scored on edge kept
against a neutral quoter at the mandated width; the read sets skew and
which side steps back). It fits the card's maker-reading story, but on main
quoting obligations exist only in market-making sessions and are reported,
not scored, and library actors inside a market-making session are
unverified, so it needs engine work first. The batch 6 model also found the
lean alone worth only 1.1 to 1.24x of a blind lean.

## The market

- **Path.** TSLA's calibration from 09:31 New York on a drawn trading day,
  4,500 slots (3,000 before the rework), at the opening hour's measured regime (#111;
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

Engine run on runner-14 (`hidden/gate0/gate0.py`, untracked): one fresh
72-session hidden set from a new secret (made on the box, never printed),
every rung a strategy component graded with `mtx grade --draw skew-maker
--trusted`, the informed rung with `--tell-latent`. Tuning before it used a
separate probe secret only (a 48-session recording and its replay model).

| Rung | Total | Mean | sd | v2-weighted share of informed |
| --- | --- | --- | --- | --- |
| most-skewed | -244 | -3.38 | 5.64 | -1.65 |
| neutral | -126 | -1.75 | 3.35 | -0.79 |
| mean-skew | -113 | -1.57 | 4.21 | -0.84 |
| seat-a | -110 | -1.53 | 4.88 | -0.83 |
| twap | -58 | -0.80 | 2.98 | -0.48 |
| nop | 0 | 0 | 0 | 0 |
| reference | 83 | 1.16 | 4.30 | 0.29 |
| informed | 250 | 3.47 | 5.60 | 1.00 |

- Signal gate: every non-reading rung loses; the best is `nop` at 0, so
  informed / best non-reading is unbounded (need 2); informed is at least
  twice the best non-reading rung on 1.000 of bootstrapped sets.
- Order `max(nop, mean-skew, most-skewed, seat-a, twap, neutral) <
  reference < informed` holds on 0.993 of 2,000 bootstrapped 72-session
  sets (need 0.85).
- Strength band: reference 0.29 of informed, v2-weighted at k = 1.5 (band
  0.2 to 0.7; raw 0.33); its per-session Sharpe 0.27 (need 0.25). Informed's
  Sharpe 0.62.
- **Passed**, at the band's low end and just over the Sharpe floor: the
  reference reads the informed maker only after one or two of its moves
  (it follows a maker in about a third of sessions on the replay).

### Second set, and the rework

A second fresh 72-session set on main with #118 to #122 merged (shared
worlds, maker odd lots, component anchors, solution exports):

| Set | Reference / informed (v2) | Reference Sharpe | Informed Sharpe | Order |
| --- | --- | --- | --- | --- |
| 1 | 0.29 | 0.27 | 0.62 | 0.993 |
| 2 | 0.46 | 0.25 | 0.46 | 0.996 |
| pooled (144) | 0.38 | **0.2494** | 0.52 | 0.999 |

Every non-reading rung lost on both sets (best: nop at 0). Pooled, the
reference's Sharpe sits under the 0.25 floor by 0.0006: the one rework.
No reference rule did better on the replay (leads of 25 to 35 bp, half
size until a wider lead, dropping a maker that loses the lead: Sharpe 0.16
to 0.22 against 0.22), so the rework lengthens the session within the
measured regime: 4,500 slots (09:31 to 10:01, inside Table 2's 09:31 to
10:30 window), where each session holds about 1.5 times the informed
maker's leads. Set 2 ran while a third container (another agent's) shared
the runner for part of it; every rung is trusted and the world waits for
its turns, and every report shows 0 late slots, 0 overruns and 0 late
sends.

### Gate 0 after the rework (4,500-slot sessions)

One fresh 72-session set from a new secret, every rung a component, one
container on runner-14:

| Rung | Total | Mean | sd | v2-weighted share of informed |
| --- | --- | --- | --- | --- |
| most-skewed | -360 | -5.00 | 6.97 | -1.36 |
| neutral | -204 | -2.84 | 3.87 | -0.74 |
| mean-skew | -109 | -1.51 | 6.48 | -0.53 |
| seat-a | -104 | -1.45 | 8.90 | -0.55 |
| twap | -56 | -0.78 | 5.25 | -0.34 |
| nop | 0 | 0 | 0 | 0 |
| reference | 255 | 3.54 | 9.00 | 0.58 |
| informed | 440 | 6.11 | 10.75 | 1.00 |

- Signal: every non-reading rung loses (best nop, 0); informed at least 2x
  the best on 1.000 of bootstraps.
- Order holds on 1.000 of 2,000 bootstrapped 72-session sets.
- Band: reference 0.58 of informed (v2, k = 1.5), Sharpe 0.39; informed's
  Sharpe 0.57. **Passed.** The longer session leaves the reference more
  time to follow once it has read the maker, so the band rose from 0.38 to
  0.58: still inside 0.2 to 0.7, above the 0.5 target.

## Risks for the full build

- Thin margins: the band (0.29) and the reference's Sharpe (0.27) sit near
  their floors on one set; the full ladder's sets should confirm them.
- The maker lean rarely fills (the skewers and the bot's lean sit behind the
  splines' 2.1 bp touch), so the task is a taker task in practice, as the
  feasibility verdict expected; obligations or widened makers would change
  that and need their own Gate 0.
- `cargo test -p mtx-cli` fails `every_task_names_its_snapshot_and_practice_set`
  until the full build's six practice sessions exist.

## Departures from the card

| Card | Built | Why |
| --- | --- | --- |
| `edge_bps` 20 to 30 | 30 to 40 | at 20 to 30 the informed skewer's view flips in diffusion often enough that following it nets about costs (replay: Sharpe 0.28 at best following); the feasibility model's 30-40 row was the card's alternative |
| Snapshot makers widened to `makers_bps` so the rivals set the touch | not widened (plain perp session; widening exists only with `making`) | the card keeps C1 a plain perp session; the skewers quote behind the splines' 2.1 bp half-width, so the maker lean fills rarely and the take expression carries the edge, as the feasibility verdict asked |
| Decoys hold views 150 to 400 slots | holds from half the informed skewer's mean run to its longest, spaced to its duty on the session's path | matches timing to the informed one per session, as batch 5's rebalancers |
| Cap measured from the near-touch book | 2 TSLA | about the splines' depth within 5 bp (calibration default `depth_base`) |
