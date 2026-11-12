# RFD 0003: `mtx studio`, a workbench for a strategy and its scenario

State: discussion
Authors: Ludo, opus, fable  ·  Started: 2026-11-12

## Problem

Writing a strategy today takes four tools that don't know about each
other: an editor for `src/lib.rs`, a terminal for `just build` and
`mtx bot run --debug`, `mtx scenario play` in a browser to see the
market, and a coding agent in another window to change either. The
agent can't see what the strategy did; the debugger can't show the
market it played; nothing shows a scenario and a strategy side by side.
`mtx studio` puts them in one page, wired together: tweak the scenario or
the strategy (by hand or through an agent), press play, watch the
session step by step against the market's chart.

## Sketch

### The page

`mtx studio` serves a web UI on localhost and opens it, for the bot in the
current directory (`--bot-path` for another: its directory or its
`Cargo.toml`). The bot is a
workspace: a strategy crate (`mtx bot new`'s layout: `Cargo.toml`,
`Justfile`, `src/lib.rs`) and the scenarios it plays (`scenarios/*.json`,
seeded with the studio's own: `practice-101`, 5 steps, and `practice-102`,
10). Four resizable panels, as Rex lays
out its panes:

```
┌───────────────────────────────┬─────────────────────────────────┐
│ editor                        │ market                          │
│ src/lib.rs, Rust highlighting │ the scenario's planned path,    │
│ build errors inline           │ then the session's mark, fills, │
│                               │ position and PnL as it plays;   │
│                               │ a playhead at the current step  │
├───────────────────────────────┼─────────────────────────────────┤
│ composer                      │ execution                       │
│ messages to the agent, which  │ controls: build, play, pause,   │
│ edits the strategy or the     │ step, restart, speed; the       │
│ scenario; agent and model     │ step's blocks (as --debug       │
│ pickers                       │ prints them), build output      │
└───────────────────────────────┴─────────────────────────────────┘
```

Dividers drag; sizes persist in `localStorage`. Each panel can be
maximized. The page is one HTML file served by `mtx`, like the review
page and `mtx scenario play`: no build step, CodeMirror 6 (with its Rust
mode) and the chart library vendored or loaded as ES modules.

### The workspace

The server owns the workspace on disk; the page edits it through the
server. A file saved in the editor is written to disk; a file the agent
changes is pushed to the page (`file/changed`), which reloads it unless
it has unsaved edits (then it asks).

### The agent

The composer talks to a coding agent over ACP (`crates/cli/src/acp.rs`,
which `mtx scenario new` already drives). The agent runs in the workspace,
so it edits `src/lib.rs` and `scenarios/*.json` as files. Each prompt
carries context the page adds: which file and scenario are open, the
last build's errors, and the last session's summary or the step it
stopped at, so "why didn't it buy here?" means the step on screen.

Discovery, at start and on demand (`agents/list`):

- ACP adapters on `PATH` (`claude-agent-acp`, `codex-acp`), or through
  `npx` when Node is installed;
- for each, whether it can authenticate (an API key in the environment,
  or the CLI's own login) and the models it offers;
- the pick is saved in the workspace (`.mtx/studio.json`) and changed
  from the composer's header.

### Play

Play builds the strategy (`just build`, output streamed), then plays the
open scenario in the grader, as `mtx bot run` does: a surfnet from the
task's snapshot, the component loaded and called each step. The world
waits for a component, so the session can stop at any step and resume:
the debugger is the grader's [`StepObserver`] blocking until the page
says go. Stops: each step (step in), a step number, a step where the
sheet isn't empty, a fill, a trap. Restart replays the same session from
the start: it is deterministic.

### The protocol

One WebSocket, `/ws`, JSON messages shaped like the Debug Adapter
Protocol's, so a later DAP adapter (VS Code) is a translation, not a
rewrite:

```json
{"seq": 4, "type": "request", "command": "continue", "arguments": {}}
{"seq": 9, "type": "response", "request_seq": 4, "success": true, "body": {}}
{"seq": 10, "type": "event", "event": "stopped", "body": {...}}
```

Requests (page → server). A failed request answers `success: false`, a
`message`, and a `body` the page can act on:

| Command | Arguments | Answers |
| --- | --- | --- |
| `initialize` | | `workspace`, `task`, `files`, `scenarios`, `agent`, and `session`: `{state: idle\|running\|stopped\|terminated, k, scenario, breakpoints}` |
| `file/read` | `path` | `text`, `version` (its content's hash) |
| `file/write` | `path`, `text`, `baseVersion` | `version`; or `conflict` with the current `text` and `version` when `baseVersion` is stale; a scenario that doesn't parse fails with `diagnostics` |
| `scenario/read` | `name` | `scenario`, `steps`, `slots_per_step`, `start_mark_usd` (`start_mark_estimated` until a session has played), `plan: [{k, mark_usd, level_pct}]`, `takers: [{k, side, size}]` |
| `build` | | `ok`, `diagnostics`; streams `output` (category `build`) and sends `built` |
| `launch` | `scenario`, `stopOnEntry`, `breakpoints`, `msPerStep` | Builds if the strategy changed, starts a session |
| `continue` / `pause` / `next` | | The session's status |
| `setBreakpoints` | `steps: [k]` (counting from 1), `onOrders`, `onFill`, `onTrap` | |
| `setSpeed` | `msPerStep` (0: as fast as it goes) | |
| `restart` / `terminate` | | |
| `session/history` | | `events`: the session's `started`, `step`, `stopped`, `terminated` so far |
| `agents/list` | | `selected: {agent, model}`, `agents: [{id, name, available, reason, models: [{id, name}]}]` |
| `agents/select` | `agent`, `model` | Saved in the workspace |
| `composer/send` | `text`, `context` | When the turn ends |
| `composer/cancel` | | |
| `composer/history` | | `messages`: each message and tool call as last sent |

Events (server → page):

| Event | Body |
| --- | --- |
| `output` | `category` (`build`, `strategy`, `grader`, `agent`), `text` |
| `built` | `ok`, `diagnostics: [{file, line, column, endLine, endColumn, severity, message}]` |
| `started` | `session` (the `Session` the strategy got), `printed`, `failed` |
| `step` | `k`, `steps`, `market`, `printed`, `failed`, `intents`, `notes`, `sent: [{action, ok, error}]`, `fuel`, `rpc_calls`, and `account: {value_usd, pnl_usd, drawdown_usd, position, position_change, level_usd}` as the session measured it at the step, before the strategy's turn: `position_change` is what filled during the step before |
| `stopped` | `reason` (`entry`, `step`, `breakpoint`, `pause`, `trap`), `k` |
| `continued` | |
| `terminated` | `scenario`, `pnl_usd`, `bound_pnl_usd`, `reference_pnl_usd`, `disqualified`, `stopped`, `not_graded`; or `error` |
| `file/changed` | `path`, `version`, `text`, `by` (`agent` or `disk`) |
| `composer/message` | `id`, `role` (`user`, `agent`, `thought`), `text` (a chunk, appended to the message with that `id`), `done` |
| `composer/tool` | `id`, `title`, `status`: the whole call each time, to replace the line with that `id` |

The page's own files (a bundled CodeMirror) live in `crates/cli/src/studio/`
and are served at `/studio/<name>`. The agent's permission requests are
granted (v1); every tool call shows in the composer.

### Pieces that exist

- `mtx_grader::wasm::StepObserver` and `StepTrace`: `mtx bot run --debug`.
- `crates/cli/src/acp.rs`, `author.rs`: the ACP client and the agents.
- `crates/cli/src/play.rs`, `charts.rs`: a scenario's planned path, charted.
- The review page's session view: a session's timeline, step by step.

## Open questions

- **One page or a desktop app?** A page on localhost first; Rex-style
  panes in a browser are enough to learn what the workbench needs.
- **Breakpoints on code lines?** A component has no debugger attached;
  stops are on steps and on what the step did. Line breakpoints would
  need wasm DWARF and a wasmtime debugger: later, if ever.
- **Several scenarios at once?** Play one; `mtx bot practice` grades the
  set. A "grade all" button can run practice and show its table.
- **How does the agent see a session?** The context the page adds to a
  prompt (the step on screen, the session's summary), and an MCP tool
  (`studio_session`) it can call for a step's trace.

## Risks

- **Two editors on one file.** The page and the agent both write
  `src/lib.rs`. The server is the one writer; the page reloads what the
  agent wrote and asks before dropping unsaved edits.
- **Agent permissions.** The agent edits files and runs commands in the
  workspace. The studio grants it the workspace only, and shows every
  tool call in the composer.

## What we learned
- 2026-11-12: the server, end to end (`crates/cli/src/studio.rs`). On a
  workspace `mtx bot new` made, with the tutorial's first strategy and
  flash-crash's practice set seeded into `scenarios/`, a script drove the
  protocol: `launch` with `stopOnEntry` stopped at entry, `next` at step 1,
  `continue` at the breakpoint at step 33 (the strategy's buy: a `target`
  of 35, four orders sent, its printed line), and the session ran to its
  end, +$124.03 as `mtx bot run` gives, with every step's event. The
  composer, through Claude Code over ACP, answered a question about the
  strategy, then changed `DIP_PCT` on request, and the page got
  `file/changed` (`by: agent`). Details for the page:
  - a session runs on its own thread and runtime: the grader's session
    isn't `Send`, and the observer blocks a worker while the page decides;
  - ACP updates a tool call by its id, mostly without its title:
    `composer/tool` carries the whole call (`id`, `title`, `status`) each
    time, so the page replaces the line with that `id`;
  - a `target`'s `maxPerStep` is `null` when it is unlimited;
  - the build is cargo's, for its diagnostics (`file` relative to the
    workspace, `line`, `column`, `endLine`, `endColumn`, `severity`,
    `message`), and copies the component to `bot.wasm`;
  - `/dev` serves a test page with a button per request; until the
    studio's page lands, `/` serves it too. `MTX_STUDIO_PAGE=<file>` serves
    a page from disk, read on each load.
- 2026-11-12: fable's changes for the page, in the server. A step carries
  the session's own account, measured before the strategy's turn, so the
  chart and `terminated` agree; fills are the position's change since the
  step before, not each fill with its price, which would need Phoenix's
  fill events parsed (later, if the chart needs them). Files carry versions
  and a stale write is a conflict. A scenario saved through `file/write` is
  checked; the plan comes in steps, from the snapshot's mark until a
  session gives the real one (143.05 against 143.11 on flash-crash). A page
  that reloads gets the session's state and its history, and the
  composer's.
- 2026-11-12: `terminate` ends the session where it is (found by fable
  against the page). It used to let the session run out unseen and report
  the whole run. Now it wakes the observer and fires a signal the
  session's runtime races the grader against: dropping the grader's future
  stops the surfnet and the bot, in 10 ms from step 33 of practice-4, with
  nothing left listening. `terminated` then says `{terminated: true, k}`,
  and the history keeps the steps played. Between `launch` and `started`,
  while the market is set up (about 4 s), the session's state is
  `starting`; breakpoints are reported only for a live session.
