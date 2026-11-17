# solana-foundation/hunt-or-breakout-tsla

A bot trades the Phoenix TSLA perp through runs at round price levels, fading stop hunts and following real breaks.

| | |
| --- | --- |
| Market | Phoenix TSLA perp, mainnet snapshot of 2026-11-13 (`phoenix-perps-2026-11-13`) |
| Sessions | 72 hidden per grade, each with 1 to 3 stop hunts and 1 to 3 real breaks; 6 practice |
| Mandate | TSLA only, ±12 TSLA; $500 loss limit; a position left at the close is closed at the oracle less 5 bp |
| Score | scoring v2 only: 0 doing nothing, 0.5 the reference, 1 the informed bot |
| Reference | reads every trader account's TSLA position; learns over the session which pushing wallet is the stop hunter and which the informed breaker (did the oracle follow its run?); rides and fades a known hunter's runs, follows a known breaker's, leaves unknown pushers alone |
| Anchors | the informed bot is told who the hunter's and the informed traders' wallets are, the hunter's hold and trigger, the grid and the informed horizon: never where, when or which way a run goes, or what kind it is |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reads who leads a run through a round level (one hunter wallet pushing, or informed wallets building) and whether the oracle follows, then fades the hunt or follows the break before it resolves.

- **Wins:** fading a hunter push while the book is through the level; following informed flow into a break before the level jumps
- **Loses:** fading every run loses the breaks; following every run loses the hunts; waiting for the oracle enters breaks late and misses hunts that end first

## The market

Sessions of 1,400 to 2,200 slots on TSLA's fitted price model. Every $2 is a round level. Each session has 1 to 3 stop hunts and 1 to 3 real breaks, in random order: before each, the price drifts to just short of the nearest level; then the book's makers jump 0.15% to 0.45% through it while the oracle stays put. In a hunt the book comes back after the hunter's hold (10 to 60 slots); a single trader (one wallet) pushed 2.4 to 4.8 TSLA through the level a couple of slots before and trades the same size back out. In a break the oracle creeps toward the level for 10 to 60 slots, then jumps on through the book, 0.15% to 0.65% of the level beyond the excursion; an informed breaker (one wallet) sweeps through the level a couple of slots before exactly as the hunter pushes, and two or three informed wallets, who see the oracle 20 to 60 slots ahead, trade it before it goes. Noise traders and chasers trade in every session. Every trader's position is on chain, in its Phoenix trader account.

## Scoring

The account is valued at the scenario's oracle (Phoenix's index, which no order can push), within the session and at its last slot; a position left open is closed there by the desk's broker less 5 bp. Scoring v2 is the only score (`reward`).

## Baselines

Gate 0's ladder on the engine (`hidden/gate0/gate0-redesign.txt`): every rung on the same 72 hidden sessions from one fresh secret, total PnL and the share of the informed bot's PnL as scoring v2 weighs it (losses 1.5 times):

| Rung | PnL | Sessions won | v2-weighted share of informed |
| --- | --- | --- | --- |
| nop | $0 | 0 | 0 |
| follow every run | −$2,382 | 8 | −0.52 |
| fade every run | −$1,440 | 18 | −0.33 |
| wait for the oracle | −$475 | 34 | −0.14 |
| push-fade (the first screen's bot: push size and flatness, no identity) | $644 | 40 | 0.04 |
| reference | $3,383 | 62 | 0.48 |
| informed bot | $6,990 | 72 | 1 |

The order (every non-reading rung and nop < reference < informed) holds on 1.000 of 2,000 bootstrapped sets; the informed bot makes more than twice the best non-reading rung on every one (every non-reading rung loses), and beats push-fade on every one. The reference's per-session Sharpe is 1.41.

## Realism limits

- A run is the makers repricing as a premium on the path; real makers reprice on their own fills and inventory.
- Stops are implied, not modelled as resting orders that fire.
- The hunter and the informed traders are scripted: they don't react to the bot.
- The snapshot's makers re-centre on every path move; the book between moves holds no memory of earlier takes.

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.
