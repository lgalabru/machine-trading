# Validation

`final-chain.json` contains the final executable's 90 chain sessions, assembled
from the four unmodified grader reports in `batches/`. They comprise six public
practice sessions, 72 generated sessions (seed 864592), and 12 constructed
three-leg stress sessions. All passed the mandate; no RPC calls were refused,
no turns overran, and no positions exceeded 70 shares or became liquidatable.

| Final build group | Sessions | PnL (USDC) | Maximum drawdown (USDC) |
| --- | ---: | ---: | ---: |
| Public practice | 6 | 918.05 | 268.31 |
| Generated | 72 | 21608.71 | 614.04 |
| Constructed stress | 12 | 21068.37 | 677.60 |

`ensemble-validation.json` records 72 further generated scenarios (seed 572813)
and the stress set using the same forecasts and position policy, before the
final emergency-price refinement. `forecast-fresh.json` and
`forecast-validation.json` are the alternative forecast policy's complete
comparison reports. `summary.json` separates every group, so constructed stress
profits are not confused with generated-session performance.

The two models were nearly tied across the 144 comparable chain scenarios;
the delivered ensemble led the alternative by approximately 5.4% on the larger
2,000-path development simulation. That simulation approximates fills and is
not a chain grade. No hidden scenarios were available or used.

`unit-tests.txt` records all eight passing tests. `../model-info.json` binds the
model parameters, source files, and delivered executable to SHA-256 digests.
The runtime does not read any research scripts or reports.
