Build a native Solana trading bot in `/app/bot`. Only this directory is
copied to evaluation. Exclude build caches and source dependencies.

The public market has 24 tokens, 64 Raydium CPMM pools and 64 Orca Whirlpools.
Initial pool state and instruction adapters are in `/opt/backrun/public` and
`/src/crates/backrun`; an optional CPI route executor is in
`/opt/backrun/programs/backrun-executor`. Run `mth-backrun playground --help`
for resettable experiments. The playground has no scheduled user flow or score.

Design and build your own development scenarios in `/app/dev-scenarios`, outside
`/app/bot`. Choose the order streams, competing behavior and edge cases you want
to test. Drive market changes with real swap transactions, reset to the public
snapshot for comparisons, and retain seeds, parameters and results so you can
replay your experiments. We supply no alternative user-flow fixtures. Your
scenario suite is a development tool and does not choose or alter evaluation.
The stock playground advances blocks on transactions for rapid experiments.
For timed competition tests, your own driver can use the public Surfpool SDK's
`BlockProductionMode::Clock` with the benchmark's 200 ms slot time. No private
schedule, reference-searcher implementation or scored evaluation is available.

You have 95 WSOL and 5 SOL in ordinary signer-owned accounts. Pre-created token
accounts and lookup tables are provided. Discover opportunities, choose routes,
input sizes and transaction parameters, and submit signed Solana transactions.
There is no benchmark atomic guard, minimum profit check or inventory restriction.
Normal Solana transaction atomicity applies. You may use direct AMM instructions
or optional custom SBF programs, installed immutably before the session.

Provide `run.sh`, a compiled Linux native executable and `manifest.json`:

```json
{"version":2,"executable":"bot","programs":[],"scratch":[]}
```

Optional programs are `{"address":"PUBKEY","file":"program.so","sha256":"HEX"}`;
optional scratch entries are `{"address":"PUBKEY","owner":"PROGRAM_ID","bytes":1024}`.
At most four programs and 16 scratch accounts (16 KiB each, 64 KiB total) are
installed. The optional `executor` field selects a declared program for SDK
route helpers; the supplied route executor is also available without it.

`run.sh` starts in its artifact directory. `MTH_BACKRUN_GATEWAY` and
`MTH_BACKRUN_TOKEN` supply the URL and credential. Every HTTP/WS connection uses
`Authorization: Bearer TOKEN`. GET `/context` for your signing key, market and
lookup tables. POST `/ready` after initializing. GET `/status` gives `running`,
`finished`, `start_slot`, `end_slot` and `slot_ms`. Initialize before ready; trade
while running; stop when finished. Searchers run continuously, without freezing.

POST `/rpc` uses ordinary JSON-RPC: account, balance, token, block, signature,
blockhash, fee, simulation and transaction methods. Use `encoding: "base64"`
for transactions/accounts and `commitment: "processed"`. Pass native simulation
options explicitly, including `accounts` if you need simulated balances.
`sendTransaction` supports normal Solana configuration, including `skipPreflight`;
signature verification cannot be disabled. Administration, airdrops, account
replacement and other Surfnet extensions are unavailable. WS `/ws` proxies
native account, program, slot, log and signature subscriptions/unsubscriptions.

Each of four searchers has one dedicated CPU, 512 MiB RAM and the same gateway
limits: 256 RPC requests, 16 simulations and 32 sends per second; eight in-flight
HTTP requests; two WS connections, at most 32 subscriptions issued per connection.
Rate limits may reject requests. A native read is coherent within that RPC call;
multiple calls, simulation and execution can observe different processed states.
Surfpool does not provide historical account snapshots per commitment.

Submit one bot once. The same artifact is evaluated in three separate private
sessions. Each session restores the same initial market and fresh capital,
competitors and isolated containers. There is no further development between
sessions. Each private immutable schedule spans 2,000 slots at 200 ms/slot
(about 6m40s).
The injector submits its fixed swaps as their slots arrive, alongside all four
searchers. Future user orders stay private. Swaps can fail as liquidity changes;
orders are never rerouted or replaced. Pool state persists throughout the run.
There is no sealed auction, user-first barrier or event-bound transaction expiry.
Normal recent-blockhash validity applies. No sandwiching facility or pending
order feed is provided.

Surfpool executes transactions in native processing-queue arrival order. Priority
fees are charged but do not improve ordering in this version. Simulation success
does not guarantee inclusion or profit. Included failures pay fees. Native runtime
transaction limits apply; there is no benchmark 8M-CU auction budget. At session
end new sends stop; admitted RPC requests drain before final balances are read.
An exited candidate misses remaining opportunities and retains its losses.

Reward is signed realized native capital change, in SOL, from trusted initial
and final on-chain snapshots. Capital includes the provided wallet's SOL and
lamports in its SPL Token accounts (including WSOL backing), plus declared
scratch-account lamports. Initial and final refundable rent are both counted,
so closing a harness-funded account creates no profit. Other token amounts,
LP positions and assets sent to untracked wallets receive zero credit. Return
capital to the tracked wallet/accounts by the end if you want it credited.
Fees are already reflected in balances and are not deducted twice. Doing nothing
earns exactly zero. A separate post-run verifier checks the fixed flow and ledger,
then computes reward. Exactly one run per private session is scored. Signed net profit and diagnostics
are reported separately for each; no aggregate score is defined. Infrastructure
failures produce no score;
invalid artifacts are disqualified (-101 SOL, outside valid rankings).
