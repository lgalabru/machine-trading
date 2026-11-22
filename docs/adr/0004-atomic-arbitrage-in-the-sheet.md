# 0004. Atomic arbitrage: one transaction per sheet group, steps that end at a trigger, an oracle bots can push

- **Status:** Accepted (2026-11-21).
- **Scope:** `mtx:strategy@0.3.0`, the grader's sessions, the mandate's
  v0.3.0, and the atomic arbitrage tasks of
  [RFD 0005](../rfd/0005-atomic-arbitrage-tasks.md).

## Context

We studied mainnet before designing the atomic arbitrage tasks
([research](../RESEARCH.md)):

- **Kamino liquidations.** Half land in the slot of the oracle update
  that made them possible, and 93% of those above $1k do. The winner
  pushed that update itself in 39% of cases, in the same transaction as
  the liquidation in 31%.
- **SOL/USDC backruns.** 18 of 21 landed in the slot of the swap they
  closed, 12 of them right behind it. A route is several swaps that must
  land together or not at all.

The repository couldn't express any of it:

- A sheet line is one transaction.
- Raw sends compose anything, but only in one-market perp sessions, and
  their audit reads Phoenix.
- A step is two slots, and nothing moves while the bot thinks.
- Kamino's oracle, Scope, is written by the scenario through overrides, so
  a bot that pushed a refresh would read stale sources.
- The mandate can't list the tokens a route passes through, and can't say
  that a route must end flat.

## Decision

- **Atomic groups.** `mtx:strategy@0.3.0` adds `tx-options.atomic`. Lines
  with the same id compile, in sheet order, into one transaction:
  - Each line's seat gives its instructions (`Seat::instructions`)
    instead of sending them.
  - The host places the compute budget first, so instruction indexes are
    final before anything refers to them.
  - A `flash` action on a lending venue becomes a flash borrow before the
    group and its repay after it.
  - A group fails as a whole, and every line in it reports the failure.
- **Trigger steps.** A scenario names its triggers: a large swap, an
  oracle update. The session lands a trigger, ends the step with the slot
  still open, lets the bot and the searchers act, and orders their sends
  in the rest of that slot through the transaction layer. Steps without a
  trigger stay two slots long. The number of steps is fixed by the
  scenario, so replay is unchanged.
- **An oracle bots can push.** A lending scenario rewrites the Scope
  entries it moves to `FixedPrice` (type 23). Each slot it writes the
  upstream price into the mapping's `generic` data, which any bot can
  read. Scope's own price changes only when someone sends
  `refresh_price_list`: a keeper on mainnet's cadence, or a bot that
  pushes first. Scope requires the refresh to be the transaction's first
  instruction after the compute budget, which an atomic group allows.
- **Mandate v0.3.0**, in the shape the spec proposes: wildcard pool
  allowances with `assets`, `limits.inventory` judged after every
  transaction, `max_flash_borrow`, and `limits.max_costs_usd`.

## Consequences

- Composition stays inside the sheet, where every line is an action the
  grader understands. The audit, the fill records and the reference bots
  need no simulation of arbitrary programs. Raw sends keep their role,
  for what the sheet can't say.
- The WIT changes version. 0.2.0 is frozen and stays supported, as 0.1.0
  is.
- A trigger step is part of a slot. The keeper's refresh, the account's
  sampling and `slot_closing` must tolerate steps that don't end on a slot
  boundary.
- `FixedPrice` makes the oracle's source a number in an account, not a
  feed with a publisher. It reproduces the lead of upstream over Scope,
  but not the shape of Pyth Lazer's updates. A task that needs those
  needs a Lazer template.
- The transaction layer must take lending and pool sessions, and actors'
  sends must enter its auction under their own names (`engine.rs` tags
  every entry as the bot's today).
