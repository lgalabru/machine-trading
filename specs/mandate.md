# Mandate v0.3.0

Status: draft

## Abstract

A mandate is a JSON document that states what an agent may trade on a
principal's behalf, and the limits it must stay within. It is written before
the agent acts, handed to the agent, and checked by an enforcer that does not
trust the agent. A mandate constrains *what* the agent does, never *why*:
strategy, signals and sizing logic are out of scope.

The specification is versioned with [semantic versioning](https://semver.org)
(see [Versions](#versions) and the [Changelog](#changelog)). v0.1.0 covers
perpetuals, spot and money markets, on one venue or several, with position
bounds, sides, limit prices and a drawdown limit. v0.2.0 adds limits on
notional (a gross cap, net exposures that may start partway through the
term, and hedge bounds) and reduce-only taking. v0.3.0 adds atomic routes:
pools, wildcard allowances over a venue's pools, an inventory limit judged
after every transaction, flash borrows and a budget for transaction costs.
All three are drafts while the major version is 0. The reference
implementation reads a subset of them, and the benchmark enforces a smaller
one; see [Implementation status](#implementation-status).

## Conventions

The key words MUST, MUST NOT, REQUIRED, SHOULD, SHOULD NOT, MAY and OPTIONAL
are to be interpreted as described in BCP 14 (RFC 2119, RFC 8174) when, and
only when, they appear in all capitals.

## Terminology

- **Principal**: whose capital the agent trades.
- **Agent**: the program that trades, such as a bot an AI agent wrote.
- **Enforcer**: whatever checks the agent against the mandate. In the
  benchmark, the grader.
- **Account**: everything the agent trades from that holds the principal's
  capital: on each venue in the universe, the accounts its venue profile
  names, and the agent authority's wallet.
- **Instrument**: something an account can hold a position in, on one venue.
- **Universe**: the instruments a mandate allows.
- **Term**: the span the mandate covers.
- **Fill**: a trade the venue records against the account, as a taker or as
  a maker. A **buy** fill increases the account's position in the
  instrument; a **sell** fill decreases it.
- **Transaction**: one the account's authority signs and sends. It
  **lands** when a block includes it and it succeeds, and **fails** when a
  block includes it and it errors; a failed one changes nothing but its
  fees. A **route** is a transaction that trades through several
  instruments and ends holding what it started with.

## The document

A mandate is a JSON object with these members:

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `version` | string | yes | `"v0.1.0"`, `"v0.2.0"` or `"v0.3.0"`. See [Versions](#versions). |
| `term` | object | no | When the mandate ends. See [Term](#term). |
| `universe` | array of allowances | yes | The instruments the account may hold, each with its own limits. MUST NOT be empty. |
| `limits` | object | yes | Limits on the account as a whole. See [Limits](#limits). |

An **allowance** is an object:

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `instrument` | string | yes | An [instrument reference](#instrument-references), or, from v0.3.0, a [wildcard](#wildcard-allowances). An instrument or a wildcard MUST NOT appear in two allowances. |
| `assets` | array of strings | no | v0.3.0, wildcards only. See [Wildcard allowances](#wildcard-allowances). |
| `max_position` | number | yes, except in a wildcard | The largest position, in the instrument's base units. With `min_position` absent, it bounds a short too: the position may go down to `−max_position`. MUST NOT be negative. A wildcard MUST NOT set it. |
| `min_position` | number | no | The smallest position allowed, in the same units. Absent means `−max_position`. MUST NOT be above `max_position`. `0` forbids a negative position: no shorting a perp, no borrowing from a money market. |
| `sides` | array of strings | no | The sides the agent may trade: `"buy"`, `"sell"`, or both. Absent means both. MUST NOT be empty or repeat a side. Not for `money-market` instruments. |
| `min_sell_price` | number | no | The lowest price at which the account may sell, in the instrument's quote currency per base unit. MUST be positive. Not for `money-market` instruments. |
| `max_buy_price` | number | no | The highest price at which the account may buy, in the same units. MUST be positive. Not for `money-market` instruments. |
| `hedge` | object | no | v0.2.0. The position may be held only against another instrument's. See [Hedges](#hedges). |
| `taking` | string | no | v0.2.0. `"reduce_only"`: a fill in which the account is the taker may only bring the position toward zero, never open it, add to it or take it past zero. Positions are then opened only by orders that rest and fill as a maker. Absent, taking is unrestricted. Not for `money-market` instruments. |
| `max_flash_borrow` | number | no | v0.3.0, `money-market` instruments only. The largest [flash borrow](#flash-borrows) from the reserve in one transaction, in its base units. MUST be positive. Absent, the account MUST NOT flash borrow from it. |

A reader MUST reject a mandate with a member it does not know, at any level.
A limit a reader doesn't know is one it can't enforce, and a mandate read as
looser than written is worse than one refused.

A reader MUST reject a `version` it does not implement.

### Versions

A version is `v<major>.<minor>.<patch>`, numbered with semantic
versioning:

- A **patch** changes no member a reader checks: wording, examples, or a
  clarification that every reader already followed.
- A **minor** version adds members, or values of a member, that an older
  reader would refuse. While the major version is 0, it may also change
  what a member means; the [Changelog](#changelog) says when it does.
- A **major** version, from 1.0.0, changes or removes what an older version
  states.

Each minor version adds members to the one before, and changes nothing it
states: v0.2.0 adds to v0.1.0, and v0.3.0 to v0.2.0. A reader of a version
reads every older one. A document that uses a member marked with a newer
version than its own MUST be rejected: a `"v0.1.0"` document with a v0.2.0
member, and a `"v0.1.0"` or `"v0.2.0"` document with a v0.3.0 member (the
`amm` or `stake-pool` kind, a wildcard, `assets`, `max_flash_borrow`,
`limits.inventory` or `limits.max_costs_usd`). A reader of the older
version would not know the member, and would refuse it. A writer SHOULD
write the lowest version a document needs, so a mandate reads the same to
every reader that can read it.

Before semantic versioning, the two versions were written `"v0"` (now
`"v0.1.0"`) and `"v0.1"` (now `"v0.2.0"`). A reader SHOULD read those
spellings as the versions they name, so documents written before the
change still load; a writer MUST write the new form.

### Instrument references

An instrument is referenced as `<venue>:<kind>:<symbol>`, for example
`phoenix:perp:SPCX` or `openbook:spot:SOL-USDC`:

- `venue` names a [venue profile](venues/), such as `phoenix` or `openbook`.
- `kind` names an [instrument profile](instruments/). This specification
  defines `perp`, `spot` and `money-market`, and from v0.3.0 `amm`
  ([pools](instruments/amm.md)) and `stake-pool`
  ([stake pools](instruments/stake-pool.md)). `future` and `option` are
  reserved for later, and a reader MUST reject them.
- `symbol` is the market's identifier on the venue, as the venue profile
  defines it. An `amm` or `stake-pool` instrument's symbol is the pool's
  account address, in base58: a pair has many pools on one venue, one per
  fee tier or curve. `orca:amm:Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE`
  is Whirlpool's SOL/USDC pool with 4 bp fees.

None of the three may be empty or contain `:`. A symbol of `*` makes the
reference a [wildcard](#wildcard-allowances), which only an allowance may
use: a hedge's `against` and an exposure's weights name instruments.

The universe MAY span several venues. Each venue in it contributes the
accounts its profile names to the account.

### Wildcard allowances

v0.3.0. A route passes through pools nobody can list in advance, and the
liquidity in a pair moves between them. A **wildcard** allowance, whose
symbol is `*`, such as `orca:amm:*`, allows every pool of its venue:

- Its kind MUST be `amm`. Other kinds are reserved for later, and a reader
  MUST reject a wildcard over them.
- It MAY carry `assets`: an array of mints, in base58. It then matches only
  the pools whose two assets are both in the array. `assets` MUST list at
  least two mints, since a pool has two and one alone would match nothing,
  and MUST NOT repeat one. Without `assets`, it matches every pool of its
  venue.
- It MUST NOT set `max_position`, `min_position`, `hedge` or `taking`: it
  allows fills, and [`limits.inventory`](#inventory) bounds what they
  leave. It MAY set `sides`, `min_sell_price` and `max_buy_price`, which
  then hold for each pool it matches.
- An allowance that names an instrument wins over a wildcard that matches
  it: the named allowance's members, and only those, hold for it.
- A named allowance MUST NOT carry `assets`.

The enforcer reads a pool's two assets from the pool itself, not from
anything the agent says.

### Term

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `from_slot` | integer | no | v0.2.0. The term's first slot. Limits that start partway through the term count from it. MUST NOT be after `until_slot`. |
| `until_slot` | integer | yes | The last slot the mandate covers. |

A mandate without a term is a template: it describes the rules, but not yet
when they apply. An enforcer that hands a mandate to an agent SHOULD set the
term first, so the agent reads everything it is held to from one document.
An enforcer that sets the term of a v0.2.0 mandate SHOULD set both ends.

### Limits

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `max_drawdown_usd` | number | no | The most, in USD, the account's value may fall below its running peak during the term. MUST be positive. |
| `min_health` | number | no | The lowest health any of the account's borrowing positions may reach, as the venue profile defines health. MUST be at least 1, the point at which the venue may liquidate. |
| `max_gross_notional_usd` | number | no | v0.2.0. The most [notional](#notional), in USD, the account may hold across the universe, long and short alike: the sum of every position's absolute notional. MUST be positive. |
| `exposures` | array of exposures | no | v0.2.0. Bounds on weighted sums of notionals. See [Exposures](#exposures). |
| `inventory` | object | no | v0.3.0. What the account may hold other than its numeraire after each transaction. See [Inventory](#inventory). |
| `max_costs_usd` | number | no | v0.3.0. The most, in USD, the account may spend over the term on transaction fees, priority fees and tips, for landed and failed transactions alike. MUST be positive. |

A mandate MAY have no limits, `"limits": {}`, when its allowances bound
everything the principal cares about. A mandate to sell a holding is an
example: the holding's value falls with the market whatever the agent does,
so a drawdown limit would measure the market, not the agent.

### Inventory

<a id="proposed-v030-atomic-routes"></a>

v0.3.0. A route must end the transaction holding what it started with, plus
a profit. The **inventory** limit states what it may leave behind. It is an
object:

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `numeraire` | string | yes | The mint the account keeps its value in, in base58. |
| `max_other_usd` | number | yes | The most, in USD, the account may hold of everything other than the numeraire after each transaction it lands. MUST NOT be negative: `0` asks it to end every transaction holding the numeraire alone. |

The value is measured as [Account value](#account-value) defines it, over
everything but the numeraire: tokens in the wallet, the base asset inside
pools, a money market's supply net of its borrows, a perpetual's
unrealized PnL. The SOL a wallet keeps for fees counts, unless SOL is the
numeraire.

### Flash borrows

v0.3.0. A **flash borrow** takes from a money market's reserve and repays
within the same transaction: the program refuses a transaction that
doesn't repay it. It is not a position, since it never outlives its
transaction, so it is bounded on its own, by `max_flash_borrow`, per
transaction and per reserve.

### Exposures

An **exposure** bounds a weighted sum of the account's notionals: a net
limit across instruments, a beta-neutral book, or a deadline to cut a
position by. It is an object:

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `name` | string | yes | Names the exposure in a breach. MUST NOT be empty, or the name of another exposure. |
| `weights` | object | yes | Each member's name is an [instrument reference](#instrument-references) in the universe, and its value a number: the instrument's weight. MUST NOT be empty, or name an instrument twice: a reader MUST reject a repeated member rather than keep one of its values. The exposure is the sum, over these instruments, of weight × [notional](#notional). |
| `max_abs_usd` | number | one of these two | The most the exposure may be, either way, in USD. MUST be positive. |
| `max_abs_fraction_of_start` | number | one of these two | The most the exposure may be, either way, as a fraction of its absolute value at the term's start. MUST be above 0 and at most 1. |
| `from_slots_after_start` | integer | no | The bound applies from this many slots after the term's `from_slot` to the term's end. Absent means 0. When the term is set and this is above 0, the term MUST have a `from_slot`. |

An exposure has exactly one of `max_abs_usd` and `max_abs_fraction_of_start`.

### Hedges

A **hedge** makes an allowance a pure hedge leg: a position the account may
hold only to offset another, never as a bet of its own. It is an object:

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `against` | string | yes | An [instrument reference](#instrument-references) in the universe, other than the allowance's own. Its allowance MUST NOT have a hedge itself. |
| `max_ratio` | number | yes | The most notional the hedge may hold, as a multiple of `against`'s absolute notional. MUST be positive. |

The hedge's notional must be zero or of the opposite sign to `against`'s,
and its absolute value at most `max_ratio` times `against`'s. With
`against` flat, the hedge must be flat.

## Rules

An agent breaks its mandate when, at any point during the term, any of
these holds:

1. **Universe.** The account holds a position, has an open order, or has a
   fill, in an instrument no allowance names and no wildcard matches. A
   route's fills leave no position between transactions, so a pool outside
   the universe may show up in fills alone.
2. **Position cap.** The account's position in an instrument is above that
   instrument's `max_position`, or below its `min_position` (`−max_position`
   when absent). Positions are measured as the instrument profile defines.
   Open orders do not count until they fill.
3. **Drawdown limit.** When the mandate sets `max_drawdown_usd`, the
   account's running peak value, from the start of the term, minus its
   current value exceeds it. Account value is measured as in
   [Account value](#account-value).
4. **Sides.** The account has a fill, or an open order, on a side its
   allowance does not list in `sides`.
5. **Limit prices.** The account has a sell fill below the instrument's
   `min_sell_price`, or a buy fill above its `max_buy_price`. Limits are
   judged per unit, at the worst price level a fill reached, before fees: the
   highest level of a buy, the lowest of a sell. A taker whose fill walks
   several price levels in one record is judged at its worst level, not its
   average, so an account can't buy through a cap or sell through a floor on
   the strength of a better average. The maker of such a record (a spline's
   owner, whose record names only the last level it reached, its best) is
   judged at the record's average price. A fill at one level is judged at
   its price.
   Resting orders at any price are allowed; only fills are checked.
6. **Health.** When the mandate sets `min_health`, the health of any of the
   account's borrowing positions, measured as the venue profile defines, is
   below it.
7. **Gross notional.** When the mandate sets `max_gross_notional_usd`, the
   account's gross notional over the instruments the universe names
   exceeds it. Pools matched only by a wildcard are bounded by the
   inventory limit instead.
8. **Exposure.** At or after an exposure's start, its absolute value exceeds
   its bound. A bound that is a fraction of the start is measured against
   the exposure's value when the term starts.
9. **Hedge.** A hedged position is on the same side as its `against`, or its
   absolute notional exceeds `max_ratio` times `against`'s.
10. **Taking.** The account has a fill as the taker, in an instrument whose
    allowance sets `taking` to `"reduce_only"`, that leaves its position
    further from zero than before the fill, or on the other side of zero.
    A fill as the maker is never a take. A take is judged against the
    position when it executes, not when the order was sent: an agent that
    can't be sure of that position can use the venue's own reduce-only
    order flag, which the venue applies at execution.
11. **Inventory.** v0.3.0. When the mandate sets `limits.inventory`, after a
    transaction of the account lands, it holds more than `max_other_usd` of
    assets other than the numeraire. A landed transaction after which the
    enforcer has no such measurement breaks it.
12. **Flash borrow.** v0.3.0. A transaction of the account flash borrows
    from a reserve whose allowance doesn't set `max_flash_borrow`, or from
    a reserve not in the universe, or borrows more than it. A borrow is
    judged whether its transaction landed or failed: the agent asked for
    it.
13. **Costs.** v0.3.0. When the mandate sets `limits.max_costs_usd`, the
    account's base fees, priority fees and tips over the term, summed over
    its landed and failed transactions, exceed it.

Rules 7 to 9 hold at any one measurement: each is checked against what the
account holds at that moment, at that moment's prices, not against ranges
over the term. As for rule 2, open orders do not count until they fill.

The rules are cumulative: a mandate allows only what every rule allows.

### Notional

An instrument's **notional** is the position, signed as its instrument
profile signs it, times its price in USD per base unit: negative for a short
or a borrow. The position is the account's net holding in the instrument,
across all of its sub-accounts on the venue. The enforcer MUST use one price
per instrument at each measurement, and state which. It SHOULD be a price
the agent's own trading cannot move, such as an index or an oracle, so that
an agent cannot loosen a notional limit by pushing a mark.

A measurement for rules 7 to 9 MUST list each instrument once, and MUST
list every instrument those rules read, flat ones included: rule 7 reads
every instrument the universe names, rule 8 an exposure's weights, rule 9 the hedge and its
`against`. A rule that reads an instrument the measurement does not list,
or lists without a finite position and a finite, positive price, is broken
at that measurement: a rule that cannot be checked is never read as kept.

### Account value

The account's value is everything it holds, each asset counted once, at the
prices the venue profiles define, plus the unrealized PnL of its perpetual
positions, as the perp profile defines, minus everything it owes, such as a
money market's borrows.

An asset two venue profiles both count, such as USDC in the authority's
wallet, MUST be counted once. When two profiles price the same asset
differently, the enforcer MUST use one price for it and state which.

## Enforcement

A mandate can be enforced at three layers:

- **Advisory.** The agent checks its own orders before sending them. This
  needs no trust in anything, but proves nothing.
- **Gate.** Something between the agent and the venue refuses transactions
  that would break the mandate. Examples are an RPC proxy, a transaction
  co-signer, or a program holding delegated authority over the account.
- **Audit.** The enforcer measures the account during and after the term
  and reports any rule it broke.

A conforming enforcer MUST audit every rule in this specification, and MAY
also gate. It MUST state how often it measures. An audit that samples can
miss a breach between two samples, and the venue profile says what is
measured. Rules 2, 3 and 7 to 9 are sampled. An enforcer that cannot
measure a rule a mandate sets SHOULD refuse the mandate rather than run it
unchecked.

The rules on fills (sides, limit prices and taking) are checked against
every fill the venue records during the term, not a sample. An enforcer
MUST read every fill, from the venue's own records, and for the taking rule
MUST tell the account's taker fills from its maker fills.

Rules 11 to 13 are judged on every transaction, not sampled. On-chain state
can only be read between transactions, so a token a route passes through
never shows up as a holding; measured after each transaction the account
lands, the account shows exactly what the route left behind. An enforcer of
a v0.3.0 mandate MUST read each of the account's transactions during the
term, landed or failed: its fees, priority fee and tips, the flash borrows
its instructions ask for, and, after a landed one, the account's holdings.

The enforcer MUST hold its own copy of the mandate, and MUST NOT read it
back from anywhere the agent can write. The copy handed to the agent is for
the agent to plan by.

What follows a breach is the enforcer's policy, not part of the mandate. A
benchmark grader disqualifies the session, or, for the limits a scenario
says it prices, stops the agent and flattens its positions at the market
(a stop-out), and scores what is left. A live deployment might halt the
agent or flatten its positions.

## Non-goals

A mandate does not describe a strategy. Signals, triggers, entry and exit
logic, and sizing are the agent's. Declarative strategy languages have
historically ended up too weak to express real strategies, or have grown
into programming languages. A mandate only bounds the outcome, so any
strategy can run under it.

A mandate bounds what the agent may do; it does not oblige it to do
anything. A duty, such as a market maker's quoting obligations (quotes on
both sides for a share of the term, within a maximum spread, at a minimum
size), is a term between the agent and a venue or whoever scores it, and is
measured and scored there.

A mandate holds limits, not prices for exceeding them. A rule that charges
for going over it, such as a participation cap that costs a fee per unit
above it, is a cost the principal accepts. It belongs in how a session is
scored, and a limit the agent may cross for a price is not one this
specification can state.

## Implementation status

The reference implementation, `mtx_core::mandate`, reads v0.1.0, v0.2.0
and v0.3.0, and the earlier spellings of the first two (`"v0"`, `"v0.1"`),
and writes the new form. It checks the part of this specification the
benchmark's tasks use or will use:

- `perp`, `spot`, `money-market` and `amm` instruments, on one venue or
  several, with `max_position`, `min_position`, `sides`,
  `min_sell_price`, `max_buy_price` and `max_drawdown_usd`;
- v0.2.0's `from_slot`, `max_gross_notional_usd`, `exposures`, `hedge`
  and `taking`;
- v0.3.0's wildcards over pools with `assets`, `limits.inventory`,
  `max_flash_borrow` and `limits.max_costs_usd`.

Its audit judges rules 1 to 5 and 7 to 13 from what an enforcer measured:
the priced measurements rules 7 to 9 read, and, for rules 11 to 13, each of
the account's transactions and each pool's two assets, which the enforcer
supplies. It rejects documents that use anything else: `min_health`, and
the `stake-pool` kind. So the example of a money-market mandate below,
which limits `min_health`, lives here rather than in
[`examples/`](examples/), which `cargo test` checks against it.

The benchmark's grader enforces less than the reader checks. Its scenarios
refuse a mandate that sets a v0.2.0 rule their session can't enforce. Of
v0.3.0, only lending and amm sessions enforce anything yet: they read back
each of the account's transactions and name their pools.

| Rule | Perp sessions | Spot sessions | Lending sessions | Amm sessions |
| --- | --- | --- | --- | --- |
| 1. Universe, 2. Position cap | every step | every step | every step | each fill's pool, from its vaults; a wildcard's pools have no cap |
| 1. Universe, through a wildcard | not enforced | not enforced | each pool, with its assets, when the session starts | each pool, with its assets, when the session starts, and every fill |
| 3. Drawdown limit | every step | every step | every step | every step |
| 4. Sides, 5. Limit prices | not checked | every fill | refused by the reader | every fill, at its average price |
| 7. Gross notional | every step | refused | refused | refused |
| 8. Exposure, in USD, over the session's one market | every step | refused | refused | refused |
| 8. Exposure across several instruments | refused | refused | refused | refused |
| 8. Exposure as a fraction of its start | refused | refused | refused | refused |
| 9. Hedge | refused | refused | refused | refused |
| 10. Taking | market making without a keeper; refused otherwise | refused | refused | refused |
| 11. Inventory | not enforced | not enforced | after every landed transaction, its wallet from its own token balances, its obligations at the step's measurement that follows it | after every landed transaction, from its own token balances |
| 12. Flash borrow, 13. Costs | not enforced | not enforced | every transaction that landed or failed | every transaction that landed or failed (an amm session lends nothing) |

Both read rule 11 from the transactions themselves, never from a step's
measurement alone: a bot that buys in one transaction and sells in the next,
within one step, held what it bought in between, and breaks the rule. The
wallet starts as the session found it; each of the account's transactions
that ran, in the order the chain ran them, sets every token account of the
account's it lists to its balance after it, and the account's own lamports.
What the wallet then holds beside the numeraire is what the account held
right after that transaction: USDC at a dollar, other tokens at the
scenario's prices (a token the session can't value breaks the rule).

An amm session holds nothing else. A lending session adds its obligations'
net positions, read at the first step's measurement after the transaction
(or the session's last): a deposit and a withdrawal within one step move the
obligation unseen. It also reads each of the account's transactions back
from the chain once the session ends: its base fee, its priority fee (its
compute unit price times the units it asked for) and its tips to Jito's
accounts, valued at the scenario's SOL price then, and each flash borrow it
asked for. A bundle that never ran is no transaction of the account's.

A perp session prices its one market at the scenario's level (its book
level where the session values the account at the book), not at Phoenix's
mark. No session prices more than its own market at one measurement yet,
so none can enforce a hedge or an exposure across instruments. Every
session starts flat, so an exposure bounded by a fraction of its start
would be bounded at zero. Only a market-making perp session without a liquidation keeper tells
taker fills from maker fills, so only it may use `taking`; it also refuses,
before they are sent, a strategy component's orders it can tell would break
the rule (see `docs/scenario-format.md`). The grader's pre-trade gate on raw sends refuses a send that would
break a position cap or the drawdown limit, but does not check rules 7 to
9: a raw send that takes the account over a notional limit is let through,
and the session is disqualified by the audit.

## Changelog

Newest first. Every version reads every older one, as
[Versions](#versions) states.

### v0.3.0 (2026-11-21)

Atomic routes, on top of v0.2.0, which is unchanged. The reasons are in
[RFD 0005](../docs/rfd/0005-atomic-arbitrage-tasks.md), and the decision in
[ADR 0004](../docs/adr/0004-atomic-arbitrage-in-the-sheet.md). A route
passes through tokens nobody can list in advance, must end the transaction
holding what it started with, and may borrow for the length of one
transaction; v0.2.0 could state none of it:

- The `amm` and `stake-pool` kinds, a pool named by its address.
- Wildcard allowances over a venue's pools (`orca:amm:*`), with `assets`,
  the mints a matched pool's assets must be among. A named allowance wins
  over a wildcard.
- `limits.inventory`: what the account may hold other than its numeraire
  after each transaction it lands (rule 11).
- An allowance's `max_flash_borrow`: the largest flash borrow from a
  reserve in one transaction, none when absent (rule 12).
- `limits.max_costs_usd`: fees, priority fees and tips over the term
  (rule 13).
- Rule 1 names fills: a fill in an instrument outside the universe breaks
  it, as a position or an order there does. Without routes, a fill there
  left one; a route's need not.
- Rule 7 reads the instruments the universe names, not the pools a
  wildcard matches.

### 2026-11-20: semantic versioning

No member changed. The versions are numbered `v<major>.<minor>.<patch>`:
`"v0"` is now `"v0.1.0"`, and `"v0.1"` is now `"v0.2.0"`. Readers still
read the old spellings as the versions they name, and writers write the new
ones. The instrument and venue profiles, which had been `v0`, are v0.1.0.

### v0.2.0 (2026-11-16, written `"v0.1"` until 2026-11-20)

Limits on notional and on taking, on top of v0.1.0, which is unchanged:

- `limits.max_gross_notional_usd`: a cap on the sum of every position's
  absolute notional (rule 7).
- `limits.exposures`: bounds on weighted sums of notionals (rule 8).
- `term.from_slot`: the term's first slot, from which exposures count.
- An allowance's `hedge`: a position held only against another
  instrument's (rule 9).
- An allowance's `taking: "reduce_only"`: a taker fill may only bring the
  position toward zero (rule 10). The benchmark's `market-making-spcx`
  task states it from its 0.13.0 release.

### v0.1.0 (2026-10-29, written `"v0"` until 2026-11-20)

The first draft: a universe of instruments with position bounds, and a
drawdown limit, for perpetuals on Phoenix. It grew in place while it was
the only draft:

- 2026-11-06: spot instruments, on OpenBook and Phoenix, and a universe
  over several venues, with `sides`, `min_sell_price` and `max_buy_price`
  (briefly numbered v1 that day, then folded back into v0).
- 2026-11-06: money-market instruments, on Kamino, with `min_position` and
  `limits.min_health`.

## Changes in later versions

Each of these would change what a reader must enforce, so each will be a new
minor version:

- More limits: `max_leverage`, a participation cap (a share of a venue's
  traded volume), and order rate limits.
- More instrument kinds: dated `future` and `option`.
- More allowed actions per instrument, such as no borrowing.
- Limit prices relative to a reference, such as the price at the start of
  the term, rather than fixed in the document.
- A gate profile for on-chain enforcement through delegated authority.

## Examples

A v0.1.0 template, as the `flash-crash-spcx` task carries it:

```json
{
  "version": "v0.1.0",
  "universe": [
    { "instrument": "phoenix:perp:SPCX", "max_position": 70.0 }
  ],
  "limits": { "max_drawdown_usd": 1000.0 }
}
```

The copy a session hands its bot adds the term: `"term": { "until_slot": 412480 }`.
See [`examples/`](examples/).

A mandate to sell a treasury's 30,000 SOL on two venues, never below $94:

```json
{
  "version": "v0.1.0",
  "term": { "until_slot": 412600 },
  "universe": [
    { "instrument": "openbook:spot:SOL-USDC", "max_position": 30000, "sides": ["sell"], "min_sell_price": 94.0 },
    { "instrument": "phoenix:spot:SOL-USDC", "max_position": 30000, "sides": ["sell"], "min_sell_price": 94.0 }
  ],
  "limits": {}
}
```

Both allowances bound the same SOL: a spot position is the account's
holdings of the base asset, wherever it sits (see the
[spot profile](instruments/spot.md#position)). The agent may only sell, on
either venue, never below $94, and has no drawdown limit, since the
holding's value moves with SOL. See
[`examples/spot-sol-usdc-sale.json`](examples/spot-sol-usdc-sale.json).

A mandate to borrow USDC against SOL on Kamino, never shorting SOL and
never borrowing more than $50,000, with a cushion above liquidation:

```json
{
  "version": "v0.1.0",
  "universe": [
    { "instrument": "kamino:money-market:SOL", "max_position": 1000, "min_position": 0 },
    { "instrument": "kamino:money-market:USDC", "max_position": 50000 }
  ],
  "limits": { "min_health": 1.25 }
}
```

The SOL allowance's `min_position: 0` means the agent may supply SOL but
never borrow it; the USDC allowance lets the position go to −50,000, a
$50,000 borrow. `min_health: 1.25` keeps the borrow at least 25% away from
the point where Kamino may liquidate it.

A v0.2.0 book: up to 30 QQQ either way, hedged with SPY at up to 1.2 times
its notional, and with the net of the two cut to a quarter of where it
started 600 slots into the term:

```json
{
  "version": "v0.2.0",
  "universe": [
    { "instrument": "phoenix:perp:QQQ", "max_position": 30 },
    { "instrument": "phoenix:perp:SPY", "max_position": 30,
      "hedge": { "against": "phoenix:perp:QQQ", "max_ratio": 1.2 } }
  ],
  "limits": {
    "exposures": [
      { "name": "deadline", "weights": { "phoenix:perp:QQQ": 1.0, "phoenix:perp:SPY": 1.2 },
        "max_abs_fraction_of_start": 0.25, "from_slots_after_start": 600 }
    ]
  }
}
```

SPY may only be held on the other side from QQQ, and at most 1.2 times
QQQ's notional: long while QQQ is short, short while it is long, flat while
it is flat. From 600 slots after `term.from_slot`, QQQ's notional plus 1.2
times SPY's must stay within a quarter of what it was when the term began,
either way, at every measurement. See
[`examples/phoenix-qqq-spy-hedged.json`](examples/phoenix-qqq-spy-hedged.json).

A v0.3.0 backrun of SOL/USDC swaps: the mandate for `backrun-sol-usdc`.

```json
{
  "version": "v0.3.0",
  "universe": [
    { "instrument": "orca:amm:*", "assets": ["So11111111111111111111111111111111111111112", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"] },
    { "instrument": "raydium:amm:*", "assets": ["So11111111111111111111111111111111111111112", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"] }
  ],
  "limits": {
    "max_drawdown_usd": 200,
    "inventory": { "numeraire": "So11111111111111111111111111111111111111112", "max_other_usd": 5 },
    "max_costs_usd": 500
  }
}
```

The bot may swap SOL against USDC through any Whirlpool or Raydium pool,
must end every transaction holding at most $5 of anything but SOL, and may
spend $500 on fees and tips. A route through a pool with any other asset,
such as SOL/USDT, breaks rule 1. See
[`examples/backrun-sol-usdc.json`](examples/backrun-sol-usdc.json).

A v0.3.0 liquidator on Kamino that funds each liquidation with a flash
borrow, and sells the collateral it seizes in the same transaction:

```json
{
  "version": "v0.3.0",
  "universe": [
    { "instrument": "kamino:money-market:USDC", "max_position": 50000, "min_position": 0, "max_flash_borrow": 250000 },
    { "instrument": "kamino:money-market:SOL", "max_position": 1, "min_position": 0, "max_flash_borrow": 1500 },
    { "instrument": "orca:amm:*", "assets": ["So11111111111111111111111111111111111111112", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"] },
    { "instrument": "raydium:amm:*", "assets": ["So11111111111111111111111111111111111111112", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"] }
  ],
  "limits": {
    "max_drawdown_usd": 500,
    "inventory": { "numeraire": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "max_other_usd": 100 },
    "max_costs_usd": 300
  }
}
```

A liquidation repays a borrower's debt and seizes its collateral at a
bonus. Here the bot flash borrows up to 250,000 USDC or 1,500 SOL, repays a
debt with it, sells the collateral it seized through a SOL/USDC pool, and
repays the flash borrow, all in one transaction. `min_position: 0` on both
reserves forbids an ordinary borrow, which would outlive the transaction:
the bot's only leverage is the flash borrow. It may keep up to 50,000 USDC
supplied to Kamino between liquidations, and, since SOL's 1-unit cap and
the inventory limit both bind, next to no SOL.

The numeraire is USDC rather than SOL. The principal counts in dollars, and
on Kamino the liquidations worth having are mostly stablecoin debt against
SOL collateral: the profit lands in USDC, and the SOL seized must be sold
within the transaction anyway to repay a USDC flash borrow. A SOL
numeraire would make every such liquidation end holding its USDC profit,
an inventory breach, or pay a second swap to turn it into SOL. The $100
left for other assets covers the SOL the wallet keeps for fees. See
[`examples/liquidation-flash.json`](examples/liquidation-flash.json).
