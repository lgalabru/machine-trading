# SPCX shock strategy

`bot.wasm` is the compiled `wasm32-wasip2` strategy component. The grader can load it directly. Build from source with `just build`; run the supplied scenarios with `mtx-practice`.

The strategy follows active price moves and trades their likely retracements once they settle. Two embedded gradient-boosted regressions forecast average index displacement over 2 and 8 seconds. The short forecast governs the first 1.5 seconds after an impulse; the longer forecast governs subsequent turns. Inputs are trailing returns, trailing ranges, impulse sizes, and elapsed market steps. All inputs are causal. The models were fitted to 10,000 separately generated scenarios; no scenario paths are embedded in the component.

A forecast beyond 0.10% opens a position in its direction. A weaker forecast retains an existing position until its sign changes. A pair of fitted action-value models checks weak exit signals: it retains the position when holding has more expected value than flattening after allowing for trading costs and a further two-basis-point buffer. The action-value models were fitted with 18 Bellman updates and a nine-basis-point one-way cost assumption. The strategy uses the session's position cap and sliced market orders, and places no resting orders. It leaves the final turn alone because the account is already valued at that turn's mark.

Risk management reads the Phoenix trader account with `getAccountInfo` each active turn. It values collateral plus SPCX's base and virtual quote balances at the mark, so drawdown includes trading costs. Position size shrinks with the remaining drawdown cushion, reserving $100 for execution and allowing a 4.5% adverse move in the next turn. Drawdown above 75% of the session limit permanently switches the strategy to repeated flattening orders. If the account read fails, it uses the SDK drawdown estimate plus a turnover cost allowance.

## Checked build

The current strategy was evaluated on the six supplied practice scenarios and 144 independently generated scenarios:

| Measure | Result |
| --- | ---: |
| Total PnL | $59,972.87 |
| Share of hindsight bound | 28.77% |
| Profitable sessions | 116 / 150 |
| Largest drawdown | $586.71 |
| Worst session | -$403.05 |
| Disqualifications | 0 |

Six additional stress scenarios used three maximum-size shock legs followed by a reversion, stall, or extension, in both directions. All six stayed within the mandate; their largest drawdown was $800.13. The final component replayed all six practice and six stress sessions twice with identical results. Three native safety tests cover permanent loss halts, retrying an unfilled exit, terminal-turn behavior, and an adverse jump plus execution costs.

`src/lib.rs` contains the policy, feature construction, execution, and account risk checks. `src/model.rs` contains the forecast trees; `src/q_model.rs` contains the action-value trees. Run the safety tests with `cargo test --release --lib`. The component needs no runtime files, environment variables, network, or external model service.
