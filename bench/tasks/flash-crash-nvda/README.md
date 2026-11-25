# solana-foundation/flash-crash-nvda

The agent's bot trades Phoenix's NVDA (NVIDIA) perpetual through crashes
and squeezes at the odds mainnet's own tape measured, beside other takers,
some of them informed, with its own trades moving the price. NVDA is where
the shocks were: 9 of the tape's 93 jumps, among them 2026-11-23's +4%
minute that went on to +16% in half an hour. Until 0.14.0 the task was
`flash-crash-spcx`; 0.15.0 renamed it, as 0.11.0 renamed
`phoenix-flash-crash`.

| | |
| --- | --- |
| Market | Phoenix NVDA perp, the 96-market snapshot of 2026-11-13 (`phoenix-perps-2026-11-13`); the family measured from mainnet's Phoenix tape (`phoenix-tape.20261125.fc443f1e`) and NVDA's calibration (`nvda.20261113.903d1120`) |
| Sessions | 72 hidden per grade (18 quiet; the shocks stratified by direction and aftermath); 6 practice. Each session is 80–180 s of market time. |
| Mandate | 62 NVDA position cap (about 1× collateral at $160); $1,000 loss limit (10% of collateral) |
| Bound | None: scored by v2 alone |
| Reference | v2's 0.5. It reads net taking off the book (who took, it can't tell) and rides the move it calls, or a confirmed move, at the full cap, with a trailing stop and a drawdown halt. |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, told the informed takers' wallets, which holds the way their recent flow points |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot tells a shock from the wobble and the small moves, rides the
shocks that keep going and cuts the ones that turn, sits out the sessions
where nothing happens, and pays for every move it trades. A better one
reads the takers on chain and learns which of them know.

- **Wins:** a threshold above the small moves; the move's way, at a size
  the loss limit survives; a stop that grows with the move; holding what
  keeps going; reading which wallets take ahead of the moves.
- **Loses:** fading every shock (45% keep going); trading every wobble
  (a quarter of the sessions are quiet, and every round trip pays fees and
  impact); no stop; blind bets at the open.

## The market

- **Shocks** in three of four sessions: NVDA crashes or squeezes, 50/50,
  in one to three legs of 1.5–9.5% (Pareto: median about 2%). Then it
  extends (~45%), reverts (~31%) or stalls (~24%): mainnet's 93 jumps
  fifteen minutes on.
- **Quiet sessions** (a quarter): only the wobble (NVDA's own volatility)
  and small moves.
- **Small moves** of 0.5–1.5%, which mostly don't give their move back.
- **Takers:** noise at NVDA's own pace (0.99 orders a minute, median
  $1,000), informed traders who see about ten seconds ahead, chasers who
  take the way the price moved; around a move, the flow NVDA's jumps
  carried (a median $20,500 in their minute).
- **Impact:** the book stays where trades leave it, half for good, the rest
  halving every 30 s; a $10,000 take walks it about 12–15 bp.
- **Stated departures** from mainnet, in the instruction: shocks far more
  common, the aftermath inside the session, direction 50/50, and the
  aftermath odds and leg sizes pooled over all markets' jumps.

## Scoring

Scoring v2 only: 0 doing nothing, 0.5 the reference, 1 the informed bot,
on the same sessions, losses counted 1.5 times. The account is valued at
the scenario's price, not where trades pushed the book.

## Baselines

On three fresh 72-session hidden sets (2026-11-25, Linux): the reference
made $23,681, $19,153 and $24,092 ($23,425, $18,746 and $23,295 weighted as
v2 weighs losses; Sharpe 0.85, 0.76 and 0.72) and the informed bot
$37,375, $31,663 and $41,460: the reference is 0.63, 0.60 and 0.57 of the
informed bot, inside the strength band on each, with no breach. On the 6
practice sessions, $1,223.87 and $1,759.34.

| Rung | What it is | Expected |
| --- | --- | --- |
| `nop` | the starter | 0 |
| `blind-long`, `blind-short` | the full cap from the open | under the reference |
| `dip-buyer` | the cap long once NVDA is 3% down, held | under the reference |
| `momentum` | the cap the move's way once NVDA is 4% from the open, held | under the reference |
| `reference` | reads the tape off the book, rides the move it calls or a confirmed one, with a trailing stop | 0.5 |
| `informed` | told the informed takers' wallets | 1 |
| exploit bots | 0.10.0 and 0.12.0's frozen flash-crash bots | below the informed bot, profitable in at most 75% of sessions |
| probes | coin flips, martingale, limit-hugger, stop-lotto, levered references, breacher, honest blends | in their v2 bands |

## Realism limits

- **One snapshot's book.** Depth and fees are mainnet's on 2026-11-13;
  luthor records no book, so the impact's persistence is the calibration's
  default.
- **No latency race.** The world waits for the bot every step, so thinking
  faster gains nothing within a step.
- **Who the takers are.** The tape doesn't name the taker; the informed
  traders, chasers and noise are the actor library's, sized and paced
  from the tape.
- **No funding.** Sessions last under three minutes.

## More

- [DESIGN.md](DESIGN.md): the design record (research, variants, levers,
  fairness).
- [instruction.md](instruction.md): what the agent is told.
