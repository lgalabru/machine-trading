# Design record: phoenix-flash-crash

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Expert research

- **Why it's a real expert task.** Flash crashes and squeezes on perp venues
  are forced flow: liquidations and stop cascades push the mark past fair
  value, then it reverts as the forced sellers (or buyers) run out. Desks
  that provide liquidity into the shock and size for the shocks that keep
  going earn the reversion; Kirilenko et al., *The Flash Crash* (2017), on
  who traded the 2010 crash and how.
- **The expert's method.** Wait for the shock to be unmistakable, fade it in
  tranches as it deepens, cap the size so an extension can't breach the
  loss limit, cut when the move goes far past where shocks usually turn, and
  hold for the reversion rather than scalping it.
- **Where non-experts go wrong.** Buying every dip (squeezes and extensions
  punish it), betting a direction at the open, taking a full position on the
  first wobble, sizing so one extension takes the session past its
  drawdown limit, and closing too early to keep the reversion.

## The central decision

When, which way, and how much to fade a shock whose aftermath is unknown.

| Option | Why it looks defensible | Why it isn't |
| --- | --- | --- |
| Bet at the open | Shocks are coming | Crashes and squeezes are 50/50 |
| Buy the dip | Most shocks revert | Half the shocks are squeezes, and 15% extend |
| Ride the move | The first leg shows the direction | Most shocks revert |
| Fade the first 1% | Earlier entry, more reversion | Wobble and fizzles are 0.4–2% and give it all back |
| Full size at once | More PnL when right | An extension breaches the drawdown limit |

**The calculation chain:** read the mark each turn → tell a shock from a
fizzle (size, and whether it holds) → take the direction from the first
real leg → size tranches by depth against the cap → set the stop where
reverting shocks rarely reach → hold to the end.

**What must be combined:** the instruction's ranges and odds, the live mark,
and the session's end slot (`MTX_END_SLOT`), which since 0.8.0 is drawn
before the path and says nothing about the shock.

**Nothing warns the agent.** A bot that fades too early or too big still
gets fills; its cost shows only in the valuation and the drawdown.

## Variants

| Regime | Best response |
| --- | --- |
| crash, revert (~32%) | long into the crash, in tranches, hold |
| squeeze, revert (~32%) | short into the squeeze, hold |
| stall (~20%) | small or no position: little reversion to earn |
| extend (~15%) | stop out, or never enter beyond the usual depth |
| fizzle only, or ends mid-shock | stay flat |

| Habit | Fails in |
| --- | --- |
| buy the dip | squeezes, extensions |
| bet at the open | half the sessions |
| ride the move | reverts, the most common aftermath |
| fade early | fizzles |
| full size, no stop | extensions (drawdown breach) |

## Levers

| Lever | What the market does | What a good bot does | How an honest trader sees it |
| --- | --- | --- | --- |
| mirrored direction | crashes and squeezes 50/50 | waits for the shock | the first real leg |
| aftermath mix | revert 65%, stall 20%, extend 15% | sizes for the extension | the odds are in the instruction |
| legs 0.6–8%, log-uniform, up to three | small legs as common as large | doesn't take the first small move as the shock | a move that holds past the wobble |
| gaps (70% land at once) | no ramp to read the size from | doesn't extrapolate the first step | — |
| fizzles (odds 0.5, 0.4–2%, give it all back) | false starts | a threshold above fizzle size | they reverse within 1.5–10 s |
| duration drawn first (80–180 s), path cut at the end | the end slot dates nothing | holds to the end | — |
| wobble (≤ ~0.25%) | noise between moves | a threshold above it | — |

**Dropped:** fixed tails and equal steps (0.4.0: they dated the aftermath and
gave the move's size away), disjoint ranges (0.4.0: Fable read "a leg under
5% means another is coming"), legs of at least 3% that ramped (0.6.0: riding
the first step paid ~6× the reference).

## No tells

- Every session can fizzle, wobble, gap or ramp; only the draws differ.
- The session's length is drawn before, and independently of, its path.
- Direction is drawn first, 50/50; the generator stratifies a set so each
  direction and aftermath appears in proportion to its odds, and that's
  disclosed.

## Detectability

Not yet measured as separations in standard deviations; the ladder stands in.
The regime oracle's lead over the reference is the value of reading the
regime better, and the reference's lead over the dip-buyer and momentum
rungs is the value of reading it at all.

## Fairness

- Every correct path runs through standard RPC and `mtx-botkit`: the
  reference does.
- The instruction's formulas are the grader's: the bound (cap × start mark ×
  total variation), the reward clamp, the valuation at the mark, the cap and
  drawdown sampled after every step.
- Readings written out: "position" is long or short; "drawdown" is from the
  running peak, at the mark, wallet USDC included; resting orders don't
  count toward the cap until they fill.
- **Penalty twins:** a position at the cap passes, one lot over it zeroes;
  a non-standard but allowed method passes, a cheatcode zeroes; a send
  outside the bot's turn is refused, not a violation.
- **Fair failures to tell apart:** a bot that doesn't build or never answers
  the hello scores near 0 for the contract, not for trading; reviews read
  the bot log first.

## Failure modes, priced

| Failure mode | How it's charged |
| --- | --- |
| fades a fizzle | the fizzle's reversal, round trip and fees |
| bets the wrong direction | the shock's size × the position |
| holds through an extension | the extension; past $1,000, zero |
| closes early | the reversion it left |
| breach (cap, market, drawdown, refused call, liquidatable) | zero for the whole grade |

## Predictions

The reference scores 1 by construction and ~0.04 reward. Frozen exploit bots
1–3×. Frontier models after 0.8.0: between the dip-buyer and the regime
oracle, with wide intervals; the 0.9.0 bench will say.

## Generations

| Version | What agents did | What changed |
| --- | --- | --- |
| 0.1.0 | three rebounding crashes | — |
| 0.2.0 | buying every crash paid | stalls and slides; 12 stratified hidden sessions; a 100 SOL cap |
| 0.3.0 | beat the reference 5× | a stronger reference: tranches, a slide stop |
| 0.4.0 | shorted the open: 2.4× with no skill | crashes and squeezes 50/50; a hindsight bound; a drawdown limit |
| 0.5.0 | read equal steps and a fixed tail: 3.5–3.9× | uneven steps and gaps, independent legs, one range for pauses and holds, a random tail, wobble; SPCX |
| 0.6.0 | rode legs that always ran ≥ 3% in steps, dated the aftermath from the end: ~6×, profitable in every session | — (mandate only) |
| 0.8.0 | — | legs 0.6–8% log-uniform, 70% gaps, fizzles, duration drawn first; 36 hidden sessions; exploit checks |
| 0.8.0 → 0.9.0 | frozen exploit bots made 3–6× the reference on fresh sets; the old reference (two fading tranches) made $525–$759 on 36 sessions, too little to rank the ladder's rungs reliably | a lockstep world; hidden sets drawn per batch from a secret; the reference graded alongside; `skill`; the ladder with a regime oracle; a new reference that rides each move and fades it once it stops ($647–$2,777 on the same sets) |
