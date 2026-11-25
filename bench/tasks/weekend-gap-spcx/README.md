# solana-foundation/weekend-gap-spcx

The agent's bot trades Phoenix's SPCX (SpaceX) perpetual into the weekend's
reopen, on a weekend measured from mainnet.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade, half with weekends that went up and half down; 6 practice. Each session is a closed period of 100–160 s of market time (a weekend's 48 hours, compressed), then 12–40 s after the reopen. |
| Mandate | 70 SPCX position cap (about 1× collateral); $1,000 loss limit (10% of collateral) |
| Scoring | v2: 0 doing nothing, 0.5 the reference, 1 the informed bot (told the informed wallets and the process, never the session's draws) |
| Reference | Fades the part of the weekend's move the reopen is expected to take back, sized by its size, entered after the makers' last re-price and held to the end. It doesn't read who trades. |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it knows how much of a weekend's move
Phoenix's reopen keeps, and reads which of the few traders positioning into
the reopen know where it opens.

- **Wins:** fading the take-back by its expected size; reading each wallet's
  flow in the last stretch and keeping the ones that hold one direction;
  entering after the makers' last re-price.
- **Loses:** following the weekend's move; fading every move at the cap;
  following the stretch's net flow, decoys included; holding through the
  weekend's walk; trading the index as fair value.

## The market

Measured (sources in [DESIGN.md](DESIGN.md#where-every-number-comes-from)):

- **The weekend's move** is the makers': one of 21 measured weekends'
  (median 0.86%, 0.05% to 2.75%; Phoenix SPCX candles, 2026-06-12 to
  11-20), jittered, either way, reached by an hourly re-price walk (0.193%
  an hour) bridged to it.
- **The reopen** keeps 0.78 of the move (95% 0.57 to 1.04), give or take
  0.50%.
- **The weekend's prints:** about 31 orders a closed period, around $918;
  one burst order at the reopen, around $3,000. None move the price.
- **No cross-market signal:** BTC's and SOL's weekend moves didn't move with
  SPCX's reopen gap over the 21 weekends.

Stated departures: time is compressed; direction is a coin flip (15 of 21
went up); the weekend book's half-spread (0.4 to 5 bp) wasn't recorded; and
two informed wallets (right 80% each) and two decoys position into the
reopen in its last 40 slots, flow the tape never saw on SPCX.

## Scoring

Positions are valued at Phoenix's mark, which sits on the index during the
closed period and on the open after it; every session ends after the
reopen. A session where more than a tenth of the library's actions fail is
not scored.

## Baselines

0.15.0: see [DESIGN.md](DESIGN.md#generations) for the ladder on its fast
set. Offline (the family's model, 100,000 sessions, losses weighted 1.5):
reference $9.4 a session, informed $23.1, follow −$48.9; reference over
informed 0.41.

`just ladder weekend-gap-spcx` grades the ladder (`checks/`): following the
move must score below the reference, and the reference below the informed
bot; the three frozen g0 Sol bots, fitted to the old family, must stay below
the informed bot. The probe set isn't wired yet (exempt, with its reason).

## Realism limits

- **A scripted weekend.** The makers' price follows the scenario; nobody's
  orders move it (price impact is refused with a closed period).
- **Twenty-one weekends.** The take-back's interval includes 1; the family
  draws its point estimate.
- **The index follows the scripted price.** Phoenix's impact pricing reads
  its own book; here the bot's quotes can't move the index.
- **The band is the makers'.** The snapshot's Phoenix isn't in a closed
  period, so its matching engine doesn't enforce the band.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-29.

## More

- [DESIGN.md](DESIGN.md): the design record (measurements, departures,
  variants, levers, fairness).
- [instruction.md](instruction.md): what the agent is told.
