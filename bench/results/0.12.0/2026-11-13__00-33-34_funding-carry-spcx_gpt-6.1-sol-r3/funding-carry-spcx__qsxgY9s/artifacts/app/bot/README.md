# SPCX strategy component

`bot.wasm` is the built `wasm32-wasip2` strategy. Rebuild with `just build`.

The bot forecasts book returns after funding over six and fifteen seconds. Its
embedded regression trees were trained on independently generated practice-family
scenarios, with additional oracle wobble and temporary price dislocations. Runtime
features include premium, index and book trends, episode progression, estimated
noise, and the market's actual funding parameters and accumulator. Mirrored model
predictions preserve symmetry between premiums and discounts.

The strategy reads Phoenix's PerpAssetMap, its own trader account, the SPCX maker
splines, and Solana's clock through one read-only `getMultipleAccounts` call per
turn. Maker centres prevent its own market orders from contaminating forecasts.
Equity includes collateral, book-valued positions and unsettled interval funding.

A 25-basis-point forecast threshold controls entry; positions are retained while
the forecast keeps their sign. The order planner enforces the session position
cap. Orders use five-share slices. Exposure decreases with drawdown, and a stop
latches at 65% of the desk's drawdown limit. The final turn sends no new trades.

Source is in `src/lib.rs`, feature construction in `src/features.rs`, and the
embedded model in `src/model.rs`. The component uses no runtime files or network
access and trades only the session's SPCX market.

Validation: this exact binary passed 90 practice/stress sessions, all profitable,
with no limit violations or failed tape orders. Six official practice sessions
returned +$2,180.37; maximum drawdown across all 90 was $244.13. Detailed totals
and the component SHA-256 are in `validation.json`.
