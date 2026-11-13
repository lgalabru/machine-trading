# Venue profile: Marinade (`marinade`), v0

Status: draft

[Marinade](https://docs.marinade.finance) issues mSOL. It is its own
program, `MarBmsSgKXdrN1egZf5sqe1TMai9K1rChYNDJgjq7aD` (Anchor), not an SPL
stake pool: one state account, a reserve, and a liquidity pool of SOL and
mSOL that pays instant exits.

## Instruments

| Reference | Profile | Meaning |
| --- | --- | --- |
| `marinade:stake-pool:MSOL` | [`stake-pool`](../instruments/stake-pool.md) (proposed) | Marinade's state below, and mSOL. |

| Account | Address |
| --- | --- |
| State | `8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC` |
| mSOL mint | `mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So` |
| Liquidity pool's mSOL leg | `7GgPYjS5Dza89wV6FpZ23kUJRG5vbQ1GM25ezspYFSoE` |
| Liquidity pool's SOL leg | PDA `[state, "liq_sol"]` |
| Reserve | PDA `[state, "reserve"]` |
| Treasury mSOL | `B1aLzaNMeFVAyQ6f3XbbUyKcH2YPHu2fqiEagmiF23VR` |

## Account

The agent authority's wallet: its lamports, its token accounts of mSOL,
and the tickets it is the beneficiary of (88-byte accounts of the program,
the beneficiary at byte 40).

## Actions

| Action | Instruction | Notes |
| --- | --- | --- |
| Stake `amount` SOL | `deposit` | Mints mSOL at the state's virtual staked SOL over its mSOL supply, first from the liquidity pool's mSOL leg. No fee. An `amount` of none stakes all but 0.05 SOL. |
| Unstake `amount` mSOL at once | `liquid_unstake` | Sells mSOL into the liquidity pool's SOL leg at `msol_price`, less a fee that falls linearly from `lp_max_fee` at an empty leg to `lp_min_fee` at `lp_liquidity_target` (1.5% to 0.03%, target 3,500 SOL, at the time of writing). The program refuses more than the leg holds. |
| Unstake as a ticket | `order_unstake`, then `claim` | A raw send. Burns mSOL for a ticket worth `msol_price` less the delayed unstake fee; `claim` pays it from the reserve once the epoch has passed and 30 more minutes. |

## Measurements

- **Position.** The mSOL, summed over every token account the authority
  owns for the mint.
- **Rate.** `msol_price / 2^32` from the state (byte offset 512): what an
  instant or delayed exit pays per mSOL before fees.
- **Value.** As the [stake-pool profile](../instruments/stake-pool.md#value):
  mSOL at the rate, the wallet's SOL and its tickets' `lamports_amount` at
  Pyth SOL/USD.
- **Fills.** Each transaction that moved the reserve's or the SOL leg's
  lamports and the authority's mSOL the opposite way: a stake (buy) or an
  instant unstake (sell), at that SOL over the mSOL.
- **Top.** The ask is the deposit's price, virtual staked SOL per mSOL; the
  bid is the rate less the liquid unstake fee for 1 SOL, absent when the
  SOL leg holds less.
- **Stats** (`venue-view.stats`): `rate`, `deposit_fee_bps` (0),
  `withdraw_fee_bps` (the liquid unstake fee for 1 SOL),
  `instant_capacity_sol` (the SOL leg above its rent), `epoch`,
  `slots_to_epoch_end`.

## Where it differs from the profile

- **Two prices.** Deposits mint at virtual staked SOL over supply; exits
  pay `msol_price`. Marinade's update keeps them equal, and between
  updates they differ by rounding.
- **Instant capacity is a pool, with a curve.** The liquid unstake fee
  rises as the SOL leg drains (about 1.04% for a small exit at 1,088 SOL on
  2026-11-12), so the bid falls with size, and a deposit that swaps from
  the mSOL leg refills it.
- **Epoch boundaries.** A session's boundary (`mtx_grader::epochs`) moves
  the state's `total_active_balance` and `msol_price` as Marinade's update
  would after the epoch's rewards on its active stake: the rate moves by the
  reward times active stake over virtual staked SOL. The price is computed
  from the state at the session's start; a bot's own Marinade trades before
  the boundary shift it by under a billionth.
- **Lenders read the same account.** Kamino prices mSOL through Scope's
  `MsolStake` entry, computed from the state's `msol_price`, which a
  boundary writes: a Scope refresh after it prices mSOL at the new rate.
- **Tickets.** A ticket counts at its SOL until claimed. The session's
  boundary lands 7,500 slots (31 minutes at 250 ms) into the new epoch, past
  Marinade's 30-minute wait, so a ticket ordered before it can be claimed
  after it.
