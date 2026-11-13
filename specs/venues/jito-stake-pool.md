# Venue profile: Jito stake pool (`jito-stake-pool`), v0

Status: draft

[Jito's stake pool](https://www.jito.network/docs/jitosol/) issues JitoSOL.
It runs on the SPL stake pool program,
`SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy`; Sanctum's pools run on forks
of it with the same instructions (`SP12tWFxD9oJsVWNavTTBZvMbA6gkAmxtVgxdqvyvhY`,
`SPMBzsVUuoHA4Jm6KunbsotaahvVikZs1JyTW6iJvbn`), so one implementation serves
them; they are not named here yet.

## Instruments

| Reference | Profile | Meaning |
| --- | --- | --- |
| `jito-stake-pool:stake-pool:JITOSOL` | [`stake-pool`](../instruments/stake-pool.md) (proposed) | The pool below, and its token. |

| Symbol | Pool | Token mint | Validator list | Reserve stake |
| --- | --- | --- | --- | --- |
| `JITOSOL` | `Jito4APyf642JPZPx3hGc6WWJ8zPKtRbRs4P815Awbb` | `J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn` | `3R3nGZpQs2aZo5FDQvd2MUQ6R7KhAPainds6uT6uE2mn` | `BgKUXdS29YcHCFrPm5M8oLHiTzZaMDjsebggjoaQ6KFL` |

The pool's withdraw authority is the PDA `[pool, "withdraw"]`; its manager
fee account is `8yoigZfzZ1nNaadumY9uPVD118225UYHTDpmjpr2nrSa`.

## Account

The agent authority's wallet: its lamports, its token accounts of the
pool's mint, and the stake accounts whose withdraw authority it is (Meta's
withdrawer, byte 44 of a 200-byte stake account).

## Actions

| Action | Instruction | Notes |
| --- | --- | --- |
| Stake `amount` SOL | `DepositSol` (14) | Mints `lamports × supply / total_lamports` tokens, less the SOL deposit fee (0 at the time of writing). The SOL goes to the reserve. An `amount` of none stakes all but 0.05 SOL, kept for fees. |
| Redeem `amount` tokens at once | `WithdrawSol` (16) | Pays `(tokens − fee) × total_lamports / supply` lamports from the reserve stake, the fee 0.1% at the time of writing. The program refuses more than the reserve holds above its rent (`SolWithdrawalTooLarge`, 35). |
| Redeem as a stake account | `WithdrawStake` (10), then the stake program's `Deactivate` and, an epoch later, `Withdraw` | A raw send. Splits the tokens' SOL off a validator's stake account into a new stake account the authority withdraws from once it has cooled down. |

The pool program refuses every action after an epoch boundary until the
pool is updated (`StakeListAndPoolOutOfDate`, 17).

## Measurements

- **Position.** The token, summed over every token account the authority
  owns for the mint.
- **Rate.** `total_lamports / pool_token_supply` from the pool account
  (byte offsets 258 and 266).
- **Value.** As the [stake-pool profile](../instruments/stake-pool.md#value):
  the token at the rate, and the wallet's SOL and its stake accounts at
  Pyth SOL/USD.
- **Fills.** Each transaction that moved the reserve stake's lamports and
  the authority's token the opposite way: a stake (buy) or an instant
  redemption (sell), at the reserve's SOL over the token.
- **Top.** For a component's venue view: the ask is the rate grossed up by
  the deposit fee; the bid is the rate less the withdrawal fee, absent when
  the reserve can't pay 1 SOL.
- **Stats** (`venue-view.stats`): `rate`, `deposit_fee_bps`,
  `withdraw_fee_bps`, `instant_capacity_sol` (the reserve above its rent),
  `last_update_epoch`, `epoch_fee_bps`, `epoch`, `slots_to_epoch_end`.

## Where it differs from the profile

- **Instant capacity is the reserve.** Jito's staker delegates what the
  reserve takes in, so it holds little: 16 SOL on 2026-11-12, 0.002 SOL a
  day later. Exits at size go through a pool (an AMM) or a stake account.
  A deposit adds to the reserve, so a wallet can redeem its own fresh
  deposit at once.
- **Epoch boundaries.** Rewards arrive as stake on the validators. A
  session's boundary (`mtx_grader::epochs`) marks every validator in the
  list updated for the new epoch, credits the epoch's reward to one
  validator no transaction can reach (one whose stake account the session
  doesn't hold), and sends the program's own `UpdateStakePoolBalance` from a
  grader account. The program sums the pool, mints its epoch fee (4%) to
  the manager in tokens, and marks the pool current, so the rate moves by
  the reward less the fee, as on mainnet.
- **Starting holdings.** A session's wallet that starts with JitoSOL staked
  it before the session; its SOL is moved from the reserve to that same
  validator, as Jito's staker would, so the reserve starts where mainnet's
  was.

## Calibration (2026-11-13)

From the pool account and GeckoTerminal's Orca JitoSOL/SOL pool
(`Hp53XEtt4S8SvPCXarsLSdGfZBuUr5mMmZmX2DRNXQKp`), 2026-05-14 to 2026-11-13,
keyless:

- **Rate drift.** 1.36 bps a day (5.1% a year). Mainnet slots ran at about
  216 ms (100,000 slots in 21,588 s on 2026-11-13), so an epoch of 432,000
  slots is about 26 hours, and the pool's last epoch moved its rate by
  1.34 bps. A boundary's reward (`reward_bps`, before the 4% fee) is about
  1.4 bps, not the 2.7 bps two-day epochs would give.
- **Dislocation.** Daily closes sit within −17.9 / +8.8 bps of the rate
  trend (5th and 95th percentiles −5.6 / +6.1). 67 of 184 days touched an
  intraday low more than 30 bps under it, and 2 more than 100: the wick
  below, and one of −103 bps.
- **The −700 bps wick of 2026-07-20 is not a dislocation.** Minute bars show
  one candle at 16:24 UTC with a low of exactly 1.2000 and a volume of
  0.00000015, closed back at 1.2906 the next minute: a dust print. It is
  left out of stress targets; the next worst lows are about −100 bps.
