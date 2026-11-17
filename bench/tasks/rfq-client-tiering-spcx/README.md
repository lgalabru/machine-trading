# solana-foundation/rfq-client-tiering-spcx

Write a bot that runs an RFQ desk in SPCX tokens for a book of clients, a few of whom know where SPCX is going, on an offline copy of Solana mainnet.

| | |
| --- | --- |
| Market | An RFQ desk in SPCX tokens, valued at Phoenix's SPCX index; mainnet snapshot of 2026-10-29 (`phoenix-spcx-2026-10-29`) |
| Sessions | 72 hidden per grade, every one drawn alike (no strata); 6 practice |
| Mandate | answers to requests only (perp cap 0.001 SPCX); $400 loss limit on the desk's value over the idle desk |
| Score | scoring v2 only: 0 doing nothing, 0.5 the reference, 1 the informed bot |
| Reference | learns the reach from its own answers, probes each client once just past the widest reach, marks every request out, and declines the clients it flags and their side of the flow |
| Anchors | the informed bot is told which clients are informed, how far ahead they see and how much of it they act on, and the reach; never the path, or when or which way anyone asks |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it prices each client to its reach and declines the clients whose requests turn out to be informed, learning who they are from a handful of requests per client.

- **Wins:** quoting benign clients just inside their reach, flagging informed clients early from markouts and from who takes a quote no uninformed client would, and reading the flagged clients' sides as the coming move
- **Loses:** quoting everyone pays every informed fill; quoting wider loses the benign flow entirely past the reach; tiering by size or leaning against recent request flow catches little of the informed flow

## The market

Sessions of 1,200 to 2,000 slots on SPCX's fitted price model (regular hours, its jumps included). 10 to 16 clients ask for one-sided prices, each 3 to 10 times a session on one Poisson schedule, in sizes lognormal around 0.75 SPCX; 15% to 45% of them, redrawn every session, see the level 45 to 120 slots ahead and ask its way. A client takes a price within its reach of its value: 0.8 to 1.6 bp a lot of 0.25 SPCX, times the square root of the size in lots, the same for every client. Requests, fills and the index are all the bot sees; ids name the client and nothing else.

## Scoring

A session's PnL is the desk's value change over the idle desk: the wallet's SPCX at Phoenix's index, less what its starting 20 SPCX would have made held. Doing nothing is exactly 0. Scoring v2 is the only score (`reward`).

## Baselines

The ladder on three fresh hidden sets of 72 sessions (`bench/checks/ladder`), total PnL as a multiple of the reference's (the reference made $64, $67 and $84):

| Rung | Set 1 | Set 2 | Set 3 |
| --- | --- | --- | --- |
| nop | 0 | 0 | 0 |
| quote-all (every request just inside its reach) | −1.08 | 0.43 | −0.51 |
| quote-wide (3 bp past the reach) | −1.28 | −0.48 | −1.05 |
| fixed-width (2 bp) | −1.03 | −0.01 | −0.59 |
| size-tier (only requests of 5 lots or more) | −0.12 | 0.25 | 0.05 |
| side-skew (against the recent requests' lean) | 0.35 | 0.36 | 0.38 |
| reference | 1 | 1 | 1 |
| informed bot | 2.22 | 1.89 | 1.60 |

Every order rule holds on 3 of 3 sets, bootstrap odds 0.98 or better. The reference's weighted PnL is 0.53 of the informed bot's over the three sets, with a per-session Sharpe of 0.56; the informed bot makes 5.2 times the best careful rung. HARBOR_RESULTS

## Realism limits

- One reach for every client in a session; real clients differ in urgency and size their tolerance to their own alternatives.
- Informed clients act on a perfect view of the level a fixed lead ahead, scaled by their correlation; real informed flow is noisier and decays.
- Clients don't adapt to the desk: a declined client keeps asking at the same rate, and a client never shops the request to other dealers.
- No hedging: the desk can't trade the perp, so inventory rides at the index.
- The index the desk sees is the level in cents, about half a basis point off what the clients value at.

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.
