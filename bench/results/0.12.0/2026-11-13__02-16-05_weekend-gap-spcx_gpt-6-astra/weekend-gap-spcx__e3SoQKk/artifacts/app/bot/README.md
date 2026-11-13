# SPCX weekend strategy

`bot.wasm` is the built `wasm32-wasip2` strategy component. Rebuild with `just build`.

The strategy reads confirmed Phoenix market-order transactions, identifies each taker's flow, and runs a bank of Kalman filters to separate permanent impact, temporary pressure, and drift. It follows recurring flow during the closed period and uses its inferred reopening value during the last 20 slots. It does not read scenario files or use scenario identities.

Orders are sliced into five-share transactions, with normal position changes limited to 20 shares per turn. Exposure never targets more than the session cap. Reopening exposure is sized against a 16% adverse external move and a remaining $850 loss budget, including the mark-to-book difference. The bot reads actual collateral and positions and stops trading when either marked or estimated liquidation drawdown exceeds $550. After reopening it retains the position unless a stop requires liquidation.

Runtime inputs are exclusively the session, market observations, and the allowed read-only RPC methods. There are no runtime files, external services, trained prediction models, or build dependencies beyond the supplied SDK.

Validation reports are kept in `validation/`. The tests cover reopening cash-flow risk and the absence of directional conviction without flow. Run `cargo test --lib` with the supplied Rust toolchain to execute them.

Final fresh 72-session sandbox validation: $18,156.86 total PnL, 0.23978 reward, $278.16 maximum drawdown, 70-share maximum position, and no disqualifications. The component used at most 10,723,790 instructions in a call. These are generated validation scenarios, not the hidden grading set.
