# SPCX strategy

`bot.wasm` is the prebuilt `wasm32-wasip2` strategy component. Rebuild with `just build`.

The strategy reads Phoenix's current funding parameters, maker spline centres, and its trader account through one permitted `getMultipleAccounts` call per turn. Maker centres avoid treating the bot's own consumption of the best quotes as a change in the market premium.

Causal price-history features feed three fixed regression models: short-horizon returns net of funding and a forecast that incorporates the cost of future position changes. Models were fitted offline on generated practice-family scenarios with additional noise and fizzles. Runtime inference requires no files, network, environment variables, or randomness.

Orders select positions in 17.5-share increments, weighing forecast returns against fees and estimated market impact. Observed fills track consumed liquidity until makers refresh. Orders use slices of at most 10 shares and limits within 0.4% of the observed quote centre.

The account-equity guard includes collateral, unrealized PnL, fees, and cumulative funding not yet settled into collateral. It scales exposure down as drawdown rises, reserving room for a 5% adverse move, and stops permanently at 65% of the drawdown limit. Orders only target SPCX and remain within the session's position cap.

Tests cover Rust/model numerical parity and position selection with caps and consumed liquidity. Practice and independently generated stress sessions were also run against the Solana sandbox. These results do not guarantee hidden-session returns.

Validation summary: 54 distinct practice/generated scenarios were profitable, with $31,419.62 total PnL and $227.61 maximum drawdown. Two separate out-of-family loss-guard stress scenarios both ended flat, with maximum drawdown $662.90 and no violations. See `validation.json` for the breakdown and artifact hash.
