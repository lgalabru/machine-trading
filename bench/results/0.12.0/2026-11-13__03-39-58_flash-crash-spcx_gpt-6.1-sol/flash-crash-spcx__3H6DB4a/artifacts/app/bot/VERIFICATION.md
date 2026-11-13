# Verification of the saved component

The final component passed 173 distinct chain scenarios with no disqualifications: 6 practice sessions, 36 development sessions, 36 validation sessions, 72 fresh sessions, and 23 selected stress cases. The stress cases were deliberately selected for adverse PnL/drawdown and are not a representative random sample.

| Set | Sessions | Total PnL (USDC) | Reward | Maximum drawdown (USDC) |
| --- | ---: | ---: | ---: | ---: |
| Practice | 6 | 929.02 | 0.1432 | 291.68 |
| Development | 36 | 12,463.19 | 0.2534 | 429.47 |
| Validation | 36 | 14,996.86 | 0.3069 | 466.53 |
| Fresh batch | 72 | 25,985.93 | 0.2617 | 588.05 |
| Selected stress cases | 23 | 7,484.77 | 0.1827 | 650.93 |

The position cap, market restriction, RPC restrictions, drawdown limit, liquidation check, and instruction budget passed in every session. Eight native Rust tests pass. A replay of the fresh batch's highest-drawdown and highest-PnL sessions was identical across two runs. The exact packaged component also reproduced the practice report after the final build.

`results/` contains the full reports and an artifact SHA-256 manifest. `just build` replaces the component atomically after a successful build; `just test` runs the native unit tests.
