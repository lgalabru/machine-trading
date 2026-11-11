# SPCX shock bot

Run `bash run.sh`. It starts the prebuilt native executable in `bin/bot`; startup requires no compiler, SDK installation, Python, downloads, or writable bot directory. The executable uses the supplied contract files and standard Solana RPC through `mtx-kit`.

The strategy tracks shock origins, extremes, recovery, recent returns, volatility, and the session clock. It also remembers earlier session extremes and the sizes and timing of previous impulses. Two embedded regression ensembles estimate short-term continuation and a more persistent directional signal. Features are oriented with the observed move, allowing the same models to trade crashes and squeezes. Entry/exit hysteresis limits unnecessary reversals and trading costs. Every live feature uses observations available at that turn.

Orders target a long, short, or flat position. `mtx-kit` clips targets to the mandate and executes market orders in slices of at most 10 shares. The bot places no resting orders. The terminal turn preserves the existing position for mark valuation.

Risk control is independent of the signal:

- Reads Phoenix collateral and unrealized PnL each turn, including realized execution costs in its equity calculation.
- Tracks peak account equity and stops opening risk after a drawdown greater than 60% of the mandate's limit.
- Reduces position size as drawdown grows, budgeting for a 5% adverse next-step move and reserving another $150 for execution and valuation differences.
- Applies the mandate's position cap to every target; the SDK also enforces it when planning orders.

`src/main.rs` contains execution and risk control, `src/features.rs` and `src/feature_policy.rs` track observable market state, and `src/model.rs` evaluates the embedded trees. `model0.txt` and `model4.txt` are compiled into the executable and require no runtime file access.

The models were fitted on 10,000 publicly generated scenarios: 3,000 from seed 819027 and 7,000 from seed 771913. Another 2,000 scenarios were held out during fitting, followed by a separate 3,000-path simulation check using seed 399817. Each ensemble has 160 trees of maximum depth five. The targets use discounted returns with decay 0.85 and a bounded directional return with decay 0.97. Training is completed offline; the running bot only evaluates the fixed models.

Exchange validation reports are in `validation/`, with an aggregate in `validation/summary.json`. Two independent generated batches, **144 sessions** in total, returned **$52,395.86 PnL**, **0.272017 reward**, and **$480.76 maximum drawdown**. There were no rule violations or late turns, and 122 sessions were profitable. A separate 24-scenario stress set was selected specifically for large simulated drawdowns: its worst actual drawdown was **$722.16**, with no violations. The six supplied practice scenarios returned **$362.51 PnL**, or **0.6720 times** their supplied reference PnL. These are local test results on the documented scenario sets.

For development, run `./build.sh` to build and atomically replace `bin/bot`. Run `cargo test --locked --release` for the six focused tests covering model integrity, directional features, neutral inventory, hysteresis, and the position risk budget. Compilation requires the Rust SDK available in the development environment; execution does not.

Diagnostic `OBS` records go to standard error: step, mark, oracle, position, entry price, account PnL, drawdown, target, and steps since the event's last extension.
