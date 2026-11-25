# solana-foundation/market-making-spcx

The agent's bot makes markets in Phoenix's SPCX (SpaceX) perpetual, against
a competing maker, an arbitrageur, and takers whose pace, sizes and
informedness are measured from Phoenix on mainnet.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29; flow from the tape `phoenix-tape.20261125.fc443f1e` |
| Sessions | 72 hidden per grade (36 calm, 24 trending, 12 with a jump); 6 practice |
| Mandate | 20 SPCX position cap (about 0.3× collateral); $250 loss limit; takes only to reduce |
| Score | Scoring v2: 0 doing nothing, 0.5 the reference, 1 the informed bot |
| Reference | It quotes a tick inside the competing makers, skews against inventory, and after one-sided fills stops quoting the side they hit for a while. It never takes. |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot earns the spread where the flow pays for it and keeps out of the
way where it doesn't. Takers are a little informed on average (a minute
on, the price has moved about 1.3 bp their way, as on Phoenix), the touch
is one tick wide, and an arbitrageur takes any quote a move left stale.

- **Wins:** quotes placed where the big orders walk to, a lean against
  inventory, requoting before a move leaves a quote behind, stepping back
  from one-sided flow.
- **Loses:** quoting at the touch for a tick (the maker fee is 0.5 bp),
  quotes left behind by a move, warehousing inventory through trends and
  jumps.

## The market

- **The book:** the grader's reactive maker at one to two ticks, 0.5 to 2
  SPCX; Phoenix's own makers widened to 3 to 8 bp behind it, with their
  mainnet depth. SPCX's real touch was one tick wide (luthor, one minute of
  quotes, 2026-11-25).
- **The price** walks at SPCX's calibrated 1.55 bp a second. Half the
  sessions are calm, a third trend, a sixth have one jump of 1.5 to 4.2%.
- **The takers** are the actor library's: eight noise wallets and four
  informed ones that see a minute ahead and are right 85% of the time, at
  SPCX's measured sizes (median $1,000) and 35 times its measured pace
  (0.33 orders a minute in regular hours: a session would hold one order).
- **The arbitrageur** takes, after each move and before the bot's turn,
  any quote more than 3.5 to 5 bp from Phoenix's index.
- **Turn order:** takers, the move, the grader's maker, the arbitrageur,
  the bot.

Every number and every departure from the measured market is in
[DESIGN.md](DESIGN.md#the-market-measured-0150).

## Scoring

Scoring v2 (see the [dataset README](../README.md#how-a-bot-is-judged)).
The informed bot is the reference told which wallets are the informed
takers: it leans with what they trade and pulls the side they hit. The
account is valued at the scenario's oracle price, not Phoenix's mark.

## Baselines

See DESIGN.md's Detectability for the ladder measured on this release.
`just ladder market-making-spcx` grades it (`checks/`): naive and skewed
quoters below the reference, the reference below the informed bot, the
frozen exploit bots below the informed bot.

## Realism limits

- **Flow multiplier:** 35 times SPCX's measured pace (stated).
- **Jumps** are far more common than on Phoenix (stated).
- **No transaction layer, no price impact:** the actors don't race the bot
  for block space, and the takers' fills don't move the price.
- **The book** beyond the touch is the snapshot's makers' curve; luthor
  records no depth yet.
- **No funding, no transaction fees.**

## More

- [DESIGN.md](DESIGN.md): the design record (research, variants, levers,
  fairness).
- [instruction.md](instruction.md): what the agent is told.
