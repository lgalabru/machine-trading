# SPCX shock bot

A Phoenix perpetuals bot for the `mtx` flash-crash / short-squeeze benchmark.
`run.sh` starts `bin/bot`; `build.sh` rebuilds it (run it after every change).

## Layout

- `src/strategy.rs`: the trading logic, pure. One call per step with the
  oracle price, the position and the account value returns a target
  position. Shared by the live bot and the simulator.
- `src/main.rs`: the live bot. Each turn it reads Hawkeye's BBO (oracle,
  mark, bid, ask), the account margin (collateral, unrealized PnL) and the
  position, feeds the strategy, and trades toward the target with IOC market
  orders (split into slices of at most 40 shares, limit 1% through the book).
- `src/bin/sim.rs`: offline simulator. Replays scenario files through the
  strategy with the chain's rendering of the path (a jump above 4% lands in
  equal pieces on consecutive steps) and a fill model (3.5 bps taker fee,
  half a cent of spread, ~4 bps slippage at 70 shares). Prints PnL, bound,
  drawdown and a PnL attribution by strategy mode. Parameters can be
  overridden as `name=value` arguments:

      cargo build --release --offline --bin sim
      $CARGO_TARGET_DIR/release/sim run_steps=3 /tmp/tune/*.json

  On 480 generated scenarios the simulator and the live grader agree within
  a few percent per session.

## Strategy

The market model from the task: a level with wobbles under 0.3%, fizzles
(0.4-2%, fully reverting within 10 s), shock legs (0.6-8%, holding 8-35 s,
up to three, each followed by another with odds of a third), and an
aftermath (revert of 50-100%, stall, or extend by 1-5%). Moves land at once
or ramp in sub-steps 3-10 slots apart.

Modes, in order of precedence:

1. **Run**: a sub-step (one-step change of at least 0.2%) landed within the
   last 4 steps and the move is rideable: it already has several sub-steps
   (a ramp), its first sub-step is small (at most 1.5%, a ramp start), or
   its first sub-step is at least 2% (the first piece of a split jump: the
   second piece follows next step for sure). Hold the full allowed position
   in the move's direction. Two equal pieces of at least 2% mark the move as
   landed at once. This mode makes most of the PnL.
2. **Fizzle**: a fresh move (started within 24 steps) of at most 2.1% from
   the anchor: fade it at full size; most such moves give it all back.
3. **FadeShock**: displacement from the anchor of at least 6%, or three legs
   counted: fade it.
4. **RideShock**: a shock of 2-6% during its hold: stand aside (weight 0;
   riding and fading both tested worse).
5. **Post**: after the aftermath's revert (an opposite move giving back at
   least half of a shock above 2.1%), the level reached becomes the anchor
   and only fizzles remain: fade fresh moves for 24 steps.
6. A level that holds 72 steps (36 s, longer than any shock pause) becomes
   the anchor regardless.

Risk: every target is capped at `(1000 − drawdown − 80) / (price × worst)`
shares, with `worst` 9% for a fade (one more leg) and the displacement
itself (at least 2%) for a ride, so one more worst-case move cannot breach
the drawdown limit. Positions are clamped to the 70-share cap before
sending, and orders are IOC so nothing rests.

Tuning used `mtx scenario generate` sets of 240 scenarios (seeds 101 and
202); the live grader reproduced the simulator's results on 30 further
scenarios (reward ≈ 0.19, reference ≈ 0.13).
