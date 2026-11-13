# SPCX strategy

`bot.wasm` is a self-contained `wasm32-wasip2` strategy component. Rebuild with `just build`; run native risk-control tests with `PATH=/usr/local/cargo/bin:$PATH cargo test --lib`.

The strategy forecasts an eight-second return after funding from 33 causal features: the book/index premium, index trends and noise, premium decay, the current funding configuration, time remaining, and evidence from completed episodes. The embedded gradient-boosted model was trained on generated practice scenarios and independent noise augmentations. Runtime inputs come exclusively from `Session`, `Market`, and read-only Phoenix account calls.

A 15-basis-point decision band limits unnecessary turnover. The strategy holds its existing position inside that band. It reads Phoenix's actual funding parameters at startup and includes collateral, open-position value, and unsettled funding when tracking drawdown.

Risk controls:

- SPCX only; targets stay within the session's position cap.
- Exposure shrinks to reserve capacity for a 6% adverse book move and execution costs.
- A drawdown above 75% of the desk limit permanently stops trading and closes exposure.
- Failed account reads disable further risk taking and request a close.
- Orders are sliced into at most 10 shares, with limits derived from current quotes. No resting orders are submitted.
- No final-step trades, because the account is valued at the book and such trades only incur costs.

`src/model.rs` contains the generated tree constants; `src/features.rs` contains the causal feature calculation. Neither training data nor Python dependencies are needed to build or run the bot. See `validation.json` for the evaluated scenarios and results. These are practice and constructed stress tests, not the hidden grading sessions.

Validation: the six supplied practice scenarios earned **$1,978.55**, all profitable, and produced identical reports in two replays. Across 58 distinct evaluated practice/stress scenarios, 56 were profitable, none violated the mandate, and the largest observed drawdown was **$378.51**. Both native risk-control tests passed. Native export checks also verified model and feature parity with the training implementation.
