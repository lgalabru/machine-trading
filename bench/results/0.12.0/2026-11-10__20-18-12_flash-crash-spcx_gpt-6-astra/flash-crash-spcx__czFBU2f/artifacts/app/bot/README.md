# SPCX shock bot

Run the prebuilt native executable with `bash /app/bot/run.sh`. It uses the supplied `MTX_*` contract and `mtx-turn-1` socket. No build, Python, model download, external API, or writable bot directory is needed at runtime. The GCC support library is bundled in `lib/`; the remaining dynamic dependencies are part of libc.

The strategy forecasts near-term returns from past and current oracle prices. Its embedded tree model uses movement sizes, elapsed times, rolling ranges, and reversals at several scales. A hysteresis band limits unnecessary position reversals. The model was trained on separately generated public-family scenarios; it reads no scenario files at runtime.

Risk controls operate independently of the forecast:

- Only SPCX is accepted. All orders are IOC market orders, sliced by the SDK at 10 shares and clipped to the mandate's position cap. Order prices are bounded to 0.5% from the observed mark.
- Actual Phoenix account equity includes fees and slippage. The wallet balance stays unchanged.
- Position sizing reserves 15% of the drawdown limit and budgets for another 5.5% adverse price move. The strategy permanently flattens after drawdown exceeds 65% of the limit.
- A failed risk-account read triggers flattening and a permanent halt. No resting orders accumulate.
- The last turn retains the existing position because the session values it at the mark and closing would add execution costs.

`src/model.bin` is embedded into the executable at compile time. `src/features.rs` computes causal price-history features; `src/model.rs` evaluates the trees; `src/risk.rs` applies the independent risk controls.

Development commands (Rust and the supplied SDK required):

```sh
./build.sh
PATH=/usr/local/cargo/bin:$PATH cargo test --release
```

The build publishes `bin/bot` atomically. Tests check inference against the training runtime, position caps, permanent halting, and consecutive adverse jumps with execution costs. See `validation.json` for completed sandbox checks. Test results do not imply guaranteed performance on future scenarios.
