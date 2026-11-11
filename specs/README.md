# Machine Trading Protocol

Specifications for letting agents trade on someone else's behalf, starting
with the **mandate**: what an agent may trade, and the limits it must stay
within.

Read them at [lgalabru.github.io/machine-trading](https://lgalabru.github.io/machine-trading/).

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
| Guides | Guides that teach instruments and strategies to people, with interactive figures. Not normative | [`guides/`](guides/instruments/) |

Examples live in [`examples/`](examples/). Every example is checked against
the reference implementation by `cargo test`.

## Status

| Spec | Version | Status |
| --- | --- | --- |
| [Mandate](mandate.md) | v0 | Draft |
| [Perpetual](instruments/perp.md) | v0 | Draft |
| [Spot](instruments/spot.md) | v0 | Draft |
| [Money market](instruments/money-market.md) | v0 | Draft |
| [Phoenix](venues/phoenix.md) | v0 | Draft |
| [OpenBook](venues/openbook.md) | v0 | Draft |
| [Kamino](venues/kamino.md) | v0 | Draft |

v0 is a draft. It specifies perpetuals, spot and money markets, on one venue
or several. The reference implementation is `mtx_core::mandate`, which
bots read and the grader audits with; it implements the part the
benchmark's grader enforces today: perpetual and spot instruments, on one
venue or several, with position bounds, sides, limit prices and a drawdown
limit. It rejects the rest; see the mandate's
[implementation status](mandate.md#implementation-status).

These specs live in this repository until a second venue or a consumer
outside the benchmark needs them.
