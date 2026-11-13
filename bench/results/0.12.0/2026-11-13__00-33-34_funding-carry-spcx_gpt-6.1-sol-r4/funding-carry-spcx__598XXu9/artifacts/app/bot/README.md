# SPCX strategy

`bot.wasm` is the built `wasm32-wasip2` strategy component. Rebuild it with `just build`.

The strategy reads Phoenix's live funding period and cap, the SPCX index, the median active maker quote centre, and its own trader account. Two embedded tree ensembles forecast funding-adjusted book returns over four and eight seconds. They were fitted on independently generated development scenarios with varied common price noise.

It targets a long or short position when the forecast exceeds the trading threshold, holds through smaller signals to reduce fees, and reduces new trading near the session's end. Orders execute in five-share slices. It places no resting orders.

Risk checks use actual collateral, virtual quote balance, position value at the maker centre, and cumulative funding. Position size falls as drawdown consumes the remaining risk budget, with a reserve for a further price move and execution costs. A drawdown stop permanently flattens the strategy. The SDK also enforces the session's position cap on each order sheet.

Runtime inputs come entirely from `Session`, `Market`, and allowed account-read RPCs. The model is embedded in the component; no runtime files or network service are required.

Final export checks: 6 practice sessions produced $2,072.02; 24 generated stress sessions produced $14,384.74. Neither set had a disqualification. Details are in `validation.json`.
