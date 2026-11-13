# Venue profile: Orca (`orca`), v0

Status: draft

[Orca Whirlpools](https://dev.orca.so/) is Orca's concentrated-liquidity
pool program on Solana, program
`whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`. A pool (a `Whirlpool`) trades
two tokens against liquidity that providers place over ranges of price
ticks (a tick `t` is the price 1.0001^t), the same model as
[Raydium's CLMM](raydium.md). Anyone can create a pool, so a pool is
identified by its address, not its name.

## Instruments

Each pool offers two instruments:

| Reference | Profile | Meaning |
| --- | --- | --- |
| `orca:spot:<SYMBOL>` | [spot](../instruments/spot.md) | Swapping against the pool `<SYMBOL>` names below. |
| `orca:amm:<SYMBOL>` | [amm](../instruments/amm.md) (draft) | Providing liquidity to the same pool: the account's positions in it. |

Whirlpools offers no other instrument this profile supports: its two-hop
swaps are a route across pools (a later venue-routing profile), its
position bundles and locked positions are ways of holding the same
positions, and its rewards are not an instrument.

Named pools:

| Symbol | Pool address | Base (A) | Quote (B) | Fee |
| --- | --- | --- | --- | --- |
| `SPCX-USDC` | `FgjxPHeYsq1axH29fnGKZi6UyRWi4tMPHgu1gc3ezA9U` | SPCX (`SPCXxcqXj6e5dJDVNovHN8744zkbhM2bYudU45BimGb`, Token-2022) | USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`) | 0.30% base, plus an adaptive fee; ticks 64 apart |

The pool is the most liquid SPCX/USDC Whirlpool (about $93,000 in it at the
snapshot, over ten times any other); the others are a fraction of that. A
pool orders its two tokens by mint address (`token_mint_a`, `token_mint_b`);
the symbol says which is the base, whatever that order.

SPCX is a Token-2022 mint: its transfers go through Token-2022's
`transfer_checked` (the `v2` instructions), its transfer-hook extension names
no program, so no hook accounts are passed, and its `ScaledUiAmount`
multiplier is 1 at the snapshot. Amounts here are the token's own units
(atoms over its decimals), not multiplied UI amounts; a multiplier other
than 1 would make the two differ.

## Account

On each pool in the universe, the account is the agent authority's wallet
token accounts for the pool's two mints, and every position in the pool
whose NFT the authority holds. A position (`Position`, a PDA of its NFT's
mint) belongs to whoever holds the NFT, in any token account of the
authority's for its mint, not only the associated one (as the wallet is
read: `getTokenAccountsByOwner` for the mint). The grader opens positions
with a Token-2022 NFT (`open_position_with_token_extensions`); one opened
with a classic SPL Token NFT, by another tool, is read the same way.

## Measurements

- **Position (spot).** The base held in the wallet, in base units, and in
  the account's positions in pools, as the spot profile counts it.
- **Position (amm).** For each position, what its liquidity `L` between
  sqrt prices `a` (lower tick) and `b` (upper tick) holds at the pool's
  sqrt price `p`: `L(b − p)/(p b)` of token A and `L(p − a)` of token B
  inside the range, all token A below it, all token B above it; plus its
  uncollected fees: `fee_owed`, and its liquidity times the growth of
  fees inside its range since it last counted (from the pool's
  `fee_growth_global` and its two boundary ticks' `fee_growth_outside`).
  The amm position is the base in all of them, fees included.
- **Fills.** A pool has no event heap. Each of the pool's instructions a
  transaction runs, its own or one another program calls, is read on its
  own, from the token transfers it makes to and from the pool's two
  vaults (never from the transaction's net, so a swap and a deposit in one
  transaction are both seen). A swap moves the vaults in opposite
  directions: base in is a sale, base out a purchase; the transfers give
  the base size and the quote paid or received, and the pool is the maker.
  The taker is the account whose tokens or signature the swap took (the
  owner of a token account it passes, or a signer), whoever paid the fee;
  for anyone else's swap, the fee payer. The price is quote over base,
  **fee included**: a swap's fee is taken from what goes in and stays in
  the vault. An instruction that moves the vaults one way only adds or
  removes liquidity, and is not a fill; it counts for the
  [sides](../instruments/amm.md#sides) rule, the account's when it takes
  its tokens, its position or its signature.
- **Top.** The pool's price (from `sqrt_price`) less its fee for a sale,
  plus it for a purchase: the price of a small swap each way, at the base
  fee (see below).
- **Value.** Wallet and positions (fees included), with the base at the Pyth
  price feed this profile names for it, never the pool's own price (the
  account's own swaps move it): for SPCX, `Equity.US.SPCX/USD`
  (`8a593d6edde7a3095213c88116d8840d01e93c2ddeb800bc891772eb8b93bb94`), and
  USDC at $1.

## How the grader trades it

A strategy's lines go to the instrument they name:

| Line | On | Instruction |
| --- | --- | --- |
| `swap` (side, amount, unit, limit) | spot | `swap_v2`: exact in for a sale of base units or a purchase for quote units, exact out otherwise; the limit (quote per base, fee included) sets the least out or the most in |
| `market` (side, size, limit) | spot | the same, as a swap of `size` base units |
| `add-liquidity` (base, quote, range, position) | amm | `initialize_dynamic_tick_array` for each of the range's tick arrays that doesn't exist, `open_position_with_token_extensions` and `increase_liquidity_v2`; or `increase_liquidity_v2` alone on `position`. The range in quote per base, rounded down to the tick grid; none is the whole curve |
| `remove-liquidity` (share, min-base, min-quote) | amm | `update_fees_and_rewards`, `decrease_liquidity_v2` of `share` of the liquidity, and `collect_fees_v2`; share 0 collects the fees only |

Where it differs from the other pools (and the profiles):

- **Liquidity is set by one asset**, as on Raydium. A deposit gives up to
  `base` and `quote`; the grader computes the liquidity the scarcer of the
  two buys over the range, a millionth short so the program's rounding up
  of what it costs never passes either amount, and the program takes what
  that liquidity needs of the other. A range entirely above the price takes
  base only, below it quote only.
- **Tick arrays.** A tick array holds 88 ticks (5,632 ticks, about 75% of
  price, at spacing 64) and is a PDA of the pool and its start tick *as
  decimal text*. The program keeps them in two layouts, fixed and dynamic
  (an uninitialized tick takes one byte); the grader reads both, and
  initializes dynamic ones. Unlike Raydium, `open_position` makes no array:
  a deposit's transaction initializes the range's two arrays where they
  don't exist, the owner paying their rent. A swap names three consecutive
  arrays from the current one in its direction, existing or not (the
  program takes a missing one and fails the swap only if it must cross it,
  where Raydium names the next initialized ones); a swap that must cross
  more than three fails.
- **Fee tiers.** The fee is a rate stored on the pool itself (here 0.30%,
  at tick spacing 64): there is no fee-tier account to read, as there is on
  Raydium. A pool with an adaptive fee (this one) adds a volatility-based
  fee on top of its base rate, from an oracle account the pool keeps; the
  grader reads the base rate for the top and the fee, and the adaptive
  part, zero when the price has been still, shows only in what a swap
  actually paid (a fill's price comes from the vaults and includes it). A
  limit that counts the base fee only can fail a swap made in a volatile
  moment, which is the limit working.
- **Fees are collected apart.** A decrease of liquidity pays the principal
  only; a position's fees stay on it until `collect_fees_v2`, so a
  withdrawal is the three instructions above. Its minimums count the fees
  too: the principal's minimum is the order's less the fees already earned,
  which only grow.
- **Rewards.** A pool can give liquidity providers reward tokens (up to
  three), collected with `collect_reward_v2`. The grader doesn't collect
  or value them; this pool has none.
- **Price limit for takers.** The grader's own takers and arbitrageur swap
  with a `sqrt_price_limit`: a swap that stops where the next unit would
  cost more than its limit, rather than failing, like a book's
  immediate-or-cancel order. A swap with no explicit limit fails if the
  pool can't fill it.
- **Closing.** Taking out all the liquidity leaves the empty position and
  its NFT with the account, as on Raydium: the position no longer counts.
  The grader doesn't close the account to reclaim its rent.
- **Shape** (`liquidity-shape`) is Meteora's; a Whirlpool position is
  uniform over its range, and the grader ignores it here.

## What a bot may run

The audit reads what an account does through a pool's swaps and positions:
fills, liquidity moves, and the positions the cap and the value count. What
the program can do with the account's tokens in any other way (base resting
in a bundled position, whose NFT is the bundle's and not the position's)
would sit outside the position cap, the account's value and the sides rule.
So the grader lets a bot run only these of the program's instructions,
whether its transaction runs them or a program it calls does, and
disqualifies a session that runs any other ("ran orca OpenBundledPosition,
which this benchmark doesn't support"):

| Use | Instructions | Where |
| --- | --- | --- |
| Swap | `swap`, `swap_v2` | Through the session's pool |
| Liquidity | `initialize_tick_array`, `initialize_dynamic_tick_array`, `open_position`, `open_position_with_metadata`, `open_position_with_token_extensions`, `increase_liquidity`, `increase_liquidity_v2`, `increase_liquidity_by_token_amounts_v2`, `decrease_liquidity`, `decrease_liquidity_v2`, `update_fees_and_rewards`, `collect_fees`, `collect_fees_v2`, `collect_reward`, `collect_reward_v2` | Through the session's pool, where the session takes liquidity on Orca (`amm` in its universe) |
| Position | `close_position`, `close_position_with_token_extensions` | Where the session takes liquidity on Orca |

A swap or a deposit through another of the program's pools is a breach,
and so is a liquidity or position instruction where the session lists Orca
for swaps only. Everything else is refused, notably position bundles
(`initialize_position_bundle`, `open_bundled_position` and the rest),
`lock_position`, `transfer_locked_position`, `reset_position_range`,
`reposition_liquidity_v2`, the two-pool `two_hop_swap` and
`two_hop_swap_v2`, and the pool's and the program's own administration.

## Snapshot

`snapshots/spcx-pools-2026-11-13` copies the named pool from mainnet with
every tick array that exists, its oracle and its mints, and pins the
program's code by hash (`docs/snapshot-recipes.md`). Nothing in it is
simulated. Its Pyth `Equity.US.SPCX/USD` account is copied as mainnet held
it: nobody had pushed the feed on chain for about six weeks before the
fork, so it reads 148.965 while the pools trade near 141. A task that
values SPCX at a current price writes the feed (an oracle writer) rather
than reading the copy.
