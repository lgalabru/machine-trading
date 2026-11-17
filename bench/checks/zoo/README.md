# The strategy zoo

A task is only as hard as its best shortcut. Each card's ladder has blind rungs
written for it, and they miss shortcuts: an odd-lot bid under a maker's reaction
size, a seller who only takes good bids because the remainder is priced at the
close, a range break on the oracle, a size tell on the tape. The zoo is a shared
library of generic strong strategies that are **told nothing**. Every ladder
runs them (`just ladder`, and so `mtx gates` and Gate 0), and `mtx gates` flags
a task where one comes near the informed bot or matches the reference.

The rungs are one process bot on `mtx-kit`'s run loop (`src/`), chosen by
`MTX_ZOO`. Like the probes, the crate is outside the workspace and never ships
in a task image. `just zoo-check` formats, lints and tests it (CI runs it).

## How a rung fits a task

A rung reads only what any bot sees: the oracle and the book each step, and the
takes on the market (the session's tape where it reports one; on Phoenix's
perpetuals, the order book's transactions read off the chain, signer, side and
size, as the cards' references read them). It adapts to the task through its
mandate and order (`src/desk.rs`):

- **The job.** With an order (`MTX_ORDER`, read with the kit's `SessionOrder`)
  the job is to trade its quantity from the starting position. On a mandate
  that may only sell (buy), the long (short) the account starts with is the job,
  to zero. Otherwise there is no job.
- **The limits.** Every order stays on a side the mandate allows, inside the
  position range, short of the job's end, inside `max_buy_price` and
  `min_sell_price`, in whole lots and ticks (the venue's). On a reduce-only
  mandate an opening trade rests at the touch instead of taking.
- **The remainder.** With a job, each rung finishes what is left in the last 3%
  of the session. Where a broker takes the remainder (`MTX_BROKER_HAIRCUT_BPS`
  or `MTX_CLOSE_OUT`), it finishes only at a price better than the broker's.

## The rungs

| Group | Rung | With a job | Without one |
| --- | --- | --- | --- |
| Pacing | `twap` | evenly over the session, crossing | trades nothing |
| | `random-twap` | the same schedule, caught up at seeded random steps (a quarter of them) | trades nothing |
| | `dump` | everything at once | trades nothing |
| | `late` | nothing until 80% of the session, then evenly | trades nothing |
| | `front-load` | a schedule that falls with the square of the time left | trades nothing |
| | `wait-close` | everything in the last 3% | trades nothing |
| Passive | `rest-touch` | rests the remainder at the touch on its side | quotes both allowed sides at the touch |
| | `odd-lot` | rests clips of `MTX_ZOO_CLIP` (0.95 base units) at the touch: under a maker's reaction size | the same clips both sides |
| | `inside-maker` | rests one tick inside the touch | one tick inside, both sides |
| | `rest-mid` | rests at the mid | at the mid, both sides |
| Patience | `good-price` | takes only when the touch, after a 5 bp fee, beats the oracle (the broker's price where there is one) | buys under the oracle, sells over it |
| Price breaks | `range-break` | the oracle `MTX_ZOO_BREAK_BPS` (25 bp) from where the session started | takes the break's side |
| | `momentum` | a move over `MTX_ZOO_LOOK` (120 slots) of two standard deviations of the path's own noise | takes the move's side |
| | `fade` | the same move, read the other way | against the move |
| Flow | `follow-flow`, `fade-flow` | the takes' net size over 60 slots, once it is three median takes | with (against) the net |
| | `most-active`, `biggest-trader` | the net of the wallet with the most takes (the most volume) | with that wallet |
| | `copy-everyone` | trades along with every take on the job's side, at its size | holds the takes' net over the hold |
| | `size-follow`, `size-fade` | a take of three median takes or more (the size tell) | with (against) the big take |

With a job, a signal is read as where the price goes next: a rung that expects
the price to move against the job (up for a buyer, down for a seller) works the
rest at once; otherwise it waits as `late` does. Without a job, the price and
flow rungs hold `MTX_ZOO_SIZE` (0.5) of the cap the way the signal points, for
`MTX_ZOO_HOLD` (300) slots past the last signal; the passive rungs quote a
quarter of that size a side and flatten in the last 3%. `MTX_ZOO_SALT` changes
`random-twap`'s draws.

## The checks

The ladder adds a rung `zoo-<name>` per zoo rung (`may_breach`: a zoo rung is
compared on the task's grade, so a breach costs it as it would cost an agent),
and `mtx gates` reads three checks from its summary:

- **`zoo-below-reference`**: every zoo rung scores under the reference, as the
  task grades them (v2 session values on a v2 task), held as the order rules are:
  on every set, and in at least `min_rule_odds` of a bootstrap over each set's
  sessions.
- **`zoo-far-from-informed`**: every zoo rung under half the informed rung (the
  signal gate's 2x bar), the same way. The informed rung is `zoo.informed`, else
  the v2 informed anchor, else the best of the signal gate's informed rungs.
- **`anchor-strength`**: see below.

A rule broken in at least 95% of the bootstrap fails, in fast mode too. One that
doesn't hold as the order rules require, short of that confidence, fails the
full gate and is inconclusive in fast mode (as infra/36's rules).

### Anchor strength

The informed bot is v2's 1. If it leaves public edge on the table, a bot told
the same facts that also takes that edge beats it, and the top of the scale is
soft. The **hybrid** is such a bot, built without knowing the task
(`src/hybrid.rs`):

- every step, the informed bot and a zoo rung both see the market as it is;
- the informed bot goes first: whenever its sheet would trade or rest anything,
  or cancel orders it has resting, its sheet goes out alone, after cancelling
  what the zoo rung has resting;
- otherwise, unless the informed bot has orders resting, the zoo rung's sheet
  goes out.

The zoo rung is the set's best by graded total; it never sees `MTX_REGIME`.
The check compares the hybrid with the informed bot on the same sessions,
paired: it **fails** when a bootstrap over the sessions puts the hybrid's total
over every set above the informed bot's with 95% confidence. A higher total
short of that is noise (inconclusive, never a failure); a hybrid that did
exactly what the informed bot did in every session is `no_power` (the zoo rung
found no idle step), reported as inconclusive.

Why it is sound: the hybrid never looks ahead and the zoo rung reads only
public data, so it is a policy any bot told the informed bot's facts could run.
When it beats the informed bot past noise, a better told policy exists, whatever
the overlay's rule. The check can miss a weak anchor, never invent one: the zoo
rung may find no idle step, and an informed bot that trades toward a target
takes back the zoo rung's fills (which costs the hybrid). It wraps one informed
bot, so a task names it (`zoo.informed`) when its signal gate lists several.

## A task's `zoo` section

`checks/ladder.json` may carry a `zoo` section. Without one, every zoo rung
runs, the checks are enforced, and `anchor-strength` fails until the task names
its hybrid or says why it has none.

```json
"zoo": {
  "rungs": "all",
  "skip": { "twap": "the task hands no order: no job to pace" },
  "hybrid": "bench/tasks/<task>/checks/rungs/hybrid"
}
```

| Key | Meaning |
| --- | --- |
| `rungs` | `"all"` (the default) or a list of the zoo's rungs |
| `skip` | rungs left out, each with the reason it doesn't apply |
| `enforce` | `true` by default. `false` turns the three checks' failures into warnings and needs `why`: only for the four tasks that predate the zoo |
| `why` | why the zoo isn't enforced |
| `informed` | the informed rung the checks compare with, and the hybrid wraps |
| `hybrid` | the task's hybrid crate (below) |
| `hybrid_exempt` | why the task has no hybrid, instead of `hybrid` |

A rung name the zoo generates (`zoo-<name>`, `zoo-hybrid`) can't be used for
another rung.

### A task-specific rung

A shortcut only one task has belongs to that task, not the zoo. Add it as an
ordinary rung of the task's ladder file with `"zoo": true`, and the two zoo
rules hold it too:

```json
{ "name": "size-tell", "dir": "bench/tasks/<task>/checks/bots/naive/rungs/size-tell", "zoo": true, "may_breach": true }
```

To let the hybrid run it, give it an `arm` (the name the hybrid's `MTX_ZOO`
takes for it) and build it in the task's hybrid crate with
`mtx_zoo::hybrid_main_with(informed, |name| ...)`, which returns the task's own
`Strategy` for that name.

A rung that is generic (it reads only public data and adapts through the
mandate) belongs in the zoo: add a `Kind` in `src/lib.rs`, its name to
`ZOO` in `bench/checks/ladder/ladder.py` (a test keeps the two lists equal), a
unit test, and a row above.

### The hybrid crate

A task's hybrid is five lines around its informed bot, as its probe crate wraps
its reference: `checks/rungs/hybrid` with `Cargo.toml` (the informed bot's crate,
`mtx-kit` and `mtx-zoo` as path dependencies, `[workspace]` empty), `build.sh`,
`run.sh` and `.gitignore` copied from `checks/rungs/probe`, and `src/main.rs`:

```rust
fn main() -> mtx_kit::BotResult {
    mtx_zoo::hybrid_main(Box::new(my_informed_crate::Informed::default()))
}
```

`just ladder` builds it. The ladder hands it what the informed rung is told
(`tell_latent` or `tell_regime`) and the arm in `MTX_ZOO`.
