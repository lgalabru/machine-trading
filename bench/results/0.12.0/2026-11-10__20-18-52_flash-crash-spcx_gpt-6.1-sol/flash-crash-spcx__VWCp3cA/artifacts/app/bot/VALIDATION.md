# Validation

All listed chain runs passed the SPCX universe, position, drawdown,
liquidation, RPC-method, and turn-budget checks.

| Set | PnL (USDC) | Reward | Maximum drawdown (USDC) |
|---|---:|---:|---:|
| Practice, 6 sessions | 1,432.03 | 0.2207 | 287.22 |
| Generated batch A, 72 sessions | 26,718.37 | 0.2720 | 682.00 |
| Generated batch B, 72 sessions | 20,233.11 | 0.2251 | 658.55 |
| Final binary check, 24 sessions | 13,104.54 | 0.3586 | 517.78 |

The 168 generated chain sessions total **60,056.02 USDC**, with aggregate
reward **0.2673** and maximum drawdown **682.00 USDC**.

Practice PnL was 2.65 times the supplied reference PnL. Separate synthetic
comparison sets contained 6,000 paths, including checks with oracle tick
rounding and the snapshot oracle starting value. Those simulations were
used for model selection; the table reports actual chain executions.

The final emergency-exit price bound was checked on the largest-drawdown
session. It flattened successfully with exactly the same PnL and drawdown
as the preceding execution configuration. The final 24-session batch used
the exact installed executable.

The feature cleanup reproduced every practice feature byte and every
practice-session PnL. Native tree predictions matched the training runtime
within the rounding of float32 prediction files.

## Build identity

- `bin/bot`: `dbed67677021be918893ac108cef36c0b967755c68e545bb5c99eb655d75ef71`
- `src/forecast.bin`: `9d3f33e6b7ec5e202e667113dc9703c8d1f160f826377b17289b7ca20d3efab3`
