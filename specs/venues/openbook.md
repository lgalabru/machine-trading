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
| `SOL-USDC` | `CFSMrBssNG8Ud1edW59jNLnq2cwrQ9uY5cM3wXmqRJj3` | SOL (wrapped, `So11111111111111111111111111111111111111112`) | USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`) |

A market's `name` field is not a symbol: two markets can share a name.

## Account

On each market in the universe, the account is the agent authority's open
orders accounts on that market: the program's `OpenOrders` accounts the
authority owns, as its `OpenOrdersIndexer` lists them, together with the
authority's wallet token accounts for the market's base and quote mints.

The wallet matters for trading too, not only for holding: a take-only order
(`place_take_order`) trades straight from the wallet's token accounts,
without an open orders account.

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
- **Fills.** Every match makes a `FillEvent`, with the taker's side
  (`taker_side`), the maker's order price (`price`, in price lots) and the
  quantity (`quantity`, in base lots). `maker` is the maker's open orders
  account; `taker` is the taker's open orders account, or the signer for a
  take-only order. The taker's side settles in the same instruction, which
  logs a `TotalOrderFillEvent` for the whole order. The maker's side settles
  either in the same instruction too, when the taker's transaction passes
  the maker's open orders account, or later, when `consume_events`
  processes the event from the market's event heap (anyone may call it,
  unless the market names a `consume_events_admin`); either
  way the program then logs a `FillLog` for that fill. Until then the
  maker's position lags its fills. An enforcer MUST count every fill: the
  `FillLog`s in the term's transactions, and the `FillEvent`s still waiting
  in the event heap. It MUST NOT infer fills from the position.
- **Open orders.** An order resting in the market's book for one of the
  account's open orders accounts. A side with any `bids_base_lots` or
  `asks_base_lots` has an open order.
- **Value.** Base and quote holdings, wallet and open orders accounts
  together, with the base at the Pyth price feed this profile names for it
  (a market's own oracle settings are optional, and not used here; for SOL,
  the SOL/USD feed
  `ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d`) and
  USDC at $1. Unsettled maker fills count
  once consumed.

## The benchmark's enforcer

`mtx grade` audits spot sessions on this profile's SOL-USDC market, alone
or with Phoenix's:

- It measures the account at every step of the session, every two slots,
  and once more after the bot's last turn. The position is the SOL held, in
  the wallet and on each venue in the universe; the value is both assets at
  the Pyth SOL/USD price and USDC at $1, each counted once, for the
  drawdown limit when the mandate sets one.
- A side has an open order when the account's resting orders lock base
  (asks) or quote (bids) on it.
- At the end of the term it reads every fill the market recorded for the
  account, as a taker or a maker: the `FillLog`s of the term's
  transactions and the `FillEvent`s still in the event heap, each
  `seq_num` once. It audits them against the allowances' sides and limit
  prices.
- It hands the bot its copy of the mandate in `MTX_MANDATE`, with the term
  set to the session's last slot, and disqualifies the session on any
  breach.

No benchmark task enforces this profile yet; `just spot-smoke` runs a
session end to end.

A spot bot may be a strategy component (`bot.wasm`), and a grade that
takes components only (`mtx grade --components-only`, the verifier's rule)
grades it. The component sees the Pyth price as its mark, oracle and index.
Its position is the SOL held, its entry is the price at the start, and its
PnL is the change in the account's value. It gets the mandate in
`session.mandate`, and the session's markets in `session.venue`. Until the
bot interface names a venue on each order, its orders go to the first
venue the scenario lists. RFD 0002's "Spot sessions" covers the rest. The first task is planned to be a treasury sale of SOL
on OpenBook and Phoenix, under a sell-only mandate with a price floor.
