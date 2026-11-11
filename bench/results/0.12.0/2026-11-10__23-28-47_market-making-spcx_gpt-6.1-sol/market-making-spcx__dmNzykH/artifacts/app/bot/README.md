# Phoenix SPCX maker

`bash run.sh` starts the prebuilt native executable in `bin/bot`. It uses the supplied `MTX_*` contract and the SDK turn protocol. Runtime needs no compiler, SDK installation, or external API.

The bot quotes around the oracle, improves the competing book by one tick, and skews its reservation price against inventory. Moderate one-sided flow changes its quotes. A strong burst can trigger a bounded directional position; a separate filter combines oracle changes and taker directions to recognize persistent trends. Flow comes from standard Solana block reads, scoped to SPCX and excluding the bot's own trades.

Risk checks value equity at the oracle, including realized PnL and fees reported by Phoenix. Ordinary making uses a six-share working limit. Directional exposure stays inside the mandate and reserves room for another adverse move. The bot cancels before replacing orders, separates aggressive trades from subsequent quoting, and stops adding exposure after its drawdown threshold.

Build with `./build.sh`; run the five offline checks with `cargo test --release`. The build replaces `bin/bot` atomically. Chain validation uses `mtx-practice`. See `validation.json` for completed validation results.
