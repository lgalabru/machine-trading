# Venue profile: Raydium (`raydium`), v0

Status: draft

[Raydium CLMM](https://docs.raydium.io/raydium/liquidity-providers/providing-concentrated-liquidity-clmm)
is Raydium's concentrated-liquidity pool program on Solana, program
`CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK`. A pool trades two tokens
against liquidity that providers place over ranges of price ticks (a tick
`t` is the price 1.0001^t). Anyone can create a pool, so a pool is
identified by its address, not its name.

## Instruments

Each pool offers two instruments:

| Reference | Profile | Meaning |
| --- | --- | --- |
| `raydium:spot:<SYMBOL>` | [spot](../instruments/spot.md) | Swapping against the pool `<SYMBOL>` names below. |
| `raydium:amm:<SYMBOL>` | [amm](../instruments/amm.md) (draft) | Providing liquidity to the same pool: the account's positions in it. |

Named pools:

| Symbol | Pool address | Base | Quote | Fee |
| --- | --- | --- | --- | --- |
| `SPCX-USDC` | `FjuBy7jjf9DXj9d3R7cHpvcnoFW2iQxf7F7P3vqx4Jza` | SPCX (`SPCXxcqXj6e5dJDVNovHN8744zkbhM2bYudU45BimGb`, Token-2022) | USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`) | 0.25%, ticks 60 apart |

A pool orders its two tokens by mint address (`token_mint_0`,
`token_mint_1`); the symbol says which is the base, whatever that order.

SPCX is a Token-2022 mint: its transfers go through Token-2022's
`transfer_checked` (the `v2`/`2` instructions), its transfer-hook extension
names no program, so no hook accounts are passed, and its `ScaledUiAmount`
multiplier is 1 at the snapshot. Amounts here are the token's own units
(atoms over its decimals), not multiplied UI amounts; a multiplier other
than 1 would make the two differ.

## Account

On each pool in the universe, the account is the agent authority's wallet
token accounts for the pool's two mints, and every position in the pool
whose NFT the authority holds. A position (`PersonalPositionState`, a PDA
of its NFT's mint) belongs to whoever holds the NFT (a mint of supply 1),
in any token account of the authority's for its mint, not only the
associated one (as the wallet is read), not to a key it records.

## Measurements

- **Position (spot).** The base held in the wallet, in base units, and in
  the account's positions in pools (in this pool and any other the universe
  names), as the spot profile counts it. Nothing rests in a pool and
  nothing is locked.
- **Position (amm).** For each position, what its liquidity `L` between
  sqrt prices `a` (lower tick) and `b` (upper tick) holds at the pool's
  sqrt price `p`: `L(b − p)/(p b)` of token 0 and `L(p − a)` of token 1
  inside the range, all token 0 below it, all token 1 above it; plus its
  uncollected fees: `token_fees_owed`, and its liquidity times the growth of
  `fee_growth_inside` since it last counted (from the pool's
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
- **Top.** The pool's price (from `sqrt_price_x64`) less its fee for a sale,
  plus it for a purchase: the price of a small swap each way.
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
| `add-liquidity` (base, quote, range, position) | amm | `open_position_with_token22_nft`, or `increase_liquidity_v2` on `position`; the range in quote per base, rounded out to the tick grid; none is the whole curve |
| `remove-liquidity` (share, min-base, min-quote) | amm | `decrease_liquidity_v2` of `share` of the liquidity, failing below the minimums (fees included); share 0 collects the fees only |

Where it differs from the profiles:

- **Liquidity is set by one asset.** A deposit gives up to `base` and
  `quote`; the grader asks for the liquidity the scarcer of the two buys
  over the range, and the program takes what that liquidity needs of the
  other. A range entirely above the price takes base only, below it quote
  only.
- **Rewards.** Taking liquidity out names every reward the pool was ever
  given (its vault, the owner's account for it, its mint), as the program
  requires; rewards are not valued.
- **Price limit for takers.** The grader's own takers and arbitrageur swap
  with a `sqrt_price_limit`: a swap that stops where the next unit would
  cost more than its limit, rather than failing, like a book's
  immediate-or-cancel order.
- **Tick arrays.** A swap passes the current tick array and the next two
  initialized ones its way; a swap that would cross more fails.
- **Shape** (`liquidity-shape`) is Meteora's; a CLMM position is uniform
  over its range, and the grader ignores it here.

## What a bot may run

The audit reads what an account does through a pool's swaps and positions:
fills, liquidity moves, and the positions the cap and the value count. What
the program can do with the account's tokens in any other way (base resting
in a limit order, outside any position) would sit outside the position cap,
the account's value and the sides rule. So the grader lets a bot run only
these of the program's instructions, whether its transaction runs them or a
program it calls does, and disqualifies a session that runs any other ("ran
raydium OpenLimitOrder, which this benchmark doesn't support"):

| Use | Instructions | Where |
| --- | --- | --- |
| Swap | `swap`, `swap_v2` | Through the session's pool |
| Liquidity | `open_position`, `open_position_v2`, `open_position_with_token22_nft`, `increase_liquidity`, `increase_liquidity_v2`, `decrease_liquidity`, `decrease_liquidity_v2` | Through the session's pool, where the session takes liquidity on Raydium (`amm` in its universe) |
| Position | `close_position` | Where the session takes liquidity on Raydium |

A swap or a deposit through another of the program's pools is a breach
(the grader reads the pool where each instruction's layout names it, not
anywhere among its accounts), and so is a liquidity or position
instruction where the session lists Raydium for swaps only. Everything
else is refused, notably the limit orders (`open_limit_order`,
`increase_limit_order`, `decrease_limit_order`, `settle_limit_order`,
`close_limit_order`), the multi-pool `swap_router_base_in`, and the pool's
and the program's own administration.

Where the session doesn't list Raydium but its chain holds the program,
the grader reads it all the same: anything the account runs on it is a
breach ("used raydium, which this session doesn't trade"). A position the
account holds in the pool counts toward its spot position and its value
whether or not the session takes liquidity on Raydium.

## Snapshot

`snapshots/spcx-pools-2026-11-13` copies the named pool from mainnet with
its fee tier, observation account, every initialized tick array and its
mints, and pins the program's code by hash (`docs/snapshot-recipes.md`).
Nothing in it is simulated. Its Pyth `Equity.US.SPCX/USD` account is copied
as mainnet held it: nobody had pushed the feed on chain for about six weeks
before the fork, so it reads 148.965 while the pools trade near 140. A task
that values SPCX at a current price writes the feed (an oracle writer)
rather than reading the copy.
