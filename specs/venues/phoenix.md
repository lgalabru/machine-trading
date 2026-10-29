# Venue profile: Phoenix (`phoenix`), v0

Status: draft

[Phoenix](https://phoenix.trade) is a perpetual futures exchange on Solana
with cross-margin trader accounts.

## Instruments

| Reference | Meaning |
| --- | --- |
| `phoenix:perp:<SYMBOL>` | The Phoenix perpetual market whose symbol is `<SYMBOL>` in Phoenix's `/exchange` metadata, such as `SPCX`. |

## Account

The account is the agent authority's Phoenix cross-margin trader account at
PDA index 0, subaccount 0, together with the authority's wallet USDC.

## Measurements

- **Position.** The trader's `base_lots` in the market, from Hawkeye's
  `view_margin_for_asset`, converted to base units with the market's lot
  size from the `/exchange` metadata.
- **Universe.** Hawkeye's `view_margin` reports `position_count`, the number
  of markets in which the trader holds a position or open orders. An account
  is outside its universe when `position_count` exceeds the number of
  universe markets that `view_margin_for_asset` reports as holding a position
  or orders (`has_position_or_orders`).
- **Value.** Collateral plus unrealized PnL at the mark, from `view_margin`,
  plus the authority's wallet USDC. Unsettled funding is not counted. USDC
  withdrawn from Phoenix to the wallet keeps its value, so withdrawing can't
  hide a drawdown.

## The benchmark's enforcer

`mth grade` audits every rule:

- It measures the account twice a second while the bot runs, and once more
  at the term's last slot.
- It reports the peak position, the most markets held outside the universe,
  and the deepest drawdown, and disqualifies the session on any breach.
- It hands the bot its copy of the mandate in `MTH_MANDATE`, with the term
  set to the session's last slot.

Separately, the benchmark gates the RPC methods a bot may call. That is a
rule of the benchmark's sandbox, not part of the mandate.
