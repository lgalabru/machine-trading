# solana-foundation/smart-money-decay-nvda

Write a bot that trades Phoenix NVDA perps off the tape: it follows the wallets that trade ahead of moves, drops the ones whose edge dies, and ignores the ones that chase, on an offline copy of Solana mainnet.

| | |
| --- | --- |
| Market | Phoenix NVDA perp, mainnet snapshot of 2026-11-13 |
| Sessions | 72 hidden per grade, each with 3 to 6 sharp moves; 6 practice |
| Mandate | 30 NVDA position cap; $500 loss limit; a position left at the end closes at the oracle less 5 bp |
| Bound | none: scoring v2 alone |
| Reference | follows the wallets whose takes have marked out well lately, and drops chasers and turned records: about 0.37 of the informed bot, weighted as v2 weighs it |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, told which wallets are informed and which are decoys, and nothing of the moves |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it keeps a record of each wallet on the tape, follows the ones the price rewards, notices when a record turns, and tells flow that leads a move from flow that follows it.

- **Wins:** per-wallet markouts with a decaying record, following credible wallets net of chasers, exiting at the information horizon
- **Loses:** following the market net flow or the price move pays costs on chasers and decoys; following a wallet on its lifetime record keeps following decoys after they turn

## The market

Sessions of 1,200 to 2,000 slots on NVDA's fitted price model, with 3 to 6 sharp moves of 0.26% to 0.60%, up or down with equal odds, each landing within two slots. One to three informed wallets take each move's way from 20 to 80 slots ahead of it; one to three decoys trade exactly like them until 20% to 60% of the way through the session, then in bursts with the same rhythm on random sides; two to four chasers take the way the book went once it moved 8 to 20 bp in 40 slots; three to six noise wallets trade at random. Every role is seated in every session on fresh wallets, with one size distribution. Every take is an ordinary Phoenix transaction, readable from the chain.

## Scoring

The account is valued at the oracle, which no order can push; a position left at the end is closed by the desk's broker at the oracle less 5 bp. Scoring v2 is the only score.

## Baselines

The engine's ladder on two 72-session hidden sets (total PnL over 144 sessions):

| Rung | PnL | Per session |
| --- | --- | --- |
| nop | $0 | 0 |
| fade the market's net flow | −$188 | −1.31 |
| follow the market's net flow | −$555 | −3.85 |
| blind short / long, the full cap | −$781 / −$1,114 | −5.43 / −7.74 |
| follow the price's move | −$4,008 | −27.83 |
| follow the busiest wallet | −$12,950 | −89.93 |
| reference | $3,356 | +23.31 |
| informed bot | $8,389 | +58.26 |

The reference's weighted PnL is 0.37 of the informed bot's, with a per-session Sharpe of 0.91; the order (every rung that doesn't read wallets < reference < informed) holds on 1.000 of bootstrapped 72-session sets.

## Realism limits

- The moves are scripted jumps on the fitted path; the informed traders see them ahead and don't react to the bot.
- Chasers read the scenario's book path, not the book the bot leaves: no bot can set them off.
- The book's makers are the snapshot's, re-centred on the path, with a refill for the depth takers use.

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.
