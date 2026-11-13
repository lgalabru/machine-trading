# SPCX strategy

`bot.wasm` is the built `wasm32-wasip2` strategy component. The component embeds all forecasts and needs no training files at runtime.

The bot reads Phoenix's session funding parameters, maker centres and its trader account through `getMultipleAccounts`. It combines premium decay, index momentum, temporary price dislocations and completed episode evidence to forecast changes in book value net of funding. Forecasts were fitted on generated practice-family scenarios, with additional oracle wobble and temporary dislocations for the hidden family's wider noise range.

Trades use bounded IOC orders in 5 SPCX slices. The position stays within the session cap. Equity checks include unsettled funding; exposure decreases as drawdown consumes the remaining risk budget, and a $500 drawdown stops further trading for the session. The final seconds reduce the amount of forecast edge available to pay execution costs.

Build with `just build` using the supplied SDK. The grader loads the existing `bot.wasm` directly.

## Validation

The final component completed 18 practice and maximum-wobble sessions: $15,482.63 aggregate PnL, $320.45 deepest drawdown, and no rule violations. All 18 sessions were profitable. Details are in `validation.json`.
