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
| `venues` | Exactly one of `perp` (with its `funding`) and `spot` (`markets`, `turn_data`) |
| `market_model` | `path`, `fair_path`, a `closed_period` with Phoenix's index through it, and the `record` of the market calibration the path was drawn from |
| `oracles` | Feeds for the prices venues read (Phoenix, Pyth, Scope), their cadence, and seeded failures |
| `actors` | The `competitor` maker, the grader's `takers` and what they are priced off (`takers_pricing`: `level`, `book` or `fair`; without it the scenario's kind picks, and `fair` needs the market model's `fair_path`) |
| `faults` | Execution faults on the bot's sends and reads |
| `tx` | The transaction layer: `raw_sends`, the programs a component's own transactions may run |
| `mandate` | What the bot may trade and its limits ([mandate spec](../specs/mandate.md)) |
| `order` | A quantity the bot has to trade by the end, and the broker's `haircut_bps` on what it leaves; on a perp, its `instrument` (the scenario's market when absent) |
| `scoring` | `reference_pnl_usd`, `bound_usd`, `participation`, `net`, `bound_on`; on a perp, `close_out`: by market symbol, the `haircut_bps` a broker takes the residual position at. `net` on a perp scores the PnL over holding `assets.start_positions` (the fair path's move where there is one, else the level's), so a bot that holds scores 0; it takes no `order` or `venue_position`, and no funding with a start position. `bound_on: "fair_path"` values the hindsight bound on `market_model.fair_path` |

Optional sections are left out when empty. Unknown fields are errors in
every section.

Each section checks only itself (`Assets::validate`, `Venues::validate`,
`MarketModel::validate`, `Actors::validate`, `Scoring::validate`, and the
leaf types' own). A few rules join sections and are listed, one function
each, at the end of `ScenarioV2::validate`: one venue kind, spot scenarios
having no path or competitor (their takers are a tape), the mandate covering exactly the scenario's markets,
a sell order fitting the wallet, and, on a perp (`positions_fit_the_market`):

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
