# Design record: toxic-day-make-or-follow-tsla

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. It is card 2 of the batch-4 cards
(`TASK_CARDS_BATCH4.md`), redesigned for the jump measurement
(`JUMP_REGIME_MEASUREMENT.md`). The departures from the card are listed
last.

Status: Gate 0 on prototypes (see `hidden/gate0/README.md`).

## Thesis

A good bot profits here because it reads from the whole tape whether this
morning's takers know where the price goes, and makes or follows
accordingly.

- **Wins:** marks out the market's takes against the oracle, makes while
  they are noise, and leans with the takers' net flow once their markouts
  show they lead the price.
- **Loses:** making into an informed tape is picked off; following a noise
  tape pays the spread and fees on every lean; a fixed flow threshold
  follows the same bursts every morning, informed or not.
- **Regimes:** a toxic morning (the fund's takes are 35-50% of the taker
  volume) or a quiet one (10-20%), even odds, with the same expected taker
  volume.
- **Mandate:** TSLA only, within 8 either way, a $400 loss limit (a breach is
  stopped out); a position left at the close is closed at the oracle less
  5 bp.

## The session: TSLA's opening half hour

Event days don't jump more than ordinary ones; the opening hour does, every
day (`JUMP_REGIME_MEASUREMENT.md`, Table 2; `docs/market-calibration.md`).
So the session is the open, not a news day:

| Setting | Value | Source |
| --- | --- | --- |
| Start | the 09:30 regular open (`start = before regular, lead 0`), no open gap | the calendar of `tsla.20261113.8b7bca8e.toml` |
| Length | 3,000-4,500 slots (20-30 minutes) | the boss's brief |
| Jump rate | 3.9 an hour (`jump_rate_per_hour`) | TSLA 09:31-10:30, all days, the calibration's jump rule |
| Jump size | median 0.382%, p90 0.652% (`[jump_size]`), not scaled by `vol_mult` | the same window |
| Diffusive volatility | `vol_mult = 2.0` | TSLA's 1 m sd is 22.3 bp at 09:31-10:00 against 7.4 bp midday; less the stated jumps' share (about 11.6 bp of it), the diffusive part is about 19 bp against the calibration's median 8.8 bp |

At 3.9 an hour a session sees 1.3-2.0 jumps; 14-27% of sessions (by
length) have none. The anchors have to hold on those, and the table in
`hidden/gate0/README.md` splits them out.

## Who is told what (checklist item 6), settled first

The card's latent is the kind of morning, quiet or toxic, drawn per session
with even odds. That is a realised draw of the session, not a fixed process
fact: telling it would tell the informed bot which half of the sessions it
is in, the class card 1 and card 8 lost their informed bots for. So the
informed bot is **not** told the morning.

It is told an identity instead: **the fund's wallets**
(`{"roster": ["informed"]}`, resolved to the session's wallets as it
starts). Every session seats the fund, with 6-10 wallets on either kind of
morning, drawn from the same range, and noise and chasers from wallets of
their own. The roster says who the fund is, and nothing about how busy it
will be, when it trades, or where the price goes. The informed bot learns
the morning the way the reference does, from the tape, but reads the fund's
own takes instead of the whole market's.

What a careful reader can learn in the session (item 7): the market's takes
marked out against the oracle; their mean markout is higher on a toxic
morning because a larger share of them come from the fund. The fund trades
from 6-10 wallets, a few takes each, so per-wallet records are thin; the
aggregate is the main route, and reading individual wallets is an allowed
second one.

## Rungs

| Rung | What it does | Reads |
| --- | --- | --- |
| informed | told the fund's wallets; leans with any net take of theirs in the last 10 slots, held 40 slots after; makes between | the fund's takes |
| reference | marks out every take (60 slots); once 30 steps are marked, a mean over the bar means toxic: lean with the takers' net flow past 1.5 TSLA in 20 slots; else make | the whole tape's markouts |
| flat | never trades | nothing |
| make | makes all session (one TSLA a side at the touch, post-only) | nothing |
| follow | leans with the takers' net flow past 1.5 TSLA all session | the flow, not whether it is informed |
| switch | leans past 3 TSLA, makes between: a fixed threshold | the flow, not whether it is informed |

## Departures from the card

- **The regime is the opening half hour, not a news-heavy session.** The
  card asked for 4-6 jumps an hour all session; the measurement found that
  only in TSLA's opening hour (3.9 an hour, every day). The card's
  1,800-2,400 slots become 3,000-4,500.
- **The informed bot is told the fund's wallets, not the kind of morning**
  (above). The card said no wallet is told; that was for an informed bot
  told the day type, which the boundary doesn't allow.
- **Edge 20-30 bp** (card: 15-20) so a lean clears the ~10 bp round trip.
