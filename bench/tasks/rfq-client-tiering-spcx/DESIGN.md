# Design record: rfq-client-tiering-spcx

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it prices each client to its reach and declines the clients whose requests turn out to be informed, learning who they are from a handful of requests per client.

- **Wins:** quoting benign clients just inside their reach, flagging informed clients early from markouts and from who takes a quote no uninformed client would, and reading the flagged clients' sides as the coming move
- **Loses:** quoting everyone pays every informed fill; quoting wider loses the benign flow entirely past the reach; tiering by size or leaning against recent request flow catches little of the informed flow
- **Regimes:** 10 to 16 clients, 15% to 45% of them informed (which ones redrawn every session), seeing 30 to 90 slots ahead; reach 0.2 to 0.5 bp per trade size, times the square root of the size
- **Mandate:** 0.001 SPCX position cap, $400 loss limit, 72 hidden sessions per grade.

## Expert research

<!-- TODO: why this is a real expert task, the expert's method, where non-experts go wrong, with sources. -->

## The central decision

<!-- TODO: the one decision the task is about, and the options that look defensible. -->

## Variants

<!-- TODO: each regime and the best response to it; a table of which habit fails where. -->

## Levers

<!-- TODO: what the market does, what a good bot does, and how an honest operator detects each lever. -->

## No tells

<!-- TODO: the audit: nothing in the path's shape gives away what a trader couldn't see. -->

## Detectability

<!-- TODO: how early a good bot can tell the regime, measured on generated sets. -->

## Fairness

<!-- TODO: what the agent is told, what it can learn from practice, and that the reference stays inside the mandate. -->

## Failure modes, priced

<!-- TODO: each way a bot can lose, and what it costs. -->

## Predictions

<!-- TODO: what you expect each model tier to score, before running them. -->

## Generations

<!-- TODO: what each version learned. -->
