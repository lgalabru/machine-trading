# solana-foundation/event-follow-through-tsla

Write a bot that trades the Phoenix TSLA perp through a scheduled headline, on an offline copy of Solana mainnet.

| | |
| --- | --- |
| Market | Phoenix TSLA perp, mainnet snapshot of 2026-11-13 (`phoenix-perps-2026-11-13`) |
| Sessions | 72 hidden per grade, each with one headline and one verdict; 6 practice |
| Mandate | TSLA only, ±12 TSLA; $600 loss limit, closed by the risk desk on a breach; a position left at the close is closed at the oracle less 5 bp |
| Score | scoring v2 only: 0 doing nothing, 0.5 the reference, 1 the informed bot |
| Reference | reads every trader account's TSLA position; once the jump is in, marks out each account's takes against the oracle and follows the accounts that keep leading it (fading those that keep losing) into the verdict |
| Anchors | the informed bot is told the informed fund's wallets: never when the headline prints, which way, or how the verdict goes |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reads which of the funds trading after a TSLA headline keeps leading the price, and positions with it before the verdict carries the jump on or takes part of it back.

- **Wins:** staying flat through the print; telling the fund whose takes keep leading the oracle from the one whose takes keep losing, and taking the first one's side before the verdict
- **Loses:** riding or fading every jump nets its costs; holding through the print is a coin flip; chasing the verdict after it prints is too late; following the market's net flow follows two mirrored funds

## The market

Sessions of 1,400 to 2,200 slots on TSLA's fitted price model. A headline prints at a slot drawn from 300 to 600: the oracle jumps 1% to 4%, up or down with equal odds, in 2 to 5 steps. 20 to 260 slots after the jump, the verdict moves it once more by 30% to 80% of the jump, on (follow-through) or back (fade), with equal odds. Two funds of 2 or 3 wallets trade all session at the same pace and sizes, both ahead of the oracle until the jump is done; from then on the informed fund keeps trading ahead of it and the other trades against what it sees. Which is which is drawn per session. Noise traders and chasers trade in every session. Every trader's position is on chain, in its Phoenix trader account.

## Scoring

The account is valued at the scenario's oracle (Phoenix's index, which no order can push), within the session and at its last slot; a position left open is closed there by the desk's broker less 5 bp. A drawdown past $600 is stopped out by the risk desk and counts as a breach. Scoring v2 is the only score (`reward`).

## Baselines

Gate 0's ladder on the engine (`hidden/gate0/`): every rung on the same hidden sessions, two fresh 72-session secrets pooled (144 sessions), total PnL and the share of the informed bot's PnL as scoring v2 weighs it (losses 1.5 times):

| Rung | PnL | Sessions won | v2-weighted share of informed |
| --- | --- | --- | --- |
| nop | $0 | 0 | 0 |
| ride every jump | −$2,161 | 65 | −0.63 |
| fade every jump | $191 | 79 | −0.27 |
| hold through the print | $427 | 77 | −0.43 |
| chase the verdict | −$1,010 | 5 | −0.19 |
| follow the net flow | −$3,206 | 59 | −0.79 |
| reference | $3,557 | 68 | 0.40 |
| informed bot | $8,104 | 137 | 1 |

The order (every non-reading rung and nop < reference < informed) holds on 0.968 of 2,000 bootstrapped sets; the informed bot makes more than twice the best non-reading rung on 0.989 of them. The reference's per-session Sharpe is 0.55.

## Realism limits

- The jump and the verdict are scripted moves of the oracle; real prints come with a burst of quotes and fills around them.
- The two funds are scripted: they see the oracle ahead and don't react to the bot. The contrary fund is a fund that traded the print right and then held the wrong view; real losers are less consistent.
- The snapshot's makers re-centre on every path move; the book between moves holds no memory of earlier takes.

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.
