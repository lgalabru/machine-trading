# SPCX shock trader

`bash /app/bot/run.sh` starts the precompiled native executable in `bin/bot`.
It needs only the supplied MTX environment, local metadata, keypair, and Solana
RPC. It does not build, download, or invoke Python at runtime.

The strategy observes the oracle every turn and remembers multi-leg shocks.
An embedded, direction-symmetric model estimates continuation and reversion.
A second estimate accounts for the cost of switching positions; hysteresis
avoids repeatedly crossing the spread when the evidence is weak. Training uses
separately generated scenarios, never a session's future path. The session's
remaining time is an input so an anticipated late rebound is not treated as
immediately available profit.

Risk controls are independent of the forecast:

- SPCX is the only accepted symbol. There are no resting orders.
- The kit clips targets to the mandate; orders are sliced into 10-share IOC
  market orders. Targets also leave cash for another 4.5% adverse mark move.
- Phoenix's margin view supplies collateral plus unrealized PnL, so the running
  drawdown includes actual execution costs. Wallet USDC is untouched.
- A 65% drawdown-budget halt is permanent. Exposure shrinks before the halt as
  headroom declines. A failed margin read requests a flat position.
- Emergency order limits account for remaining cash, fees, and a further cash
  reserve. The final turn makes no unnecessary trade: open positions are valued
  at the mark by the session contract.

Implementation: `src/main.rs` connects the policy and execution;
`src/signal.rs` constructs causal features; `src/model.rs` evaluates embedded
regression trees; `src/risk.rs` contains the loss-budget controls.
`src/model.bin` is compiled into the executable, not loaded at runtime.

To rebuild in the supplied development image:

```sh
./build.sh
```

To build a candidate without replacing the checked executable:

```sh
BOT_BUILD_OUT=/tmp/candidate/bin/bot ./build.sh
```

`cargo test --release` checks exported predictions and features, adverse-jump
risk paths, worst-price emergency fills, persistent loss halts, and final-turn
behavior. The `research/` scripts are development-only and are not used by
`run.sh`. Completed validation results are recorded in `reports/`.
