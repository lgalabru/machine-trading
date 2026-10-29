# Machine Trading specs

Specifications for letting agents trade on someone else's behalf, starting
with the **mandate**: what an agent may trade, and the limits it must stay
within.

A mandate constrains what an agent does, never why. It holds no strategy,
signals or triggers: strategies are code, and agents write code well. What a
principal needs from an agent is a boundary they can state up front and check
afterwards, the way an investment mandate bounds a fund manager and a loss
limit bounds a desk trader.

## Layout

The layering follows the [Machine Payments Protocol](https://paymentauth.org/),
which separates abstract payment intents from the methods that implement them
on each rail:

| Layer | What it defines | Specs |
| --- | --- | --- |
| Core | The mandate document, its rules, and how they are enforced | [`mandate.md`](mandate.md) |
| Instruments | What a position is for each kind of instrument, and how its limits apply | [`instruments/`](instruments/) |
| Venues | How each venue's accounts are measured against a mandate | [`venues/`](venues/) |

Examples live in [`examples/`](examples/). Every example is checked against
the reference implementation by `cargo test`.

## Status

| Spec | Version | Status |
| --- | --- | --- |
| [Mandate](mandate.md) | v0 | Draft |
| [Perpetual](instruments/perp.md) | v0 | Draft |
| [Phoenix](venues/phoenix.md) | v0 | Draft |

v0 covers what the `phoenix-flash-crash` task enforces today: one perpetual
on one venue, a position cap, and a drawdown limit. The reference
implementation is `mth_botkit::mandate`, which bots read and the grader
audits with.

These specs live in this repository until a second venue or a consumer
outside the benchmark needs them.
