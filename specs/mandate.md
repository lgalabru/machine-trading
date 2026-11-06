# Mandate v1

Status: draft

## Abstract

A mandate is a JSON document that states what an agent may trade on a
principal's behalf, and the limits it must stay within. It is written before
the agent acts, handed to the agent, and checked by an enforcer that does not
trust the agent. A mandate constrains *what* the agent does, never *why*:
strategy, signals and sizing logic are out of scope.

v1 extends v0 with spot instruments, a universe that can span several
venues, the sides an agent may trade, and limit prices. Every v0 mandate is a
valid v1 mandate with the same meaning. See [Changes from v0](#changes-from-v0).

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

## The document

A mandate is a JSON object with these members:

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `version` | string | yes | `"v1"` for this specification, or `"v0"`. |
| `term` | object | no | When the mandate ends. See [Term](#term). |
| `universe` | array of allowances | yes | The instruments the account may hold, each with its own limits. MUST NOT be empty. |
| `limits` | object | yes | Limits on the account as a whole. See [Limits](#limits). |

An **allowance** is an object:

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `instrument` | string | yes | An [instrument reference](#instrument-references). An instrument MUST NOT appear in two allowances. |
| `max_position` | number | yes | The largest position, long or short, in the instrument's base units. MUST be positive. |
| `sides` | array of strings | no | The sides the agent may trade: `"buy"`, `"sell"`, or both. Absent means both. MUST NOT be empty or repeat a side. |
| `min_sell_price` | number | no | The lowest price at which the account may sell, in the instrument's quote currency per base unit. MUST be positive. |
| `max_buy_price` | number | no | The highest price at which the account may buy, in the same units. MUST be positive. |

A reader MUST reject a mandate with a member it does not know, at any level.
A limit a reader doesn't know is one it can't enforce, and a mandate read as
looser than written is worse than one refused.

A reader MUST reject a `version` it does not implement. A v1 reader MUST
accept `"v0"` documents, which can contain none of the members v1 adds, and
read them as v1.

### Instrument references

An instrument is referenced as `<venue>:<kind>:<symbol>`, for example
`phoenix:perp:SPCX` or `openbook:spot:SOL-USDC`:

- `venue` names a [venue profile](venues/), such as `phoenix` or `openbook`.
- `kind` names an [instrument profile](instruments/). v1 defines `perp` and
  `spot`. `future`, `option` and `money-market` are reserved for later
  versions, and a v1 reader MUST reject them. A v0 document MUST NOT use
  `spot`.
- `symbol` is the market's identifier on the venue, as the venue profile
  defines it.

None of the three may be empty or contain `:`.

The universe MAY span several venues. Each venue in it contributes the
accounts its profile names to the account.

### Term

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `until_slot` | integer | yes | The last slot the mandate covers. |

A mandate without a term is a template: it describes the rules, but not yet
when they apply. An enforcer that hands a mandate to an agent SHOULD set the
term first, so the agent reads everything it is held to from one document.

### Limits

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `max_drawdown_usd` | number | in v0 | The most, in USD, the account's value may fall below its running peak during the term. MUST be positive. OPTIONAL in v1. |

A v1 mandate MAY have no limits, `"limits": {}`, when its allowances bound
everything the principal cares about. A mandate to sell a holding is an
example: the holding's value falls with the market whatever the agent does,
so a drawdown limit would measure the market, not the agent.

## Rules

An agent breaks its mandate when, at any point during the term, any of
these holds:

1. **Universe.** The account holds a position, or has an open order, in an
   instrument not in the universe.
2. **Position cap.** The absolute size of the account's position in an
   instrument exceeds that instrument's `max_position`. Positions are
   measured as the instrument profile defines. Open orders do not count until
   they fill.
3. **Drawdown limit.** When the mandate sets `max_drawdown_usd`, the
   account's running peak value, from the start of the term, minus its
   current value exceeds it. Account value is measured as in
   [Account value](#account-value).
4. **Sides.** The account has a fill, or an open order, on a side its
   allowance does not list in `sides`.
5. **Limit prices.** The account has a sell fill below the instrument's
   `min_sell_price`, or a buy fill above its `max_buy_price`. A fill's price
   is the price the venue records for it, before fees. Resting orders at any
   price are allowed; only fills are checked.

The rules are cumulative: a mandate allows only what every rule allows.

### Account value

The account's value is everything it holds, each asset counted once, at the
prices the venue profiles define, plus the unrealized PnL of its perpetual
positions, as the perp profile defines.

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
measured.

The rules on fills (sides and limit prices) are checked against every fill
the venue records during the term, not a sample. An enforcer MUST read every
fill, from the venue's own records.

The enforcer MUST hold its own copy of the mandate, and MUST NOT read it
back from anywhere the agent can write. The copy handed to the agent is for
the agent to plan by.

What follows a breach is the enforcer's policy, not part of the mandate. A
benchmark grader disqualifies the session. A live deployment might halt the
agent or flatten its positions.

## Non-goals

A mandate does not describe a strategy. Signals, triggers, entry and exit
logic, and sizing are the agent's. Declarative strategy languages have
historically ended up too weak to express real strategies, or have grown
into programming languages. A mandate only bounds the outcome, so any
strategy can run under it.

A mandate holds limits, not prices for exceeding them. A rule that charges
for going over it, such as a participation cap that costs a fee per unit
above it, is a cost the principal accepts. It belongs in how a session is
scored, and a limit the agent may cross for a price is not one this
specification can state.

## Changes from v0

- The `spot` instrument kind, defined by the [spot profile](instruments/spot.md).
- A universe that spans several venues, with the account's value added up
  across them, each asset counted once.
- `sides`, `min_sell_price` and `max_buy_price` on an allowance, and the
  rules that check them against every fill.
- `max_drawdown_usd` is optional.

## Changes in later versions

Each of these would be a new version, since a reader must reject what it
cannot enforce:

- More limits: `max_notional_usd`, `max_leverage`, a participation cap (a
  share of a venue's traded volume), and order rate limits.
- More instrument kinds: dated `future`, `option`, and `money-market`
  (supply and borrow).
- More allowed actions per instrument, such as reduce-only, or no borrowing.
- Limit prices relative to a reference, such as the price at the start of
  the term, rather than fixed in the document.
- A gate profile for on-chain enforcement through delegated authority.

## Examples

A v0 template, as the `phoenix-flash-crash` task carries it:

```json
{
  "version": "v0",
  "universe": [
    { "instrument": "phoenix:perp:SPCX", "max_position": 70.0 }
  ],
  "limits": { "max_drawdown_usd": 1000.0 }
}
```

The copy a session hands its bot adds the term: `"term": { "until_slot": 412480 }`.
See [`examples/`](examples/).

A v1 mandate to sell a treasury's 30,000 SOL on two venues, never below $94:

```json
{
  "version": "v1",
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
holding's value moves with SOL. v1 is not yet implemented by the reference
implementation, so this example lives here rather than in `examples/`.
