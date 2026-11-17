# Design record: toxic-day-make-or-follow-tsla

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. It is card 2 of the batch-4 cards
(`TASK_CARDS_BATCH4.md`), redesigned for the jump measurement
(`JUMP_REGIME_MEASUREMENT.md`). The departures from the card are listed
last.

Status: Gate 0 failed on version 2 once main's #114 made informed wallets err; the bots were adapted (flow over the longest horizon, no flips) and Gate 0 rerun once (`hidden/gate0/README.md`).

## Thesis

A good bot profits here because it reads from the whole tape whether this
morning's bursts of one-sided flow know where the price goes, and follows
them on a morning they do and stands aside on one they don't.

- **Wins:** marks out the market's bursts against the oracle, and leans
  with them once their markouts clear a round trip's costs.
- **Loses:** following every burst pays a round trip for each algo parent
  on a quiet morning; standing aside every morning earns nothing; making at
  the open is run over by the drift under one-sided flow.
- **Regimes:** two funds trade in bursts every session, an informed fund
  and an execution algo; on a toxic morning the fund is the busy one (60-90
  times the market's trade rate while it trades, the algo a quarter of
  that), on a quiet morning the algo is. Even odds; the bursts' sizes,
  pace and rhythm are the same either way.
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

It is told an identity instead: **the informed fund's wallets**
(`{"roster": ["informed"]}`, resolved to the session's wallets as it
starts). Every session seats the fund and the algo, each from 6-10 wallets
of its own drawn from one range, on either kind of morning. The roster says
who the fund is, and nothing about how busy it will be, when it trades, or
where the price goes. The informed bot follows the fund's own takes, on a
toxic morning (its bursts are big) and a quiet one (they are small); it
never needs the morning.

What a careful reader can learn in the session (item 7): the bursts of net
taker flow, marked out against the oracle. On a toxic morning the big
bursts are the fund's and lead the price; on a quiet morning they are the
algo's and don't. A session has a few bursts (the fund's edge holds about
5% of the time, and the algo works as often), so the read takes a real part
of the session and is noisy. Reading individual wallets is a second route:
each wallet carries a sixth to a tenth of its fund's takes.

## Version 1 (dev runs only), and why it was reworked

Version 1 had one informed fund whose share of the taker volume the
morning set (35-50% toxic, 10-20% quiet), with noise making up the rest,
and asked the bot to make on a quiet morning. Two dev sets (12 and 16
sessions, their own secrets) showed it could not pass:

- Making at the open loses on both mornings (-$14 and -$8 a session): the
  fills' own markout is about -$1, and the rest is the inventory's drift
  under one-sided flow (fund and chasers).
- The fund trades only while its edge holds, about 5% of the time, so it
  trades in bursts on either morning; a burst is informed whatever the
  morning, so the morning doesn't decide anything. With its pace capped to
  keep bursts plausible, its share fell to 0-26% and the two mornings
  merged.
- No untold rung made money: the reference -$22 and -$2 a session,
  following the net flow -$84 and -$116, following bursts -$25, while the
  told bot made +$25 and +$18.

Version 2 keeps the session and the told fact, drops making from the
reference, and makes bursts ambiguous: an execution algo (a new generic
library actor, `algos`) works parent orders in bursts with the fund's
rhythm, and the morning picks which of the two is busy.

## The fund's hit rate (checklist item 16d)

The fund runs at the library's `hit_rate` 0.85 (main's #114): each of
its wallets, for one `horizon_slots` at a time, reads the move the wrong way
with chance 0.15 and trades against it, on a view of its own. Michael kept
0.85 (2026-11-17) because an informed trader is right on average and not
always. A fund that is never wrong would make its single takes a perfect
signal, and the card would test copying, not reading. So every bot that
follows flow reads it over at least one horizon (150 slots, the longest a
fund trades on), needs wallets to agree (at least two that way and more
than the other way), and never flips a lean after entry. The instruction
may state that the fund is right about 85% of the time.

## Rungs

| Rung | What it does | Reads |
| --- | --- | --- |
| informed | told the fund's wallets; leans with their net flow over the last 150 slots past 1 TSLA, with at least two wallets that way and more than the other way, held 40 slots after the last signal its way, never flipped; flat between | the fund's takes |
| reference | marks out each burst (net taker flow past 5 TSLA over 150 slots, at least two accounts that way and more than the other way, each new side) 60 slots on; once two are marked, a mean over 10 bp means toxic: lean with the bursts, never flipped; else flat | the bursts' markouts |
| flat | never trades | nothing |
| make | makes all session (one TSLA a side at the touch, post-only, inventory within 2) | nothing |
| follow | leans with the takers' net flow past 2.5 TSLA over 150 slots all session | the flow, not whether it is informed |
| switch | leans with bursts (5 TSLA over 150 slots), makes between: a fixed threshold | the flow, not whether it is informed |
| burst | leans with bursts every morning, flat between: the reference without the read | the flow, not whether it is informed |

## Departures from the card

- **The regime is the opening half hour, not a news-heavy session.** The
  card asked for 4-6 jumps an hour all session; the measurement found that
  only in TSLA's opening hour (3.9 an hour, every day). The card's
  1,800-2,400 slots become 3,000-4,500.
- **The informed bot is told the fund's wallets, not the kind of morning**
  (above). The card said no wallet is told; that was for an informed bot
  told the day type, which the boundary doesn't allow.
- **No making on a quiet morning.** At TSLA's open resting quotes lose on
  both mornings (version 1), so the quiet-morning call is to stand aside;
  `make` and `switch` stay as rungs. The card's "make or follow" becomes
  "follow or stand aside".
- **The mornings differ in who bursts, not in the informed share of a
  steady flow.** An execution algo (a real desk participant) supplies the
  uninformed bursts; volume, sizes and rhythm match by construction.
- **Edge 20-30 bp** (card: 15-20) so a lean clears the ~10 bp round trip.
