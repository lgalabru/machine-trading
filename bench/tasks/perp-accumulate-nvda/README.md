# solana-foundation/perp-accumulate-nvda

A bot works a client's buy order in Phoenix NVDA perps by the close, under a price cap, against taker flow that may know where the price is going and a book that is thin near the touch, and decides how much to leave to the prime broker.

| | |
| --- | --- |
| Market | Phoenix NVDA perp, mainnet snapshot of 2026-11-13 (`phoenix-perps-2026-11-13`) |
| Sessions | 72 hidden per grade (22 calm, 25 informed up, 25 informed down); 6 practice |
| Mandate | NVDA only, buys only, position 0 to Q (20 to 45), a price cap 0.8% to 2% over arrival; a loss limit of 5% of the order's notional |
| Score | scoring v2 only: 0 doing nothing, 0.5 the reference, 1 the informed bot |
| Reference | works the order passively, reads the flow through the grader's maker's lean, buys ahead of a confirmed rise and stands aside ahead of selling |
| Anchors | the informed bot is told the session's kind, the move's timing and size, and the maker's settings, and nothing of the walk or the noise |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reads whether one-sided taker flow is informed, paces its buying to how fast the book recovers from its own takes, and leaves to the broker what is cheaper to leave.

- **Wins:** front-loading ahead of informed buying, standing aside ahead of informed selling, slicing at the pace the maker refills
- **Loses:** fixed schedules lose in one of the mirrored regimes; trading every burst loses to fake-outs; sweeping the book pays the backstop

## The market

Sessions of 1,200 to 2,000 slots on NVDA's fitted price model. A session is calm (30%) or carries an informed move of 0.71% to 1.14% up or down (35% each), from an onset 15% to 70% into it, over 40 to 240 slots. A burst of taker orders trades the move's way 20 to 100 slots ahead of it, and the noise leans its way after; 0 to 2 fake-out bursts, drawn alike, come in every session. Near the touch the book is the grader's maker (2 to 5 NVDA at 3 to 6 bp, requoting every 2 to 12 slots, leaning 1 bp a unit against its inventory); the snapshot's makers are a deep backstop 15 to 30 bp out. The order is Q of 20 to 45 NVDA; the broker takes the rest at the closing index plus 10 to 30 bp.

## Scoring

A session's PnL is what the bot saved against the broker buying all of Q at the close: `Q close (1 + h) - (fills and fees + (Q - bought) close (1 + h))`, valued at the scenario's own closing index, which no order can push. Doing nothing is exactly 0. A sell fill, a resting ask, or a fill over the price cap breaks the mandate. Scoring v2 is the only score (`reward`).

## Baselines

The fast ladder (`mtx gates --fast`: one set of 12 sessions from the public label `gates-fast-v1`), total PnL and scoring v2's raw score:

| Rung | PnL | v2 raw |
| --- | --- | --- |
| nop | $0 | 0.00 |
| dump (the whole order at the open) | −$41 | −0.78 |
| TWAP-cross | $59 | 0.27 |
| TWAP-passive | $1 | −0.04 |
| reference | $70 | 0.48 |
| informed bot | $137 | 1 |
| cap-blind (exploit) | $58, one cap breach | −1 |
| sell-back (exploit) | −$159, a sell in every session | −1 |

The order rules (`dump < TWAP`, `max(TWAP) < reference < informed`) hold; the reference's weighted PnL is 0.49 of the informed bot's, with a per-session Sharpe of 0.85. The Harbor checks (oracle at 0.5, nop at 0, cheaters) need the task image, which waits for the infra the task depends on.

## Realism limits

- The near-touch book is one grader maker; real NVDA perp books have several makers that requote on every oracle update.
- The snapshot's makers re-centre on every path move and don't refill otherwise; they are widened to a backstop rather than modelled as a resilient curve.
- Informed flow is scripted ahead of each move; it doesn't react to the bot.
- The broker's price is the closing index plus a fixed haircut; a real broker's risk price would move with the size left and the session's volatility.

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.
