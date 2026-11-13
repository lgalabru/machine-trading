# Venue profile: Kamino (`kamino`), v0

Status: draft

[Kamino Lending](https://kamino.com/docs/products/borrow.md) (klend) is a
peer-to-pool money market on Solana, program
`KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD`. It runs several isolated
**lending markets**; each market has one **reserve** per asset (more for
fixed-rate borrowing), and the reserves of one market back each other as
collateral.

## Instruments

| Reference | Meaning |
| --- | --- |
| `kamino:money-market:<SYMBOL>` | The reserve `<SYMBOL>` names in the table below. |
| `kamino:money-market:<ADDRESS>` | The reserve at the base58 address `<ADDRESS>`, in its own lending market. |

Named reserves:

| Symbol | Lending market | Reserve address | Asset |
| --- | --- | --- | --- |
| `SOL` | Main (`7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF`) | `d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q` | SOL (`So11111111111111111111111111111111111111112`) |
| `USDC` | Main | `D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59` | USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`), variable rate |
| `SPCX-BACKPACK` | Backpack (`4iRHKGsTq3e4uut6e4PfyV9AEbNuXaMu9JaSP378p9qy`) | `GrtBFz6BSky1PiyVBL7jy3w4Z1hKz2wvSjT4vd2iouC3` | SPCX (`SPCXxcqXj6e5dJDVNovHN8744zkbhM2bYudU45BimGb`), Token-2022; collateral only |
| `USDC-BACKPACK` | Backpack | `7gHxajRCcU5sb9rQrpYDvkugSqgGsi9EG947pW7xJueT` | USDC |
| `SPYX-XSTOCKS` | xStocks (`5wJeMrUYECGq41fxRESKALVcHnNX26TAWy4W98yULsua`) | `UvXjBuC7YZYaGB9Rn1PpBD1GySmjzunXgE8Zev9ua8d` | SPYx (`XsoCS1TfEyfFhfvj8EtZ528L3CaKBDBRqRapnBbDF2W`), Token-2022 with a UI multiplier |
| `USDC-XSTOCKS` | xStocks | `97zoywd8mPZsGTg8q1wdD2Wgkdrs2tqusp1Qqcxbyj7E` | USDC |

A symbol names one reserve, never an asset: a market has other reserves of
the same asset (fixed-rate ones), and USDC has a reserve in every market. A
reserve outside Kamino's primary market is named `<ASSET>-<MARKET>`.

## Account

In each lending market the universe uses, the account is every
**obligation** the agent authority owns in that market, together with the
authority's wallet. An obligation's address is a PDA of a tag, an id, the
owner, the market and two seed accounts, so one owner can have several.

The wallet holds a supply too when the agent supplies without an
obligation: `deposit_reserve_liquidity` gives the depositor the reserve's
**collateral token**, a claim on the pool, which counts as supplied.

## Measurements

An obligation stores its deposits, its borrows and their values as of its
last refresh: `refresh_reserve` for every reserve it touches, then
`refresh_obligation`, which accrues interest and reprices. Everything below
is read from an obligation **refreshed in the slot measured**, for example by
simulating those instructions; an enforcer MUST NOT read a stale obligation.

- **Supplied.** For a reserve: the collateral tokens deposited in the
  account's obligations for that reserve (`deposits[i].deposited_amount`),
  plus those in the authority's wallet, converted to the asset at the
  reserve's collateral exchange rate. Interest earned shows as a rising
  exchange rate.
- **Borrowed.** For a reserve: the account's obligations' debt to it
  (`borrows[i].borrowed_amount_sf`, a scaled fraction, 2^60), in the asset's
  units. Interest owed shows as a rising debt.
- **Position.** Supplied minus borrowed, in the asset's base units.
- **Health.** For each obligation with debt: its `unhealthy_borrow_value_sf`
  divided by its `borrow_factor_adjusted_debt_value_sf`. The first sums each
  deposit's value times its reserve's **liquidation LTV**; the second sums
  each borrow's value times its reserve's **borrow factor** (1 for USDC, 1.25
  for SOL at the time of writing). Kamino may liquidate an obligation once
  its LTV, the second over the deposits' value, reaches its liquidation LTV:
  once health is 1 or below.
- **Value.** For each obligation, `deposited_value_sf` minus
  `borrowed_assets_market_value_sf` (the debt's value, without borrow
  factors), plus wallet collateral tokens at the exchange rate and the
  reserve's price. Prices are each reserve's `market_price_sf`, from the
  oracle the reserve is configured with.

## Liquidation

Liquidation is permissionless. Kamino's
[docs](https://kamino.com/docs/products/borrow/liquidations.md) and the
program's `max_liquidatable_borrowed_amount` agree on its shape:

- Each liquidation repays at most a share of the obligation's debt, the
  market's close factor (20% in the docs), in exchange for collateral worth
  more than it repays: the **liquidation bonus**, between each reserve's
  minimum and maximum (1% and 10% for SOL and USDC at the time of writing),
  growing with how far the LTV has passed the liquidation LTV.
- An obligation whose LTV passes the market's insolvency threshold (95% in
  the docs) can be liquidated in full at once.
- Kamino also liquidates for other reasons than health: auto-deleveraging,
  and the end of a fixed-rate borrow's term. They are the venue's, and this
  profile does not count them as the agent's breach.

## Instrument mapping

Every reserve is a [money-market](../instruments/money-market.md)
instrument, and Kamino offers no other kind. Where Kamino's reserves
differ from the profile, or the profile leaves the venue to say:

- **Collateral and borrowing are per reserve.** A reserve with a
  loan-to-value of 0 (Backpack's and the Main market's USDC, among others)
  can be supplied but backs no borrow; one with a borrow limit of 0
  (Backpack's SPCX) can back borrows but can't be borrowed. The mandate's
  `min_position` can only forbid what the reserve allows.
- **Health is the obligation's, not the reserve's.** One obligation's
  deposits back all its borrows in that lending market, so every reserve of
  one market reports the same health.
- **A reserve can refuse an action the profile allows** while it is outside
  its price checks: a reserve whose oracle price is older than its maximum
  age, or more than its divergence threshold away from its TWAP, refuses
  borrows and withdrawals (repays, deposits and liquidations still go
  through). Deposit and borrow limits, and a utilization ceiling on
  borrowing, refuse more.
- **An emptied obligation is closed.** A withdrawal that leaves an
  obligation with no deposits and no debt closes it; the next deposit opens
  it again.
- **Tokenized stocks scale their UI amounts.** xStocks mints carry a UI
  multiplier (Token-2022 `ScaledUiAmount`). klend and this profile count raw
  atoms; the multiplier is what a wallet displays.

## Lending actions

`mtx:strategy@0.2.0`'s lending lines map to klend's instructions, each sent
as one transaction after a refresh of every reserve the obligation touches
and of the obligation:

| Line | klend instruction | Notes |
| --- | --- | --- |
| `deposit(amount)` | `deposit_reserve_liquidity_and_obligation_collateral_v2` | None deposits the whole wallet balance. |
| `withdraw(amount)` | `withdraw_obligation_collateral_and_redeem_reserve_collateral_v2` | The amount is converted to collateral tokens at the reserve's exchange rate; none withdraws everything. |
| `borrow(amount)` | `borrow_obligation_liquidity_v2` | A borrow names its amount; none is not sent. |
| `repay(amount)` | `repay_obligation_liquidity_v2` | None repays all of the debt. |
| `liquidate{obligation, collateral, repay, min-received}` | `liquidate_obligation_and_redeem_reserve_collateral_v2` | `repay` is in the debt reserve's asset (the line's venue), `min-received` in the collateral's; the program pays no less. |

A reduce-only lending line is not sent. Anything else klend offers (flash
loans, elevation groups, other obligations, deposits without an
obligation) goes through a raw transaction.

## The benchmark's enforcer

A scenario with a `lending` section runs a lending session on a snapshot of
Kamino's markets (`snapshots/kamino-2026-11-13`: the Backpack and xStocks
markets, their reserves, Scope's prices and the klend, Farms and Scope
programs, pinned by code hash). Only the reserves the scenario names are
the bot's universe.

- **Measured** every step, after the step's blocks: each of the bot's
  obligations refreshed in that slot by a simulation of `refresh_reserve`
  and `refresh_obligation`, and the wallet's assets and collateral tokens.
- **Valued** at the scenario's price path for each reserve's asset: the
  price its oracle feed is planned to walk, before the feed's failures. A
  feed that goes stale or prints off the path changes what the bot can do
  on Kamino, not what its account is worth. Amounts are the program's.
- **Kept current** by the engine's keeper: every step it refreshes each of
  the session's reserves, where interest accrues, and fails the session if
  a refresh lands on any slot but the step's.
- **Oracles.** Every reserve reads Scope. The scenario's Scope feeds write
  the price entry a reserve's chain starts with, any further chain entries
  restamped, and the TWAP entry with an hour's moving average of the path,
  so a fast move opens the divergence Kamino checks. Every other session
  reserve gets a feed that holds its price fresh.
- **Others in the market.** Synthetic borrowers, drawn from the scenario's
  seed near their liquidation threshold, and competing liquidators that
  act in a drawn order after a drawn delay; their liquidations are in the
  report. Real mainnet obligations are not copied.
- **Not enforced yet:** the mandate's `min_health` limit.
