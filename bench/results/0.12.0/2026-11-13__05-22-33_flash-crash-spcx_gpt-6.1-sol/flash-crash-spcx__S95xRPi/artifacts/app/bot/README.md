# SPCX shock trader

`bot.wasm` is the self-contained `wasm32-wasip2` strategy component. It trades only the session's SPCX market, through the `mtx-kit` order sheet.

The strategy forecasts discounted returns from causal price history: recent returns, ranges, impulse sizes and ages, moving averages, and the session clock. Two embedded tree ensembles combine short momentum with the chance of a reversal. Inputs are reflected into the latest impulse's direction so crashes and squeezes share the same model. The models were trained on generated scenarios from the stated family; no practice or graded session path is embedded.

A turnover penalty limits repeated position flips during noise. The position target is long, flat, or short, reduced as drawdown grows. The bot reads its own Phoenix trader account with the permitted `getAccountInfo` RPC, includes realized PnL and fees in its account-value estimate, reserves room for another adverse move, and permanently flattens after its drawdown stop. It uses immediate market orders in 10-share slices and leaves no resting orders.

## Build and check

```sh
just build
mtx-practice
```

Builds require the supplied `/opt/mtx/sdk` and Rust toolchain. The component itself uses no filesystem, network, environment, training libraries, or external model files.

The causal feature tests can also run without a chain:

```sh
just test
```

See `evaluation.json` for replay results. These results describe generated and practice scenarios, not the hidden grading batch.

## Checked results

| Batch | Sessions | Reward | Largest drawdown |
| --- | ---: | ---: | ---: |
| Generated calibration | 72 | 0.330 | $589 |
| Independent validation and practice | 78 | 0.292 | $577 |
| Independent stress | 144 | 0.288 | $606 |
| Combined | 294 | 0.300 | $606 |

Every session stayed within the position cap and passed the market, RPC, drawdown, and liquidation checks. The cleaned component matched the selected strategy's PnL exactly across all 294 scenarios. The final artifact also passed two identical runs of the six practice scenarios: $1,378.55 total PnL, 2.56 times the supplied reference, with all six profitable.
