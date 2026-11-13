# SPCX Phoenix strategy

`bot.wasm` is the built `wasm32-wasip2` strategy component. The grader can load it directly. Source is in `src/lib.rs`.

The bot quotes around the oracle with inventory, drift, and flow adjustments. Its ordinary inventory band is 6 SPCX. Small additional touch orders maintain visibility of takers whose price limits do not reach its wider quotes.

At startup it discovers the recently funded taker cohort from public Phoenix trader accounts. Each turn it reads the cohort and its own account in one `getMultipleAccounts` request. Signed inventory changes provide an order-arrival signal independent of which maker filled an order. No account addresses, scenario paths, or future prices are embedded.

Dense one-sided arrivals trigger a position of 19 SPCX in that direction. The signal uses both 6-slot and 12-slot windows, expires, resets after a jump, and respects the scenario's separation between jumps and its final quiet period. It retains inventory during the signal and otherwise unwinds with passive quotes. This avoids repeatedly paying a spread to flatten after false signals.

Orders are canceled before replacement, prices are tick-rounded, and quotes use post-only execution. Directional trades use 4-SPCX slices. The SDK cuts the sheet to the session's position cap. An irreversible per-session stop at an estimated $100 drawdown cancels quotes and targets zero, reserving room below the $250 mandate for a jump and execution costs. Equity estimation uses the oracle and adjusts raw trader balances for unsettled maker fills.

Runtime uses only approved read RPCs and the order sheet. It requires no files, environment variables, network access, or external process.

## Build

```sh
cd /app/bot
just build
```

The build replaces `bot.wasm` atomically after compilation succeeds.

## Validation

The selected logic was checked on 102 distinct scenarios. The final cleaned component was run directly on 42 sessions, including an exact reproduction of all six practice PnLs.

| Set | Sessions | PnL (USDC) | Reward | Maximum drawdown (USDC) |
| --- | ---: | ---: | ---: | ---: |
| Practice and training | 30 | 1283.92 | 0.178 | 89.68 |
| Independent validation | 36 | 1608.14 | 0.170 | 115.95 |
| Final independent validation | 24 | 831.97 | 0.157 | 64.84 |
| Large-jump stress | 12 | 1170.43 | 0.201 | 112.65 |

All runs passed the market, position, drawdown, RPC, and component-budget checks. Combined independent validation: 60 sessions, $2440.12 PnL, reward 0.165. The stress set deliberately selects unusually large repeated jumps.

`validation.json` records individual session results, aggregate checks, and the source and component hashes.
