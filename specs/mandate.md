# Mandate v0

Status: draft

## Abstract

A mandate is a JSON document that states what an agent may trade on a
principal's behalf, and the limits it must stay within. It is written before
the agent acts, handed to the agent, and checked by an enforcer that does not
trust the agent. A mandate constrains *what* the agent does, never *why*:
strategy, signals and sizing logic are out of scope.

v0 is a draft, and grows as instruments are added: perpetuals, spot and
money markets so far, on one venue or several. The reference implementation
reads a subset of it; see [Implementation status](#implementation-status).

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
| `version` | string | yes | `"v0"` for this specification. |
| `term` | object | no | When the mandate ends. See [Term](#term). |
| `universe` | array of allowances | yes | The instruments the account may hold, each with its own limits. MUST NOT be empty. |
| `limits` | object | yes | Limits on the account as a whole. See [Limits](#limits). |

An **allowance** is an object:

| Member | Type | Required | Meaning |
| --- | --- | --- | --- |
| `instrument` | string | yes | An [instrument reference](#instrument-references). An instrument MUST NOT appear in two allowances. |
| `max_position` | number | yes | The largest position, in the instrument's base units. With `min_position` absent, it bounds a short too: the position may go down to `−max_position`. MUST NOT be negative. |
| `min_position` | number | no | The smallest position allowed, in the same units. Absent means `−max_position`. MUST NOT be above `max_position`. `0` forbids a negative position: no shorting a perp, no borrowing from a money market. |
| `sides` | array of strings | no | The sides the agent may trade: `"buy"`, `"sell"`, or both. Absent means both. MUST NOT be empty or repeat a side. Not for `money-market` instruments. |
| `min_sell_price` | number | no | The lowest price at which the account may sell, in the instrument's quote currency per base unit. MUST be positive. Not for `money-market` instruments. |
| `max_buy_price` | number | no | The highest price at which the account may buy, in the same units. MUST be positive. Not for `money-market` instruments. |

A reader MUST reject a mandate with a member it does not know, at any level.
A limit a reader doesn't know is one it can't enforce, and a mandate read as
looser than written is worse than one refused.

A reader MUST reject a `version` it does not implement.

### Instrument references

An instrument is referenced as `<venue>:<kind>:<symbol>`, for example
`phoenix:perp:SPCX` or `openbook:spot:SOL-USDC`:

- `venue` names a [venue profile](venues/), such as `phoenix` or `openbook`.
- `kind` names an [instrument profile](instruments/). This specification
  defines `perp`, `spot` and `money-market`, and proposes `amm` (a pool's
  liquidity, a draft for review) and `stake-pool`. `future` and `option` are
  reserved for later, and a reader MUST reject them.
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
| `max_drawdown_usd` | number | no | The most, in USD, the account's value may fall below its running peak during the term. MUST be positive. |
| `min_health` | number | no | The lowest health any of the account's borrowing positions may reach, as the venue profile defines health. MUST be at least 1, the point at which the venue may liquidate. |

A mandate MAY have no limits, `"limits": {}`, when its allowances bound
everything the principal cares about. A mandate to sell a holding is an
example: the holding's value falls with the market whatever the agent does,
so a drawdown limit would measure the market, not the agent.

## Rules

An agent breaks its mandate when, at any point during the term, any of
these holds:

1. **Universe.** The account holds a position, or has an open order, in an
   instrument not in the universe.
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
   `min_sell_price`, or a buy fill above its `max_buy_price`. A fill's price
   is the price the venue records for it, before fees. Resting orders at any
   price are allowed; only fills are checked.
6. **Health.** When the mandate sets `min_health`, the health of any of the
   account's borrowing positions, measured as the venue profile defines, is
   below it.

The rules are cumulative: a mandate allows only what every rule allows.

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

## Implementation status

The reference implementation, `mtx_core::mandate`, reads and enforces the
part of this specification the benchmark's tasks use today: `perp`,
`spot`, `money-market` and (proposed) `amm` and `stake-pool` instruments, on
one venue or several, `max_position`, `min_position`, `sides`,
`min_sell_price`, `max_buy_price`, and `max_drawdown_usd`. It rejects
documents that use anything else, so the example of a money-market mandate
below, which limits `min_health`, lives here rather than in
[`examples/`](examples/), which `cargo test` checks against it.

## Changes in later versions

Each of these would change what a reader must enforce, so each will be a new
version once v0 is final:

- More limits: `max_notional_usd`, `max_leverage`, a participation cap (a
  share of a venue's traded volume), and order rate limits.
- More instrument kinds: dated `future` and `option`.
- More allowed actions per instrument, such as reduce-only, or no borrowing.
- Limit prices relative to a reference, such as the price at the start of
  the term, rather than fixed in the document.
- A gate profile for on-chain enforcement through delegated authority.

## Examples

A v0 template, as the `flash-crash-spcx` task carries it:

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

A mandate to sell a treasury's 30,000 SOL on two venues, never below $94:

```json
{
  "version": "v0",
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
  "version": "v0",
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
