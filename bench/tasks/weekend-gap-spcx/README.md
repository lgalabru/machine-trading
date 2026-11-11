# solana-foundation/weekend-gap-spcx

The agent's bot trades Phoenix's SPCX (SpaceX) perpetual over a weekend,
while SpaceX's own market is closed, and through the reopen.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade, stratified by who trades (26 informed trader and worker, 17 informed alone, 17 worker alone, 12 neither); 6 practice. Each session is a closed period of 100–160 s of market time, then 12–40 s after the reopen. |
| Mandate | 70 SPCX position cap (about 1× collateral); $1,000 loss limit (10% of collateral) |
| Bound | Hindsight: the full cap held the right way through every move of the perp, the reopen's included, at no cost |
| Reference | Reward ~0.22. It forecasts the perp from the flow (a model of who is informed plus trees fitted to generated sessions), trades the closed period on it, and holds a position sized for the gap into the reopen. |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it works out where SPCX will open from the
perp's own weekend: which off-hours flow knows something, how far it has
taken the perp, and whether it's done. It holds that gap into the reopen,
sized so that a wrong read can't breach its loss limit.

- **Wins:** reading each account's flow on chain and whether its impact
  stays; following the informed trader, past its level while it's still
  trading; fading a worker's order and the drift; sizing for the open's
  spread.
- **Loses:** treating Phoenix's closed-period index as fair value (it's the
  perp's own price, smoothed); following the perp's weekend move, or fading
  it, whoever moved it; following the most one-sided account; holding the
  cap into the reopen on an uncertain read.

## The market

- **Closed-period pricing, as Phoenix does it:** the index follows the
  perp's own price (an EMA, never more than 3% behind), the mark sits on the
  index, the perp's price stays within 1/15 of the last external price, and at the
  reopen the index and the makers snap to the open, within one step.
- **The weekend:** 60% of weekends have news someone trades on (the open
  0.5–12% away, either way); the rest are quiet (0.2–3%). The open adds a
  surprise of 0.2–0.6%.
- **The flow:** eight taker accounts send market orders through the same
  RPC. An informed trader (the news weekends) moves the perp toward the
  open for good; a worker (60%, independently) works a 12–50 share order
  one way, and its impact decays; the rest is noise. Who is who is drawn per
  session; impact per share (0.08–0.2%) and the decay's half-life (60–240
  slots) too. The makers' price also drifts with no trade behind it.
- **No tells:** the informed trader and the worker trade in the same sizes
  at the same pace; only whether their impact stays tells them apart.

## Scoring

The bound is per session: the hindsight bound above, over the perp's path
and the reopen. Positions are valued at Phoenix's mark, which sits on the
index during the closed period and on the open after it; every session ends
after the reopen. A session where more than a tenth of the takers' orders
fail voids the grade.

## Baselines

| Agent | Reward | What it shows |
| --- | --- | --- |
| oracle (the reference bot) | 0.219 (skill 1.000) | The task is solvable, and the reference is calibrated |
| nop (the starter bot) | 0.0 | Doing nothing earns nothing |
| cheater | 0.0 (tampered, 3 refused calls) | Cheatcodes through the proxy disqualify; `/tests` is unreadable; writing the reward file counts as tampering |
| sneaky | 0.0 | The surfnet's own port answers nothing without its secret path (stopped after 13 sessions: it runs in real time) |

On two fresh 72-session sets (g1; skill; the reference made $14,234 and
$16,732; each rung graded in its own container):

| Rung | Set 1 | Set 2 |
| --- | --- | --- |
| nop | 0.00 | 0.00 |
| follow | −0.15 | 0.05 |
| fade | −0.10 | −0.24 |
| basis | 0.00 (breaches) | 0.00 (breaches) |
| fitted (frozen ridge regression) | 0.20 | 0.27 |
| g0-reference (the structural model, the gap only) | 0.31 | 0.21 |
| reference | 1.00 | 1.00 |
| g0 Sol bots, frozen (three) | 1.02, 1.20, 0.89 | 1.00, 1.17, 0.80 |
| told (who is who, the half-life, the open) | 1.53 | 1.39 |
| path-oracle (the whole path, planned after costs) | 2.25 | 1.99 |
| hindsight (the bound) | 5.16 | 4.44 |

Every order rule holds on both sets with bootstrap odds 1.000, told beats
every frozen bot on both sets (odds 1.000), and no rung outside the ones
allowed to breach was refused or disqualified. The reference's deepest
drawdown was $729.

`just ladder weekend-gap` grades the ladder (`bench/checks/ladder`) on fresh
hidden sets: following and fading the perp's move, trading the basis
against the index, and a regression fitted to generated sessions must score
below the reference; the reference told who is informed, and the one told
where SPCX opens, above it.

## Realism limits

- **A scripted weekend.** The perp's price follows the scenario: the takers'
  orders move it by the scenario's impact, not by the book they hit, and the
  bot's own orders don't move it.
- **The index follows the scripted price.** Phoenix's impact pricing reads
  its own book, so a bot's quotes could move the index; here they can't. The
  index's time constant is compressed with the weekend.
- **The band is the makers'.** The snapshot's Phoenix isn't in a closed
  period, so its matching engine doesn't enforce the band; the makers never
  quote outside it.
- **No funding to speak of.** The mark sits on the index, so the premium
  that funding charges stays near zero over a session.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-29.

## More

- [DESIGN.md](DESIGN.md): the design record (research, variants, levers,
  fairness, generator-fitting).
- [instruction.md](instruction.md): what the agent is told.
