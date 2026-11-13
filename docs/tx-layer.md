# The transaction layer

A scenario can turn on a transaction layer (`tx_layer`). With it on, nothing
the bot sends runs when it arrives. The grader collects the sends, then
builds each slot's block. Fees, compute budgets, Jito tips and bundles,
congestion, drops and blockhash expiry then decide what lands, when, and at
what cost.

Without `tx_layer`, every session runs exactly as before.

The code is in `crates/grader/src/txlayer/`.

## Collect, then build

1. **Collect.** The bot's sends go into one pool:
   - a process bot's through its guard: `sendTransaction`, the staked route
     at `/staked` where the scenario offers one, and the mock block engine
     under `/api/v1/`;
   - a component's through the grader, with each line's transaction options
     applied. Raw sends take their options too.

   A send returns its signature (or its bundle's id) at once. An entry
   records:
   - its arrival: the step it was sent in and its sequence number in that
     step;
   - the route it took;
   - each transaction's compute-unit limit and price, signatures, written and
     read accounts (lookup tables resolved), blockhash, Jito tips, and any
     `jitodontfront` key.

2. **Build.** Before each slot closes, the builder picks what lands:
   1. The slot's **leader** skips it, or produces it. Leaders come from a
      seeded set of synthetic validators: 4 slots each, chosen by stake, each
      running Jito or not. No real validator is named.
   2. An entry is **eligible** once three things hold:
      - its route's latency (a seeded draw, in slots) has passed;
      - its delivery wasn't lost. A loss is a seeded draw per delivery
        attempt, more likely when the block is full and the send pays less
        than the price that clears it;
      - its blockhash is still valid: 150 blocks.
   3. On a **Jito leader**, bundles are ranked:
      - by tip per requested compute unit, then by tip;
      - a bundle that conflicts with one already taken is skipped. Jito's tip
        accounts don't count as a conflict;
      - a bundle with a `jitodontfront` transaction anywhere but first is
        refused.

      A bundle lands whole, at the top of the block, if its tip outbids the
      background on the hot accounts it writes, and it fits a quarter of the
      block.
   4. **Transactions** are sorted:
      - by what they pay per requested compute unit, base fee included;
      - then by arrival;
      - then by a seeded hash of the sender and its sequence number. Never by
        a transaction's bytes, so grinding a memo can't win a tie.

      One lands if its price clears the block and every hot account it
      writes, and if its requested units fit what the background left of the
      block (100M) and of each account it writes (12M). With the calibrated
      small chance, two adjacent conflicting transactions swap places.
3. **Commit.** The session sends the picks to the surfnet one at a time, in
   order, each awaited: bundles through `sendBundle`, which is all or
   nothing, and transactions through `sendTransaction`. Then it produces the
   slot. A transaction that lands and fails still pays its fee. A failed
   bundle costs nothing.

A skipped slot is an empty block: block height keeps pace with slots.

## Background demand

Mainnet blocks hold thousands of transactions; a session holds tens. The
background ("ghosts") isn't executed. For each slot and each hot account the
scenario names (and the block as a whole), it sets two things:

- the **clearing price** a transaction must pay to land;
- the **share of the cap** the background takes.

Per slot:

- **Regime.** Calm, busy or spike: pinned by the scenario's windows of
  steps, or drawn from the calibration's per-slot switching
  (`drawn_regimes`), or calm.
- **Contention.** A seeded draw against the regime's `p_contended`. An
  uncontended account clears at 0, so a zero-fee send can land, as on
  mainnet.
- **Price.** Its log follows an AR(1) around the regime's mean.

The path is drawn before the session from the seed alone. A `congestion`
fault in the same scenario becomes a floor on the block's price over its
window.

## Calibration

The numbers come from `calibration/tx_layer/<id>.toml`. Each file is fitted
to sampled public mainnet blocks (keyless, rate-limited), and its
`<id>.inputs.json` cites every download by URL and SHA-256. The grader
carries the shipped files in its binary, and a scenario names one by id.

The file shipped, `mainnet-fees.20261113.43786520`, covers:

- **The samples:** 300 full blocks in calm, busy and spike windows.
- **Regimes:** from the leaders' fee rewards over 39 hours.
- **Network numbers:** the skip rate and Jito stake share, from
  `getBlockProduction`, `getVoteAccounts` and Jito's validator list.
- **Tips:** from Jito's public tip floor.
- **Classes:** `block`; `amm_pool`, from three SOL/USDC pools; `oracle`,
  from Pyth SOL/USD.

Its limits are in its `[meta] limits`:

- `book` has no sample of its own: no Phoenix market was hot enough, so it
  copies `amm_pool`.
- The spike window's fees were block-wide, not on the pools. A task that
  wants a local spike pins a regime window, or a congestion floor.
- `inversion_p` is a default, not fitted.

A test (`the_shipped_calibration_draws_what_it_observed`) draws 20,000
slots per class and regime. It checks the drawn clearing prices against the
observed ones, at the uncontended share and at each observed percentile:
a Kolmogorov-Smirnov distance within what a sample of that size allows.

## The scenario section

```json
"tx_layer": {
  "seed": 17,
  "calibration": "mainnet-fees.20261113.43786520",
  "hot_accounts": [{ "address": "HCPybc8PFzS3gL2AHWXtT7jT1WDYSva7A6qf9aVGiLMf", "class": "book" }],
  "regimes": [{ "from_step": 20, "to_step": 120, "regime": "spike" }],
  "drawn_regimes": false,
  "routes": { "staked": { "latency_slots": [0, 1], "loss": 0.002, "loss_congested": 0.05, "fee_lamports": 10000 } },
  "validators": 24,
  "max_pending": 64,
  "fees_in_pnl": true,
  "native_sol": 1.0,
  "sol_usd": 150.0
}
```

Every field but `seed` and `calibration` has a default.

- **`routes`.** `rpc` and `jito` are always there. `staked` exists only if
  the scenario lists it. A route's `fee_lamports` is its price per send. It
  counts in the fee ledger and in PnL, but is not paid on chain.
- **`max_pending`.** Past this many pending sends, a send is refused with
  `-32429`, as a rate-limited node would.
- **`native_sol`.** Replaces the session's usual SOL grant.
- **`fees_in_pnl`.** Values the wallet's SOL at `sol_usd`. Rent moved into
  token accounts the bot can close counts as the bot's, not as a cost.
- **Session kinds.** Only perp sessions take the layer for now. A spot
  venue's seats join when they can send through the pool.

## What the bot sees

- **Fees.** `getRecentPrioritizationFees`, with or without an account
  filter, comes from the background demand: for each of the last 150
  slots, the price it took to land.
- **Leaders.** `getSlotLeader`, `getSlotLeaders`, `getLeaderSchedule` and
  `getBlockProduction` are the synthetic validators' answers.
- **The mock block engine** at `/api/v1/bundles` and `/api/v1/transactions`
  (`?bundleOnly=true`):
  - `sendBundle` and `sendTransaction`;
  - `getBundleStatuses` and `getInflightBundleStatuses`;
  - `getTipAccounts`: Jito's eight real tip accounts, which exist in the
    session as on mainnet;
  - `simulateBundle`, answered by the surfnet's sandbox.

  `GET /api/v1/bundles/tip_floor` gives the calibration's tip percentiles.
- **Statuses.** A send's status is null until it lands. Nothing pending is
  visible to anyone.

A component's session says `applies_tx_options: true`, and its market
carries `fees`: recent prices on the hot accounts, the tip floor, and the
next 8 leaders. Each line's options apply:

| Option | Effect |
|---|---|
| `cu_limit` | The transaction's compute-unit limit. None, with other options set, is what its simulation used plus 10% and 1,000. |
| `cu_price` | Its priority fee, in micro-lamports per compute unit. |
| `tip` | Adds a transaction paying one of the tip accounts, and makes the line a bundle on the Jito route. |
| `bundle` | Lines with the same id go as one bundle, in sheet order, at most five transactions, tip included. |
| `route` | `rpc`, `staked`, or `jito { bundle_only, dont_front }`. `dont_front` adds a `jitodontfront` key, read-only, to the bundle's first transaction. |
| `retry_until_slot` | Rebroadcasts the same transaction each slot until then if a delivery is lost. Once its blockhash has expired and it never landed, the grader signs it again on a new one. |

A raw send that sets its own compute budget can't also set `cu_limit` or
`cu_price` in its options; the send is refused.

Each send's status is `delayed` until something becomes of it. A later
step's market then says what: `landed` or `failed`, with its slot, fee and
tip (a bundle's tip shows on its first transaction), or `expired`.

## What the report says

The session report gains a `tx_layer` section:

- **`blocks`:** every slot where something landed or was refused, or the
  leader skipped. Each gives the regime, the clearing price, and what landed
  in order: sender, fee, tip, requested units, whether it succeeded, and its
  bundle.
- **`sends`:** what became of each of the bot's sends: its step and sequence
  number, route, state, slot, fee, tip, delivery attempts, and times signed
  again.
- **`fees`:** the ledger. It splits what the bot paid into base, priority,
  tips, failed transactions' fees, and route fees. It is checked against the
  wallet's SOL change, and `unexplained` is 0 when they agree.

`--replay` compares the whole report, so ordering is part of the replay
check.

## Raw sends' simulation budget

Each raw send costs the grader a simulation when taken and another when
sent, and one can take a good fraction of a second. A session's raw sends
get at most `MAX_SIMULATED_PER_SESSION` (128) simulations in all, on top of
the 16 per step. The budget is a count, not a clock, so the same session
refuses the same sends however loaded the machine is.
