# Design record: flipped-feed-tsla

The README will say what the task is; this says why it's built this way,
and what each version learned. Card C2 of `TASK_CARDS_BATCH6.md` (`flipped-feed-nvda`), moved to TSLA,
the card's fallback, after gate 0 failed on NVDA (see Gate 0).

## Thesis

A good bot profits here because it checks a provided signal's sign against
the market before trusting it: it marks the house model's calls out against
the index, counts a burst of calls as one piece of evidence, commits to which
gateway reports the model's side only once the evidence is clear, and then
trades every call the right way round.

- **Wins:** following the calls from the gateway that reports the model's
  own side and fading those from the other, as soon as the markouts settle
  which is which.
- **Loses:** trusting every call as published; fading every call; trusting
  the gateway the desk usually uses; committing on the first call's
  markout; trading the move after a call with no convention at all;
  trading on a schedule.
- **Regimes:** which gateway is flipped is drawn per session with even
  odds; the model is right for a whole horizon at a time with chance 0.8;
  paths come in mirrored pairs.
- **Mandate:** TSLA only, a cap of 2 TSLA either way (about $780, near
  the near-touch book), a $150 loss limit, a broker that closes what is
  left at the index less 5 bp, 72 hidden sessions per grade. (NVDA: a cap
  of 4 NVDA, about $640.)

## The central decision

Each call: buy, sell, or nothing. The call says a side; which gateway sent
it says whether that side is the model's or its opposite. Until the
markouts say which gateway is which, every call is a fair coin less the
crossing cost. After, every call is worth the model's edge.

## The feed

The house model (the library's `actors.feed`) sees the index
`horizon_slots` ahead (96 to 240 slots, 24 to 60 s, drawn per session) and,
while the move there is more than `edge_bps` (20 to 30, per session) from
the book's price, calls its way at `calls_per_minute` (5 to 7 per session;
about a dozen calls a session), no two calls nearer than 16 slots. Its view
is drawn right or wrong once a horizon, right with chance `hit_rate` 0.8
(16d: the engine's per-horizon draws, the case the model planned on), so
calls in one burst share their luck.

Each call goes out as an SPL Memo transaction signed by one of two gateway
wallets, drawn per call:

```text
{"call":3,"side":"buy","horizon_slots":160,"confidence":0.71}
```

The bot reads them with `getSignaturesForAddress` on the wallets
`MTX_FEED` lists (the memo text comes with each signature). One gateway
reports the model's side (taker side); the other reports the side a maker
would be filled on: the opposite. Which one is drawn per session from the
session's seed. A call's horizon, pace and confidence are the same through
either, and `confidence` grows with how far past the edge the model sees
the move, never with whether it is right.

**Stated stylisation.** A real feed's convention flips now and then, after
an upstream change (maker side against taker side, as on-chain fills name
both). Here one gateway is flipped in every session, and which is a fair
coin. The frequency is a stated stylisation (checklist 11, 13).

## Departures from the card

| Card | Built | Why |
| --- | --- | --- |
| One feed whose convention is normal or flipped, 50/50 per session; informed told `feed_convention` | Two gateways carry the model's calls, drawn per call; one reports the model's side and one the opposite, which is drawn 50/50 per session; informed told which gateway wallet is which (roster identity) | Checklist 6: the session's convention is a realised draw and may not be told. Who is who is identity: both gateway kinds are seated every session from fresh wallets, so the roster reveals nothing of the future, as B6's spoofer and iceberg. Each gateway is still flipped on half the sessions. The evidence a call gives is the card's (one bit, which gateway is the model's), and follow-all now nets zero within every session, not only across them |
| TSLA 3,000 slots (NVDA 4,500) from 09:31 | TSLA 4,800 slots, 20 min (NVDA 7,200, 30 min), from the 09:30 open, gaps off | The engine's slots are 0.25 s; the model's were 0.4 s. Horizons, gaps and holds are the model's times scaled by 1.6 |
| Informed: take strong calls, lean quotes on the rest | Takes every call at full size | The feasibility model's informed (taker only), which is conservative for the ratio. The maker lean waits for the full build |
| Reference: blocked t-test, small maker leans before commit | Bayes per burst (hit 0.75 assumed, commits at 0.95), flat before commit | The model's Bayes-per-block reader (0.32 to 0.33 of informed under per-horizon draws) is the one the card's band wants; the blocked t sat at 0.22 |
| NVDA, TSLA the fallback | TSLA | Gate 0 on NVDA failed the order (see Gate 0); one rework allowed |
| Tier 2 (calibrated or overconfident confidence), tier 3 (10:00 release, flat window) | Not built | Tier 3 waits on a measurement of 10:00 releases; tier 2 waits on the base tier passing |

## No tells

- Every session seats noise, chasers, a refill and both gateways from
  fresh wallets of the session's keyring. No actor trades on the model, so
  the tape can't confirm a call.
- Calls through either gateway carry the same horizon, pace and
  confidence; the gateway is drawn per call. `MTX_FEED` lists the wallets in
  keyring order, and which one is flipped is drawn from the session's seed,
  independently of that order.
- Paths come in mirrored pairs, so a fixed lean or schedule nets about
  nothing but costs.

## The anchors (Gate 0 prototypes)

- **The reference (0.5)** reads both gateways every step, marks each call
  out over its horizon against the index, groups calls within 240 slots
  into bursts, scores each finished burst by its majority, adds
  ln(0.75/0.25) per burst to the log-odds that gateway 0 is the model's,
  and commits at odds of 0.95 (three net bursts). Flat until then; after,
  it acts as the informed bot does.
- **The informed bot (1)** is told the session's roster: the wallet of
  the gateway that reports the model's side and of the one that reports
  the opposite (identity, `{"roster": ["gateway-taker-side",
  "gateway-maker-side"]}`). Not when a call comes, its side, whether the
  model is right, or the path. It follows each call from the model's
  gateway and fades each from the other at full size until the call's
  horizon.

## The ladder's non-reading rungs

| Rung | Rule |
| --- | --- |
| nop | Never trades |
| follow-all | Takes every call as published |
| fade-all | Takes every call the other way |
| follow-first | Trusts the first wallet `MTX_FEED` lists (lesson 2: "always act as if H1") |
| post-move | No convention: 64 slots after a call, follows the index's move since the call if over 5 bp, for 240 slots (the model's tuned 40, 5, 150 at 0.4 s) |
| twap | Buys evenly to the cap over the first half and back to flat over the second |

## Gate 0

Engine runs on runner-2 (`hidden/gate0/`, untracked: `build.sh`,
`run.sh`, `gate0.py`), every rung a component, one hidden set of 72
sessions (36 mirrored pairs) drawn from a fresh secret never printed.

### NVDA (failed)

| Rung | Total | Mean | v2-weighted share of informed |
| --- | --- | --- | --- |
| fade-all | -544 | -7.55 | -4.74 |
| follow-all | -483 | -6.71 | -4.22 |
| follow-first | -411 | -5.71 | -3.90 |
| twap | -54 | -0.75 | -0.74 |
| nop | 0 | 0 | 0 |
| post-move | 74 | 1.03 | 0.28 |
| reference | 75 | 1.04 | 0.39 |
| informed | 202 | 2.80 | 1.00 |

- Signal gate: informed / best non-reading (post-move) = 2.72 (need 2),
  but on only 0.768 of bootstrapped sets.
- Order `max(non-reading) < reference < informed` on 0.490 of
  bootstrapped sets (need 0.85): post-move ties the reference.
- Strength band: reference 0.39 of informed, v2-weighted; per-session
  Sharpe 0.27.
- Edge vs costs (16b): informed nets 4.2 bp a call (760 calls, 10.6 a
  session, median 5); following or fading blind costs 10 to 11 bp a call.
  Under the card's 5 bp kill line.
- The reference committed in 26 of 72 sessions.
- Failed on the order. Post-move made 0.28 of informed against the
  model's 0.14: the engine's moves carry on after a call more than the
  model's. The model put post-move at 0.05 or below on TSLA, with 10 bp a
  call net, so the one rework moves the card there.

### TSLA (failed: retired)

| Rung | Total | Mean | v2-weighted share of informed |
| --- | --- | --- | --- |
| fade-all | -535 | -7.43 | -3.88 |
| follow-all | -489 | -6.79 | -3.50 |
| follow-first | -455 | -6.32 | -3.56 |
| twap | -61 | -0.85 | -0.58 |
| nop | 0 | 0 | 0 |
| reference | 52 | 0.72 | 0.19 |
| post-move | 57 | 0.79 | 0.03 |
| informed | 248 | 3.45 | 1.00 |

- Signal gate: informed / best non-reading (post-move) = 4.35 (need 2);
  on 0.917 of bootstrapped sets.
- Order on 0.479 of bootstrapped sets (need 0.85): post-move's raw total
  still sits at the reference's, though v2 weighs its losses down to 0.03.
- Strength band: reference 0.19 of informed, v2-weighted (need 0.2);
  per-session Sharpe 0.23 (need 0.25).
- Edge vs costs: informed nets 4.0 bp a call (11.0 calls a session,
  median 9, none in 9 sessions); blind following or fading costs 7 to 9 bp
  a call. The reference committed in 32 of 72 sessions.
- Failed on the order, the band and the Sharpe. With the one rework
  spent, the card retires.

**Why.** On the engine the informed edge is about 12 to 14 bp gross a
call, against the model's 18 to 19, so the net is 4 bp, not 8 to 10. The
reference spends most sessions learning which gateway is which (it
commits in under half), and what it earns after committing is a thin
edge on the few calls left. Meanwhile a convention-free reader of the
move after a call earns as much raw PnL, because the engine's moves keep
going after a call fires. A feed card needs an edge per call several times
the crossing cost (or maker-side expression) before the sign read can
pay.

## Generations

- **Gate 0 prototypes, NVDA.** Learned: the feed's calls land and read
  as memos; the reference commits in about a third of sessions; post-move
  is the binding rung on NVDA, not follow-all.
- **Rework: TSLA** (the card's fallback), cap 2 TSLA, 4,800 slots.
  Failed band, Sharpe and order; retired.
