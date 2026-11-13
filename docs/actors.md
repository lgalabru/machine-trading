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
  "stop_hunters": [{ "level_step_usd": 1.0 }],
  "front_runners": [{ "pattern_steps": 2, "share": 0.5, "on_send": true }],
  "arbitrageur": { "latency_slots": 2, "edge_bps": 5.0 },
  "rfq_clients": { "clients": 6, "informed_share": 0.34, "lead_slots": 40, "desk_base": 5.0, "desk_usdc": 2000.0 },
  "liquidator": {}
}
```

| Actor | What it does | Settings |
| --- | --- | --- |
| `noise` | Takes at random: Poisson arrivals, a coin-flip side, lognormal sizes. | `wallets`, `flow_mult`, `size_mult`, `limit_bps`, `active` |
| `informed` | Sees the scenario's level `horizon_slots` ahead and takes its way while the move is over `edge_bps`. | `wallets`, `horizon_slots`, `edge_bps`, `intensity_mult`, `size_mult`, `limit_bps`, `active` |
| `chasers` | Takes the way the book moved once it moved over `trigger_bps` in the last `lookback_slots`: directional flow that knows nothing. | `wallets`, `lookback_slots`, `trigger_bps`, `intensity_mult`, `size_mult`, `limit_bps`, `active` |
| `decoys` | Each wallet trades as the informed do until a slot drawn in `informed_for`, then keeps its pace on a view it draws at random and holds for `horizon_slots`: a record that looks informed, then isn't. | `wallets`, `horizon_slots`, `edge_bps`, `informed_for`, `intensity_mult`, `size_mult`, `limit_bps` |
| `spoofers` | Rests a wall `distance_ticks` behind the touch and pulls it once the touch comes within `cancel_ticks`, or after `lifetime_slots`; may sell into the other side while it stands (`harvest_mult`). With `ahead_slots`, walls only while it sees the level that far ahead going against the wall (support painted ahead of a fall). One wallet throughout. | `side`, `distance_ticks`, `cancel_ticks`, `wall_mult`, `lifetime_slots`, `harvest_mult`, `ahead_slots`, `active` |
| `stop_hunters` | Pushes the price through a round level on a `level_step_usd` grid once it comes within `trigger_bps`, and trades back `hold_slots` later. | `level_step_usd`, `trigger_bps`, `push_mult`, `overshoot_ticks`, `hold_slots`, `cooldown_slots`, `limit_bps`, `active` |
| `front_runners` | Reads the bot's position step by step; once the bot has traded one way `pattern_steps` steps running, each over `min_mult` trade sizes, trades that way ahead of its next turn (`share` of its last step) and back out after `hold_steps`. With `on_send`, it reads the bot's transactions instead, and front-runs one carrying a trade the pattern's way (an immediate order, or a limit through the touch, of `min_mult` or more) that the chain would run: `share` of that order, just before it lands. At most once a turn, and never more than `max_inventory_mult` trade sizes held. | `min_mult`, `pattern_steps`, `share`, `hold_steps`, `limit_bps`, `max_inventory_mult`, `on_send` |
| `arbitrageur` | Every `latency_slots`, sells a bid over the reference by `edge_bps` and buys an ask under it. The reference is the market's own index (a perp's), else the scenario's level. | `latency_slots`, `edge_bps`, `size_mult`, `max_inventory_mult` |
| `rfq_clients` | Ask the bot for quotes on `rfq:<symbol>`; take a quote no worse than their value by their reach: `tolerance_bps` for one trade size, `sqrt(n)` times that for `n`. Informed clients value the asset at the level `lead_slots` ahead, by `correlation`. Fills settle as swaps against the bot's wallet, which starts with `desk_base` of the asset and `desk_usdc` more USDC. | `clients`, `informed_share`, `lead_slots`, `correlation`, `per_100_slots`, `size_mult`, `deadline_slots`, `tolerance_bps`, `two_way_share`, `desk_base`, `desk_usdc` |
| `liquidator` | Races the bot for liquidations on a lending venue, best bonus first. Idle until a session has one. | `latency_slots`, `min_bonus_bps` |

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
earns against the index, no more. The report's `actors.desk` gives the
desk's token, USDC and index at the start and the end.

Before each swap, and again after one fails, the grader reads both wallets'
token accounts. A swap the bot's wallet can't settle (an account missing or
not its own, frozen, asking for a memo, or short of what it pays) isn't
sent, or failed for that: the request counts as `unsettled`, the bot's own
doing. A client that has traded its funds away with the bot declines
(`client_unfunded`).

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
