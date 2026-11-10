# RFD 0001: Traits split by trading mechanism, state typed by instrument

State: discussion
Authors: opus, for Ludo Galabru  ·  Started: 2026-11-10

## Problem

The specs now define three instrument kinds (`perp`, `spot`,
`money-market`) and three venues (Phoenix, OpenBook, Kamino). The code that
trades and measures them grew one venue at a time, and it shows:

- `luce`'s `ExecutionDriver` (`crates/luce/src/core/execution.rs`) trades
  order books: `submit_order`, `cancel_order`, `cancel_all`, `order_book`.
  Phoenix perps implement it. PR #19 implements it again for Phoenix v1 and
  OpenBook spot, which is right: their orders are the same as a perp's.
- What an account holds has no shared shape. Perps have a core `Position`
  (`crates/luce/src/core/position.rs`). PR #19 adds two `Holdings` structs,
  `phoenix_v1::Holdings` and `openbook_v2::Holdings`, for the same idea.
  Lending has its own `LendPosition` (`crates/luce/src/lending/mod.rs`).
- The grader measures the account with Phoenix code inline in
  `crates/core/src/session.rs`: Hawkeye's `view_margin` and
  `view_margin_for_asset`. `mtx_botkit::mandate::Observed` holds one
  instrument's peak position. A spot or Kamino task would need its own
  measuring code in the session, and the mandate audit would need to learn
  each.
- `mtx_botkit::run`'s `Market` is shaped for one perp: `position.size`,
  `cap`, `mark`.

The question this RFD asks: should instruments get their own traits, and if
not, where do their differences live?

## Sketch

Split traits by **how an instrument trades**, type **what an account holds**
by instrument, and add one trait for **what an enforcer measures**.

### 1. Trading: one trait per mechanism

| Mechanism | Instruments | Trait |
| --- | --- | --- |
| Order book | `perp`, `spot` | `ExecutionDriver`, as it is. `Instrument::kind` already says `Spot` or `Perpetual`. |
| Pool | `money-market` | `LendingProvider`, with `plan_borrow` and `plan_repay` beside `plan_deposit` and `plan_withdraw`. |

No `SpotDriver` or `PerpDriver`: their methods would be identical. The
difference between them is what a fill does to the account, which is state.

### 2. State: one core type per instrument kind

Each type is what the matching instrument profile in `specs/instruments/`
defines, in `luce`'s core, and every venue fills in the same one:

```rust
/// What the account holds in one instrument, as its profile defines it.
pub enum Holding {
    /// `specs/instruments/perp.md`: a signed position and its entry.
    Perp(Position),
    /// `specs/instruments/spot.md`: the base asset, wherever it sits.
    Spot(Holdings),
    /// `specs/instruments/money-market.md`: supplied and borrowed.
    MoneyMarket(LendingBalance),
}

pub struct Holdings {
    pub base: Asset,
    /// Wallet, free on the venue, and locked behind open orders, in base units.
    pub wallet: Decimal,
    pub free: Decimal,
    pub locked: Decimal,
}

pub struct LendingBalance {
    pub asset: Asset,
    pub supplied: Decimal,
    pub borrowed: Decimal,
}
```

`Holding::position()` is the mandate's position: the perp's size,
`wallet + free + locked` for spot, `supplied − borrowed` for a money market.
PR #19's two `Holdings` become this one type.

### 3. Measuring: one trait per venue profile

What an enforcer reads, whatever the venue trades. It is the list each
venue profile in `specs/venues/` already spells out:

```rust
pub trait AccountMeter: Send + Sync {
    /// The account's holding in each instrument it holds anything in.
    async fn holdings(&self) -> Result<Vec<(InstrumentRef, Holding)>, MeterError>;
    /// Instruments with open orders, for the universe rule.
    async fn open_orders(&self) -> Result<Vec<InstrumentRef>, MeterError>;
    /// Fills since `after`, for the sides and limit-price rules.
    async fn fills(&self, after: Slot) -> Result<Vec<RecordedFill>, MeterError>;
    /// The account's value, in USD, at the profile's reference prices.
    async fn value_usd(&self) -> Result<Decimal, MeterError>;
    /// Health of each borrowing position, where the venue has one.
    async fn health(&self) -> Result<Vec<(InstrumentRef, Decimal)>, MeterError> {
        Ok(vec![])
    }
}
```

The grader holds one meter per venue in the universe, sums value with each
asset counted once (`specs/mandate.md`, Account value), and feeds an
`Observed` that has a peak position per instrument instead of one. The
Phoenix code in `session.rs` becomes `PhoenixPerpMeter`; OpenBook,
Phoenix v1 and Kamino add meters and touch nothing else in the grader.

### 4. The run loop

One `Strategy` trait. `Market` keeps its prices and clock, and carries the
account as `holdings: Vec<(InstrumentRef, Holding)>` from the meter, with
the current perp fields as shortcuts for a one-perp task. The order sheet's
cap check reads `Holding::position()`.

### Order of work

1. Core types: `Holding`, `Holdings`, `LendingBalance`. PR #19 switches to
   them.
2. `AccountMeter`, and `PhoenixPerpMeter` from `session.rs`, with the
   current tasks' grades unchanged (the ladder and the calibrated practice
   sets check that).
3. `Observed` per instrument, and the mandate audit over it.
4. The first spot meter, with the spot grading session.

## Open questions

- **Does `fills` belong on the meter, or on the driver?** The driver sees
  its own fills; an enforcer must not trust the bot's process and reads the
  venue's records (`FillLog`s and the event heap on OpenBook, `Fill` events
  on Phoenix v1). We'd find out by writing the OpenBook meter: if it shares
  nothing with the driver's fill parsing, keep them apart.
- **Pending maker fills.** On OpenBook a maker's position lags its fills
  until `consume_events`. Does `holdings` report the settled position, and
  `fills` the truth? The spot profile says the enforcer counts fills; a
  spike on Surfpool would show how far they drift in a session.
- **One meter per venue, or per instrument?** Phoenix runs perps and spot as
  separate programs with separate accounts. Two meters, `PhoenixPerpMeter`
  and `PhoenixSpotMeter`, are simpler; one is closer to the venue profile.
- **Is `Holding` an enum or a trait?** An enum keeps the mandate audit a
  `match`; a trait lets a new kind land without touching core. Three kinds,
  all specified, argue for the enum.

## Risks

- **Moving `session.rs`'s measuring code can change grades.** Phoenix
  perps' value and position must come out bit for bit the same, or every
  published result stops comparing. The calibrated practice sets and the
  ladder are the check, and the refactor waits for a quiet bench.
- **Designing for venues we haven't run.** Only Phoenix perps are graded
  today. A meter shaped before the first spot session may be wrong; step 4
  is where it gets tested, and the trait should change then rather than be
  frozen now.
- **PR #19 lands first.** If it merges with its own `Holdings`, the core
  type becomes a migration instead of a starting point.

## What we learned

- 2026-11-10: started from PR #19's review. Its drivers fit
  `ExecutionDriver` unchanged; its holdings didn't fit anything.
- 2026-11-10: step 1 landed, agreed with #19's author first. `luce::core`
  has `Holding`, `Holdings` and `LendingBalance` (`core/holding.rs`), and
  the `AccountMeter` trait with `RecordedFill` and `MeterError`
  (`core/meter.rs`). Health is a `Decimal`, like every other amount in
  `luce`. `LendingProvider` gained `plan_borrow` and `plan_repay`,
  defaulting to `Unsupported`, and `LendPosition::balance()` turns a
  deposit into a `LendingBalance`. Nothing uses them yet: the grader still
  measures in `session.rs`, which is step 2.
