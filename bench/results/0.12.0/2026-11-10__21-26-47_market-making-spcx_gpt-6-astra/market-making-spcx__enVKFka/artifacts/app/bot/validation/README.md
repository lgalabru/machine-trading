# Validation

The final strategy was checked on 72 valid generated sessions and the six supplied practice sessions. All used real sandbox RPC and transactions.

| Batch | Sessions | PnL (USDC) | Normalized reward | Deepest drawdown (USDC) |
| --- | ---: | ---: | ---: | ---: |
| Generated | 72 | 3264.43 | 0.197184 | 144.10 |
| Practice | 6 | 362.69 | 0.252475 | 27.16 |

Generated-session peak absolute inventory was 19.500 SPCX. No session was disqualified; there were no refused RPC calls, positions outside SPCX, liquidatable accounts, or position/drawdown-limit breaches. There were 0 turn overruns and 0 late sends.

The generated cases comprise 24 comparison cases and 48 new cases. One originally generated case had overlapping waypoints and failed the grader's input validation; a fresh independently generated case replaced it. No failing trading result was omitted.

Three unit tests verify model-export parity, symmetric flow signals and cooldowns, and oracle-priced reversal orders within the position cap. See the JSON reports and `unit-tests.txt` for details.

These are development validation results, not the hidden grading result.
