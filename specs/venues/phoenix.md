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

The account is every Phoenix trader account the agent authority holds,
together with the authority's wallet USDC: its cross-margin trader account at
PDA index 0, subaccount 0, and any other it registers (another PDA index, an
isolated subaccount), found on chain as the Phoenix program's `Trader`
accounts whose header names the authority. Each measurement below sums over
them, so a position held in another trader account counts as one held in the
cross-margin account.

### Measurements

- **Position.** The traders' `base_lots` in the market, summed, from
  Hawkeye's `view_margin_for_asset`, converted to base units with the market's lot
  size from the `/exchange` metadata.
- **Universe.** Hawkeye's `view_margin` reports `position_count`, the number
  of markets in which a trader holds a position or open orders. A trader
  holds markets outside the universe when its `position_count` exceeds the
  number of universe markets that `view_margin_for_asset` reports it holding
  a position or orders in (`has_position_or_orders`); the account is outside
  its universe when any trader is.
- **Value.** Collateral plus unrealized PnL at the mark, from `view_margin`,
  summed over the traders, plus the authority's wallet USDC. Unsettled funding is not counted. USDC
  withdrawn from Phoenix to the wallet keeps its value, so withdrawing can't
  hide a drawdown.
- **Fills.** Each instruction that trades a market logs, through Phoenix's
  `Log` and `LogEventLengths` self-invocations, a `Header` event (the
  market's sequence number, its `asset_id` and the acting trader account),
  then an `OrderFilled` event for each resting order it took from and a
  `SplineFilled` event for each spline, each naming the maker's trader
  account, the maker's side, the price in ticks, `base_lots_filled` and
  `quote_lots_filled`. Each run of fills is closed by a `TradeSummary` event
  whose `trader` is the run's taker, on the side opposite the makers', and
  which carries the fee the taker paid (`fee_in_quote_lots`). The taker need
  not be the header's trader: an instruction can match two other parties (a
  spline that re-centres crosses a stale resting order) and logs that under
  the same header. The header's trader is the taker only for a run no
  summary closes. The summary's fee is split across its run of fills by
  `quote_lots_filled`, each share rounded down and the last fill taking the
  remainder, so the shares add up to it exactly. A maker's fee is its
  `maker_fee_rate` applied to the fill's `quote_lots_filled` as Phoenix
  applies it, in whole quote lots: a fee rounds up, a rebate rounds toward
  zero.
  Positions live in Phoenix's market-wide state, so a maker's trader account
  is in none of the transactions that fill it: an enforcer reads every
  transaction that names the market's order book (its `marketPubkey` in the
  `/exchange` metadata) and counts each fill where one of the account's
  traders is the taker or the maker. It MUST NOT infer fills from the
  position.
- **Resting orders.** An `OrderPlaced` event after a trader's `Header` is an
  order it left on the book, its side the sign of its `quantity`.

### Several markets

One cross-margin trader account trades every perpetual market, and Phoenix
margins them together. On a universe of several perpetual markets, an
enforcer measures each market's position from `view_margin_for_asset` for
that market, and the account's collateral, unrealized PnL and
`position_count` from `view_margin` once, for every market. Markets the
account holds outside the universe are its `position_count` less the
universe markets it holds a position or orders in. Each fill names its
market by the `Header` event's `asset_id`; a market's sequence numbers are
its own, so an enforcer tells two markets' fills apart by the market as
well as the sequence number.

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
- A session on several perpetual markets measures the one account on each
  of them, and lists them in `MTX_MARKETS`; a component gets a seat on
  each, every one tradable at its market address.

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

A spot bot may be a strategy component (`bot.wasm`), and a grade that
takes components only (`mtx grade --components-only`, the verifier's rule)
grades it. The component sees the Pyth price as its mark, oracle and index.
Its position is the SOL held, its entry is the price at the start, and its
PnL is the change in the account's value. It gets the mandate in
`session.mandate`, and the session's markets in `session.venue` and
`session.venues`. Each line of its sheet goes to the market it names, the
first venue the scenario lists if it names none. RFD 0002's "Spot
sessions" covers the rest.

The grader trades a component's lines on Phoenix v1 as market (immediate
or cancel), limit, post-only and cancel-all orders. A post-only order's
expiry (`expires_slot`) goes on the order as its last valid slot, which
Phoenix v1 enforces. Sizes round down to whole lots, prices away from a
worse fill.

Separately, the benchmark gates the RPC methods a bot may call. That is a
rule of the benchmark's sandbox, not part of the mandate.
