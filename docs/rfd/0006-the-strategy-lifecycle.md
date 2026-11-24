# RFD 0006: The strategy lifecycle: holds, not a state someone sets

State: discussion
Authors: Ludo Galabru, with Claude  ·  Started: 2026-11-24

## Problem

A strategy's life in the runtime was one enum, set by whoever acted last:

```rust
enum StrategyState { Running, Paused { why: String }, Stopped { why: String } }
```

Reading `crates/runtime/src/live/strategies.rs` on 2026-11-24 we found five
things wrong with it. The first three could lose money.

1. **A resume talked the enforcer out of a breach.** `resume` set any
   paused strategy to `Running`. One paused for a drawdown over its
   mandate ran again, sent a step of orders, and was paused again. A
   `resume` of every strategy did it to all of them, and undid the kill
   switch for strategies a person had paused for another reason.
2. **The kill switch stopped at the first failure.** `kill` paused the
   strategies one after another and returned on the first whose orders
   couldn't be cancelled. The ones after it kept running.
3. **A restart forgot the breach.** The desired state kept `run`, set from
   `state == Running`, and was written only after a command. A strategy
   the enforcer paused was still `run: true` on disk: after a plugin
   reload it ran again, its drawdown counted from zero.
4. **The cause was prose.** `why` was `"added"`, `"paused"`, `"the kill
   switch"`, `"the enforcer: …"` or a sentence about the feed. A sidecar
   could not branch on it. A later pause overwrote an earlier one's
   reason.
5. **Changes were not events.** Only the enforcer's pause and a trap were
   told. A sidecar learned the rest by asking `status`, and never when.

## Sketch

A strategy steps unless someone holds it or it can't. So we keep who holds
it, and derive the state.

```rust
enum Hold { Operator, Kill, Feed, Mandate }      // least grave first
enum StopCause { Start, Changed, Trap }

struct Lifecycle {
    holds: BTreeMap<Hold, String>,                // each with its reason
    stop: Option<(StopCause, String)>,
    since_ms: u64,
}

enum StrategyState {                              // what a sidecar reads
    Starting { why },
    Running,
    Paused { holds: Vec<Hold>, why },             // why: the gravest hold's
    Stopped { cause: StopCause, why },
}
```

`crates/core/src/lifecycle.rs` holds the rules, pure, with a test over
every set of holds. The state is never set: `Lifecycle::state()` computes
it, so it cannot say `running` while a hold stands.

| Hold | Set by | Lifted by |
| --- | --- | --- |
| `operator` | `pause`, an `add` without `run` | `resume` |
| `kill` | `kill`, on every strategy | `resume` |
| `feed` | the enforcer, when the feed is behind or dropped events | `resume`, if the feed is fresh |
| `mandate` | the enforcer, on a breach | `resume`, if what was observed no longer breaks the mandate; a new `mandate` |

- A hold keeps its first reason. Lifting one never forgets another.
- A `resume` that leaves a strategy held is refused with `held` and says
  why. What it could lift, it lifted.
- A drawdown never un-happens, and `Observed` keeps the worst one. So a
  breach is in practice lifted by a new mandate, which audits from there
  on. That is what we want: a person decides the new limit.
- `kill` holds every strategy first, with no await between them, then
  cancels orders. A command to every strategy reaches them all and names
  the ones it failed on.
- `Stopped` wins over held in what is told, and the holds wait under it:
  a killed strategy that is upgraded comes back paused.
- The desired state keeps the holds (`Stored.holds`), all but `feed`. A
  step that changes them sets a flag, and the loop writes the state at the
  next slot. `spec.run` is still written, for an older runtime.
- Every change is a `state` event with the state and `since_ms`. `paused`
  and `stopped` are still sent.
- `Starting` is a strategy the desired state holds while the node catches
  up after a restart. It used to be told as `Stopped`.

The protocol is 3: an old reader doesn't know `starting`.

### What the runtime does not model

Where a strategy is in its career (an idea, a candidate, graded, scouted
on paper, staged in shadow, live, retired) is the harness's to know: most
of those stages happen before a runtime has heard of it. The runtime
supplies two facts to it: the state above, and whether it sends orders
(`shadow`). luthor's `status.lifecycle` puts them together.

## Open questions

- **Should a fresh feed lift its own hold?** Today a person resumes. A
  feed 40 slots behind for two seconds pauses every strategy until someone
  looks. We would find out by counting feed pauses on mtx-alpha and how
  long each lasted before the feed was fresh. If they are short and
  frequent, the enforcer should lift what it set, after N fresh slots.
- **Mode as a transition.** Shadow to live is `remove` then `add`: the
  strategy's steps, stats and observed drawdown start over. A `promote`
  command would keep them. It needs the bot's chain swapped under it
  (`ShadowChain` for the real one), which `start_bot` decides once.
- **An add under way** is in no status: its name is taken and nothing
  says so. It could be told as `Starting`.
- **Who acted.** The `state` event says what changed, not which connection
  asked. With two sidecars that matters.

## Risks

- A sidecar that resumed blindly after an enforcer pause now gets `held`.
  luthor removes a breached strategy and never resumes one, so it is
  unaffected. A script that did will stop working, on purpose.
- The holds are written by a flag checked each slot. A crash in the 400 ms
  between a breach and the write loses the hold. The drawdown itself is
  not kept across restarts either, which is the larger hole: see below.
- `Observed` is not persisted. After a restart a strategy held for its
  mandate stays held (the hold is kept), but one that was near its limit
  starts counting from zero.

## What we learned

- 2026-11-24. Implemented as above. The exhaustive test (16 hold sets,
  stopped or not, 4 cures) found nothing the rules didn't say. Building
  the manager-level tests needed a `Strategy` with no component, which the
  struct allowed: a strategy that didn't start is already one.
