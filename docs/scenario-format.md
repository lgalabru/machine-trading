# Scenario files

A scenario is the market a session replays and the rules it is scored by.
`mtx scenario generate` writes them, `mtx calibrate` adds the reference's
PnL, and `Scenario::load` reads them. The code is
`crates/grader/src/scenario_v2.rs`.

## Versions

A file says which version it is in `schema`. Without it, it is v1: the flat
layout every shipped file uses. `"schema": 1` is accepted and means the same.
Both load into the same in-memory scenario, and the two convert into each
other without loss. A file is parsed once, as the version it names, so
errors point into the file and a repeated key is an error.

`Scenario::save`, and with it `mtx scenario generate` and `mtx calibrate`,
still writes v1: the task images, CI and the practice files read that
layout. `Scenario::save_v2` writes v2. Moving what is written to v2 is a
separate change that migrates the practice files, CI, the images and these
docs together.

## v2 sections

| Section | Holds |
| --- | --- |
| `schema`, `name`, `description`, `duration_slots` | The version, labels and the session length |
| `assets` | `symbol`; a spot session's starting `wallet`; a perp session's `start_positions`, signed base units by market symbol |
| `venues` | Exactly one of `perp` (with its `funding`), `spot` (`markets`, `turn_data`) and `lending` (Kamino's reserves, the bot's wallet and opening, borrowers and liquidators; [Kamino](../specs/venues/kamino.md)) |
| `market_model` | `path`, `fair_path`, a `closed_period` with Phoenix's index through it, the `record` of the market calibration the path was drawn from, and a lending session's `warps` (clock jumps) |
| `oracles` | Feeds for the prices venues read (Phoenix, Pyth, Scope), their cadence, and seeded failures |
| `actors` | The `competitor` maker, the grader's `takers` and what they are priced off (`takers_pricing`: `level`, `book` or `fair`; without it the scenario's kind picks, and `fair` needs the market model's `fair_path`) |
| `faults` | Execution faults on the bot's sends and reads |
| `tx` | The transaction layer: `raw_sends`, the programs a component's own transactions may run |
| `mandate` | What the bot may trade and its limits ([mandate spec](../specs/mandate.md)) |
| `order` | A quantity the bot has to trade by the end, and the broker's `haircut_bps` on what it leaves; on a perp, its `instrument` (the scenario's market when absent) |
| `scoring` | `reference_pnl_usd`, `informed_pnl_usd`, `reference_max_drawdown_usd`, `informed_max_drawdown_usd`, `bound_usd`, `participation`, `net`, `stop_out`; on a perp, `close_out`: by market symbol, the `haircut_bps` a broker takes the residual position at |

`informed_pnl_usd` is what the task's informed bot makes on the scenario
(`mtx calibrate --informed`): the 1 of scoring v2 (`mtx grade --scoring
v2`), as `reference_pnl_usd` is its 0.5. Without it on every scenario,
scoring v2 falls back to the cost-aware hindsight bound (`ceiling:
"hindsight"`): perfect foresight on the reference session's marks, paying
the taker fee and half-spread on every trade. It needs the reference's
session (the grade's `--reference` run, or `details/reference/` on a
rescore), and is never computed on the bot's own marks.
`reference_max_drawdown_usd` and `informed_max_drawdown_usd`, written by
`mtx calibrate` beside the PnL, are the anchor of v2's near-breach charge:
the informed bot's drawdowns when it is the ceiling, else the reference's.
A grade that runs the reference (`--reference`) uses the drawdowns it
measures instead.

Optional sections are left out when empty. Unknown fields are errors in
every section.

Each section checks only itself (`Assets::validate`, `Venues::validate`,
`MarketModel::validate`, `Actors::validate`, `Scoring::validate`, and the
leaf types' own). A few rules join sections and are listed, one function
each, at the end of `ScenarioV2::validate`: one venue kind, spot scenarios
having no path or competitor (their takers are a tape), lending scenarios
moving prices through their Scope feeds only (no path, actors, order or spot
scoring; clock jumps inside the session and without faults), the mandate
covering exactly the scenario's markets, a sell order fitting the wallet, and,
on a perp (`positions_fit_the_market`):

- `start_positions` and `close_out` are perp-only and name the scenario's
  market; a start position is not 0 and lies inside the mandate's range for
  it;
- an order is in the scenario's market and ends inside the range; a
  reduce-only order is no bigger than the starting position it reduces (a
  reduce-only buy closes a short, so it needs one; on spot it is refused);
- the order's market has no `close_out`: the order's broker already takes
  what is left of it.

A perp order and close-outs price the close at the scenario's own level at
the last slot (the index, not Phoenix's mark, which quotes can push), and
the session values the account there too (`valuation` `oracle`). A start
position is a real Phoenix fill in setup, against a grader counterparty at
the start mark, before the start value is measured; a position Phoenix
won't margin fails the session's setup.

## The risk desk: `scoring.stop_out`

By default a session that breaks its mandate disqualifies the bot. A
perp scenario can price a breach instead:

```json
"scoring": { "stop_out": { "on": ["drawdown"], "close": "market", "slice_base": 100 } }
```

- `on`: the mandate limits whose breach the desk closes instead of
  disqualifying. Only `drawdown` so far; a later limit (a money market's
  `min_health`, say) joins the list.
- `close`: `market`, the only mode: the desk cancels the bot's orders and
  sends market orders against its position through the book, each at most
  `slice_base` base units (one order when absent) and filling no worse
  than 4% from the best price, signed with the bot's key.

The breach is detected on the step measurement the audit reads (peak value
minus value over `max_drawdown_usd`). The bot is stopped first and gets no
more turns; the desk sends at most 8 orders a step until the position is
flat, and the session ends on the step it is flat (or at its last slot).
The PnL is the account's value then, so the close pays the book's impact
and the taker fee. The report lists each close in `forced_closes` (reason,
step, the value before and after, the orders, whether it ended flat), and
the audit then reads the drawdown as at most the limit: every other rule
still disqualifies. Each close order records its transaction's
`signature`. The desk trades in the bot's account but is not the bot: in
a session that reads fills, the fills of those transactions are marked
`forced` in `perp_fills`, and the sides and price rules (and an order's
"traded the other way" and "grew the position") leave them out; they still
count toward the position and the PnL. Because the desk closes to flat, a
scenario with `stop_out` must allow flat in every instrument's range: a
`min_position` above 0 is refused. A component sees `MTX_STOP_OUT=1` in its session vars.
The code is `crates/grader/src/risk.rs` and the session loop.

## Example

```json
{
  "schema": 2,
  "name": "crash-1",
  "description": "SPCX drops 8% and rebounds",
  "duration_slots": 400,
  "assets": { "symbol": "SPCX" },
  "venues": { "perp": {} },
  "market_model": { "path": [{ "slot": 10, "level_pct": -8.0 }] },
  "mandate": { "...": "as in the mandate spec" },
  "scoring": { "reference_pnl_usd": 12.5 }
}
```

## Adding to the format

A new capability adds a section, or a field to the section it belongs to,
with its own `validate`. v1 never gains fields; the conversion in
`ScenarioV2::try_from` and `into_scenario` carries what the session needs
until the session reads v2 directly.
