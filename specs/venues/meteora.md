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
whose `owner` is the authority, whatever its account's size. A position is
opened over at most 70 consecutive bins and holds a share of each.
`increase_position_length` widens it, up to 1,400 bins, growing its account:
the bins past the 70th follow the account's fixed part, 112 bytes each
(`PositionBinData`: shares, rewards, fees). `decrease_position_length`
narrows it again. The grader reads every bin of a grown position, for the
cap and the value, and deposits into and withdraws from it across its whole
range.

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
- **Grown positions.** A deposit into, or withdrawal from, a position
  grown past 70 bins names every bin array its range reaches, and makes
  any of them that doesn't exist yet, the owner paying its rent.

## What a bot may run

The audit reads what an account does through a pool's swaps and positions:
fills, liquidity moves, and the positions the cap and the value count. What
the program can do with the account's tokens in any other way (base resting
in a limit order, outside any position; a rebalance that takes base in and
quote out in one instruction, which would read as a swap) would sit outside
the position cap, the account's value and the sides rule. So the grader
lets a bot run only these of the program's instructions, whether its
transaction runs them or a program it calls does, and disqualifies a
session that runs any other ("ran meteora PlaceLimitOrder, which this
benchmark doesn't support"):

| Use | Instructions | Where |
| --- | --- | --- |
| Swap | `swap`, `swap2`, `swap_exact_out`, `swap_exact_out2`, `swap_with_price_impact`, `swap_with_price_impact2` | Through the session's pool |
| Liquidity | `initialize_position`, `initialize_position2`, `initialize_position_pda`, `initialize_bin_array`; `add_liquidity`, `add_liquidity2`, `add_liquidity_by_strategy`, `add_liquidity_by_strategy2`, `add_liquidity_by_strategy_one_side`, `add_liquidity_by_weight`, `add_liquidity_by_weight2`, `add_liquidity_one_side`, `add_liquidity_one_side_precise`, `add_liquidity_one_side_precise2`; `remove_liquidity`, `remove_liquidity2`, `remove_liquidity_by_range`, `remove_liquidity_by_range2`, `remove_all_liquidity`; `claim_fee`, `claim_fee2`, `claim_reward`, `claim_reward2`, `update_fees_and_rewards`, `update_fees_and_reward2` | Through the session's pool, where the session takes liquidity on Meteora (`amm` in its universe) |
| Position | `close_position`, `close_position2`, `close_position_if_empty`, `increase_position_length`, `increase_position_length2`, `decrease_position_length` | Where the session takes liquidity on Meteora |

A swap or a deposit through another of the program's pools is a breach
(the grader reads the pool where each instruction's layout names it, not
anywhere among its accounts), and so is a liquidity or position
instruction where the session lists Meteora for swaps only. Growing or
shrinking a position is allowed: the account counts every bin of a grown
position, so it hides nothing, and an LP widens a position this way.
Everything else is refused, notably `place_limit_order` and
`cancel_limit_order` (limit orders in the bins, `function_type` 2),
`rebalance_liquidity`, `go_to_a_bin`, `initialize_position_by_operator`,
`update_position_operator`, and the pool's and the program's own
administration.

Where the session doesn't list Meteora but its chain holds the program,
the grader reads it all the same: anything the account runs on it is a
breach ("used meteora, which this session doesn't trade"). A position the
account holds in the pool counts toward its spot position and its value
whether or not the session takes liquidity on Meteora.

## Snapshot

`snapshots/spcx-pools-2026-11-13` copies the named pool from mainnet with
its oracle, every bin array with liquidity and its mints, and pins the
program's code by hash. Nothing in it is simulated; its Pyth SPCX account is
stale, as [Raydium's profile](raydium.md#snapshot) says.
