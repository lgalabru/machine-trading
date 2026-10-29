# Mandate v0

Status: draft

## Abstract

A mandate is a JSON document that states what an agent may trade on a
principal's behalf, and the limits it must stay within. It is written before
the agent acts, handed to the agent, and checked by an enforcer that does not
trust the agent. A mandate constrains *what* the agent does, never *why*:
strategy, signals and sizing logic are out of scope.

## Conventions

The key words MUST, MUST NOT, REQUIRED, SHOULD, SHOULD NOT, MAY and OPTIONAL
are to be interpreted as described in BCP 14 (RFC 2119, RFC 8174) when, and
only when, they appear in all capitals.

## Terminology

- **Principal**: whose capital the agent trades.
- **Agent**: the program that trades, such as a bot an AI agent wrote.
- **Enforcer**: whatever checks the agent against the mandate. In the
  benchmark, the grader.
- **Account**: the venue account the agent trades from, holding the
  principal's capital.
- **Instrument**: something an account can hold a position in, on one venue.
- **Universe**: the instruments a mandate allows.
- **Term**: the span the mandate covers.

## The document

A mandate is a JSON object with these members:

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `version` | string | yes | `"v0"` for this specification. |
| `term` | object | no | When the mandate ends. See [Term](#term). |
| `universe` | array of allowances | yes | The instruments the account may hold, each with its own limits. MUST NOT be empty. |
| `limits` | object | yes | Limits on the account as a whole. See [Limits](#limits). |

An **allowance** is an object:

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `instrument` | string | yes | An [instrument reference](#instrument-references). An instrument MUST NOT appear in two allowances. |
| `max_position` | number | yes | The largest position, long or short, in the instrument's base units. MUST be positive. |

A reader MUST reject a mandate with a member it does not know, at any level.
A limit a reader doesn't know is one it can't enforce, and a mandate read as
looser than written is worse than one refused.

A reader MUST reject a `version` it does not implement.

### Instrument references

An instrument is referenced as `<venue>:<kind>:<symbol>`, for example
`phoenix:perp:SPCX`:

- `venue` names a [venue profile](venues/), such as `phoenix`.
- `kind` names an [instrument profile](instruments/). v0 defines `perp`.
  `spot`, `future`, `option` and `money-market` are reserved for later
  versions, and a v0 reader MUST reject them.
- `symbol` is the market's identifier on the venue, as the venue profile
  defines it.

None of the three may be empty or contain `:`.

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
| `max_drawdown_usd` | number | yes | The most, in USD, the account's value may fall below its running peak during the term. MUST be positive. |

## Rules

An agent breaks its mandate when, at any point during the term, any of
these holds:

1. **Universe.** The account holds a position, or has an open order, in an
   instrument not in the universe.
2. **Position cap.** The absolute size of the account's position in an
   instrument exceeds that instrument's `max_position`. Positions are
   measured as the instrument profile defines. Open orders do not count until
   they fill.
3. **Drawdown limit.** The account's running peak value, from the start of
   the term, minus its current value exceeds `max_drawdown_usd`. Account
   value is measured as the venue profile defines.

The rules are cumulative: a mandate allows only what every rule allows.

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

## Changes in later versions

Each of these would be a new version, since a v0 reader must reject what it
cannot enforce:

- More limits: `max_notional_usd`, `max_leverage`, and order rate limits.
- More instrument kinds: `spot`, dated `future`, `option`, and
  `money-market` (supply and borrow).
- Allowed actions per instrument, such as reduce-only or no borrowing.
- A universe that spans several venues, with value aggregated across them.
- A gate profile for on-chain enforcement through delegated authority.

## Example

A template, as a scenario carries it:

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
