# SPCX shock strategy

`bot.wasm` is the built `wasm32-wasip2` strategy component. Rebuild with `just build` in this directory using the supplied SDK.

The policy forecasts the next second's oracle return from 37 features describing impulses, retracements, elapsed time, and price changes at several time scales. The regression trees were trained and refined on 30,000 generated scenarios. A four-basis-point decision band limits trading; the policy follows continuing moves and takes the opposite side when it expects a reversal.

The strategy reads its Phoenix trader account each turn to measure equity including fees and execution costs. Position size shrinks as drawdown rises, with a 150 USDC reserve and sizing against another 6% adverse step. It uses only the session's SPCX market, no resting orders, and market orders sliced into ten shares by `mtx-kit`.

`src/policy.bin` is compiled into the component. There are no runtime model files, HTTP calls, or dependencies outside the strategy interface and its read-only account RPC.

Validation reports are in `validation/`. This version passed 162 chain sessions: six supplied practice sessions, 144 generated sessions, and 12 selected adverse paths, with no disqualifications. The 144 generated sessions returned 0.230 of their hindsight bound; maximum drawdown across all 162 sessions was 740.17 USDC. These are offline test results.
