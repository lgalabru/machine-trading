# Venue profile: HumidiFi (`humidifi`), v0

Status: draft

HumidiFi is a proprietary market maker on Solana, program
`9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp`: one firm trades spot pairs
from its own tokens, at a price it publishes into each market's account. An
account trades with it by swap, and cannot rest an order. The
[prop AMM guide](../guides/venues/prop-amm.md) explains the mechanism.

The program is closed and publishes no interface, and a market's account
stores its mints and vaults masked. This profile does not unmask them: it
reads a market's vaults from the token accounts the market owns. It
describes schema-8 markets: accounts of 1,728 bytes, owned by the program,
whose last eight bytes hold the integer 8. A reader MUST reject any other
market account, and the firm can change the layout without notice.

## Instruments

| Reference | Meaning |
| --- | --- |
| `humidifi:spot:<ADDRESS>` | The HumidiFi market at the base58 address `<ADDRESS>`. |

This profile names no markets yet, so a market is referenced by its
address. A market's pair cannot be searched for by mint, since the mints
are masked.

A market's **vaults** are the token accounts its account owns. Its **quote**
is USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`), and its **base**
is the other mint it owns a token account for. A reader MUST reject a
market that owns no USDC token account, or token accounts for more than one
other mint: tokens anyone sent to a market make its pair ambiguous. Where a
market owns several token accounts for one mint, the vault is the one its
swaps move.

## Account

On each market in the universe, the account is the agent authority's
wallet token accounts for the market's base and quote mints. The venue
holds nothing for the account between trades: there is no balance to settle
and no order to rest.

## Measurements

- **Position.** The base held in the wallet, in base units. For SOL, the
  wallet's holding is its wrapped SOL token account; native SOL is not
  counted, since the authority spends it on transaction fees.
- **Fills.** The program logs no fill. A fill is an instruction of the
  HumidiFi program on the market, in a transaction the agent authority
  signed, that moves tokens out of one of the market's vaults. It counts
  whether the transaction calls the program itself or reaches it through
  another program, such as an aggregator. Its quantities are the token
  transfers inside that instruction: the base quantity is what the base
  vault paid or received, and the quote quantity what the quote vault
  received or paid. The fill is a **buy** when the base vault paid, and a
  **sell** when it received. Its price is the quote quantity divided by the
  base quantity, in quote units per base unit. The program reports no fee
  apart from that price. An enforcer MUST read every such instruction in
  the term's transactions, and MUST NOT infer fills from the wallet's
  balances, which transfers change too.
- **Swaps that fail.** A swap against a quote older than the market's
  staleness limit, a few slots, fails with the program's custom error
  `0xfaded`. A failed transaction is not a fill.
- **Open orders.** None: the venue has no orders.
- **Value.** The wallet's base and quote holdings, with the base at the
  Pyth price feed for it (for SOL, the SOL/USD feed
  `ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d`) and
  USDC at $1. The market's own quote is not a reference price.

Wallet balances that both HumidiFi and another venue in the universe count
are counted once, as the [mandate](../mandate.md#account-value) requires.

## The benchmark's enforcer

No benchmark task enforces this profile yet.
