# SPCX Phoenix maker

`bot.wasm` is the ready-to-run `wasm32-wasip2` strategy component. Rebuild with `just build`.
The build installs the component atomically, retaining the previous working component if compilation fails.

The strategy:

- Quotes around the oracle, improves the available book, and skews prices and sizes toward its desired inventory.
- Reads preceding blocks to observe all taker submissions, including unfilled orders, while excluding its own hedges.
- Combines a Poisson arrival-rate test and a compiled burst classifier. Strong signals acquire a directional position; weaker signals adjust passive quotes.
- Uses a separate compiled drift classifier for ordinary inventory. Signals expire, and a detected jump clears the signal and unwinds the position.
- Reserves 0.1 SPCX below the mandate cap, limits individual market-order slices to 8 SPCX, and bounds execution prices to 40 basis points from the oracle.
- Reads its Phoenix account to track equity at the oracle. Position limits shrink with remaining drawdown room, reserving 45 USDC for settlement and execution and budgeting a 4.5% adverse move. A final drawdown guard cancels quotes and flattens.

The models were fitted on independently generated scenarios from the published family. Their trees are Rust constants in `src/model.rs` and `src/trend.rs`; execution needs no files, network access, Python, or training dependencies. The component uses only the allowed `getBlock` and `getAccountInfo` RPC methods and trades through the SPCX order sheet.

Validation results are recorded in `validation.json`. These are local practice/generated tests, not hidden evaluation results.

## Local validation

| Set | Sessions | PnL (USDC) | Maximum drawdown (USDC) |
| --- | ---: | ---: | ---: |
| Practice | 6 | +329.16 | 26.86 |
| Generated validation | 24 | +1,544.62 | 47.77 |
| Additional generated set | 24 | +1,085.51 | 32.73 |
| Fresh stress set | 72 | +4,119.51 | 87.91 |

All 126 standard sessions passed the mandate checks. Three deliberately adverse risk scenarios also passed; their maximum drawdown was 202.92 USDC. Peak position stayed at 19.9 SPCX. Rust model inference matched 11,722 independent Python predictions with zero numerical difference.

`validation.json` records the artifact checksum, scenario generator seeds, and detailed summary metrics.
