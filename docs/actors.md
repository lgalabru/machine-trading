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
  "spoofers": [{ "side": "bid", "distance_ticks": 5, "cancel_ticks": 2, "wall_mult": 8.0 }],
  "stop_hunters": [{ "level_step_usd": 1.0 }],
  "front_runners": [{ "pattern_steps": 2, "share": 0.5, "on_send": true }],
  "arbitrageur": { "latency_slots": 2, "edge_bps": 5.0 },
  "rfq_clients": { "clients": 6, "informed_share": 0.34, "lead_slots": 40 },
  "liquidator": {}
}
```

| Actor | What it does | Settings |
| --- | --- | --- |
| `noise` | Takes at random: Poisson arrivals, a coin-flip side, lognormal sizes. | `wallets`, `flow_mult`, `size_mult`, `limit_bps`, `active` |
| `informed` | Sees the scenario's level `horizon_slots` ahead and takes its way while the move is over `edge_bps`. | `wallets`, `horizon_slots`, `edge_bps`, `intensity_mult`, `size_mult`, `limit_bps`, `active` |
| `spoofers` | Rests a wall `distance_ticks` behind the touch and pulls it once the touch comes within `cancel_ticks`, or after `lifetime_slots`; may sell into the other side while it stands (`harvest_mult`). One wallet throughout. | `side`, `distance_ticks`, `cancel_ticks`, `wall_mult`, `lifetime_slots`, `harvest_mult`, `active` |
| `stop_hunters` | Pushes the price through a round level on a `level_step_usd` grid once it comes within `trigger_bps`, and trades back `hold_slots` later. | `level_step_usd`, `trigger_bps`, `push_mult`, `overshoot_ticks`, `hold_slots`, `cooldown_slots`, `limit_bps`, `active` |
| `front_runners` | Reads the bot's position step by step; once the bot has traded one way `pattern_steps` steps running, each over `min_mult` trade sizes, trades that way ahead of it (`share` of its last step) and back out after `hold_steps`. With `on_send`, also just before each of the bot's transactions lands. | `min_mult`, `pattern_steps`, `share`, `hold_steps`, `limit_bps`, `on_send` |
| `arbitrageur` | Every `latency_slots`, sells a bid over the reference by `edge_bps` and buys an ask under it. The reference is the market's own index (a perp's), else the scenario's level. | `latency_slots`, `edge_bps`, `size_mult`, `max_inventory_mult` |
| `rfq_clients` | Ask the bot for quotes on `rfq:<symbol>`; take a quote that is within `tolerance_bps` of their value and better than the screen. Informed clients value the asset at the level `lead_slots` ahead, by `correlation`. | `clients`, `informed_share`, `lead_slots`, `correlation`, `per_100_slots`, `size_mult`, `deadline_slots`, `tolerance_bps`, `two_way_share` |
| `liquidator` | Races the bot for liquidations on a lending venue, best bonus first. Idle until a session has one. | `latency_slots`, `min_bonus_bps` |

`active` is a window of the session, `{ "from": 100, "until": 300 }` in slots
into it. `record: true` keeps every action in the report's tape, and counts
the actions whose transactions the chain has, executed (`on_chain`).

## Requests for quotes

A component sees the requests open at its step in `market.rfqs` and answers
with a response line on the request's venue. A request's id names its client
(`c<n>-<k>`). At the start of the next step the client takes the answer if it
suits it, and the trade settles on the session's market as two orders: the
client rests the size at the agreed price (on the tick grid; a price through
the screen settles one tick inside it), the bot's account takes it, and the
client cancels what is left.

## When actors fail

Each order, cancel and take counts as an action. A session where more than
10% of the library's actions failed didn't see the market its scenario
describes: the grade fails as infrastructure, as it does for the takers.

## Reading them

`bench/checks/actors/read.py` reads a recorded session as anyone watching the
chain could, and checks that each actor can be told apart from that record
and that reading pays: a follower of the informed flow and a buyer at
displayed support do better when they skip what the record gives away.
`just actors-smoke` runs it on a session with every actor.
