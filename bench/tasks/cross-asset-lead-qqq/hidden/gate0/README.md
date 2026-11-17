# Gate 0

The ladder's method, as `smart-money-decay-nvda`'s: every rung graded on one
fresh 72-session hidden set (`bench/checks/ladder/ladder.py`), the informed
bot told each session's NVDA roster (`--tell-latent`: which NVDA wallets are
informed and which are decoys; not the moves, nor QQQ's share of them).
`gate0.py` reads the ladder's output and prints the signal gate, the
strength band and how often the order holds on bootstrapped sets:

```sh
python3 -I bench/checks/ladder/ladder.py cross-asset-lead-qqq --mtx target/release/mtx \
  --snapshot snapshots/phoenix-perps-2026-11-13 --sets 1 --count 72 --jobs 4 --out <out>
python3 gate0.py <out>/cross-asset-lead-qqq
```

The rungs, all trading QQQ (NVDA's cap is the smallest the mandate takes):

| Rung | What it does |
| --- | --- |
| `nop` | never trades |
| `follow-nvda-flow` | the cap the way NVDA's net taker flow went |
| `follow-qqq-flow` | the cap the way QQQ's own net taker flow went |
| `momentum` | the cap the way NVDA's oracle moved |
| `twap` | buys evenly to the cap, sells at the end |
| `reference` | NVDA wallet records marked out on NVDA's oracle, credible wallets' flow into QQQ |
| `informed` | the reference's machinery, told the NVDA roster, following the informed from their first take |

No rung trades NVDA directly: the mandate allows none.
