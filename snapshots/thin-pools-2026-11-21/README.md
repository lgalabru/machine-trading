# thin-pools-2026-11-21

The chain state of `backrun-thin-pools`
([RFD 0005](../../docs/rfd/0005-atomic-arbitrage-tasks.md)): a fork of mainnet
at slot 472,390,207 (2026-11-21), SOL at $69.41, 497 accounts (1.7 MB
gzipped). It holds ten Orca Whirlpool and Raydium CLMM pools over SOL, USDC,
JitoSOL, JTO and GEOD, and JitoSOL's stake pool.

The pools come from the multi-token cycles in the Jito explorer's top 200
(`docs/RESEARCH.md`, "LST cycles and epoch steps"). Most of them are thin,
and thin pools are what the task is about: a retail swap moves them hundreds
of basis points, and a bot closes the gap at once. Each address was checked
on mainnet before the build: its owner program, mints, fee, tick spacing,
liquidity, price, vault balances, and its last swap (every pool had traded
within the last 1,900 slots). None was dropped or added.

## The graph

The cycles through SOL that the task can trade:

- three legs: SOL→JitoSOL→JTO→SOL, SOL→JTO→JitoSOL→SOL,
  SOL→GEOD→USDC→SOL, SOL→USDC→GEOD→SOL;
- two legs, between two pools of one pair: SOL/USDC, JTO/JitoSOL, SOL/JTO,
  SOL/GEOD.

GEOD is `7JA5eZdCzztSfQbJvS8aVVxMFfd81Rs9VvwnocV1mKHu`, the Geodnet token (9
decimals, about $0.23).

Depth is what it takes to move a pool's price down or up by 10, 50 and
100 bp, in SOL's worth of what goes in, fees included. It comes from the
kit's exact quote on this snapshot, so it counts the ticks crossed
(`tests/thin_pools_snapshot.rs`):

| Pool | Pair | Fee | Tick spacing | Price | 10 bp −/+ | 50 bp −/+ | 100 bp −/+ |
|---|---|---|---|---|---|---|---|
| Whirlpool `Czfq3x…` | SOL/USDC | 4 bp | 4 | 69.41 | 627 / 662 | 3,100 / 3,490 | 5,853 / 7,323 |
| Raydium `3ucNos…` | SOL/USDC | 4 bp | 1 | 69.41 | 132 / 133 | 658 / 698 | 1,196 / 1,456 |
| Whirlpool `Hp53XE…` | SOL/JitoSOL | 1 bp | 1 | 0.7625 | 18,216 / 25,079 | 18,219 / 37,037 | 18,221 / 37,055 |
| Whirlpool `G2FiE1…` | JTO/JitoSOL | 30 bp | 64 | 0.002384 | 5.2 / 5.2 | 26 / 26 | 53 / 52 |
| Whirlpool `4UVvSV…` | JTO/JitoSOL | 100 bp | 128 | 0.002396 | 0.20 / 0.20 | 0.99 / 0.99 | 2.0 / 2.0 |
| Raydium `JVoPtW…` | SOL/JTO | 100 bp | 120 | 317.7 | 0.14 / 0.14 | 0.72 / 0.69 | 1.45 / 1.37 |
| Whirlpool `2UhFny…` | SOL/JTO | 30 bp | 64 | 320.3 | 0.87 / 0.87 | 4.6 / 4.4 | 9.5 / 8.7 |
| Whirlpool `FsPFkR…` | SOL/GEOD | 30 bp | 64 | 301.4 | 0.82 / 0.81 | 4.1 / 4.1 | 8.2 / 8.1 |
| Raydium `3RA4PD…` | SOL/GEOD | 5 bp | 1 | 302.1 | 0.46 / 0.46 | 2.3 / 2.3 | 4.7 / 4.6 |
| Raydium `BkX9RS…` | GEOD/USDC | 25 bp | 60 | 0.2305 | 2.3 / 2.3 | 12 / 12 | 24 / 23 |

The price is the second token per unit of the first.

Three things stand out:

- **The JTO and GEOD pools are thin.** Under 1 SOL moves five of them by
  10 bp, and 1.4 SOL moves the Raydium SOL/JTO pool by 100 bp. The two
  SOL/USDC pools and the SOL/JitoSOL pool are hundreds to over 100,000 times deeper.
- **SOL/JitoSOL holds all its liquidity near the peg.** All of its JitoSOL
  (13,900, worth 18,200 SOL) sits within 10 bp of the price, so the price
  falls past 100 bp once that is sold. The pool prices JitoSOL at 1.311486
  SOL, 10 bp under the stake pool's 1.312796.
- **The two JTO/SOL pools are 0.8% apart** (317.7 and 320.3 JTO a SOL),
  inside their 100 bp and 30 bp fees.

## What it holds

| What | Accounts |
|---|---|
| Ten pools (`clone-pool`, 12 tick arrays a side) | each pool, its config, mints, vaults, its Whirlpool oracle (if it exists) or its Raydium observation and bitmap extension, and the tick arrays within 12 of the current one that exist: 25, 25, 24, 20, 14, 8, 19, 4, 17 and 9 in the table's order |
| JitoSOL's stake pool | the pool `Jito4A…`, its reserve stake `BgKUXd…` (48,977 SOL), its manager fee account `8yoigZ…` and the JitoSOL mint |
| Programs | Whirlpool, Raydium CLMM, SPL Stake Pool, SPL Token, Token-2022, Associated Token Account and Memo, each checked by `expect_code_sha256` |

The first SOL/USDC pool's price is the snapshot's `mark_price_usd`: a recipe
of pools alone gets its price from its first SOL/USDC `clone-pool`.

`DepositSol` and `WithdrawSol` need only those four stake-pool accounts,
the withdraw authority (a PDA with no data), and the clock and stake-history
sysvars. The validator list (730 KB) and the validators' stake accounts are
left out. Only `UpdateValidatorListBalance` and `UpdateStakePoolBalance`
read them, and no session runs either. Both instructions refuse a stake
pool that is not updated for the current epoch, and epoch 1093 ends at slot
472,608,000, 217,793 slots (about 24 hours) after the fork. A session that
crosses that slot can't deposit or withdraw until a scenario rewrites
`last_update_epoch`.

Build it again with:

```sh
MTX_FORK_RPC_URL=<a mainnet RPC> just snapshot-build thin-pools-2026-11-21 --force
```

A rebuild forks mainnet as it is at that time, so the pools' prices and
depths will differ. The build fails if any program has been upgraded since.

## What was checked on it

`crates/grader/tests/thin_pools_snapshot.rs` is an ignored test. Run it
with `cargo test -p mtx-grader --test thin_pools_snapshot -- --ignored
--nocapture`. It starts the snapshot offline and checks four things:

- **Depth.** It produces the table above.
- **Swaps.** It swaps 1 SOL's worth into every pool, in both directions,
  exact in. Each of the 20 swaps lands and moves the trader's two balances
  by exactly the kit's quote.
- **An atomic route.** It sends 1 SOL through SOL→JitoSOL→JTO→SOL
  (`Hp53XE…`, `G2FiE1…`, `2UhFny…`) as one v0 transaction. The transaction
  has 7 instructions: the compute budget, four idempotent account opens and
  three `swap_v2`. Its accounts come from a lookup table written the way a
  session writes its own: each pool's `pools::route_accounts` and the
  trader's token accounts, 86 addresses. The transaction is 548 bytes; a
  legacy one would be 1,411, over the 1,232 limit. It used 107,844 CU. Only
  the trader's SOL moved, by exactly the three chained quotes: 1 SOL in,
  0.99194 out (−80.6 bp, the 61 bp of fees plus the impact on two thin
  pools).
- **The stake pool.** A `DepositSol` of 10 SOL mints 7.61733 JitoSOL at the
  pool's rate exactly (15,757 CU). A `WithdrawSol` of those tokens returns
  9.98999 SOL, keeping the 10 bp withdrawal fee (21,005 CU).

## What a session must do with it

Each pool's route accounts number 15 to 37, 279 counted pool by pool; the
pools share their programs, mints and token programs, so a session's
lookup table holds 229 addresses with the bot's five token accounts, under
a table's 256 (a session whose accounts outgrow it writes as many tables
as it needs, `crates/grader/src/lending/route.rs`). With it, a three-leg
cycle as a searcher sends it is 619 bytes and a four-swap route 709. A
scenario may list up to 12 pools (`pools::POOLS_MAX`).
