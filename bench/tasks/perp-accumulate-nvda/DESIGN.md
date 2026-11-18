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

- **The reference (0.5)** reads the flow through the grader's maker's
  lean, buys ahead of a confirmed rise and stands aside ahead of a sell
  burst. Without a move it buys nothing until no move can start (75% of
  the session), then finishes under the broker's price (the shared
  `finish`: a bid at the front of the book and takes of the asks under the
  broker's price less the fee and 3 bp, at an even pace; whatever still
  beats the broker in the last 120 slots). It doesn't parse the tape and
  doesn't size anything to the move.
- **The informed bot (1)** is told what #47's boundary allows: identity
  and process facts, never a session's draws. The latent names the
  `informed` taker account in its roster, which the session resolves to
  that account's wallet, and gives the book's process (the grader's
  maker's half-spread, quote size and look delay, and the backstop). The
  informed trader is seated in every session and trades noise like every
  account; ahead of a move its burst comes from it, and each fake-out from
  one other account. The bot maps the wallet to its Phoenix trader account,
  reads the orders that wallet sends, filled or not (its transactions'
  Phoenix market orders: side and size), and takes 4 or more one-way
  orders within 120 slots, at least 0.5 NVDA net, as the burst: up, it buys the rest at once
  (paying up to 60 bp over the broker); down, it waits until the oracle has
  fallen 0.5% from the high and stopped falling. It also reads the oracle
  as any bot can: a 0.25% break from its range over 150 slots, held 30
  slots, is a move under way, and a burst read later overrules it. It
  buys nothing before a signal: without one it finishes from 75% of the
  session as the reference does, and after a fall from when the fall has
  landed (or 85%). It isn't told whether the session moves, which way, when
  or how far.

  On the fast set (12 sessions, 2026-11-17, after #113/#114) three versions
  were tried: the read-only bot ($182.6), the same plus the oracle break
  ($178.0: one session where a false break down came before the burst),
  and a version that capped early passive buying at a fifth of the order
  and took asks at an even pace late ($170.9: worse in falling sessions).
  The second was the anchor until the screen of 2026-11-18 showed that
  waiting beats its early passive bid (below).

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

## Gate exception (Michael, 2026-11-17)

On the full 72-session ladder this card passes Gate 0's ratio at informed
≥ 1.9× the best non-reading rung (not 2×), with the order on ≥ 85% of
bootstrapped sets and the reference at 0.2–0.7 of the informed bot as
usual. The card is realistic and mirrored; the ratio is held down by the
engine's cheap crossing (TWAP-cross makes about 0.8× the reference: small
IOC slices against the grader's maker cost little), not by a design flaw.

## The full ladder (72 sessions, 2026-11-17)

| Rung | Total | Weighted (losses 1.5x) |
| --- | --- | --- |
| nop | $0 | $0 |
| dump | -$82.8 | -$736.9 |
| TWAP-cross | $22.8 | -$286.6 |
| TWAP-passive | -$66.7 | -$148.8 |
| reference | $329.5 | $243.5 |
| informed | $817.8 | $726.1 |

Informed is 35.9x the best non-reading rung; the reference is 0.335 of
the informed bot's weighted PnL (per-session Sharpe 0.40); `max(dump,
TWAP-cross, TWAP-passive) < reference` holds on 0.999 of bootstrapped sets
and `reference < informed` on 1.000. The card's pairwise rules `dump <
TWAP-cross` and `dump < TWAP-passive` are replaced by the one rule over all
three: over 72 sessions every non-reading rung loses or breaks even, and
ranking one losing naive rung against another tests nothing the card is
about (they held on 0.69 and 0.50 of sets: a tie).

## The re-gate (72 fresh sessions, 2026-11-18)

After #118 (every role plays the same world), #119 (the maker steps in
front of any order of at least one lot) and the informed bot's held
oracle break, on a fresh secret:

| Rung | Total | Weighted (losses 1.5x) |
| --- | --- | --- |
| nop | $0.0 | $0.0 |
| dump | $127.4 | -$511.3 |
| twap-cross | $105.4 | -$169.2 |
| twap-passive | -$26.5 | -$90.5 |
| reference | $441.2 | $374.4 |
| informed | $1122.4 | $1027.6 |

The informed bot makes 8.81x the best non-reading rung (dump, by raw
total); the reference holds 0.393 of the informed bot's total (0.364
weighted); `max(dump, twap-cross, twap-passive) < reference` holds on 0.860
of bootstrapped sets and `reference < informed < hindsight` on 1.000. No
rung breached or was disqualified. Gate 0 passes. The order rule sits near
its 0.85 floor because dump's raw total turned positive (its losses still
weigh it at -$511).

## Screen after the re-gate (provisional, unguarded; 2026-11-18)

Hidden set anchors: reference $234.9, informed $754.3.

| Run | Reward | Raw | Bot PnL | Reads transactions |
| --- | --- | --- | --- | --- |
| Sol 1 | 1.000 | 1.145 | $904.6 | yes (every taker's market orders) |
| Sol 2 | 1.000 | 1.122 | $881.0 | no |
| Sol 3 | 1.000 | 1.148 | $908.2 | yes |
| Opus 1 | 1.000 | 1.092 | $850.3 | yes |
| Opus 2 | 1.000 | 1.060 | $816.1 | no |
| Opus 3 | 0.870 | 0.870 | $619.1 | no |

Five of six runs beat the informed anchor, two of them without reading a
transaction: an oracle break (0.25% from the recent low or high) to buy
everything or stand aside, and otherwise no buying until about 75% of the
session, then below the broker's price. The informed bot's passive bid
before a move costs it in falling sessions, and no ladder rung plays the
wait-late strategy, so Gate 0's non-reading bar was too low. The run that
rested a bid all session (Opus 3) landed below the informed bot.

## The redesign (decided 2026-11-18)

The one redesign the saturated-task policy allows, for a weak ladder and
anchors: the screen's best non-reading play beat the informed anchor, and
no rung played it.

- **A new non-reading rung, `wait-break`** (`checks/bots/naive`), modelled
  on Sol 2 and Opus 2: it reads only the oracle. A rise of 0.25% from its
  low over 300 slots is a move up, and it buys the rest at once, paying up
  to 40 bp over the oracle; a fall of 0.25% from the high is a move down.
  Otherwise, and after a fall, it buys nothing until 75% of the session and
  then finishes under the broker's price. It joins the non-reading gate and
  the order rule: `max(dump, twap-cross, twap-passive, wait-break) <
  reference`.
- **The informed bot** plays the best known play plus the wallet read: no
  buying before a signal, the oracle break beside the burst read, and the
  late finish under the broker.
- **The reference** likewise where it needs no told fact: it keeps its
  flow read and its confirmed-rise buying, and stops buying early without
  a move; it finishes late under the broker.

Unit checks: the shared `finish` (nothing before its start, takes under the
broker and a unit bid at the front, no dear asks, the rest at the close),
the reference buying nothing early in a flat session, and `wait-break`
(waits in a flat session then finishes; buys the rest on a break up;
waits on a break down).

**The re-gate** runs once, with the other held cards, after the infra list
lands (permanent impact, #20, changes this market: every taker's flow now
moves the price). The rule then: if the reading signal (the informed bot
over the best non-reading rung, now `wait-break`) is under 1.9x, the family
needs its one redesign (slower onsets, oracle fake-outs, less of the move
left after the onset); if that fails too, it retires.

## Structural tells (checked before the screen)

Nothing structural marks the informed trader or the move's direction: the
eight taker accounts are funded alike and all trade noise from the start;
a burst, real or fake-out, comes from one account with the same count,
spacing, sizes and limits; the informed account is drawn per session and
shows nothing before its burst. The one prior is disclosed: onsets fall in
15-70% of the session and fake-outs in 15-85%, so a burst late in the
session is more likely a fake-out.

## Departures from the card

| Card | Built | Why |
| --- | --- | --- |
| Q 40-90 NVDA (0.6-1.4x collateral) | Q 20-45 | the near-touch book is the grader's maker (2-5 units a look, 1 bp a unit of lean); at 40-90 every bot ends on the backstop and the task is the backstop's price |
| Haircut h 15-50 bp | 10-30 bp | at 15-50 bp the haircut swamps the information edge: told came to 1.25x the best TWAP in Gate 0 |
| Resilience as a refill half-life of 4-40 slots | the maker's look delay (2-12 slots, jittered) and its lean, over a backstop at 15-30 bp | the snapshot's NVDA makers hold about 12 NVDA a tick and re-centre on every path move (measured: a 36.5-unit take moved the ask 8 ticks and nothing came back in 60 slots until a move restored it), so a refill on them measures nothing. The makers are widened to a backstop with a new opt-in `makers_bps` (grader) apart from the maker's own half-spread |
| No drawdown limit | a loss limit of 5% of the order's notional | the scenario format requires one on a perp; it is out of reach of the family's moves |
| Informed move 1.5-3x the median jump | 2.5-4x (0.71-1.14%) | with the informed bot told only who the informed trader is, the engine's TWAP-cross (0.84x the reference on the fast set) left it at 1.85x the best non-reading rung; a mirrored, bigger move raises what reading earns and leaves the non-reading rungs' expectation unchanged. Realism: NVDA's calibration (nvda.20261113.903d1120) puts the median one-minute jump at 0.284% and its p90 at 0.489% (0.58 jumps an hour beyond 5 robust sigma); 0.71-1.14% over 10-60 s is 1.5-2.3x that p90, a news-sized move: rare in regular hours, and smaller than the median close-to-open move the same file records (1.04%) |
| Informed lead 20-150 slots | 20-100 | with the lean, a lead past 100 slots reached the session's start too often |
| Noise rate as market making's, 0.2-0.45 a slot | 0.06-0.15 a slot, sizes around 0.15 | with the market-making flow the burst was buried and a passive bid filled the whole order from noise sells |
| v1 reward and skill | scoring v2 only (`--scoring v2-headline`) | one reported score (GRADING_DECISIONS, 2026-11-14) |
| Reference reads `getBlock` | reads the flow through the grader's maker's lean | the kit's Phoenix event decoder isn't in the component build; the tape stays readable for any bot that decodes it |
