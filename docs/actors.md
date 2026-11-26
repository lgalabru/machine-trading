# Actors

A session's market has other participants besides the bot. Two come with
every engine: the takers a scenario lists, and the grader's competing maker.
A scenario can add more from the actor library, in its `actors` section. The
code is `crates/grader/src/actors/` (the `Actor` trait, the `Cast`) and
`actors/library/`.

Every library actor trades from wallets of its own through a `Seat` on the
session's market, with ordinary transactions anyone reading the chain sees.
Its keys come from the session's keyring and its randomness from a seed the
keyring derives, so a session replays the same (`mtx grade --replay`). Each
of its transactions is unique among its own, so two alike orders in one slot
both land.
Today the library runs in perp sessions; a venue whose seats take and rest
orders (an AMM's take is a swap) runs it unchanged.

## The section

Sizes and rates are multiples of the market's own, as its calibration
measures them: `scale` holds the market's taker trades a minute and its
median trade size (and that size's lognormal sigma), from the calibration's
`venues.<market>.trades_per_minute` and `trade_size`.

```json
"actors": {
  "scale": { "trades_per_minute": 1.5, "trade_size": 0.02, "trade_size_sigma": 1.0 },
  "noise": [{ "wallets": 3, "flow_mult": 2.0 }],
  "informed": [{ "horizon_slots": 40, "edge_bps": 30.0, "intensity_mult": 4.0 }],
  "chasers": [{ "wallets": 2, "trigger_bps": 10.0, "intensity_mult": 4.0 }],
  "decoys": [{ "horizon_slots": 40, "edge_bps": 30.0, "informed_for": { "from": 60, "until": 160 } }],
  "spoofers": [{ "side": "bid", "distance_ticks": 5, "cancel_ticks": 2, "wall_mult": 8.0, "ahead_slots": 40 }],
  "icebergs": [{ "side": "bid", "display_mult": 1.0, "parent_mult": 6.0 }],
  "stop_hunters": [{ "level_step_usd": 1.0 }],
  "front_runners": [{ "pattern_steps": 2, "share": 0.5, "on_send": true }],
  "index_chasers": [{ "wallets": 4, "trigger_bps": 2.7, "intensity_mult": 2.0 }],
  "arbitrageur": { "latency_slots": 2, "edge_bps": 5.0 },
  "rfq_clients": { "clients": 6, "informed_share": 0.34, "lead_slots": 40, "desk_base": 5.0, "desk_usdc": 2000.0 },
  "liquidator": {},
  "refill": { "half_life_slots": [4, 40], "levels": 5 },
  "pool": { "wallets": 6, "roles": ["noise", "stop_hunters"] }
}
```

| Actor | What it does | Settings |
| --- | --- | --- |
| `noise` | Takes at random: Poisson arrivals, a coin-flip side, lognormal sizes. | `wallets`, `flow_mult`, `size_mult`, `limit_bps`, `active` |
| `informed` | Sees the scenario's level `horizon_slots` ahead and takes its way while the move is over `edge_bps`. Its view is imperfect: for a `horizon_slots` at a time, with chance `1 - hit_rate` (default 0.85) it reads the move the wrong way and trades against it, and `view_noise_bps` blurs what it sees each step. Right on average, and not always; each wallet has its own view, so the wallets err on their own. | `wallets`, `horizon_slots`, `edge_bps`, `hit_rate`, `view_noise_bps`, `intensity_mult`, `size_mult`, `limit_bps`, `active` |
| `chasers` | Takes the way the book moved once it moved over `trigger_bps` in the last `lookback_slots`: directional flow that knows nothing. | `wallets`, `lookback_slots`, `trigger_bps`, `intensity_mult`, `size_mult`, `limit_bps`, `active` |
| `index_chasers` | Reads the index (the scenario's level) and, while it is more than `trigger_bps` from where the book's makers stand (a path whose `premium_pct` trails the level), takes the way it moved: public information read fast, against quotes that haven't caught up. | `wallets`, `trigger_bps`, `intensity_mult`, `size_mult`, `limit_bps`, `active` |
| `decoys` | Each wallet trades as the informed do until a slot drawn in `informed_for`, then keeps its pace on a view it draws at random and holds for `horizon_slots`: a record that looks informed, then isn't. While it sees ahead it errs as the informed do. | `wallets`, `horizon_slots`, `edge_bps`, `hit_rate`, `view_noise_bps`, `informed_for`, `intensity_mult`, `size_mult`, `limit_bps` |
| `spoofers` | Rests a wall `distance_ticks` behind the touch and pulls it once the touch comes within `cancel_ticks`, or after `lifetime_slots`; each wall's distance and lifetime are drawn within `jitter` (default 0.4) of the settings, its size and a harvest's lognormally at `size_sigma` (default 0.3, a fixed spread of about 50 to 80 percent either way at two sigmas, whatever the market's own sigma), so no one value is the spoofer's. It may sell into the other side while it stands (`harvest_mult`). With `ahead_slots`, walls only while it sees the level that far ahead going against the wall (support painted ahead of a fall). One wallet throughout. | `side`, `distance_ticks`, `cancel_ticks`, `wall_mult`, `lifetime_slots`, `harvest_mult`, `ahead_slots`, `jitter`, `size_sigma`, `active` |
| `icebergs` | A large parent order shown a slice at a time: it rests `display_mult` trade sizes `distance_ticks` behind the touch on its side, and when the slice trades puts a slice back at the same price (drawn at `size_sigma` like a spoofer's, its first at a distance within `jitter` of `distance_ticks`), until `parent_mult` trade sizes are done. It never pulls on approach, which is how a reader tells it from a spoofer's wall; if the touch moves more than `max_away_ticks` from its slice, it puts the slice up again behind the new touch. One wallet, its own: it reads its fills from the wallet's position. | `side`, `distance_ticks`, `display_mult`, `parent_mult`, `max_away_ticks`, `jitter`, `size_sigma`, `active` |
| `stop_hunters` | Pushes the price through a round level on a `level_step_usd` grid once it comes within `trigger_bps`, and trades back about `hold_slots` later: each push's size, overshoot, hold and the cooldown after are drawn (`size_sigma`, `jitter`), so no one value is a hunter's. With `wallets` over 1, each hunt runs from one wallet it draws, and trades back from that wallet. | `wallets`, `level_step_usd`, `trigger_bps`, `push_mult`, `size_sigma`, `overshoot_ticks`, `hold_slots`, `cooldown_slots`, `limit_bps`, `jitter`, `active` |
| `front_runners` | Reads the bot's position step by step; once the bot has traded one way `pattern_steps` steps running, each over `min_mult` trade sizes, trades that way ahead of its next turn (`share` of its last step) and back out after `hold_steps`. With `on_send`, it reads the bot's transactions instead, and front-runs one carrying a trade the pattern's way (an immediate order, or a limit through the touch, of `min_mult` or more) that the chain would run: `share` of that order, just before it lands. At most once a turn, and never more than `max_inventory_mult` trade sizes held. | `min_mult`, `pattern_steps`, `share`, `hold_steps`, `limit_bps`, `max_inventory_mult`, `on_send` |
| `arbitrageur` | Every `latency_slots`, sells a bid over the reference by `edge_bps` and buys an ask under it. The reference is the market's own index (a perp's), else the scenario's level. It looks right after a step's blocks, before the bot's turn; with `after_turn`, at the start of the next step instead (an arbitrageur that sees the index a step late). | `latency_slots`, `edge_bps`, `size_mult`, `max_inventory_mult`, `after_turn` |
| `rfq_clients` | Ask the bot for quotes on `rfq:<symbol>`; take a quote no worse than their value by their reach: `tolerance_bps` for one trade size, `sqrt(n)` times that for `n`. Informed clients value the asset at the level `lead_slots` ahead, by `correlation`, and are mistaken with chance `1 - hit_rate` (default 0.85): that request asks and values the other way. Fills settle as swaps against the bot's wallet, which starts with `desk_base` of the asset and `desk_usdc` more USDC. | `clients`, `informed_share`, `lead_slots`, `correlation`, `hit_rate`, `per_100_slots`, `size_mult`, `deadline_slots`, `tolerance_bps`, `two_way_share`, `desk_base`, `desk_usdc`, `over_idle_desk` |
| `liquidator` | Races the bot for liquidations on a lending venue, best bonus first. Idle until a session has one. | `latency_slots`, `min_bonus_bps` |
| `refill` | Puts back the depth takers consume. The snapshot's makers are splines that only come back when a path move re-centres them (measured 0% in 60 slots on every market); this rests post-only orders from the old touch back toward the makers' own touch, so a gap halves every half-life, drawn once per session from `half_life_slots`. Never crosses the spread or goes past the makers' touch, stops adding to a side at `max_position_mult` trade sizes, and pulls its orders when the book level moves. Opt-in: without it, depth behaves as before. | `half_life_slots` (`[low, high]`), `levels`, `size_mult`, `react_slots`, `max_position_mult`, `min_gap_ticks`, `active` |

## Onboarding order

The library's wallets are listed role by role, but a session onboards them in
an order drawn from its seed, so the order of their first transactions, which a
bot can read with `getSignaturesForAddress`, says nothing of who is of a kind.

## A shared wallet pool

By default every actor trades from wallets of its own, so an address says who
acts. With `pool`, `wallets` of them are shared: each actor of a listed role
(`noise`, `informed`, `chasers`, `decoys`, `stop_hunters`) takes its own
`wallets` from the pool at random, from its own seed, so a session replays the
same and two actors may hold one wallet. What a wallet did is then the sum of
its roles'. An actor can't want more wallets than the pool holds, and the
pool's roles are the only ones that draw on it. A pooled actor's wallets are
the pool's, listed first in `Library::wallets` (role `pool`).

## Telling the informed bot who is who

The task's informed bot (scoring v2's 1) may be told the session's identity
and process facts, never its realised draws (`docs/gates.md`). Who is who is
the session's own addresses, which come from the grade's secret, so a hidden
set can't write them down. Its latent state (`mtx_grader::latent::Latent`)
names the kinds to tell, as `{"roster": ["informed", "decoy"]}`, and the
session fills in their wallets as it starts, handed over as `MTX_REGIME`:
`{"wallets": {"informed": ["<pubkey>"], "decoy": ["<pubkey>"]}}`. The kinds are
the report's actors' (`noise`, `informed`, `chaser`, `decoy`, `spoofer`,
`iceberg`, `stop-hunter`, …), and `rfq-informed`, the ids of the RFQ clients
that are informed (`c3` for the requests `c3-0`, `c3-1`, …). The wallets
are the actors' authorities (signing keys). Phoenix's events and fills name the
trader account, a PDA of the authority, so a bot must map each with
`Exchange::trader_account(&authority)` before matching them; unmapped, nothing
matches and it silently trades uninformed. A kind the
session doesn't have is an error, not a silent gap. A regime with no `roster`
is handed over unchanged.

`active` is a window of the session, `{ "from": 100, "until": 300 }` in slots
into it. `record: true` keeps every action in the report's tape, and counts
the actions whose transactions the chain has, executed (`on_chain`).

## Requests for quotes

A component sees the requests open at its step in `market.rfqs` and answers
with a response line on the request's venue. A request's id names its client
(`c<n>-<k>`), and which clients are informed holds all session. At the start
of the next step the client takes the answer if it suits it: the quote is no
worse than the client's own value by its reach, which grows with the square
root of the request's size, as sweeping that size on a screen costs more.
The book plays no part: the default reach, 1 bp for one SPCX trade size
(about $3), is some 60 bp at $10k, near the 46 bp that routing $10k through
SPCX's spot pools cost in 2026-10.

A fill is bilateral. It settles as one transaction that the client and the
bot both sign (the grader holds both keys; the bot agreed in its response):
the seller's spot token goes to the buyer, and the buyer's USDC to the
seller, at exactly the quoted price (to USDC's last unit). The client pays
the transaction's fee; the bot pays no fee at all. The bot sees the fill in
`market.fills`, as the maker, on the RFQ venue.

In a perp session the spot token is the asset's own mint (SPCX:
`SPCXxcqXj6e5dJDVNovHN8744zkbhM2bYudU45BimGb`, 6 decimals). The snapshot
doesn't carry it, so setup writes a plain Token-2022 mint at that address,
without the real one's extensions (transfer hook, frozen-by-default
accounts, pausing, permanent delegate), and funds each client with the
token and USDC. The bot's wallet holds what it trades, outside its perp
account: the session values it as the spot profile does
(specs/instruments/spot.md), USDC at 1 and the token at its reference price,
Phoenix's index for the perp. The USDC is in the perp account's metered
value already, which counts the wallet's USDC; the token is added to it. The
start value is read once setup has funded the desk, so an idle desk's PnL is
its token's move at the index, and a fill changes the value by what it
earns against the index, no more. With `over_idle_desk`, the idle desk's
earnings come out: the session's PnL and the drawdown its mandate is audited
by are the desk's less what `desk_base` of the asset would have made held, so a
desk that quotes nothing scores 0, and a desk that does is scored on its fills
and the inventory they leave. `over_idle_desk` and `scoring.net` both score over holding the start, but
differently: `net` values the held start position of the perp (at the fair
path's move where there is one, else the level's) and takes its own drawdown
series, while `over_idle_desk` takes the desk's token at Phoenix's index. Use
`net` for a perp position the bot starts with and `over_idle_desk` for an RFQ
desk's inventory; a scenario with both is refused. The report's `actors.desk` gives the
desk's token, USDC and index at the start and the end.

Before each swap, and again after one fails, the grader reads both wallets'
token accounts. A swap the bot's wallet can't settle (an account missing or
not its own, frozen, asking for a memo, or short of what it pays) isn't
sent, or failed for that: the request counts as `unsettled`, the bot's own
doing. A client that has traded its funds away with the bot declines
(`client_unfunded`).

With `on_send`, a front-runner acts on the bot's sends as they arrive, so it
can't run in a scenario with the transaction layer (`tx_layer`, see
`docs/tx-layer.md`), which pools the bot's sends until a block takes them:
such a scenario is refused when it loads. An actor competes under the layer
by sending into its auction instead, as a lending session's liquidators
and keeper do (below).

## A lending session's actors

A lending session (`venues.lending`, `crates/grader/src/lending/`) has
actors of its own, outside the library. Every number below is the
research's (`docs/RESEARCH.md`, "Kamino Lend liquidations", and
`ops/research/atomic-arb/kamino-liquidations/NOTES.md` §3 and §6), and
every draw comes from the scenario's lending seed, keyed by the actor, the
borrower's place and the step, so a session replays them.

- **Borrowers** (`borrowers`): obligations seeded near their liquidation
  threshold; some top up or repay once their health falls under 1.05. They
  act through the grader's endpoint, at once.
- **Liquidators** (`liquidators`), without the transaction layer, look
  after each step's blocks at Scope's price, and liquidate what is below 1
  after `delay_steps`, in a drawn order, landing at once.
- **Liquidators under the layer** bid in its auction, each under its own
  name (`kamino-liquidator/<n>`):
  - They read the **upstream**: a pushed feed's `FixedPrice` in Scope's
    mappings, as a bot can. An obligation is liquidatable when its debt,
    adjusted by its borrow factor, reaches its deposits' value times their
    liquidation threshold, at upstream prices (the program's own values give
    each side's weights, so an elevation group's terms hold).
  - **How many race** grows with what they would repay: on average 0.56 of
    them under $10, 2.6 from $10 to $1,000 and 4.4 above, each joining with
    the chance that makes that mean, all of them when there are fewer.
  - **When**: a latency in slots per liquidator and obligation, drawn so
    that 50% act in the slot they see it, 71% within 2 slots and 90% within
    21; above $1,000 repaid, 93% in the same slot.
  - **What**: one transaction, from its own USDC: a push of the
    collateral's Scope entries when Scope still reads the obligation
    healthy, the refreshes, and the liquidation of the close factor's share
    of the largest debt against the largest deposit. It sends only what its
    simulation passes; one that lands after another's liquidation fails
    with `ObligationHealthy`, and pays its fee, as 30,928 did on mainnet.
  - **Its bid**, drawn once per liquidator: 25 to 45% of the bonus it
    expects to keep (half the reserve's minimum bonus), as a Jito tip in a
    bundle for 46% of them and as a priority fee for the rest. It pays that
    share per compute unit as mainnet's liquidations did on their median
    333,000 units: a transaction asking fewer pays less in all, and ranks
    where theirs did. A tip and a priority fee rank alike
    (`docs/tx-layer.md`, "Ranking").
  - **A session's own race** (`liquidators.race`, every member optional):
    `bid_share` (the range each bid share is drawn from), `tipping_share`
    (the chance a liquidator tips), `same_slot_share` (the share of races
    up to $1,000 joined in the slot the obligation turned; the lag keeps
    the research's shape, and above $1,000 the same slot takes
    `1 − 0.07 (1 − s) / 0.5`) and `competition` (the research's mean count
    per size, times this; above $1,000 repaid it only raises the mean). Absent, each is the research's. The
    `liquidation-flash` family draws them per session.
  - The liquidators look at the obligations in the borrowers' order, not
    their addresses': each role of a grade (the reference, the informed
    bot, the bot) plays with keys of its own, and the order they send in
    keys the layer's latency draws.
  - The report lists each attempt in `lending.actions`, with the slot it
    landed in, its latency, its bid, and whether it pushed Scope, and the
    layer's report counts each liquidator's sends (`tx_layer.senders`).
- **Scope's keeper** (`scope-keeper`), in a session with a pushed feed,
  pushes every pushed entry every `keeper.every_slots` (5 by default, Scope's
  median cadence while active) at a low priority fee, one
  `refresh_price_list` per feed: through the layer's auction where there is
  one, timed to land on its cadence, else at once. Between its pushes,
  Scope lags the upstream unless someone pushes first.
- **The pools' arbitrageur**, in a session with pools (`venues.amm`): after
  each step's blocks, before the other actors and the bot, it swaps each
  pool back to the scenario's price once it strays more than 4 bp (the
  pools' fee), with a real swap the program stops at that price. Without it
  nothing moves the pools but the session's own swaps, and selling seized
  SOL into a pool left at the opening price after a fall would earn the
  fall, not the bonus. Its swaps land at once.

## An amm session's actors

An amm session (`venues.amm` alone, `crates/grader/src/amm/`) has actors of
its own, outside the library, each sending through the transaction
layer's auction under its own name but the whale. The numbers are the
research's (`docs/RESEARCH.md`, "SOL/USDC backruns on concentrated pools",
and `ops/research/atomic-arb/clmm-backrun/NOTES.md`), and every draw comes
from the section's `seed` through `txlayer::draw`, keyed by the actor, the
slot or its count of looks, so a session replays them exactly.

- **The whale** (`amm-whale/<n>`): its chunks are the session's triggers.
  Each lands through the grader's endpoint, as mainnet's did (an ordinary
  transaction, 15,000 lamports in fees, no tip), in the slot the scenario
  names, from one of its accounts drawn per chunk: a market order on one
  pool, a sale of the base or a buy of it, or, for a chunk with an
  `impact_bps`, a swap the program stops once the price has moved that
  far (halved until it fills where the pool's liquidity ends first: a thin
  pool's trigger, `backrun-thin-pools`). The step ends right behind it
  (`docs/tx-layer.md`, "Trigger steps"). The report keeps each chunk
  (`amm.triggers`): the gap between the pools before and after it (a
  session of two pools), what the best cycle of up to four legs would
  have made right after it, what the layer landed
  in the rest of its slot, and the first of the bot and the searchers to
  land and run behind it within 4 slots (`winner`, `winner_after_slots`).
- **The calm flow** (`background`): each slot, each pool swaps with the
  chance its rate a minute gives (150 slots a minute), either side at even
  odds, a log-normal size in SOL's worth through the median and the 90th
  percentile, capped. What the step drew is sent after its blocks, through the auction
  at a 30,000 micro-lamport priority fee (about 15,000 lamports in all).
- **Cycle searchers** (`cycle-searcher/<n>`): after each step's blocks,
  each reads every pool and finds the cycle from SOL back to SOL of at
  most its `max_legs` legs that profits most on the pools' exact quotes
  (`amm::cycle::Graph::best`, which lists every simple cycle through SOL
  once, skips those whose prices after fees multiply to 1 or less, and
  sizes the rest by golden-section search, as the research's detector
  does). On two SOL/USDC pools that is the sale of SOL where it is dear
  and its buy back where it is cheap; on a graph, a route through a third
  token too, unless the searcher only looks at two legs; or a fixed size,
  like the research's 200-SOL bot. If it expects at least its
  `min_profit_sol`, it sends the swaps as one transaction: each leg but
  the last may give 10 bp less than quoted, and the last must give back
  the SOL put in and the least profit, so a cycle that comes too late
  fails whole and pays its fee (a tip's bundle fails unpaid). It bids `bid_share` of what it
  expects as a priority fee over its simulated units, or as a tip, after a
  latency drawn per send. It sends again only once its last send is no
  longer pending.
- **Pool searchers** (`pool-searcher/<n>`): milestone D's pools
  arbitrageur, competing. After each step's blocks each reads every pool
  and, when one strays from its reference price (SOL's for SOL/USDC, the
  ratio of its tokens' `reference_sol` otherwise) past its fee and
  `margin_bps`, swaps it back to the reference less the fee, with a swap the program
  stops at that price and that must give what it was quoted. It bids
  `bid_share` of what that makes at the reference (the research's: 76 to
  99%). Between chunks it is what keeps the calm flow's pools on the
  reference; behind a chunk it races the cycles, as mainnet's strongest
  backrunners did.

The searchers and the flow hold 200,000 SOL, 20 million USDC, 200,000
SOL's worth of every other token and 1,000 native SOL each, far more than
any of their trades take. The layer's report
counts each sender's sends (`tx_layer.senders`), and `amm.attempts` lists
each searcher's send: what it expected, what it bid, its latency and what
became of it.

## The reactive competitor

A market-making scenario's grader-side maker (`competition`) can opt into
the reactive competitor with `"reactive": {}`: it keeps its prices in whole
ticks, steps `step_ticks` inside any rival it sees, jitters its look timing,
and widens after one-sided fills. It reacts to a resting order of at least
`q_react` base units. Absent, `q_react` is the market's minimum lot, so any
order the market accepts counts as a rival, however small: a bid a few
percent under some round size is still a bid. An explicit `q_react` (0 or
more) keeps its value, and quotes under it set no touch.

## When actors fail

Each order, cancel, take and swap counts as an action. A session where more
than 10% of the library's actions failed didn't see the market its scenario
describes: the grade fails as infrastructure, as it does for the takers.
Front-runners, which trade on the bot's own sends, are left out of that
count (their tallies say `reacts_to_bot`): the bot can make their actions
fail, and no bot can void its own session. An RFQ swap the bot's wallet
can't settle is no action at all; any other failed swap counts.

## Reading them

`bench/checks/actors/read.py` reads a recorded session as anyone watching the
chain could: it classifies each wallet from its own record, and prices what
reading pays. Three strategies trade one unit every 4 slots on what the last
40 showed, marked out 40 slots later net of the half spread: **told**
follows the informed wallets' net flow (the answer key); the **careful
blind** reads no wallet, and is the better of following the market's net
taker flow and following the price's move; the **reader** follows the
wallets whose takes have been rewarded lately, learnt online. Chasers and
decoys keep the net flow from carrying the informed signal, so only each
wallet's record does. Walls are priced the same way: a blind follower buys
as a big bid goes up, while the reader fades the walls of wallets it has
seen pull most of their orders. `just actors-smoke` runs it on six
sessions with every actor, each with fresh wallets, and requires told to
make at least twice the careful blind summed over the six: one session's
careful blind swings by thousands of bps with its draws (from about −2,500
to +3,300 bps over 52 sessions, against told's +3,100 to +6,500), and the
sum is what the edge is judged on. A path whose own move predicts the next
as well as any reader could (a long trend) leaves nothing for reading to
add: the check is for paths where the price's history isn't the answer.
The smoke then checks the RFQ desk's accounting with an idle bot and one
that only answers requests (`bench/checks/actors/desk.py`).
