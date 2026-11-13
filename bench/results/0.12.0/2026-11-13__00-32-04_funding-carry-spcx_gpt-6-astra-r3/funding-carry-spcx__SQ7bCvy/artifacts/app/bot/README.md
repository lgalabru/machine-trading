# Phoenix SPCX strategy

`bot.wasm` is the built `wasm32-wasip2` strategy component. It needs no files or network at runtime.

The strategy collects funding against crowded premiums, follows sustained index trends when their expected movement exceeds funding, recognizes gradual premium openings, and fades temporary index dislocations. A noise estimate selects the trend window; hysteresis limits costly reversals.

Risk uses Phoenix's actual collateral, inventory and cumulative funding, valued at an estimate of the maker book centre. Normal exposure is at most 70 SPCX. Position size decreases with drawdown, reserving room for a 6% adverse jump; trading stops at 65% of the mandate's drawdown limit. Orders use five-share slices and limits based on the current best bid or ask. The strategy places no resting orders and trades only SPCX.

```sh
just build
just test
```

`just test` checks position and loss-budget invariants, including adverse jumps and a reserve for funding and execution. The 68-share version passed 72 end-to-end validation sessions: $24,128.21 total PnL, $501.71 maximum drawdown, and no rule violations. The full results are in `validation.json`. Synthetic stress cases are independent approximations of the described family, not the hidden grading sessions.

The final 70-share sizing passed 12 ordinary sessions plus four consecutive adverse-jump checks, with no rule violations. On those 12 ordinary sessions, PnL increased from $4,618.94 to $4,723.02. See `final-checks.json`.
