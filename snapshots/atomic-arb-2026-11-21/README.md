# atomic-arb-2026-11-21

The chain state the three atomic-arbitrage tasks share
([RFD 0005](../../docs/rfd/0005-atomic-arbitrage-tasks.md), `liquidation-flash`,
`backrun-sol-usdc`, `backrun-thin-pools`): a fork of mainnet at slot
472,329,069 (2026-11-21), SOL at $70.26, 372 accounts.

## What it holds

| What | Accounts |
|---|---|
| Kamino Lend's main market | market `7u3HeH…`, lookup table `FGMSBi…`, SOL reserve `d4A2pr…` and USDC reserve `D6q6wu…` with their vaults, fee receivers, collateral mints and vaults, and farms (`kamino-markets`) |
| Scope's main feed | `OraclePrices` `3t4JZc…`, `OracleMappings` `4zh6bm…`, `OracleTwaps` `6L6vUt…`, `Configuration` `6cMwdb…` (the PDA that links the three), `TokenMetadatas` `3wHxoH…` |
| Orca Whirlpool SOL/USDC `Czfq3x…` (4 bp, tick spacing 4) | pool, config, mints, vaults, 25 tick arrays (±12, fixed and dynamic). The oracle PDA `FoKYKt…` doesn't exist on mainnet (no adaptive fee), so it is absent here too (`clone-pool`) |
| Raydium CLMM SOL/USDC `3ucNos…` (4 bp, tick spacing 1) | pool, `amm_config`, observation, bitmap extension, mints, vaults, the 25 initialized tick arrays nearest the price (12 each way and the current one) (`clone-pool`) |
| Programs | klend, Kamino Farms, Scope, Whirlpool, Raydium CLMM, SPL Token, Token-2022, Associated Token Account, Memo, each checked by `expect_code_sha256` |

Build it again with:

```sh
MTX_FORK_RPC_URL=<a mainnet RPC> just snapshot-build atomic-arb-2026-11-21 --force
```

A rebuild forks mainnet as it is then: the pools' prices and the reserves'
state differ, and it fails if any program has been upgraded since.

## What was checked on it

`crates/grader/tests/atomic_arb_snapshot.rs` (ignored; run it with
`cargo test -p mtx-grader --test atomic_arb_snapshot -- --ignored --nocapture`)
starts the snapshot offline and checks:

- **Swaps.** 500 SOL sold through each pool with the `amm` venue's seat lands,
  and moves the trader's balances by exactly the kit's quote: 35,108.53 USDC
  on Whirlpool (70.2171 a SOL against a top of 70.2390, 3 bp of impact),
  35,065.65 USDC on Raydium (70.1313 against 70.2380, 15 bp).
- **Lending.** A throwaway account deposits SOL, borrows USDC, repays and
  withdraws on the main market.
- **A price a bot pushes** (ADR 0004):
  - Scope's SOL entry 3 (a `MostRecentOf` on mainnet) becomes a `FixedPrice`
    through surfpool's `kamino-scope-price-source` template, with
    `crate::scope::fixed_price_mapping`. The template accepts paths beyond
    its listed properties: `price_types.3`, `price_info_accounts.3` and
    `generic.3.0` to `generic.3.19`.
  - The price account must be Scope's own program id, which is what Scope
    stores for "no account". With the zero key, `refresh_price_list` skips
    the entry ("no mapping is set") and the price stays put.
  - A top-level `refresh_price_list` (after the compute budget only;
    anything else first fails with `RefreshWithUnexpectedIxs`) writes the
    price into `OraclePrices` stamped with its own slot, and klend's
    `refresh_reserve` then takes it with every price check passing (status
    `0b111111`).

## What a lending scenario must do with it

The SOL reserve reads entry 3 for its price (120 s max age) and entry 455, a
Pyth Lazer EMA, for its TWAP (240 s max age, at most 1,000 bp apart). A
scenario that only pushes entry 3 loses the TWAP-age check (status
`0b110111`) after 240 s, so the reserve's price is no longer fully checked
(which klend actions refuse such a price was not tested here). Making
entry 455 a `FixedPrice` too, refreshed in the same `refresh_price_list`,
restores every check. The USDC reserve reads 13 (price) and 456 (TWAP) the
same way.
