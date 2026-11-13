# Validation

Tested artifact SHA-256: `6558eb6e3ae84c44f80e72e1018b71c58a8f400ac83f67505ef8c9922446dca7`.

| Sandbox set | Sessions | PnL (USDC) | Reward | Maximum drawdown | Disqualifications |
| --- | ---: | ---: | ---: | ---: | ---: |
| Comparison | 102 | 39,724.19 | 0.282310 | 678.88 | 0 |
| Fresh final validation | 144 | 52,080.87 | 0.287634 | 574.95 | 0 |
| Adverse jump stress | 12 | -6,864.44 | n/a | 748.37 | 0 |

The stress cases deliberately move against the strategy to test the loss controls;
their losses are separate from ordinary-session performance.

All 258 sandbox sessions stayed within the SPCX position cap and drawdown mandate,
with no refused RPC calls, strategy traps, instruction overruns, positions in other
markets, or liquidatable final accounts.

The largest strategy call consumed 12,175,479 WebAssembly instructions.
Four unit tests pass, covering feature/model parity and independent risk controls.

The full fresh report is `validation.json`; individual reports are in `reports/`.
