# SPCX market maker

`bot.wasm` is the prebuilt `wasm32-wasip2` strategy component. The strategy
requires no runtime files, environment variables, or external network access.

Build in this directory with `just build`. The build replaces the component
atomically and writes its SHA-256 to `bot.wasm.sha256`.

The strategy posts oracle-centered, inventory-skewed quotes. Small inside
quotes observe low-limit taker flow; the larger quotes earn spread and reduce
inventory. It discovers recent trader accounts through read-only RPC and
excludes makers using the book's recent-trade identities. Changes in SPCX
positions provide the flow observations, including very small partial fills.

A Poisson likelihood model compares buy and sell arrival counts with their
estimated background rates across several horizons and burst intensities.
Strong signals establish directional inventory; weaker signals skew quotes.
A symmetric Bayesian drift model provides a slower inventory bias. Reversing
flow exits stale forecasts, and inventory otherwise unwinds passively. The
bot also takes stale quotes when the oracle edge exceeds transaction fees.

Inventory targets stay below 20 SPCX. The strategy values collateral and
virtual quote balances at the oracle, includes fees in its equity estimate,
and cancels quotes and flattens after a $100 drawdown. The highest intended
position is 19.5 SPCX, leaving room below the mandate's cap.

Validation covered six supplied practice scenarios and 60 generated scenarios
from two seeds, with unchanged strategy parameters:

| Measure | Result |
| --- | ---: |
| Sessions | 66 |
| Total PnL | $2,608.15 |
| Aggregate reward | 0.174342 |
| Maximum drawdown | $55.16 |
| Largest position | 19.500 SPCX |
| Disqualified sessions | 0 |
| Refused RPC calls | 0 |

`validation.json` contains both complete reports and their aggregate results.
The final source cleanup is also checked by deterministic replay against the
original results. These are offline practice/generated results.

Reproduce the generated sets with `mtx scenario generate --family market-making`
using seed `8123981`, count `36`, prefix `train`, and seed `73920415`, count `24`,
prefix `holdout`. Grade them with `mtx-practice DIR/*.json`. The supplied practice
set can be run with `mtx-practice /opt/mtx/practice/*.json`.
