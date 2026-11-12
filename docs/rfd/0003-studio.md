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

`mtx studio [DIR]` serves a web UI on localhost and opens it. `DIR` is a
workspace: a strategy crate (`mtx bot new`'s layout: `Cargo.toml`,
`Justfile`, `src/lib.rs`) and the scenarios it plays (`scenarios/*.json`,
seeded from a task's practice set). Four resizable panels, as Rex lays
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

Requests (page → server):

| Command | Arguments | Does |
| --- | --- | --- |
| `initialize` | | Answers the workspace: its files, scenarios, tasks, the agent pick |
| `file/read` | `path` | The file's text |
| `file/write` | `path`, `text` | Saves it |
| `scenario/read` | `name` | The scenario, and its planned path as the chart draws it |
| `build` | | `just build`; streams `output` (category `build`) and ends with `built` |
| `launch` | `scenario`, `stopOnEntry`, `breakpoints` | Builds if stale, starts a session |
| `continue` / `pause` | | Runs, or stops at the next step |
| `next` | | One step, then stops |
| `setBreakpoints` | `steps: [k]`, `onOrders`, `onFill`, `onTrap` | Where a running session stops |
| `setSpeed` | `msPerStep` (0: as fast as it goes) | Pace while running |
| `restart` / `terminate` | | |
| `agents/list` | `refresh` | The agents discovery found |
| `agents/select` | `agent`, `model` | Picks one; saved in the workspace |
| `composer/send` | `text`, `context` | A prompt to the agent |
| `composer/cancel` | | Stops the agent's turn |

Events (server → page):

| Event | Body |
| --- | --- |
| `output` | `category` (`build`, `strategy`, `grader`, `agent`), `text` |
| `built` | `ok`, `diagnostics: [{file, line, column, severity, message}]` |
| `started` | the `Session` the strategy got, and the scenario's step count |
| `step` | one step's trace: `mtx_grader::wasm::StepTrace` as JSON (market, printed, sheet, cuts, sent, fuel, rpc) |
| `stopped` | `reason` (`entry`, `step`, `breakpoint`, `pause`, `trap`), `k` |
| `continued` | |
| `terminated` | the session's report: PnL, bound, reference, why it stopped |
| `file/changed` | `path`, `text`, `by` (`agent` or `disk`) |
| `composer/message` | `role`, `text` chunk, `done` |
| `composer/tool` | the agent's tool call: `title`, `status`, `path` if it touched a file |

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
