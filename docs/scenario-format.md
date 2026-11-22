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
| `venues` | Exactly one of `perp` (with its `funding`, `liquidator` and `impact`), `spot` (`markets`, `turn_data`), `lending` (Kamino's reserves, the bot's wallet and opening, borrowers, liquidators and Scope's keeper; [Kamino](../specs/venues/kamino.md)) and `amm` (pools, the bot's wallet, a reference price, the whale whose chunks are the session's triggers, the calm flow and the searchers; see [An amm session](#an-amm-session-triggers-and-searchers)); and, beside `lending`, `amm`'s `pools` alone (see [Lending with pools and a pushed oracle](#lending-with-pools-and-a-pushed-oracle)) |
| `market_model` | `path`, `fair_path`, a `closed_period` with Phoenix's index through it, the `record` of the market calibration the path was drawn from, and a lending session's `warps` (clock jumps). `record.jump_rate_per_hour`, `record.jump_size` and `record.scheduled` (all optional, absent unless stated) state a jump rate, jump sizes and scheduled releases in place of the calibration's. Values must come from measured real event or opening-window data with the source cited, and no all-session rate may exceed the instrument's measured opening-window rate (reference table, Phoenix 1 m archives and the calibration's 5-sigma rule: [market calibration](market-calibration.md#stated-jump-regimes), realism rule: [market realism](../skills/trading-task-design/references/market-realism.md)) |
| `oracles` | Feeds for the prices venues read (Phoenix, Pyth, Scope), their cadence, and seeded failures; a lending session's Scope feed may be `pushed` |
| `actors` | The `competitor` maker (in a market-making scenario with the bot's quoting `obligations`, and `makers_bps`, where the snapshot's makers are widened to when not `competitor_bps`, at least that: a book whose depth near the touch is the grader's maker alone, the snapshot's deep curve behind it), the grader's `takers` and what they are priced off (`takers_pricing`: `level`, `book` or `fair`; without it the scenario's kind picks, and `fair` needs the market model's `fair_path`) |
| `faults` | Execution faults on the bot's sends and reads |
| `tx` | The transaction layer: `layer`, fees, busy blocks and Jito bundles deciding what lands (see `docs/tx-layer.md`); `raw_sends`, the programs a component's own transactions may run |
| `mandate` | What the bot may trade and its limits ([mandate spec](../specs/mandate.md)) |
| `order` | A quantity the bot has to trade by the end, and the broker's `haircut_bps` on what it leaves; on a perp, its `instrument` (the scenario's market when absent) |
| `scoring` | `reference_pnl_usd`, `informed_pnl_usd`, `reference_max_drawdown_usd`, `informed_max_drawdown_usd`, `bound_usd`, `participation`, `net`, `bound_on`, `stop_out`; on a perp, `close_out`: by market symbol, the `haircut_bps` a broker takes the residual position at. `net` on a perp scores the PnL over holding `assets.start_positions` (the fair path's move where there is one, else the level's), so a bot that holds scores 0; it takes no `order` or `venue_position`, and no funding with a start position; if the scenario has a `market_model.fair_path`, it must end at the level the path ends at (within 0.01%), or it is refused. `bound_on: "fair_path"` values the hindsight bound on `market_model.fair_path` |

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
scoring; clock jumps inside the session and without faults), amm scenarios
on their own wallet and path under the transaction layer, a lending
session's liquidators taking `delay_steps` without the transaction layer and
none with it, the mandate covering exactly the scenario's markets (a lending
session's: its reserves, and each of its pools, named or under its venue's
wildcard), a sell order fitting the wallet, and,
on a perp (`positions_fit_the_market`):

- `start_positions` and `close_out` are perp-only and name the scenario's
  market; a start position is not 0 and lies inside the mandate's range for
  it;
- an order is in the scenario's market and ends inside the range; a
  reduce-only order is no bigger than the starting position it reduces (a
  reduce-only buy closes a short, so it needs one; on spot it is refused);
- the order's market has no `close_out`: the order's broker already takes
  what is left of it.

A perp session can trade several markets of one cross-margin Phoenix
account (see [Several perp markets](#several-perp-markets)).

A perp order and close-outs price the close at the scenario's own level at
the last slot (the index, not Phoenix's mark, which quotes can push), and
the session values the account there too (`valuation` `oracle`). A start
position is a real Phoenix fill in setup, against a grader counterparty at
the start mark, before the start value is measured; a position Phoenix
won't margin fails the session's setup. Scoring v2 takes a start position only with
an order or `scoring.net`, which score the session against doing nothing
(for `net`, holding the start, so a bot that holds scores 0, v2's nop),
and only on a market without the scenario's `funding`: otherwise doing
nothing moves with the price, or earns or pays the funding, and v2 refuses
the grade. A v1
grade takes a funded start position as it is: the funding the start earns
or pays is in the session's PnL, order or not.

## Lending with pools and a pushed oracle

RFD 0005's `liquidation-flash` needs three things a lending scenario adds
to its sections: pools to sell seized collateral through, a Scope price the
bot can push, and the transaction layer's auction.

```json
"venues": {
  "lending": { "seed": 23, "reserves": ["SOL", "USDC"], "wallet": { "USDC": 1000 },
               "borrowers": { "count": 8, "collateral": "SOL", "debt": "USDC",
                              "health": [1.0005, 1.02], "size_usd": [1200, 2400] },
               "liquidators": { "count": 4 },
               "keeper": { "every_slots": 5, "cu_price": 100 } },
  "amm": { "pools": ["orca:amm:Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE",
                     "raydium:amm:3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"] }
},
"oracles": { "seed": 5, "feeds": [{ "name": "sol", "cadence_slots": 1,
  "source": { "kind": "scope", "account": "3t4JZcueEzTbVP6kLxXrL3VpWx45jDer4eqysweBchNH",
              "index": 3, "twap_index": 455, "mode": "pushed" },
  "path": [{ "slot": 280, "level_pct": -4.5 }] }] },
"tx": { "layer": { "seed": 29, "fees_in_pnl": true, "native_sol": 1.0, "sol_usd": 70.26 } }
```

- **`venues.amm.pools`** (v2 only: `Scenario::save` refuses a scenario
  with pools, and `save_any` writes it as v2): one to four cloned pools of
  the snapshot, by instrument (`orca:amm:<address>`, `raydium:amm:<address>`). The bot gets a
  seat on each, and each must trade only the session's reserves' assets, so
  the account values what it holds at the scenario's prices. The mandate
  covers each, named or under its venue's wildcard; a wildcard's `assets`
  are checked against the pool's own when the session starts. A pools
  arbitrageur swaps each pool back to the scenario's price after every
  step's blocks (`docs/actors.md`). The session writes one address lookup
  table of every reserve, Scope and pool account a route names, and the
  bot's own token accounts, which its seats compile with and its venue
  metadata names (`lookup_table`), so a push, a flash loan, a liquidation
  and a swap fit one transaction.
- **A pushed Scope feed** (`"mode": "pushed"`, lending sessions only):
  before the cheatcodes lock, the feed's entry and its TWAP entry become
  `FixedPrice` (Scope's type 23) at the price Scope holds then. Each
  publication then writes the path (and an hour's EMA of it, for the TWAP)
  into the mapping's `generic` data: the upstream, which the bot reads in
  `scope_mappings` (its reserve's metadata names it, with `scope_entries`).
  Scope's own price moves only when someone sends `refresh_price_list`.
  The feed's reserve must price from its entry alone (its chain is empty).
  The default, `written`, writes Scope's price as before.
- **`venues.lending.keeper`**: who pushes the pushed entries on a schedule,
  every `every_slots` (5 by default: Scope's median cadence while active
  on the main market) at a priority fee of `cu_price` micro-lamports per
  unit (100 by default). A session with a pushed feed has one, at the
  defaults if the section doesn't say.
- **`venues.lending.liquidators`** under the layer: no `delay_steps`
  (refused there; required without the layer). Each liquidator bids in the
  slot auction at a latency and with a bid the research measured
  (`docs/actors.md`), or by the session's own `race`:
  `{ "bid_share": [0.2, 0.6], "tipping_share": 0.7, "same_slot_share": 0.4,
  "competition": 1.3 }`, each member optional.
- **`venues.lending.borrowers.size_sigma`** (optional): the borrowers'
  collateral is log-normal around the geometric middle of `size_usd`, with
  this spread of its logarithm (at most 4), clipped to the range; absent,
  log-uniform over it.
- **The mandate** may be v0.3.0: the session reads back each of the bot's
  transactions for rules 11 to 13 (`specs/mandate.md`).

`bench/checks/kamino/liquidation-flash.json` is such a scenario, on
`snapshots/atomic-arb-2026-11-21`; the `liquidation-flash` generator
(`crates/grader/src/liquidation_flash.rs`) draws them for the task of that
name.

## An amm session: triggers and searchers

RFD 0005's `backrun-sol-usdc` is a session of its own kind: `venues.amm`
alone. The bot holds a wallet of SOL and USDC among the pools, a whale
works large orders into one of them a chunk at a time, and each chunk is a
**trigger** ([ADR 0004](adr/0004-atomic-arbitrage-in-the-sheet.md)): the
step ends right behind it, with its slot still open, and the bot and the
searchers race to backrun it in that slot.

```json
"venues": { "amm": {
  "pools": ["orca:amm:Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE",
            "raydium:amm:3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"],
  "seed": 41,
  "wallet": { "SOL": 1500 },
  "whale": { "accounts": 3, "orders": [
    { "pool": "raydium:amm:3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv", "side": "sell",
      "chunks": [{ "slot": 41, "sol": 500 }, { "slot": 113, "sol": 600 }] }] },
  "background": { "pools": [{ "pool": "orca:amm:Czfq3x…", "swaps_per_minute": 15,
                              "median_sol": 3.4, "p90_sol": 41, "max_sol": 280 }] },
  "searchers": [{ "latency_slots": [0, 2], "bid_share": 0.34, "pays": "priority_fee" },
                { "latency_slots": [0, 1], "bid_share": 0.0, "pays": "priority_fee", "fixed_sol": 200 }],
  "pool_searchers": [{ "latency_slots": [1, 3], "bid_share": 0.85, "pays": "priority_fee", "margin_bps": 2 }],
  "bound_usd": 400
} },
"tx": { "layer": { "seed": 43, "fees_in_pnl": true, "native_sol": 10.0, "sol_usd": 70.24 } }
```

- **`pools`**: one to twelve cloned pools; the first trades SOL for USDC,
  and every other token a pool trades has a reference price in
  `reference_sol`. The session's lookup table holds every account a swap
  through them names, once each, and the bot's token accounts; past a
  table's 256 addresses the session writes as many as it needs. The bot's
  seats compile with them and the venue metadata names them
  (`lookup_table`, the first, and `lookup_tables` where there are several,
  with each pool's `program`, mints and vaults).
- **`seed`** (required): every actor's draws.
- **`wallet`**: whole `SOL` (held wrapped) and `USDC`; the bot also holds
  an empty account of every other token its pools trade. The layer's
  `native_sol` pays fees and tips.
- **`reference_sol`** (optional): every other token's reference price, in
  SOL a unit, by mint: what the account values it at and the pool
  searchers trade its pools back to. It doesn't move during a session
  unless `reference_paths` moves it.
- **`reference_paths`** (optional): by mint (each one `reference_sol`
  names), waypoints (`slot`, `level_pct`) that move that token's
  reference price, in percent of its start, as `market_model.path` moves
  SOL's: up to 4,096 in slot order inside the session, each within 50%.
  The venue metadata a bot reads lists in `reference_sol` only the
  tokens whose price no path moves.
- **`reference_usd`** (optional): SOL's price at the start, which
  `market_model.path` moves (its waypoints, as a perp's level) and the
  account is valued at, never the pools' own prices, which a trade moves.
  Without it, the first pool's price as the session is set up. Where the
  mandate's inventory numeraire is SOL, the account is measured in SOL
  (its USDC at the reference then) and priced at the starting reference,
  so the path moves no PnL by itself.
- **`whale`**: its `accounts` (each chunk's is drawn from the seed) and
  its parent `orders`: a pool, a `side` (`sell` or `buy` the base) and its
  `chunks`, each a `slot` (from the session's start) and a size in `sol`
  (at most 2,000; SOL's worth of the base at the reference prices where
  the base isn't SOL). A chunk with an `impact_bps` (up to 10,000) is
  sized by the move it makes instead: a sale divides the pool's price by
  1 + the impact, a buy multiplies it, `sol` is the most it trades, and
  where the pool's liquidity ends short of the price the impact is halved
  until the swap fills. Every chunk is a trigger; chunks land in
  increasing slots, after slot 0 and before the session's last. Mainnet's
  SOL/USDC chunks were 200 to 1,000 SOL, often round, every 30 to 200
  slots, into the thinner pool, as the `backrun-sol-usdc` family
  (`crates/grader/src/backrun.rs`) draws them; thin pools' triggers move
  them a median 545 bp, as `backrun-thin-pools`
  (`crates/grader/src/thin_pools.rs`) draws them.
- **`background`**: each pool's calm flow, `swaps_per_minute` (at most
  150, a slot each) of either side, log-normal through `median_sol` and
  `p90_sol` (SOL's worth), capped at `max_sol`.
- **`searchers`** (two pools at least): each closes the most profitable
  cycle from SOL back to SOL through the pools, of at most `max_legs` legs
  (2, a pair's two pools, unless it says 3 or 4), with one atomic
  transaction, after a latency drawn in `latency_slots` per send (0 lands
  in the trigger's own slot), bidding `bid_share` of the profit it expects
  as a `priority_fee` or a `tip` (a bundle), at the size that profits most
  or `fixed_sol`, if it expects at least `min_profit_sol` (0.01). The
  cycles are every simple one through SOL (`crate::amm::cycle`): no pool
  twice, no token but SOL twice.
- **`pool_searchers`**: each trades any pool back to its reference price
  (SOL's dollar price for SOL/USDC, else the ratio of its tokens'
  reference prices) once it strays past its fee and `margin_bps`, bidding
  `bid_share` of what that makes at the reference.
- **`bound_usd`**: what the reward divides PnL by.

An amm scenario runs under the transaction layer and has no fair path,
warps, oracles, library actors, order, faults (they count two-slot steps)
or spot scoring. Its mandate's universe is its pools, named or under a
venue's wildcard with `assets`; v0.3.0's rules 11 to 13 are judged on every
transaction of the bot's (see `crates/grader/src/amm.rs`).

**Steps.** A step is two slots; a step that reaches a chunk's slot ends
there instead (`crate::trigger::schedule`). The count is fixed by the
session's length and the chunks' slots, so the hello announces it and a
replay meets the same steps. The hello's `slots_per_step` stays 2: a step
that ends on a trigger is shorter, and a bot reads each step's `slot`. On a trigger step the bot's `Step` (and a
component's `market.trigger`) names the chunk: its signature, its slot,
its pool, its side and size; it has landed, and the pools show it.

`bench/checks/amm/backrun-sol-usdc.json` is such a scenario, on
`snapshots/atomic-arb-2026-11-21`; `just backrun-smoke` grades doing
nothing and `bots/backrun-probe` on it.
`bench/checks/amm/backrun-thin-pools.json` is one over ten pools and five
tokens, on `snapshots/thin-pools-2026-11-21`, with `reference_sol`,
triggers sized by `impact_bps` and searchers of two and three legs; `just
thin-pools-smoke` grades doing nothing and the task's reference on it.

## Several perp markets

A perp session on several markets of one account names them in keyed maps,
one per section, and leaves that section's one-market fields out:

| Map | Holds, per market symbol |
| --- | --- |
| `venues.perp.markets` | `funding`, as `venues.perp.funding` (not with it) |
| `market_model.markets` | `path`, `fair_path` and `record`, as the section's own fields (not with them); no `closed_period` |
| `market_model.joint` | Optional: a joint draw of the markets' paths: `matrix_source` (`design` for a matrix the author states, or a correlation file's id, with its `matrix_sha256`), `assets` in the matrix's order, the correlation `matrix`, and each asset's draw in `records`. Like a single draw's record, never its seed |
| `actors.markets` | `competitor`, `takers` and `takers_pricing`, as the section's own fields (not with them). One pool of taker accounts trades every market, so `account` numbers are shared |

`assets.symbol` is the primary market, one of the maps' keys: it is the one
`MTX_SYMBOL` names and the report's one-market fields describe. The actor
library, if any, trades it.

One rule joins the maps (`markets_agree`): `venues.perp.markets` names at
least two markets, and `market_model.markets` and `actors.markets`, where
given, name the same ones. The mandate's universe is exactly their perps,
each with its own cap. Each market's funding, path and flow are checked as
a one-market scenario's, and an error names the market. A Phoenix oracle
feed may write any market's oracle, but not one whose market has a path.
`start_positions` and `close_out` may name any of the markets, and an
`order` may be on any of them; each is checked against its market's range.

Each market's position is valued by its own rule (`oracle`, `book` or
`mark`, as a one-market scenario's), and the bound is each market's at its
own cap and start mark, summed. A session's report lists each market
(`markets`: start and end marks, positions, valuation, flow), and each step
records each market (`markets` in the timeline's steps).

Net scoring (`scoring.net`) works per market: each market's start position is held to that market's fair close (else its level), and the holds are summed, so a bot that holds every market scores 0. The same hold, step by step, comes off the drawdown the bot is shown and the risk desk reads. `scoring.bound_on` works on several markets too. Scoring v2 scores a session on several markets against the informed bot, not the cost-aware hindsight ceiling.

What a session on several markets doesn't do yet:

- **Library actors** (`actors.library`) trade the primary market only.
- **Making fills** (`making_fills`) record the primary market's fills only.
- **Markout:** the grade's markout, luck and residual split is a
  one-market split; a session on several markets reports none, whatever
  fair paths it has.
- **Raw sends** (`tx.raw_sends`) are refused: their pre-check audits one
  market's position and value.
- **Closed periods** stay a one-market scenario's.
- **The liquidation keeper** (`venues.perp.liquidator`) is refused: it
  liquidates the scenario's market only.
- **Price impact** (`venues.perp.impact`) is refused: its script plays one
  market's path.
- **Process bots** built on `mtx_kit::run` trade the primary market only:
  its run loop holds one trader. A process bot that wants the others reads
  `MTX_MARKETS` (`["phoenix:perp:NVDA", "phoenix:perp:SPCX"]`, the primary
  first) and trades them itself; a strategy component gets a seat on every
  market, and the planner cuts each market's lines to its own cap.

There is no v1 form: `Scenario::save` refuses such a scenario and
`save_v2` writes it (`save_any` writes each scenario in the layout it has,
as `mtx calibrate` does). `mtx market-cal generate` draws one from a template
on several markets, given `--calibration SYMBOL=FILE` per market and
`--correlation`.

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

On a session on several markets the desk closes every market of the
account. It cancels the bot's orders on each, then goes round the markets
in symbol order, one order to each market still open per round, within the
same 8 orders a step: one market's book can't take the whole budget while
another's exposure runs on, and a market whose book took nothing gets no
more orders that step. `slice_base` caps every market's orders in that
market's own base units, so on markets whose prices differ widely (SPY
against SPCX, say) it slices the dearer one finer in dollars; set it for
the market that most needs it. Each close order names its `market`, the
forced close records each market's position at the trigger in
`positions_before` (`position_before` stays the primary's), and `flat`
holds only once every market is flat. The desk's fills on any market are
marked `forced`.

## Market making: obligations and reduce-only taking

A market-making scenario can hold its bot to two terms a designated market
maker works under, each opt-in; without them a session runs and grades as
before.

**Quoting obligations**, `actors.competitor.obligations` (v2) or
`making.obligations` (v1):

```json
"actors": { "competitor": { "competitor_bps": 12, "competition": { "...": "" },
  "obligations": { "presence": 0.75, "max_half_spread_bps": 18, "min_size": 1.0 } } }
```

- `presence`: the share of the session's steps, in (0, 1], at which the bot
  must quote both sides;
- `max_half_spread_bps`: how far each side may sit from fair value (the
  scenario's level, the price the account is valued at), in basis points of
  it;
- `min_size`: what the bot's orders within that band must add up to on each
  side, in base units.

After each of the bot's turns, before the next step's takers trade, the
session reads Phoenix's order book and samples the bot's resting orders
(orders past their last valid slot don't count). The report's
`obligations` gives the session's `steps` (what `presence` is a share of),
the steps it quoted both sides within the band at the size (`two_sided`),
the steps it fell short and why (`one_sided`: a side with no order;
`too_wide`: a side with orders, none within the band; `too_small`: a side
within the band under `min_size`), the steps a stop-out kept the session
from reaching (`unsampled`, which count as missed), `uptime` and whether it
`met` `presence`. The book names each order's trader by Phoenix's trader id;
the session finds the bot's from an order of its resting where only the
bot's own market events place one. Obligations are reported, not scored: a
shortfall is no breach and changes no PnL or reward.

**Reduce-only taking**, the mandate's `"taking": "reduce_only"` on the
scenario's market (a v0.2.0 mandate; [mandate spec](../specs/mandate.md) rule
10). Only a market-making perp session records which of the bot's fills it
took (`making_fills`, `role` `taker`), so only one, without a
`liquidator`, may use it. Two layers enforce it:

- before an order goes out, the grader refuses one it can tell would take
  liquidity to open, add to or flip the position. A component's market or
  immediate-or-cancel order is one; so is a resting order priced at or
  through the best opposite price of the *other* traders on Phoenix's book
  (the bot's own orders are left out, however many it has resting, and a
  sheet that cancels first and then crosses is judged the same). If the
  cancel-all simulation can't read the touch, a side the bot has orders
  resting on is trusted and left to the audit. A component's raw send is
  simulated before it goes and refused the same way if its fills would take
  to open, add to or flip. A process bot has no sheet:
  the RPC guard simulates each transaction it sends (`sendTransaction`,
  `sendBundle`, the staked route) against the paused chain, and refuses one
  whose fills would move the position as a taker to open, add to or flip
  it (a post-only order, a cancel or a match against its own orders moves
  nothing). An order flagged reduce-only goes (Phoenix applies the flag),
  and so does a post-only one. A refused order is not sent, its status or
  the RPC error tells the bot why, and `turns.refused_takes` counts it: the
  act doesn't happen, so it is no breach;
- the audit judges every taker fill against the position just before it,
  counted from the start position through the fills of all the bot's trader
  accounts, its cross-margin account and any subaccount it registers, in
  Phoenix's order (a match between the bot's own accounts, which the fill
  record marks, moves nothing). A take that opened, added to or flipped the
  position is a `taking` breach, as any mandate rule's. This catches what
  the checks can't see before the trade: a take from a subaccount, or a cross
  of a touch the session couldn't read.

The code is `crates/grader/src/obligations.rs`, `crates/grader/src/taking.rs`
(the check before an order goes out is `crates/runtime/src/taking.rs`) and
the perp session.

A market-making scenario is on one market, so both terms apply to the
primary market of a session that lists several.


## The liquidation keeper: `venues.perp.liquidator`

By default nothing liquidates a perp session, and a bot that ends it
liquidatable is disqualified. A scenario can add a competing keeper, as
mainnet has, and price the liquidation instead:

```json
"venues": { "perp": { "liquidator": { "reaction_slots": [2, 12], "check_every_steps": 1 } } }
```

- `reaction_slots`: `[min, max]`, the slots between the first step the bot's
  account reads liquidatable and the keeper's first liquidation. It is drawn
  once per session from the session's seed (role `liquidator`), reported as
  `liquidator.delay_slots`, and never in the bot's environment: the bot is
  told the range.
- `check_every_steps`: how often the keeper looks. Every step by default.

After each step's blocks land, the keeper reads Phoenix's margin view of
every trader account the bot holds (the cross-margin account and any
subaccount, as the meter does). When one reads liquidatable and the reaction
time has passed, it sends `LiquidateViaMarketOrder` for up to the market's
maximum liquidation size, or `LiquidationTransfer` once the account reads
past the backstop margin, and again on each later step while the account
stays liquidatable. Phoenix decides what a call closes and charges: the
keeper computes no liquidation. A call Phoenix refuses (the bot traded back
to health in the same block) is a `crank_refusals` entry, and the session
goes on. When the account reads healthy again the reaction time starts over
with the same draw.

The bot keeps running with what the liquidation leaves it. Each stretch of
liquidations is one entry of `forced_closes` with reason
`{"kind": "liquidation", "by": "keeper"}`: the first step it fired at, the
account's value before and after, and the keeper's orders, each with its
transaction's `signature`. They are made in the bot's account but are not
the bot's: in a session that reads fills, their fills are marked `forced` in
`perp_fills`, as the risk desk's are, and the sides and price rules leave
them out. `liquidator` reports the draw,
the number of liquidations that landed and `cost_usd`, the sum over stretches
of the value before less the value after: Phoenix's penalty and the fill's
impact, which are inside the account's value and score. After the last step,
with the bot stopped, the keeper takes one more turn that doesn't wait for the
reaction time, and the account is measured after it. An account that still
ends liquidatable is then valued as it is, not disqualified; the mandate's
other rules, a drawdown limit included, still apply.

A scenario that lists Phoenix's program in `tx.raw_sends` is refused with a
liquidator: a bot that could send Phoenix's own instructions could liquidate
its accounts itself. The code is `crates/grader/src/keeper.rs`, and
`bench/checks/keeper/run.sh` checks it end to end.

## Price impact: `venues.perp.impact`

By default a perp session's book is centred on the path, and the path is a
schedule: the snapshot's makers come back at full depth on every planned
move. A bot that walks the book pays for the depth it takes and then finds
the book back where it was. A scenario can keep the book where trades leave
it instead:

```json
"venues": { "perp": { "impact": { "permanent_share": 0.5, "half_life_slots": 10, "max_bps": 150, "probe": 10 } } }
```

- `permanent_share`: of the distance trades walked the book, the share that
  stays for good, from 0 to 1 (0.5 when absent). The rest fades.
- `half_life_slots`: slots the fading part takes to halve.
- `max_bps`: the most the impact moves the book either way, in basis points
  of the book's level (150 when absent, 190 at most).
- `probe`: the size, in base units, the book is priced at (10 when absent):
  where the book is, each side, is what a market order of this size would
  fill for on average. Larger than the odd lots that rest at the touch, and
  small beside the makers' depth.
- `resistance`: stretches where the book is harder to move (none when
  absent, 8 at most), described below.

The path is then played by a Surfpool scenario script, not a schedule of
overrides (`surfnet_registerScenario`'s `scripts`). On the first slot of
each step it asks Phoenix what a market order of the probe size would fill
for, each side: a buy that fills further from the makers' centre than it
did on their own quotes is depth taken, and the same for a sale. It centres
the makers on the path's book level plus what was walked, at once in full,
and lets the fading part decay. On the step's second slot it reads the
makers' quotes as the move left them, which the next step's trades are
measured against: Phoenix's makers quote further out on a side they were
filled on.

What this means for a session:

- **The impact is never more than the trade moved the book**, so a round
  trip costs the spread at least: a bot is not paid to push the price.
- **Every taker's trades count**: the bot's, the tape's and the actors'.
  The book is read at the probe's size, not at the best bid and ask: an odd
  lot resting at the touch moves the touch and would hide a walk behind it.
  A book too thin to fill the probe is not read, and leaves no impact.
- **Only the book moves.** The oracles stay on the scenario's index, and
  the account is valued at the scenario's book level (`valuation` `book`),
  not at the price trades pushed the book to.
- **The first slot centres the makers and the index on the starting mark**,
  where a session without impact leaves them as the snapshot had them
  until the path's first move. A planned move lands on the first slot of
  the step the schedule would land it in, and is uncrossed in that slot.
- Each step records `impact_bps`, how far the makers are centred from the
  book's level, and a component sees `MTX_IMPACT=1` in its session vars.

The impact is state the script writes, not orders. Volume is still what
traded: the bot's fills and the takers', which are transactions.

Impact is refused with market making or a closed period (their makers are
placed their own way), with the scenario's own `oracles` (the script writes
Phoenix's index), with the `refill` actor (the fading part is the depth
coming back), with the transaction layer (it lands the bot's sends on
any slot, and the script reads trades between steps) and on a session with
several markets (the script plays one market's path). The code is
`crates/grader/src/impact.rs`, and `bench/checks/impact/run.sh` checks it
end to end.

On the 2026-10-29 SPCX snapshot, buying 35 SPCX (about $5,000) walks the
asks 2 ticks (1.4 bp), and 350 SPCX (about $49,000) walks them 28 ticks
(20 bp).

### Resistance: `impact.resistance`

A level where the book is harder to push through: while the book sits
inside a zone, a trade moves it a fraction of what the trade walked, and
the makers put the rest of the depth back.

```json
"impact": {
  "half_life_slots": 10,
  "resistance": [
    { "from_bps": 5, "to_bps": 30, "strength": 4 },
    { "from_bps": -60, "to_bps": -40, "strength": 10 }
  ]
}
```

- `from_bps`, `to_bps`: where the zone is, as the book's distance from the
  path's level, in basis points of the session's starting mark. Negative is
  below the level. A zone may hold the level (`-10` to `10`); zones may
  touch and may not overlap, and none reaches past `max_bps`.
- `strength`: how many ticks of walking move the book one tick inside the
  zone, above 1 and 50 at most.

With the first zone above, the 350 SPCX buy that walks the asks 28 ticks
moves the book 12: the 7 ticks to the zone, and a quarter of the 21 inside
it. The makers are centred 12 ticks up, with their full depth, so the next
buy starts 16 ticks under where the first one ended. The fading part and
the limit work as before: what fades is walk, and the book comes back
along the same zones; `max_bps` is the book's distance, so reaching it
through a zone takes that much more walking.

A zone is a place on one fixed curve from "ticks walked" to "ticks the book
moved", measured from the path's level. That is what keeps the round trip's
property: the curve never falls and is the same whichever way the book is
going, so buying and selling back never sells above where it bought,
however the trade is cut up and whatever the waits. It is also why a zone
is not a price, does not give way once crossed and does not resist one
direction only. Each of those is a curve that changes under the trader: buy
where the book barely moves, sell where it moves in full, and the
difference is profit no market paid for. To put a wall at a price, give
the path a level there and a zone around it.

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
