# solana-foundation/funding-carry-spcx

The agent's bot trades Phoenix's SPCX (SpaceX) perpetual while its book
trades at a premium or a discount to its index, and Phoenix's funding pays
the side against the gap.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade (36 positioning days, 36 discovery days); 6 practice |
| Mandate | 70 SPCX position cap (about 1× collateral); $1,000 loss limit |
| Bound | Hindsight on the book, plus the funding a position against every premium would collect |
| Reference | Reward ~0.24 (skill 1). The best g0 GPT-6.1 Sol screen bot, frozen and made replayable: tree models of the next seconds' return after funding. Hidden from the agent: it isn't described in the instruction and practice doesn't run it |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it tells a premium the crowd's positioning
opened from one that information opened, and prices the carry the way
Phoenix pays it. It fades the first (the premium converges and Phoenix's
funding pays it while it bleeds), follows the second (the index catches up
with the book and both move on), and learns from the session's own earlier
premiums which kind of day it is and how long the index takes to follow.

- **Wins:** carry priced with the session's funding parameters and cap;
  watching the index, not just the book; cutting a fade when the index
  starts to follow, and following instead; acting early once the day has
  shown itself.
- **Loses:** fading every premium ("funding pays it, and every gap
  closes": on a discovery day it closes by the index, after a move several
  times the premium); following every premium; waiting for certainty every
  time (the carry bleeds out); the mainnet funding parameters in the
  `/exchange` file; deciding each episode as if it were the first.

## The market

- **Funding** is Phoenix's own program: the interval's average gap between
  the mark and the index, times the interval over the funding period,
  capped per interval, paid to the side against the gap. Each session draws
  its interval (2-6 s), period (16-60 s) and cap (0.6-1.6% of the price)
  and sets them on chain before the bot starts. The mark is priced from
  the book (its mid at the last trade).
- **The day.** Half the sessions are positioning days (5-30% of premiums
  informed), half discovery days (70-95%).
- **Episodes.** The book opens a premium of 0.3-1.5% either way. A crowded
  one bleeds out (halving every 5-30 s, drawn per session); an informed one
  is followed by the index after a lag (1.5-10 s, drawn per session), and
  the index then moves 1.5-6× the premium its way while the book stays
  ahead, before catching up. Both kinds open alike, and every gap closes.
- **Noise.** The premium jitters, the index wobbles, crowded premiums
  sometimes widen first, and fizzles move the index and the book together
  and back.
- **The tape.** Grader-run takers trade both ways every 0.75-2 s, from the
  market-making task's shared accounts, and keep the mark fresh. They fill
  resting orders like any taker.

## Scoring

- **Bound:** 70 SPCX × the starting mark × (the book's total variation +
  the funding against every premium, each at most the cap).
- **Valuation:** at the scenario's book level (the makers' centre), plus
  funding accrued and not yet settled, plus wallet USDC. The drawdown limit
  uses the same value.
- **Void sessions:** a session where more than a tenth of the tape's orders
  fail voids the grade.

## Baselines

Harbor checks on the task's g1 build, before its release, one secret:

| Agent | Reward | What it shows |
| --- | --- | --- |
| oracle (the reference) | 0.237 (skill 1.0001) | The task is solvable, and the reference replays: its PnL matched the verifier's own reference run within $2.40 over 72 sessions |
| nop (the starter bot) | 0.0 | Doing nothing earns nothing |
| cheater | 0.0 | Cheatcodes through the proxy disqualify; writing the reward file counts as tampering |
| sneaky (g0) | 0.0 | It doesn't answer turns, so its sessions run in real time; the 19 graded before it was stopped had PnL 0 and no refused calls |

`harbor check` (Codex, GPT-6.1 Sol): 10 of 11 criteria passed on g0;
the one that failed (the tape's void rule wasn't in the instruction) was
fixed. The leak audit finds no reference, solution, test or ladder file in
the agent's image.

`just ladder funding-carry-spcx` grades the ladder on fresh hidden sets. On two
fresh sets of 30 (skill; reference $13,657 and $14,901, reward about 0.24):

| Rung | Set 1 | Set 2 |
| --- | --- | --- |
| fader (every premium, at once) | -0.43 | -0.45 |
| follower (every premium, at once) | 0.20 | 0.25 |
| practice (g0's model, fitted on the practice family) | 0.23 | 0.35 |
| nosession (g1 model, no session history or wobble) | 0.69 | 0.64 |
| gbm (g1 model, fitted on the hidden family) | 0.78 | 0.70 |
| reference | 1.00 | 1.00 |
| told (the reference told each episode's kind) | 1.38 | 1.34 |
| frozen g0 Sol bots 2 and 3 | 1.06 / 0.72 | 1.01 / 0.83 |
| hindsight | 6.02 | 5.53 |

Every order rule held on both sets, with bootstrap odds of 0.96 or more
per set; every frozen bot loses some sessions and stays below told; no rung
was refused or disqualified. Blind long and short were not rerun for g1.

Screens (GPT-6.1 Sol, Codex, one hour, one secret per batch):

| Generation | Rewards | Skill |
| --- | --- | --- |
| g0 | 0.304, 0.294, 0.279 | 1.66, 1.55, 1.55 (against a reference that raced the funding parameters; see DESIGN.md) |
| g1 | 0.188, 0.204, 0.249 | 0.96, 1.04, 1.27 |

## Realism limits

- **The mark is the book's.** On a surfnet, Phoenix's mainnet mark (a
  median of the oracles with an EMA of the book's gap, other venues' perp
  prices, and the book) can't follow a book premium: the sandbox restamps
  the oracle readings on every scenario move, which freezes the EMA, and
  its oracle templates report the same price for spot and perp. The task
  sets SPCX's mark to its book at setup instead, so funding follows the
  premium as it would through the EMA on mainnet, without the lag.
- **Compressed time.** Funding settles every few seconds over a period of
  under a minute, so a session of a few minutes carries what would take
  days on mainnet's hourly funding.
- **A scripted market.** The premium and the index follow the scenario's
  path; the bot's own trades don't move them, and the tape is noise.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-29.

## More

- [DESIGN.md](DESIGN.md): the design record (research, variants, levers,
  fairness, and why the reference is a fitted model).
- [instruction.md](instruction.md): what the agent is told.
