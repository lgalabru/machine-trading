# Venue profile: the router (`router`), v0

Status: draft

The router is Jupiter-style routing across a session's own pools: the best
way to swap an amount of one token for another through the
[Raydium](raydium.md) and [Meteora](meteora.md) pools the session trades,
split across paths and through other tokens, as one transaction. It is not
a protocol and holds nothing: every route is the pools' own swaps, so what
it trades are the pools' instruments. A scenario turns it on (a spot
session's `spot.router`); without it there is no router, and a bot that
wants a route across pools builds it itself.

## Instruments

| Reference | Profile | Meaning |
| --- | --- | --- |
| `router` (a pseudo-venue, no instrument of its own) | [spot](../instruments/spot.md), on each pool a route crosses | A route is one swap on each pool it crosses, each a fill of that pool's `<venue>:spot:<SYMBOL>` instrument. |

No new instrument profile: a route is spot trading on the pools it
crosses, audited and valued as the spot profile says, pool by pool.

## Where it routes

- **Only through the mandate's universe.** A route crosses the pools of
  the spot instruments in the universe: the session's own pair on its pool
  venues, and the other pairs its scenario lists in `router.through`
  (`raydium:spot:SPCX-SOL`, `meteora:spot:SOL-USDC`, …), which the universe
  must list too. A quote naming any other pool is refused, so a route goes
  nowhere the bot couldn't trade by hand.
- **Up to 3 pools a path, 4 a route.** Every path of up to `max_hops`
  (default 3) pools from the token in to the token out, crossing no pool
  or token twice; the amount split across them in twentieths, each to the
  path whose output it raises most, no pool on two paths, at most 4 pools
  in all (what one transaction holds with the lookup table, under 1.4M
  compute units).
- **Exact quotes.** Each pool's quote is its program's own arithmetic
  (Raydium CLMM's ticks, fee modes, dynamic fee and limit orders; Meteora
  DLMM's bins, variable fee at the session's clock and limit orders) on the
  state at the quote's slot: a swap that lands on that state pays it to the
  atom. It doesn't land on that state: the world moves between the bot's
  turn and its block.
- **Deterministic.** A route is a function of the pools' state; ties go to
  fewer hops, then the scenario's venue order, then the pool's address.

## How it trades

A route is one v0 transaction: a compute limit, the token accounts its
legs pay into (made where missing), then each leg's swap (Raydium's
`swap_v2`, Meteora's `swap2`), exactly in, naming the session's address
lookup table (made at setup with every routed pool's accounts).

Where it differs from Jupiter:

- **The pools' own instructions, not Jupiter's program.** A leg after
  another spends what the leg before guarantees (its quote less the
  slippage), so the route never fails for want of the token in between;
  whatever that leg pays above it stays in the wallet as that token, valued
  at its reference price. Jupiter's program spends what the leg actually
  paid.
- **Least out per path.** Each path's last leg carries its share of the
  route's least out, in proportion to what the path is quoted to pay; one
  path short fails the whole transaction.
- **ExactIn only.** `swapMode=ExactOut` is refused.
- **Only the copied pools.** Mainnet routes go through venues the session
  doesn't copy (proprietary oracle AMMs, other pools of the same pair):
  they are absent here, and routes through what is copied pay less than
  mainnet's.

## The service

At the bot's RPC URL, Jupiter's v1 shape for the fields it supports:

| Call | Takes | Answers |
| --- | --- | --- |
| `GET /quote` | `inputMint`, `outputMint`, `amount` (atoms), optional `slippageBps` (default 50), `swapMode` (ExactIn), `onlyDirectRoutes`, `maxHops`, `dexes`, `excludeDexes` (labels `Raydium CLMM`, `Meteora DLMM`) | `inAmount`, `outAmount`, `otherAmountThreshold`, `slippageBps`, `priceImpactPct` (against the reference prices), `routePlan` (each leg's `swapInfo` with `ammKey`, `label`, mints, amounts and fee; a path's first leg with its `percent` and `bps` of the amount, a later one with 100), `contextSlot` |
| `POST /swap-instructions` | `{quoteResponse, userPublicKey}`, the quote as given or edited (the bot's own key only) | `computeBudgetInstructions`, `setupInstructions`, `swapInstructions` (every leg, in order), `swapInstruction` (the leg, for a route of one), `addressLookupTableAddresses`, `computeUnitLimit` |

The same as JSON-RPC methods `mtx_routerQuote` and
`mtx_routerSwapInstructions`, each taking the same object as its first
parameter: what a strategy component's `rpc` reaches. A quote's legs are
re-quoted on the pools as they are when its instructions are asked for;
an edited `otherAmountThreshold` is what the legs enforce. At most 64 calls
a step. They are answered in a batch too, beside any other call. In a
session without a router they answer a plain "method not found" error (and
`GET /quote` a 404): a probe is no refusal and costs the bot nothing. A
process bot signs and sends the transaction itself; a component
sends a `swap` on the pseudo-venue `router` (raw sends are perp-only so
far).

## The pseudo-venue

A component's session lists `router` (kind `spot`, tradable) last among its
venues. A `swap` line there is routed and sent as one transaction:

| Line | Route |
| --- | --- |
| `swap` sell, base units | the amount of the session's base in, for its quote; `limit` × amount the route's least out |
| `swap` buy, quote units | the amount of quote in, for base; amount ÷ `limit` the least out |
| `market` sell | as a sale of base units |

A purchase of base units or a sale for quote units (exact out) is not
sent. A leg after another allows the scenario's `router.slippage_bps`
(default 50). The next step's notes give the route: the amount in, its
quote, its least out, and each path's share and pools. The legs' fills read
back on their pools as any swap's.

## Mandate

A route's legs are spot fills on the instruments they cross: position caps,
sides and price limits apply to each, in that instrument's own units. A
price floor on `raydium:spot:SPCX-USDC` (USDC per SPCX) says nothing of a
sale through `meteora:spot:SPCX-SOL` (SOL per SPCX): a mandate that bounds
a sale's price bounds every pair it lets the router cross.
