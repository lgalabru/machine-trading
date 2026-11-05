# Private three-scenario AMM evaluation

This is maintainer documentation. Neither these regime descriptions nor the
flow generator is exported into the public development SDK or agent image.

A good bot profits because it sizes executable trades against changing liquidity,
responds to competition, and realizes inventory value after fees. This extends
the original continuous arrival-order task without changing its trading rules.

| Private scenario | Design choice | Observable evidence | Failure mode exercised |
| --- | --- | --- | --- |
| Liquidity-driven | Uneven pair demand; venue preference weights local effective depth and initial fee/impact-adjusted execution quality, with some venue exploration | Committed swaps and resulting pool state | Scanning only the previously busiest venue misses opportunities |
| Incentivized | Preserve the original market, snapshot, user wallets, provenance and Raydium-heavy flow bytes | Committed swaps and resulting pool state | Assuming all users always choose the deepest venue |
| Meme frenzy | Four or five varied buy/sell episodes on shallow token/WSOL pools, with background and counter-direction flow and alternative paths | Executed directional flow, reserve changes and inventory | Rigid cycle-only search or untested inventory liquidation |

The second name describes venue incentives as an interpretation of the existing
flow; there are no new incentive payments. Runtime migration changes only the
execution-policy provenance and seals. The old release and recorded Terra result
remain immutable and separate from this new version.

The frenzy uses WSOL as quote. It introduces no synthetic USDC or unsupported
stable-value assumption. A sell puts meme tokens into the pool and removes WSOL;
acceptance checks these actual reserve deltas. Episode turns, durations, sizes
and gaps vary at generation time. A fixed midpoint or runtime rerouting is not
used. All scenarios expose identical initial pool state, so initial metadata
does not identify the future regime. Every scheduled order remains fixed even
when a prior swap changes its execution quality or causes it to fail.

Generation uses a private seed once. Orders, provenance and seed stay in private
fixture storage and verifier images. `set.json` hashes the full set; individual
fixture seals and flow hashes identify each input exactly. The generator records
its own source hash and Python version. Grading never invokes generation.

## Development and evaluation

Supply initial market, real program binaries, resettable playground and execution
rules. Ask the development model to invent its own reproducible scenarios. Do
not supply evaluation regime names, schedules, generators, seeds, reference code
or scored evaluations to it. An identical public market is used in all sessions.

`mtx evaluate` orchestrates one Harbor development/evaluation trial, then two
copy-only Harbor trials. The latter invoke no model and only copy the submitted
artifact; they do not run candidate code in the development or grader container.
Every trial starts a new Compose project, volumes, chain and four searchers.
This deliberately uses separate container lifetimes rather than trusting a bot
to reset its own process state. Artifact hashes, runtime hashes and hardware
fingerprints must match across all reports. If a session fails infrastructurally,
retain the earlier reports and mark the overall set incomplete; do not turn the
missing session into zero, retry automatically or pick a best result.

Signed profit is reported per scenario. No aggregate formula was selected.
Disqualification is separate from infrastructure failure. Existing capital and
inventory rules remain unchanged: non-native token amounts receive zero terminal
credit, which makes liquidation relevant without imposing an atomic guard.

## Deferred work

UI/live-view work, developer-experience grading and engineering-quality grading
are outside this implementation. Two Surfpool proposals remain separate:

- Native transaction scenario actions: resolve fresh blockhashes and runtime
  accounts at scheduled slots, submit through the ordinary runtime and record
  outcomes. Generic action scheduling belongs in Surfpool; market-specific swap
  selection stays in the benchmark.
- Optional priority-fee ordering in clock mode: bounded pending queue, explicit
  selection points, pinned validator reward-to-cost policy, capacity, validity
  and account-conflict handling. User injection and bots must use the same queue.
  Already executed transactions cannot be reordered. Existing fees are charged;
  ordering is the missing behavior. Keep fast transaction-driven development mode.

Neither proposal authorizes publishing upstream, changing this arrival-order
release, or comparing scores across different execution policies as equivalent.
