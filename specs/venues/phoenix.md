# Venue profile: Phoenix (`phoenix`), v0

Status: draft

[Phoenix](https://phoenix.trade) runs two kinds of market on Solana, each its
own program: perpetual futures with cross-margin trader accounts, and spot
order books (Phoenix v1, program `PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY`).

## Instruments

| Reference | Meaning |
| --- | --- |
| `phoenix:perp:<SYMBOL>` | The Phoenix perpetual market whose symbol is `<SYMBOL>` in Phoenix's `/exchange` metadata, such as `SPCX`. |
| `phoenix:spot:<SYMBOL>` | The Phoenix v1 spot market `<SYMBOL>` names in the table below. |
| `phoenix:spot:<ADDRESS>` | The Phoenix v1 spot market at the base58 address `<ADDRESS>`. |

Named spot markets:

| Symbol | Market address | Base | Quote |
| --- | --- | --- | --- |
| `SOL-USDC` | `4DoNfFBfF7UokCC2FQzriy7yHK6DY6NVdYpuekQ5pRgg` | SOL (wrapped, `So11111111111111111111111111111111111111112`) | USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`) |

## Perpetual markets

### Account

The account is the agent authority's Phoenix cross-margin trader account at
PDA index 0, subaccount 0, together with the authority's wallet USDC.

### Measurements

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

## Spot markets

### Account

On each spot market in the universe, the account is the agent authority's
seat on that market: its `TraderState` in the market account, which the
program keeps for every trader with a seat, together with the authority's
wallet token accounts for the market's base and quote mints.

The wallet matters for trading too: a `Swap` order trades straight from the
wallet's token accounts, without a seat.

### Measurements

For a market with base lot size `BL` and quote lot size `QL`, the trader's
`TraderState` holds:

- **Base**: (`base_lots_free` + `base_lots_locked`) × `BL`, in base atoms.
- **Quote**: (`quote_lots_free` + `quote_lots_locked`) × `QL`, in quote atoms.

From them:

- **Position.** The base held in the wallet, plus the trader state's base,
  in base units. For SOL, the wallet's holding is its wrapped SOL token
  account; native SOL is not counted, since the authority spends it on
  transaction fees.
- **Fills.** Each match is a `Fill` event in the instruction's audit log,
  with the maker (`maker_id`), the maker order's `order_sequence_number`,
  the price (`price_in_ticks`) and the size (`base_lots_filled`). The
  sequence number gives the maker's side (`Side::from_order_sequence_number`),
  and the taker's is the opposite; the log's header names the taker, as
  `signer`. Phoenix settles both sides of a match in the same instruction,
  so the position never lags a fill.
- **Open orders.** An order resting in the market's book whose trader index
  is the authority's seat.
- **Value.** Base and quote holdings, wallet and trader state together, with
  the base at the Pyth price feed for it (for SOL, the SOL/USD feed
  `ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d`) and
  USDC at $1.

Wallet balances that both Phoenix and another venue in the universe count,
such as the authority's USDC, are counted once, as the
[mandate](../mandate.md#account-value) requires.

## The benchmark's enforcer

`mtx grade` audits every rule for the perpetual markets:

- It measures the account twice a second while the bot runs, and once more
  at the term's last slot.
- It reports the peak position, the most markets held outside the universe,
  and the deepest drawdown, and disqualifies the session on any breach.
- It hands the bot its copy of the mandate in `MTX_MANDATE`, with the term
  set to the session's last slot.

It audits spot sessions on the spot market this profile names, alone or
with OpenBook's:

- It measures the account at every step of the session, every two slots,
  and once more after the bot's last turn. The position is the SOL held, in
  the wallet and on each venue in the universe; the value is both assets at
  the Pyth SOL/USD price and USDC at $1, each counted once, for the
  drawdown limit when the mandate sets one.
- A side has an open order when the account's resting orders lock base
  (asks) or quote (bids) on it.
- At the end of the term it reads every fill the market recorded for the
  account, as a taker or a maker, from the `Fill` events in the audit logs
  of the term's transactions, and audits them against the allowances'
  sides and limit prices.
- It hands the bot its copy of the mandate in `MTX_MANDATE`, with the term
  set to the session's last slot, and disqualifies the session on any
  breach.

No benchmark task enforces the spot markets yet; `just spot-smoke` runs a
session end to end.

Separately, the benchmark gates the RPC methods a bot may call. That is a
rule of the benchmark's sandbox, not part of the mandate.
