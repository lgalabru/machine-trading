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

Named reserves, in Kamino's primary market, the SOL/BTC Market
(`7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF`):

| Symbol | Reserve address | Asset |
| --- | --- | --- |
| `SOL` | `d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q` | SOL (`So11111111111111111111111111111111111111112`) |
| `USDC` | `D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59` | USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`), variable rate |

The same market has other USDC reserves, such as fixed-rate ones, so a
symbol names one reserve, never an asset.

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

## The benchmark's enforcer

No benchmark task enforces this profile yet.
