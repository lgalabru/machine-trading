# SPCX weekend strategy

`bot.wasm` is the prebuilt `wasm32-wasip2` strategy component. It trades only the session's SPCX market through `mtx-kit`'s order sheet.

## Strategy

- Read current Phoenix market-order transactions with **confirmed** commitment; decode side, lots, limit, and signer using Phoenix's instruction discriminator and venue unit conversions.
- Track each taker's direction, pace, sizes, and aggressiveness. A mixture of 45 Kalman filters compares eight possible informed accounts, plus no informed account, across five impact-decay half-lives.
- Separate permanent impact, temporary inventory pressure, thin-book drift, and impact per share. Delay assimilation until recent transactions' price impact has reached the book; ignore book distortions caused by the bot's own executions.
- Trade the estimated near-term movement during the closed period, then switch toward the estimated reopening value. The reopening estimate uses permanent impact rather than the transiently distorted book price.
- Use a spread-aware inaction region to control turnover. Submit five-share IOC slices with price limits, normally up to 25 shares per turn. The last ten slots permit a complete repositioning, still in five-share slices.
- Retain the existing position after reopening, avoiding an unnecessary exit spread when the account will be valued at the mark. The drawdown guard remains active.

## Risk controls

The normal position ceiling is 69.5 shares, additionally clamped to the session cap. No resting quotes are placed. The kit independently cuts the order sheet to its position mandate.

Equity and drawdown are calculated from the trader account's actual collateral, virtual quote balance, and marked SPCX position, including execution costs. Exposure is reduced after a $600 drawdown; at $850 the bot sends only closing trades. Before reopening, sizing budgets for adverse outcomes across plausible taker identities and directions, quiet weekends, and surprises. These are conservative scenario-based estimates, not a mathematical guarantee against an unbounded surprise distribution.

Live code reads no files, environment variables, external services, or wall clock. It uses only `Session`, `Market`, and the allowed `getSignaturesForAddress`, `getTransaction`, and `getAccountInfo` RPC methods. All trading goes through `Orders`.

## Build and test

```sh
cd /app/bot
just build
just test
mtx-practice
```

The build requires the supplied `/opt/mtx/sdk` and Rust toolchain. The grader needs only the already-built `bot.wasm`; there is no runtime dependency on development tools or test datasets.

Four native regression tests cover directional-flow invalidation, reopening behavior, adverse-gap sizing, and finite bounded positions under mixed flow and jumps. `validation.json` records full sandbox validation results for the delivered component. The hidden grading scenarios were not available during development.

## Validation of the delivered artifact

- 72 generated sandbox sessions: **$19,914.71 PnL**, **0.2621 reward**, no disqualifications.
- Maximum drawdown in that set: **$232.70**; profitable sessions: **86.1%**.
- Eight separately selected stress sessions also passed; maximum drawdown **$540.49**.
- Four native regression tests passed.

Reports: `validation.json` and `stress-validation.json`. The 72-session set was generated with `mtx scenario generate --family weekend-gap --out DIR --prefix val --count 72 --seed 53751903`.

Artifact SHA-256: `58d08efd60bac636f2bc7418f6b4a2786d662d6d7132804cc163ca8f519d9115`.
