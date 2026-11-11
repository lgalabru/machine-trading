This is a native Rust SPCX strategy for the Phoenix offline benchmark. `run.sh` starts the prebuilt, statically linked `bin/bot`; running it needs no compiler, SDK, Python, downloads, or Phoenix HTTP API.

The bot forecasts the next four seconds from causal oracle-price observations, move and rebound state, elapsed time, and the known session clock. Two embedded tree models were trained on independently generated scenarios. It retains a strong primary forecast and averages weaker forecasts with the secondary model. Entry thresholds and separate exit thresholds reduce unnecessary turnover. Only the allowed market is accessed.

Risk control reads Phoenix's actual margin equity, including execution costs. It tracks the session peak, reduces size to retain headroom for another adverse jump, and permanently targets zero after a drawdown exceeds 49% of the mandate's limit. The kit clips every target to the mandate's cap and executes in 10-share slices. The strategy places no resting orders.

Build in the supplied development environment with `bash build.sh`. Builds replace the executable atomically; `BOT_OUTPUT` can point to a separate candidate executable during testing. Check the native model predictions and risk invariants with `cargo test --release --lib`. The model files and their format are described in `models/manifest.json` and are embedded into the executable at build time.

Validation results are in `validation/`. Practice and generated scenarios are tests of the supplied scenario family; their PnL does not predict a particular hidden batch.

The selected executable earned $56,206.98 across 144 generated evaluation sessions (30.04% of their hindsight bound). It passed all 170 chain sessions, including six practice sessions and 20 deliberately selected stress cases, with no rule violations. The deepest observed drawdown was $749.66. The complete records and comparison are in `validation/summary.json`.
