# This strategy

A trading strategy for the machine-trading bench: a Rust crate (`src/lib.rs`)
on mtx-kit, built into a WebAssembly component. What it may do and how it
runs: https://lgalabru.github.io/machine-trading/guides/bots/writing-a-bot.html

## Every line is documented

Whoever reads this code is learning to trade from it. When you write or
change the strategy, every line you touch says what it does and why:

- Each constant has a `///` comment: what it is, its unit (percent, units of
  the market, dollars, steps) and why it has that value.
- Each type, field and function has a `///` comment: what it's for.
- Each line of logic in a body has a `//` comment, above it or at its end:
  what it does and why, in words a trader can follow without reading Rust.
  Only closing braces, blank lines and a bare `Ok(..)` go without.
- When a line changes, its comment changes with it. A comment that no longer
  matches its code is worse than none.

## Sizes

Sizes are in units of the market: 70 SPCX, 0.15 BTC. Size orders and
thresholds from the cap (`market.cap`), never in whole units: a rule like
"trade only when the change is at least 1 unit" never trades BTC.
