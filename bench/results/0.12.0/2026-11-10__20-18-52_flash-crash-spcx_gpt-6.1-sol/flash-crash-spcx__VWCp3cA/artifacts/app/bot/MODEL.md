# Forecast model

The selected model was trained offline on 64,000 paths drawn with the task's
public `mtx scenario generate` interface: 16,000 with seed 394822319 and 48,000
with seed 154573924. It uses LightGBM regression, 1,200 trees, at most 31 leaves
per tree, depth at most 7, learning rate 0.035, and minimum leaf size 2,400.

The regression target is future price changes discounted by 0.8 per half-second
step. Price-history features and targets are oriented by the most recent
meaningful move so crashes and squeezes share the same model. Inference uses
only observations already received. Session duration and time remaining are
known from the turn protocol.

Features describe returns at multiple lags, distances and ages of extrema,
recent jump sizes and ages, and the current directional run. Separate generated
sets were used to compare forecasting horizons and trading thresholds. Chain
practice runs then verified fees, fills, exposure, drawdown, and turn timing.

Binary tree format, little endian:

- `u32` tree count, `u32` node count;
- one `u32` root index per tree;
- nodes: `i32 feature`, `f64 threshold`, `u32 left`, `u32 right`.

A negative feature denotes a leaf, whose threshold field holds its value.
Tree values sum to a forecast in basis points, divided by 10,000 for trading.
