# Venue profile: BisonFi (`bisonfi`), v0

Status: draft

BisonFi is a proprietary market maker on Solana, program
`BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi`: one firm trades spot pairs
from its own tokens, at a price it publishes into each pool's account. An
account trades with it by swap, and cannot rest an order. The
[prop AMM guide](../guides/venues/prop-amm.md) explains the mechanism.

The program is closed and publishes no interface. This profile reads only
what it needs from a pool's account, at the offsets below, as observed on
version-3 pools. A reader MUST reject a pool account that does not carry
both version tags, and the firm can change the layout without notice.

## Instruments

| Reference | Meaning |
| --- | --- |
| `bisonfi:spot:<SYMBOL>` | The BisonFi pool `<SYMBOL>` names in the table below. |
| `bisonfi:spot:<ADDRESS>` | The BisonFi pool at the base58 address `<ADDRESS>`. |

Named pools:

| Symbol | Pool address | Base | Quote |
| --- | --- | --- | --- |
| `SOL-USDC` | `8FnX3xo2yYw3EUE6w3nQA4GfXGS9wpK6oj3veJpbFzLo` | SOL (wrapped, `So11111111111111111111111111111111111111112`) | USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`) |

A pool's name field is not a symbol: several pools can trade the same pair.

A pool's account, owned by the program:

| Bytes | Holds |
| --- | --- |
| 0..8, 8..16 | The version tags: `ES78ZeanFtP` and `W723RTUpoZ` in base58. |
| 72..80 | The slot the firm last published its quote in, as a little-endian integer. |
| 120..152 | The base vault's address. |
| 152..184 | The quote vault's address. |
| 184..216 | The base mint. |
| 216..248 | The quote mint. |

The vaults are token accounts the pool's account owns.

## Account

On each pool in the universe, the account is the agent authority's wallet
token accounts for the pool's base and quote mints. The venue holds nothing
for the account between trades: there is no balance to settle and no order
to rest.

## Measurements

- **Position.** The base held in the wallet, in base units. For SOL, the
  wallet's holding is its wrapped SOL token account; native SOL is not
  counted, since the authority spends it on transaction fees.
- **Fills.** The program logs no fill. A fill is an instruction of the
  BisonFi program on the pool, in a transaction the agent authority signed,
  that moves tokens out of one of the pool's vaults. It counts whether the
  transaction calls the program itself or reaches it through another
  program, such as an aggregator. Its quantities are the token transfers
  inside that instruction: the base quantity is what the base vault paid or
  received, and the quote quantity what the quote vault received or paid.
  The fill is a **buy** when the base vault paid, and a **sell** when it
  received. Its price is the quote quantity divided by the base quantity,
  in quote units per base unit. The program reports no fee apart from that
  price. An enforcer MUST read every such instruction in the term's
  transactions, and MUST NOT infer fills from the wallet's balances, which
  transfers change too.
- **Swaps that move nothing.** A swap against a quote the firm has stopped
  refreshing, one more than about two slots old, succeeds without moving any
  tokens, whatever minimum output it asked for. It is not a fill.
- **Open orders.** None: the venue has no orders.
- **Value.** The wallet's base and quote holdings, with the base at the
  Pyth price feed for it (for SOL, the SOL/USD feed
  `ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d`) and
  USDC at $1. The pool's own quote is not a reference price.

Wallet balances that both BisonFi and another venue in the universe count
are counted once, as the [mandate](../mandate.md#account-value) requires.

## The benchmark's enforcer

No benchmark task enforces this profile yet.
