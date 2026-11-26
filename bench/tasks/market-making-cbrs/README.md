# solana-foundation/market-making-cbrs

The agent's bot makes markets in Phoenix's CBRS (Cerebras) perpetual, a
thin book with a wide spread, against a competing maker, an arbitrageur,
and takers whose sizes and pace are measured from Phoenix's CBRS on
mainnet. Until 0.14.0 this task was `market-making-spcx`.

| | |
| --- | --- |
| Market | Phoenix CBRS perp, mainnet snapshot of 2026-11-25; flow from the tape study `phoenix-tape.20261125.3cb49758` |
| Sessions | 72 hidden per grade (40 calm, 28 trending, 4 with a jump), 4 to 6.7 minutes each; 6 practice |
| Mandate | 33 CBRS position cap (about 0.3× collateral); $250 loss limit; takes only to reduce |
| Score | Scoring v2: 0 doing nothing, 0.5 the reference, 1 the informed bot |
| Reference | It quotes a tick inside the competing makers around the index, skews against inventory, and steps off a side after one-sided fills or a one-sided run on the tape. It never takes. |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

CBRS is where Phoenix's makers earn: luthor's mainnet paper maker, quoting
inside a 13.9 bp half-spread, made about +7 bp a fill after fees. A good bot
quotes inside the wide spread with size the flow can bear, keeps its
inventory small, and requotes when the index moves before the arbitrageur
picks off what it left behind.

- **Wins:** quoting inside the competitor, skewing inventory away, showing
  size when the flow is uninformed, requoting on every index move.
- **Loses:** quotes left behind by a move, inventory carried through a
  trend or a jump, quoting outside the competitor (no fills).

## The market

- **The book:** the grader's reactive maker at 10.4 to 18.1 bp from the
  index (around CBRS's measured 13.9 bp touch), 2 to 5 CBRS, post-only;
  Phoenix's own makers widened to the same, following the index 1 to 2 s
  late.
- **The price** walks at CBRS's measured 3.44 bp a second (SPCX's
  calibrated walk scaled; CBRS has no calibration). 40 of 72 sessions are
  calm, 28 trend, 4 have one jump of 1.5 to 4.2%.
- **The takers** are the actor library's noise takers at CBRS's measured
  sizes (median $298) and 30 times its measured pace (0.185 orders a
  minute: a session would hold one), and the pinger's $3 print a minute.
  On the tape CBRS's takers lose; here they know nothing.
- **The arbitrageur** takes any quote more than 3.5 to 5 bp from Phoenix's
  index: at a 1-slot latency before the bot's turn, at 2 slots after it.
- **Turn order:** takers, the move, the grader's maker, a fast arbitrageur,
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
`just ladder market-making-cbrs` grades it (`checks/`): naive and skewed
quoters below the reference, the reference below the informed bot, the
frozen exploit bots below the informed bot.

## Realism limits

- **Flow multiplier:** 30 times CBRS's measured pace, and sessions twice
  the other perp tasks' length (stated).
- **Takers' markout:** on the tape CBRS's takers lose 7.6 bp a minute on;
  here they know nothing.
- **Order sizes** are lognormal; on the tape they are nearly one size.
- **Jumps** are about 70 times Phoenix's rate (stated).
- **Volatility** is CBRS's measured level (an upper bound) on SPCX's
  calibrated shape.
- **No transaction layer, no price impact, no funding, no transaction fees.**

## More

- [DESIGN.md](DESIGN.md): the design record (research, variants, levers,
  fairness).
- [instruction.md](instruction.md): what the agent is told.
