# Design record: perp-accumulate-nvda

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. It is card 1 of the Phoenix perps batch
(`TASK_CARDS_BATCH1.md`); the departures from the card are listed last.

## Thesis

A good bot profits here because it reads whether one-sided taker flow is informed, paces its buying to how fast the book recovers from its own takes, and leaves to the broker what is cheaper to leave.

- **Wins:** front-loading ahead of informed buying, standing aside ahead of informed selling, slicing at the pace the maker refills, resting bids where sellers come to it.
- **Loses:** fixed schedules lose in one of the mirrored regimes; trading every burst loses to fake-outs; sweeping the book pays the backstop; buying everything in a calm session pays more than the broker charges.
- **Regimes:** calm 30%, informed up 35%, informed down 35% (stratified: 22, 25, 25 of 72).
- **Mandate:** NVDA only, position 0 to Q (20 to 45), buys only, a price cap 0.8% to 2% over arrival, a loss limit of 5% of the order's notional (out of reach of the family's moves).

## Expert research

Working a client order against a deadline is the core of agency execution
(Almgren and Chriss 2000; Kissell, *The Science of Algorithmic Trading*,
2013). The expert's method has three parts, and the task tests each:

- **Urgency follows information.** Execution desks speed up when the flow
  says the price is about to run away from them, and slow down when it says
  they'll get a better price by waiting (Kyle 1985: informed flow leads the
  move; Easley, López de Prado and O'Hara 2012 on order-flow toxicity).
  The non-expert runs a fixed schedule, which loses symmetrically.
- **Pacing to resilience.** Impact decays at the pace the book refills
  (Obizhaeva and Wang 2013). An expert slices at the pace the liquidity
  comes back and measures it from their own takes; the non-expert sweeps,
  or slices by the clock.
- **The residual decision.** A desk compares the cost of finishing in the
  market with what the broker will charge for the remainder (a close
  auction or a risk price). Finishing at any cost is a non-expert habit.

## The central decision

Each step: buy now (crossing, at the maker's ask or the backstop), rest a
bid and let sellers come, or wait and leave more to the broker. The
defensible options and where each fails are below.

## Variants

| Regime | Best response | TWAP-cross | TWAP-passive | Dump | Waiting |
| --- | --- | --- | --- | --- | --- |
| Calm | rest bids, cross only what beats the broker | pays spread and fee all session | fine | pays the backstop | fine |
| Up | buy before the move, paying up to the move | half the move missed | most of the move missed, crosses at the top | captures the move but pays the backstop | buys at the top |
| Down | stand aside until it lands, then rest bids | loses the share bought before | its bids are the informed sellers' exit | loses the whole move | right |

## Levers

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| Informed flow | 6-14 orders the move's way 20-100 slots ahead of the onset, then the noise leans 10-25 points that way | front-load (up) or stand aside (down) | the taker tape on the market's order book (`getSignaturesForAddress`, `getTransaction`), or the grader's maker's lean, which moves 1 bp a unit with what takers buy from it |
| Fake-outs | 0-2 bursts in every regime, drawn exactly like informed ones, with nothing after | wait for the lean and the price to confirm | persistence: after a fake-out the flow goes back to balanced and the price doesn't move |
| Resilience | the maker requotes every 2-12 slots and leans against its inventory; behind it the backstop is 15-30 bp out | slice at the pace the maker comes back | its own takes: lift the maker, see the ask jump to the backstop, time its return |
| The broker | 10-30 bp over the close | cross only while the ask plus the fee beats it | in the session vars |
| The cap | 0.8-2% over arrival | never bid or buy above it | in the mandate |

## No tells

- Every regime runs the same machinery: the same actors are seated, the
  same draws come in the same order (the move is drawn in calm sessions
  too and not laid), bursts and fake-outs have the same count, spacing,
  sizes, limits and accounts.
- The move lands in eight uneven steps, its onset is uniform over 15-70%
  of the session, and its ramp is 40-240 slots: neither the first step
  nor the session's length dates or sizes it.
- Up and down are mirrored with equal odds, so a fixed schedule has no
  edge in expectation, and the Gate 0 model's dump and TWAP rungs come out
  around the haircut less their costs.
- The scenario's description shows only the order (Q, the cap, the
  haircut), which the session vars already give.
- Generator-aware bots without reads: the best is TWAP-passive (rest bids,
  cross late), which is a ladder rung and sits below the reference.

## Detectability

In the Gate 0 model, the reference's burst detector (an EWMA over about 30
slots at 2.5 standard deviations for 8 slots) catches the informed burst
before the onset in most informed sessions, and a fake-out puts it on
watch about as often: the price and the lean then decide. In the engine
the reference reads the flow through the maker's lean, a noisier window
than the tape (see "The reference" below).

## Gate 0

`hidden/gate0/model.py` (README beside it) simulates the family with the
engine's book as measured on the #60 snapshot and runs the card's ladder
on 1,000 sessions, bootstrapping 72-session sets. Recorded runs, seeds 21,
22, 23:

| Seed | told / best careful-unaware | order holds | reference, v2 weighted, of told's | reference per-session Sharpe |
| --- | --- | --- | --- | --- |
| 21 | 4.09 | 0.965 | 0.41 | 0.44 |
| 22 | 4.24 | 0.971 | 0.44 | 0.45 |
| 23 | 4.29 | 0.939 | 0.47 | 0.49 |

Gate 0 passed: told is at least 2 times the best careful-unaware rung
(TWAP-passive), nop and dump are below the reference, and the order
(`dump < min(TWAP)`, `max(TWAP) < reference < told`, `nop < reference`)
holds on at least 85% of bootstrapped sets. The engine's own ladder is in
the README.

## Fairness

- The agent is told the family's every range and odd, the order, how the
  PnL is computed, and scoring v2's rule in plain words (D5).
- Everything a good bot uses is on chain: the tape, the book, the oracle,
  the vars. The reference uses only the book and the oracle.
- Practice is six sessions from the public generator, which also draws
  more; it carries neither anchor.
- The reference stays inside the mandate in every session (`just ladder`).

## The anchors

- **The reference (0.5)** works the order passively, reads the flow
  through the grader's maker's lean, buys ahead of a confirmed rise, stands
  aside ahead of a sell burst, and leaves the broker what costs more to
  buy. It doesn't parse the tape and doesn't size anything to the move.
- **The informed bot (1)** is told what #47's boundary allows: identity
  and process facts, never a session's draws. The latent names the
  `informed` taker account in its roster, which the session resolves to
  that account's wallet, and gives the book's process (the grader's
  maker's half-spread, quote size and look delay, and the backstop). The
  informed trader is seated in every session and trades noise like every
  account; ahead of a move its burst comes from it, and each fake-out from
  one other account. The bot maps the wallet to its Phoenix trader account,
  reads that account's NVDA position every step, and takes a net change of
  0.6 NVDA within 120 slots as the burst: up, it buys the rest at once
  (paying up to 40 bp over the broker); down, it waits until the oracle has
  fallen 0.35% from the high and stopped falling; until then, or in a calm
  session, it works passively. It isn't told whether the session moves,
  which way, when or how far.

## Failure modes, priced

From the Gate 0 model, per session (mean USD): dump −2.5 to −3.2 (it
pays the backstop for all of Q and wears every down move), TWAP-cross
+1.8 to +2.1 (it pays the spread and the fee on all of Q), TWAP-passive
+3.0 (it loses up moves), reference +6.2 to +6.5, told +12.5.

## Predictions

Frontier agents find the passive habit quickly (the broker's haircut makes
resting bids pay) and land near TWAP-passive, about 0.2 to 0.3. Reading
the flow is what lifts a bot past the reference; few will tell fake-outs
from informed bursts.

## Generations

- **v1 (this build).** Gate 0 on the engine's measured book; built on PR #76
  (orders, the per-fill audit), #36 (the probe gate), #44 (the v2
  template) and #47 (the informed bot on hidden sets). Gate 0's told rung
  knew the onset; the built informed bot knows only the latent state
  (#47's rule), so it is weaker than Gate 0's told.

## Departures from the card

| Card | Built | Why |
| --- | --- | --- |
| Q 40-90 NVDA (0.6-1.4x collateral) | Q 20-45 | the near-touch book is the grader's maker (2-5 units a look, 1 bp a unit of lean); at 40-90 every bot ends on the backstop and the task is the backstop's price |
| Haircut h 15-50 bp | 10-30 bp | at 15-50 bp the haircut swamps the information edge: told came to 1.25x the best TWAP in Gate 0 |
| Resilience as a refill half-life of 4-40 slots | the maker's look delay (2-12 slots, jittered) and its lean, over a backstop at 15-30 bp | the snapshot's NVDA makers hold about 12 NVDA a tick and re-centre on every path move (measured: a 36.5-unit take moved the ask 8 ticks and nothing came back in 60 slots until a move restored it), so a refill on them measures nothing. The makers are widened to a backstop with a new opt-in `makers_bps` (grader) apart from the maker's own half-spread |
| No drawdown limit | a loss limit of 5% of the order's notional | the scenario format requires one on a perp; it is out of reach of the family's moves |
| Informed lead 20-150 slots | 20-100 | with the lean, a lead past 100 slots reached the session's start too often |
| Noise rate as market making's, 0.2-0.45 a slot | 0.06-0.15 a slot, sizes around 0.15 | with the market-making flow the burst was buried and a passive bid filled the whole order from noise sells |
| v1 reward and skill | scoring v2 only (`--scoring v2-headline`) | one reported score (GRADING_DECISIONS, 2026-11-14) |
| Reference reads `getBlock` | reads the flow through the grader's maker's lean | the kit's Phoenix event decoder isn't in the component build; the tape stays readable for any bot that decodes it |
