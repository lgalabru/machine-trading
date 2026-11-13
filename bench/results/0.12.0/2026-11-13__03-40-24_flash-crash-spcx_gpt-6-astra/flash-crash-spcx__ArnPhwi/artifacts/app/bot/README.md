# SPCX shock strategy

`bot.wasm` is the ready-to-run `wasm32-wasip2` strategy component. It uses only
SPCX orders and one read-only trader-account RPC per active turn. It needs no
runtime files, environment variables, network services, or ML libraries.

```
just build
just test
mtx-practice
```

The compiled tree model estimates the value of holding a share through the next
move, including the option to change direction later. Inputs are causal oracle
returns, moving averages, extrema, recent move segments, and the session clock.
A 10-basis-point switching threshold accounts for trading costs and avoids
repeated reversals on weak signals. It can ride a continuing shock, fade a likely
reversion, or retain its position when neither trade justifies its cost.

The policy was trained on public generated scenarios, with complete scenarios
held out for validation. It never receives a scenario path or scenario identity
at runtime. The model weights are constants in `src/model.rs`.

Risk control is separate from the model. It reads Phoenix collateral and virtual
positions to include actual fees and realized PnL, values the position at the
mark, and tracks peak equity. Exposure is at most 70 SPCX and decreases as the
remaining drawdown budget shrinks. A 5.5% adverse-move reserve and 10% cash buffer
protect the mandate; a 70% drawdown halt remains in effect for the session.
Orders use 10-share slices. No orders rest, and the final turn adds no trade.

`src/features_golden.rs` checks feature and inference parity against the Python
training pipeline. The risk tests check caps, jump reserve, and persistent halt
behavior. JSON validation reports contain the full sandbox results; these are
practice results, not guarantees about hidden scenarios.

Final fresh validation: **144 sessions, $52,080.87 PnL, 0.287634 reward, zero
disqualifications**. See [VALIDATION.md](VALIDATION.md) for risk and artifact checks.
