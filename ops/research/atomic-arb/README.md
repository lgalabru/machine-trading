# Atomic arbitrage research

The scripts and full notes behind the studies in
[`docs/RESEARCH.md`](../../../docs/RESEARCH.md), run on 2026-11-21. The
condensed findings live there; each `NOTES.md` here has every number with
its sample size.

| Folder | Study |
| --- | --- |
| `jito-explorer/` | The Jito explorer's top and latest SOL arbitrages: `fetch.py`, `analyze.py`, `classify.py`, `trigger.py` |
| `kamino-liquidations/` | Nine days of Kamino Lend liquidations, their pre-state and a detector |
| `clmm-backrun/` | SOL/USDC backruns between Whirlpool and Raydium CLMM, whale bursts and a detector |
| `lst-cycles/` | LST cycles, epoch steps, stake-pool values and a cycle detector |
| `ordering/` | Who wins a contested slot: the SOL/USDC backruns' and Kamino liquidations' contenders, with their fees and tips, and the block-ordering rules scored against them (2026-11-22, `rules.py`) |

The scripts read a mainnet RPC URL from `json_rpc_url` in
`~/.config/solana/cli/config.yml` and never print it. The downloaded data,
about 1 GB, is not kept in the repository: each script's docstring says
what it fetches. Run them from a scratch directory with `python3 -I`.

A few scripts read the explorer sample from `/tmp/arb-research/round1/`,
where `jito-explorer/fetch.py` and `analyze.py` wrote it: `<sample>.json`,
`<sample>.recs.json` and `tx/<signature>.json`.
