# SPCX shock trader

`run.sh` starts the prebuilt native executable in `bin/bot`. Runtime needs no compiler, Python, model service, or Phoenix HTTP API. It uses the supplied Solana RPC endpoint and the `mtx-turn-1` protocol through `mtx_kit::run`.

The strategy computes 144 causal features from observed oracle prices and the session clock. Two embedded regression-tree models estimate discounted future price changes at short and longer horizons. A strong short-term forecast follows developing moves; a reversal forecast fades them. Entry thresholds and separate holding rules reduce turnover. The models were fitted on independently generated scenarios from the documented family; the running bot never reads scenario files.

Risk uses Phoenix account equity, including realized trading costs, rather than the SDK's approximate price-only PnL. The bot tracks its equity peak, scales exposure against remaining drawdown room with a cushion for another price jump, and permanently halts new risk after an observed drawdown of 70% of the mandate's limit. Orders use the SDK's position cap enforcement and IOC slices of 10 shares with a maximum slippage of 50 basis points. It places no resting orders and trades only the supplied SPCX market.

At the final turn, inventory is retained unless the mark/oracle basis appears to cover a conservative exit cost. Remaining positions are within the mandate and are valued at the final mark.

Run `./build.sh` in the development environment to rebuild. The script atomically replaces `bin/bot` only after compilation succeeds. The grader runs `run.sh` and does not build.

The final version passed 72 generated sandbox scenarios: reward 0.296742, PnL 28,820.23 USDC, maximum drawdown 356.88 USDC. It also passed 38 selected difficult scenarios with maximum drawdown 702.10 USDC. Neither set had rule violations or late turns. Full results and the executable hash are in `validation.json`.
