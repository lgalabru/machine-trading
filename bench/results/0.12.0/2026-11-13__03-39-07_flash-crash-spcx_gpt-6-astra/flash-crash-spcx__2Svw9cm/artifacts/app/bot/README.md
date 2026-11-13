`bot.wasm` is the ready-to-load `wasm32-wasip2` strategy component for Phoenix SPCX.

Build from this directory with:

```sh
just build
```

The build replaces `bot.wasm` atomically after compilation succeeds. The component embeds its model weights and needs no runtime files, network access, or environment variables.

The strategy combines two- and five-second return forecasts from gradient-boosted trees. Its 65 features describe observed oracle returns, ranges, extrema, impulses, and the age and size of recent moves. Direction normalization applies the same logic to crashes and squeezes. Entry and exit thresholds differ to avoid repeatedly paying execution costs for weak changes in the forecast.

The models were fitted to 50,000 independently generated public scenarios. Model and policy comparisons used separate validation paths, including a further 10,000-path test set. No scenario paths or future observations are available to the running component.

Risk and execution:

- Positions stay within the session's cap. Orders use the SDK's market-order slicing; the strategy leaves no resting orders.
- One allowed `getAccountInfo` read per turn obtains the trader's quote collateral. Drawdown uses collateral plus the position's mark-valued unrealized PnL, including realized trading costs. Constant wallet USDC cancels out of drawdown.
- Position size reserves $150 and budgets for a further 6% adverse move. A $700 observed drawdown permanently halts trading and targets a flat position.
- Failure to read or decode the margin account also triggers the permanent halt.
- The final turn adds no orders; existing positions receive the session's terminal mark valuation.

`src/features.rs` contains the causal feature calculations, `src/model_fast.rs` and `src/model_slow.rs` contain the fitted trees, and `src/lib.rs` combines forecasts, risk controls, and execution. `validation.json` records the completed checks for the delivered artifact.
