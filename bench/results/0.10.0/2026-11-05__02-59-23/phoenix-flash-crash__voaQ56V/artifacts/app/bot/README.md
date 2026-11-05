# SPCX shock bot

A shock-fading bot for Phoenix's SPCX perpetual, built on `mtx-botkit`.

## Strategy (`src/policy.rs`)

Every step (two slots) the bot reads the oracle price, its position and
equity, and sets a target position:

- **Fade moves.** A move of 0.35% or more away from the quiet reference
  level is faded (short a squeeze, long a crash). Small moves (up to 2.1%)
  are faded at full size for 11 s, since fizzles of that size fully retrace
  within 10 s; a move that holds longer is a shock leg and the fade is
  resized to a leg size that scales with the shock. After the aftermath
  (a retrace of half the shock, or 2.2% absolute, that holds) every move is
  a fizzle and is faded at full size.
- **Ride chunked moves.** Phoenix caps each oracle update at 4%, so the
  grader lands a bigger move as equal pieces on consecutive steps. A step
  move of 2% or more is ridden at full size until a step passes without a
  follow-up piece. Moves still in progress (ramps) are also ridden.
- **Drawdown budget.** Position is capped at `(room left under the $1,000
  drawdown limit - $60) / (9% x price)`, so the largest single-step adverse
  move the family produces (an 8% leg plus wobble) cannot breach the
  limit; after an adverse move the position is cut to the new cap.

Parameters were tuned in an offline simulator over 2,400 generated
scenarios (`mtx scenario generate`) and validated live with `mtx grade`.

## Layout

- `run.sh`: entry point the grader runs; starts `bin/bot`.
- `build.sh`: builds the crate and copies the binary to `bin/bot`.
- `bin/bot replay <trace.json>`: replays recorded level series through
  the policy to check the Rust port against the simulator.
