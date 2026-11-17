# Design record: flipped-feed-nvda

The README will say what the task is; this says why it's built this way,
and what each version learned. Card C2 of `TASK_CARDS_BATCH6.md`.

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
- **Mandate:** NVDA only, a cap of 4 NVDA either way (the near-touch book),
  a $150 loss limit, a broker that closes what is left at the index less
  5 bp, 72 hidden sessions per grade.

## The central decision

Each call: buy, sell, or nothing. The call says a side; which gateway sent
it says whether that side is the model's or its opposite. Until the
markouts say which gateway is which, every call is a fair coin less the
crossing cost. After, every call is worth the model's edge.

## The feed

The house model (the library's `actors.feed`) sees the index
`horizon_slots` ahead (96 to 240 slots, 24 to 60 s, drawn per session) and,
while the move there is more than `edge_bps` (20 to 30, per session) from
the book's price, calls its way at `calls_per_minute` (4 to 6 per session;
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
| 4,500 slots from 09:31 | 7,200 slots (30 min) from the 09:30 open, gaps off | The engine's slots are 0.25 s; the model's were 0.4 s. Horizons, gaps and holds are the model's times scaled by 1.6 |
| Informed: take strong calls, lean quotes on the rest | Takes every call at full size | The feasibility model's informed (taker only), which is conservative for the ratio. The maker lean waits for the full build |
| Reference: blocked t-test, small maker leans before commit | Bayes per burst (hit 0.75 assumed, commits at 0.95), flat before commit | The model's Bayes-per-block reader (0.32 to 0.33 of informed under per-horizon draws) is the one the card's band wants; the blocked t sat at 0.22 |
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

To come.

## Generations

- **Gate 0 prototypes.**
