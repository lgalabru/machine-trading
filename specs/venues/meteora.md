# Venue profile: Meteora (`meteora`), v0

Status: draft

[Meteora DLMM](https://docs.meteora.ag/product-overview/dlmm-overview/what-is-dlmm)
is Meteora's liquidity book program on Solana, program
`LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo`. A pool (an `LbPair`) holds
its liquidity in discrete **bins**, each a constant-sum pool at one price:
bin `id`'s price is (1 + `bin_step`/10⁴)^id Y atoms per X atom. A swap
empties bins one at a time from the active one. Anyone can create a pool,
so a pool is identified by its address, not its name.

## Instruments

Each pool offers two instruments:

| Reference | Profile | Meaning |
| --- | --- | --- |
| `meteora:spot:<SYMBOL>` | [spot](../instruments/spot.md) | Swapping against the pool `<SYMBOL>` names below. |
| `meteora:amm:<SYMBOL>` | [amm](../instruments/amm.md) (draft) | Providing liquidity to the same pool's bins: the account's positions in it. |

Named pools:

| Symbol | Pool address | Base (X) | Quote (Y) | Fee |
| --- | --- | --- | --- | --- |
| `SPCX-USDC` | `7nVQtYQipN564E9oBi6yZFGox4WL6CR4zsLyYipqgTWu` | SPCX (`SPCXxcqXj6e5dJDVNovHN8744zkbhM2bYudU45BimGb`, Token-2022) | USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`) | 0.75% base, plus the variable fee; bins 75 bps apart |

A named pool's X is its base and Y its quote.

SPCX is a Token-2022 mint: its transfers go through Token-2022's
`transfer_checked` (the `v2`/`2` instructions), its transfer-hook extension
names no program, so no hook accounts are passed, and its `ScaledUiAmount`
multiplier is 1 at the snapshot. Amounts here are the token's own units
(atoms over its decimals), not multiplied UI amounts; a multiplier other
than 1 would make the two differ.

## Account

On each pool in the universe, the account is the agent authority's wallet
token accounts for X and Y, and every position (`PositionV2`) in the pool
whose `owner` is the authority. A position covers at most 70 consecutive
bins and holds a share of each.

## Measurements

- **Position (spot).** The base held in the wallet, in base units, and in
  the account's positions in pools (in this pool and any other the universe
  names), as the spot profile counts it. Nothing rests and nothing is
  locked.
- **Position (amm).** For each position and each of its bins, its shares
  over the bin's `liquidity_supply`, times the bin's `amount_x` and
  `amount_y`; plus its unclaimed fees: each bin's `fee_x_pending`
  (`fee_y_pending`), and its shares' liquidity times the growth of the
  bin's `fee_amount_x_per_token_stored` since it last counted. The amm
  position is the base in all of them, fees included.
- **Fills.** As for [Raydium](raydium.md#measurements): each of the
  pool's instructions is read on its own from its transfers to and from
  the reserves (`reserve_x`, `reserve_y`); a swap moves them in opposite
  directions, giving the side, the base size and the quote paid or
  received, fee included, with the account whose tokens or signature it
  took as the taker and the pool the maker. Reserves moving one way are
  liquidity, not a fill; it counts for the
  [sides](../instruments/amm.md#sides) rule. A position names its owner in
  its account, so a deposit or withdrawal is also the owner's of the
  position it passes.
- **Top.** The active bin's price less the pool's fee now for a sale, plus
  it for a purchase.
- **Value.** As for Raydium: at the Pyth feed this profile names for the
  base (for SPCX, `Equity.US.SPCX/USD`), USDC at $1, never the pool's own
  price.

## How the grader trades it

| Line | On | Instruction |
| --- | --- | --- |
| `swap` (side, amount, unit, limit) | spot | `swap2` (exact in: a sale of base units, a purchase for quote units) or `swap_exact_out2`; the limit sets the least out or the most in |
| `market` (side, size, limit) | spot | the same, as a swap of `size` base units |
| `add-liquidity` (base, quote, range, position, shape) | amm | `initialize_position` (and `initialize_bin_array` for any missing array), then `add_liquidity_by_strategy2`, or the latter alone on `position` |
| `remove-liquidity` (share, min-base, min-quote) | amm | `remove_liquidity_by_range2` of `share` of every bin, then `claim_fee2`; share 0 claims the fees only |

Where it differs from the profiles, and from [Raydium](raydium.md):

- **Shape.** `spot`, `curve` and `bid-ask` are the program's strategies for
  any mix of X and Y (`SpotImBalanced`, `CurveImBalanced`,
  `BidAskImBalanced`): the same in every bin, most at the active bin, or
  least there. None is `spot`.
- **Range.** A range covers the bins whose prices lie in it, at most 70; a
  wider range is refused. No range is the widest position, 70 bins around
  the active bin, not the whole curve.
- **No minimum on the way out.** The program's withdrawal takes no
  minimum. The grader checks `min-base` and `min-quote` against the
  position's share and fees when it sends, and refuses the line below them;
  the pool can still move before the transaction lands.
- **Fees.** The fee is the base fee plus a variable fee that grows with
  recent volatility (`volatility_accumulator`) and decays with time. The
  named pool collects its fee in Y only (`collect_fee_mode` 1): a sale of X
  pays it out of the Y it receives, a purchase out of the Y it pays.
  Either way the fill's price includes it.
- **No price limit.** The program's swap has none, so the grader's own
  takers and arbitrageur size a swap from the bins (what they hold before
  the limit, fee counted) and send it exact in.
- **Bin arrays.** A swap passes the active bin array and the next two with
  liquidity its way; a swap that would cross more fails.
- The pool also takes limit orders in its bins (`function_type` 2); the
  grader doesn't place them.

## Snapshot

`snapshots/spcx-pools-2026-11-13` copies the named pool from mainnet with
its oracle, every bin array with liquidity and its mints, and pins the
program's code by hash. Nothing in it is simulated; its Pyth SPCX account is
stale, as [Raydium's profile](raydium.md#snapshot) says.
