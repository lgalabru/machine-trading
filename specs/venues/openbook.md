# Venue profile: OpenBook (`openbook`), v0

Status: draft

[OpenBook v2](https://github.com/openbook-dex/openbook-v2) is a
permissionless central limit order book on Solana, program
`opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb`. Anyone can create a
market, so a market is identified by its address, not its name.

## Instruments

| Reference | Meaning |
| --- | --- |
| `openbook:spot:<SYMBOL>` | The OpenBook v2 market `<SYMBOL>` names in the table below. |
| `openbook:spot:<ADDRESS>` | The OpenBook v2 market at the base58 address `<ADDRESS>`. |

Named markets:

| Symbol | Market address | Base | Quote |
| --- | --- | --- | --- |
| `SOL-USDC` | `CFSMrBssNG8Ud1edW59jNLnq2cwrQ9uY5cM3wXmqRJj3` | SOL (wrapped, `So11111111111111111111111111111111111111112`) | USDC (`EPjFWdd5AufqSSquM8ZiZRXbwhqRBLx1uExtCRgXk68w`) |

A market's `name` field is not a symbol: two markets can share a name.

## Account

On each market in the universe, the account is the agent authority's open
orders accounts on that market: the program's `OpenOrders` accounts the
authority owns, as its `OpenOrdersIndexer` lists them. With the authority's
wallet token accounts for the market's base and quote mints, they hold
everything the account has on the venue.

## Measurements

For a market with base lot size `BL` and quote lot size `QL`, each open
orders account's `position` holds:

- **Base**: `base_free_native` + `asks_base_lots` × `BL`, in base atoms.
- **Quote**: `quote_free_native` + `bids_quote_lots` × `QL` +
  `locked_maker_fees`, in quote atoms.

From them:

- **Position.** The base held in the wallet, plus the base of every open
  orders account in the account, in base units. For SOL, the wallet's
  holding is its wrapped SOL token account; native SOL is not counted, since
  the authority spends it on transaction fees.
- **Fills.** Each fill is a `FillEvent` in the market's event heap, with
  its side, price and quantity. A taker's fill settles into its open orders
  account at once. A maker's fill settles only when the event heap is
  consumed (`consume_events`, permissionless): until then, the maker's
  position lags its fills. An enforcer MUST read fills from the events, not
  infer them from the position.
- **Open orders.** An order resting in the market's book for one of the
  account's open orders accounts. A side with any `bids_base_lots` or
  `asks_base_lots` has an open order.
- **Value.** Base and quote holdings, wallet and open orders accounts
  together, with the base at the market's Pyth price feed (for SOL, the
  SOL/USD feed
  `ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d`) and
  USDC at $1. Unsettled maker fills count
  once consumed.

## The benchmark's enforcer

No benchmark task enforces this profile yet. The first is planned to be a
treasury sale of SOL on OpenBook and Phoenix, under a sell-only mandate with
a price floor.
