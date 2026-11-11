A native Rust market maker for Phoenix SPCX. `run.sh` starts the prebuilt `bin/bot`; it does not build, download, or write into the bot directory.

The strategy prices from the oracle and reads the competing best bid and ask after cancelling its old quotes. It quotes one tick inside the competing prices, with an inventory skew and a minimum oracle spread. Normal inventory is modest; stronger signals can use up to 18 shares, within the session mandate.

Public Solana transaction history supplies taker order counts, including orders that did not fill. The bot filters for SPCX market-order instructions, deduplicates signatures, and excludes its own transactions. A Poisson likelihood model looks for unusual directional bursts across several windows. It withdraws the exposed quote side during suspected informed flow and trades toward the signal when evidence is stronger. A separate Bayesian estimate of oracle drift supplies a slower inventory target. Jump cooldowns and the session clock limit unnecessary signal trades.

Risk controls value collateral and positions at the oracle, reserve room for an adverse price jump and execution costs, reduce existing positions when capacity contracts, and flatten and stop after a drawdown threshold. Bid and ask sizes account for the current position, including any market orders sent during the turn. Only SPCX is accepted.

Build in the supplied development environment:

```sh
/app/bot/build.sh
PATH=/usr/local/cargo/bin:$PATH cargo test --release --manifest-path /app/bot/Cargo.toml --bin bot
```

The tests check SDK packet decoding, position bounds under every bid/ask fill combination, and the adverse-move reserve. `validation.json` records the completed simulation checks. The bot needs only the contract's environment variables and standard Solana RPC methods at runtime.

Completed validation on 72 fresh generated scenarios: $2,782.16 total PnL, $69.36 maximum drawdown, 18-share peak position, and no mandate violations, refused calls, late sends, or turn overruns. Mean bot turn time was 47 ms. These are generated-scenario results, not hidden evaluation results.
