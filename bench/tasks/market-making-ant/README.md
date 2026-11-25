# solana-foundation/market-making-ant

The agent's bot makes markets in Phoenix's ANT (Anthropic) perpetual,
against a competing maker, an arbitrageur, and takers whose pace, sizes and
informedness are measured from Phoenix's ANT on mainnet. Until 0.14.0 this
task was `market-making-spcx`.

| | |
| --- | --- |
| Market | Phoenix ANT perp, mainnet snapshot of 2026-11-25; flow from the tape `phoenix-tape.20261125.fc443f1e` |
| Sessions | 72 hidden per grade (40 calm, 28 trending, 4 with a jump); 6 practice |
| Mandate | 35 ANT position cap (about 0.3× collateral); $250 loss limit; takes only to reduce |
| Score | Scoring v2: 0 doing nothing, 0.5 the reference, 1 the informed bot |
| Reference | It quotes a tick inside the competing makers, skews against inventory, and steps off a side after one-sided fills or a one-sided run on the tape. It never takes. |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot earns the spread where the flow pays for it and keeps out of the
way where it doesn't. Takers are a little informed on average (a minute
on, the price has moved about 4.4 bp their way, as on Phoenix's ANT, where
they paid 2 to 3 bp to cross), the touch is about 2.7 bp, and an
arbitrageur takes any quote a move left stale.

- **Wins:** turning inventory over fast, requoting before a move leaves a
  quote behind, stepping back from one-sided flow, reading who trades.
- **Loses:** resting quotes the informed run over, quotes left behind by a
  move, warehousing inventory through trends and jumps.

## The market

- **The book:** the grader's reactive maker at 2.0 to 3.5 bp, 2 to 5 ANT,
  and Phoenix's own makers widened to the same, with their mainnet depth.
- **The price** walks at SPCX's calibrated 1.55 bp a second (ANT has no
  calibration yet). 40 of 72 sessions are calm, 28 trend, 4 have one jump
  of 1.5 to 4.2% (about 70 times Phoenix's rate).
- **The takers** are the actor library's: eight noise wallets and four
  informed ones that see a minute ahead, trade while the move is over 7 bp,
  and are right 85% of the time, at ANT's measured pace (11.6 orders a
  minute) and sizes (median $100).
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
`just ladder market-making-ant` grades it (`checks/`): naive and skewed
quoters below the reference, the reference below the informed bot, the
frozen exploit bots below the informed bot.

## Realism limits

- **Volatility** is SPCX's calibrated one: ANT has no calibration yet.
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
