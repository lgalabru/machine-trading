// A stand-in for `mtx studio`'s server, for working on the page without a
// surfnet or an agent: `studio.html?mock`. It speaks the protocol of
// docs/rfd/0003-studio.md over the same message shapes as /ws, plays made-up
// flash crashes against a dip buyer simulated here, and has an "agent" that
// edits the strategy's DIP_PCT. Nothing in it is the grader's arithmetic.

const STRATEGY = `//! Buys a dip, sells the rebound.
//!
//! The first strategy of the tutorial: watch how far the mark has fallen
//! from where the session started, buy half the cap once it is down
//! DIP_PCT, and sell when it has come halfway back.

use mtx_kit::prelude::*;

/// How far the mark must fall from the start, in percent, to buy.
const DIP_PCT: f64 = 1.0;
/// How much of the fall must come back, as a share, to sell.
const RECOVER: f64 = 0.5;

#[derive(Default)]
struct Dip {
    start: f64,
    low: f64,
}

impl Strategy for Dip {
    fn start(&mut self, session: &Session) -> Result<(), String> {
        self.start = session.start_mark;
        self.low = session.start_mark;
        println!("started: {} for {} steps, cap {}", session.symbol, session.steps, session.cap);
        Ok(())
    }

    fn step(&mut self, market: &Market) -> Result<Orders, String> {
        let mut orders = Orders::new();
        let holding = market.position.size;
        let change_pct = (market.mark / self.start - 1.0) * 100.0;
        self.low = self.low.min(market.mark);
        if holding == 0.0 && change_pct <= -DIP_PCT {
            println!("down {:.2}% from the start: buying", -change_pct);
            orders.target_position(market.cap / 2.0);
        } else if holding > 0.0 && market.mark >= self.low + RECOVER * (self.start - self.low) {
            println!("back {:.2} from the low: selling", market.mark - self.low);
            orders.target_position(0.0);
        }
        Ok(orders)
    }
}

mtx_kit::export!(Dip);
`;

const CARGO = `[package]
name = "bot"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
mtx-kit = "0.12"
`;

const START = 143.05;

const MANDATE = { version: "v0", universe: [{ instrument: "phoenix:perp:SPCX", max_position: 70 }], limits: { max_drawdown_usd: 1000 } };

function levels(sc) {
  const steps = Math.ceil(sc.duration_slots / 2);
  const out = [];
  let i = 0, level = 0;
  for (let k = 0; k < steps; k++) {
    while (i < sc.path.length && sc.path[i].slot <= k * 2 + 1) level = sc.path[i++].level_pct;
    out.push(level);
  }
  return out;
}

const hash = (text) => { let h = 2166136261; for (let i = 0; i < text.length; i++) h = Math.imul(h ^ text.charCodeAt(i), 16777619); return (h >>> 0).toString(16).padStart(8, "0"); };

export function mock(deliver) {
  const files = new Map([["Cargo.toml", CARGO], ["src/lib.rs", STRATEGY]]);
  const obvious = (name, description, duration_slots, path) => files.set(`scenarios/${name}.json`, JSON.stringify({ name, description, symbol: "SPCX", duration_slots, mandate: MANDATE, path }, null, 2) + "\n");
  obvious("practice-101", "Five steps: SPCX falls 3% at step 2, holds at step 3, and is back where it started at step 4. Buy the dip, sell the recovery.", 10, [{ slot: 3, level_pct: -3 }, { slot: 7, level_pct: 0 }]);
  obvious("practice-102", "Ten steps: SPCX rises 3% at step 3 and is back at step 5, then falls 3% at step 7 and 3% more at step 9: this dip doesn't come back.", 20, [{ slot: 5, level_pct: 3 }, { slot: 9, level_pct: 0 }, { slot: 13, level_pct: -3 }, { slot: 17, level_pct: -6 }]);
  let seq = 0;
  let pick = { agent: "claude-code", model: null, busy: false };
  let session = null; // {scenario, state, k, bps, ms, timer, events, args, sim}
  let messages = [];
  let mid = 0;
  let cancelled = false;

  const send = (m) => setTimeout(() => deliver({ seq: ++seq, ...m }), 0);
  const event = (name, body) => {
    if (session && ["started", "step", "stopped", "terminated"].includes(name)) session.events.push({ event: name, body });
    send({ type: "event", event: name, body });
  };
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const reject = (message, body = {}) => Object.assign(new Error(message), { body });
  const read = (name) => JSON.parse(files.get(`scenarios/${name}.json`));
  const scenarios = () => [...files.keys()].filter((p) => p.startsWith("scenarios/")).map((p) => p.slice(10, -5)).sort();
  const status = () => (session ? { state: session.state, k: session.k, scenario: session.scenario, breakpoints: session.bps, msPerStep: session.ms } : { state: "idle" });

  async function build() {
    event("output", { category: "build", text: "building the strategy for wasm32-wasip2…" });
    await sleep(250);
    event("output", { category: "build", text: "   Compiling bot v0.1.0 (/mock/bot)" });
    await sleep(350);
    const text = files.get("src/lib.rs");
    const lines = text.split("\n");
    const diagnostics = [];
    lines.forEach((line, i) => {
      const at = line.indexOf("todo!()");
      if (at >= 0) diagnostics.push({ file: "src/lib.rs", line: i + 1, column: at + 1, endLine: i + 1, endColumn: at + 8, severity: "warning", message: "the strategy would trap here: `todo!()` panics" });
    });
    if ((text.match(/\{/g) || []).length !== (text.match(/\}/g) || []).length) {
      diagnostics.push({ file: "src/lib.rs", line: lines.length, column: 1, endLine: lines.length, endColumn: 2, severity: "error", message: "this file contains an unclosed delimiter" });
    }
    const dip = text.match(/const DIP_PCT: f64 = ([^;]*);/);
    if (!dip || !Number.isFinite(Number(dip[1]))) {
      const i = Math.max(0, lines.findIndex((l) => l.includes("const DIP_PCT")));
      diagnostics.push({ file: "src/lib.rs", line: i + 1, column: 1, endLine: i + 1, endColumn: lines[i].length + 1, severity: "error", message: "expected a number for `DIP_PCT`: mismatched types, expected `f64`" });
    }
    const ok = !diagnostics.some((d) => d.severity === "error");
    for (const d of diagnostics) event("output", { category: "build", text: `${d.severity}: ${d.message}\n  --> ${d.file}:${d.line}:${d.column}` });
    event("output", { category: "build", text: ok ? "    Finished `release` profile [optimized] target(s) in 0.61s\nbuilt bot.wasm" : "error: could not compile `bot` (lib) due to 1 previous error" });
    event("built", { ok, diagnostics });
    return { ok, diagnostics, dip: dip ? Number(dip[1]) : 1 };
  }

  // The dip buyer, and an account marked at each step.
  function simulate(sc, dipPct) {
    const lv = levels(sc), cap = sc.mandate.universe[0].max_position;
    const start = START + 0.06;
    const sim = { steps: [], start, cap, n: lv.length };
    let pos = 0, entry = 0, cash = 0, peak = 0, low = start, pending = 0, prevPos = 0;
    for (let k = 0; k < lv.length; k++) {
      const mark = Math.round(start * (1 + lv[k] / 100) * 100) / 100;
      if (pending) {
        cash -= pending * (mark + Math.sign(pending) * 0.02);
        entry = pos + pending === 0 ? 0 : mark;
        pos += pending; pending = 0;
      }
      const pnl = cash + pos * mark;
      peak = Math.max(peak, pnl);
      const account = { value_usd: 10100 + pnl, pnl_usd: pnl, drawdown_usd: peak - pnl, position: pos, position_change: pos - prevPos, level_usd: mark };
      prevPos = pos;
      low = Math.min(low, mark);
      const change = (mark / start - 1) * 100;
      const printed = [], intents = [], sent = [];
      let target = null;
      if (pos === 0 && change <= -dipPct) { printed.push(`down ${(-change).toFixed(2)}% from the start: buying`); target = cap / 2; }
      else if (pos > 0 && mark >= low + 0.5 * (start - low)) { printed.push(`back ${(mark - low).toFixed(2)} from the low: selling`); target = 0; }
      if (target !== null) {
        intents.push({ kind: "target", position: target, slice: 10, maxPerStep: null, slippageBps: 200 });
        let missing = target - pos;
        const side = missing > 0 ? "bid" : "ask", limit = Math.round(mark * (missing > 0 ? 1.02 : 0.98) * 100) / 100;
        pending = missing;
        while (Math.abs(missing) >= 1) { const size = Math.min(10, Math.abs(missing)); sent.push({ action: { kind: "market", side, size, limit }, ok: true, error: null }); missing -= Math.sign(missing) * size; }
      }
      sim.steps.push({
        k, steps: lv.length,
        market: { k, slot: 462882345 + 2 * k + 2, last: k === lv.length - 1, steps_left: lv.length - 1 - k, slots_left: 2 * (lv.length - 1 - k), mark, oracle: mark - 0.06, index: mark - 0.06, best_bid: mark - 0.08, best_ask: mark - 0.05, position: { size: pos, entry_usd: entry }, cap, max_drawdown: 1000, resting_bids: 0, resting_asks: 0, pnl, peak_pnl: peak, drawdown: peak - pnl },
        printed, failed: null, intents, notes: [], sent, fuel: 600 + (target !== null ? 4800 : 0) + (k === 0 ? 3000 : 0), rpc_calls: 0, account,
      });
    }
    sim.pnl = cash + pos * sim.steps.at(-1).market.mark;
    return sim;
  }

  const breaks = (s, trace, next) => {
    const b = s.bps || {};
    if ((b.steps || []).includes(trace.k + 1)) return true;
    if (b.onOrders && trace.intents.length) return true;
    if (b.onFill && next && next.account.position_change !== 0) return true;
    return Boolean(b.onTrap && trace.failed);
  };

  function tick(s) {
    if (session !== s || s.state !== "running") return;
    const trace = s.sim.steps[s.k + 1];
    if (!trace) {
      s.state = "terminated";
      const sc = read(s.scenario);
      event("terminated", { scenario: s.scenario, pnl_usd: s.sim.pnl, bound_pnl_usd: 1124.39, reference_pnl_usd: sc.reference_pnl_usd ?? null, disqualified: null, stopped: null, not_graded: null });
      return;
    }
    s.k = trace.k;
    event("step", trace);
    if (s.once || breaks(s, trace, s.sim.steps[s.k + 1])) {
      const reason = s.once ? "step" : "breakpoint";
      s.once = false; s.state = "stopped";
      event("stopped", { reason, k: s.k });
      return;
    }
    if (s.pausing) { s.pausing = false; s.state = "stopped"; event("stopped", { reason: "pause", k: s.k }); return; }
    s.timer = setTimeout(() => tick(s), s.ms);
  }

  async function launch(args) {
    if (session) { clearTimeout(session.timer); session.state = "terminated"; }
    const name = args.scenario || scenarios()[0];
    if (!files.has(`scenarios/${name}.json`)) throw reject(`no scenario ${name}`);
    const built = await build();
    if (!built.ok) throw reject("the strategy doesn't build", { diagnostics: built.diagnostics });
    const sc = read(name);
    const s = (session = { scenario: name, state: "running", k: -1, bps: args.breakpoints || {}, ms: args.msPerStep ?? 0, events: [], args, sim: simulate(sc, built.dip) });
    event("started", { session: { steps: s.sim.n, slots_per_step: 2, start_slot: 462882345, end_slot: 462882345 + sc.duration_slots, symbol: "SPCX", start_mark: s.sim.start, start_oracle: START, cap: s.sim.cap, max_drawdown: 1000, vars: [] }, printed: [`started: SPCX for ${s.sim.n} steps, cap ${s.sim.cap}`], failed: null });
    if (args.stopOnEntry) { s.state = "stopped"; event("stopped", { reason: "entry", k: 0 }); }
    else s.timer = setTimeout(() => tick(s), 0);
    return { scenario: name };
  }

  async function compose(text, images = []) {
    pick.busy = true; cancelled = false;
    const say = (id, role, chunk, done = false) => {
      const m = messages.find((x) => x.id === id) || messages[messages.push({ id, role, text: "" }) - 1];
      m.text += chunk; m.done = done;
      event("composer/message", { id, role, text: chunk, done });
    };
    const tool = (id, st, call) => {
      const t = messages.find((x) => x.id === id) || messages[messages.push({ id, tool: true }) - 1];
      Object.assign(t, { kind: null, input: {}, output: "", diffs: [], locations: [], ...call, id, status: st });
      event("composer/tool", { ...t });
    };
    const stream = async (id, role, words) => { for (const w of words.split(/(?<= )/)) { if (cancelled) break; say(id, role, w); await sleep(24); } say(id, role, "", true); };
    const id = `m${++mid}`;
    const echo = messages[messages.push({ id, role: "user", text, done: true, images: images.length }) - 1];
    event("composer/message", echo);
    const reply = `m${++mid}`, thought = `m${++mid}`;
    await sleep(300);
    await stream(thought, "thought", "The strategy buys once the mark is DIP_PCT below the start and sells at half the way back. Let me look at the file before changing anything. ");
    const number = text.match(/(\d+(?:\.\d+)?)\s*%?/);
    if (!cancelled && /dip|buy|threshold|%/i.test(text) && number) {
      const run = "r" + mid, before = files.get("src/lib.rs");
      const read = { run, title: "Read src/lib.rs", kind: "read", summary: "Read lib.rs", runSummary: "Read lib.rs", input: { file_path: "/mock/bot/src/lib.rs" } };
      tool("t" + mid, "in_progress", read); await sleep(400);
      tool("t" + mid, "completed", { ...read, output: before.split("\n").slice(0, 14).join("\n") + "\n…" });
      const next = before.replace(/const DIP_PCT: f64 = [^;]*;/, `const DIP_PCT: f64 = ${Number(number[1]).toFixed(1)};`);
      const edit = { run, title: "Edit src/lib.rs", kind: "edit", summary: "Edit lib.rs", runSummary: "Read lib.rs, edit lib.rs, run scenario check", input: { file_path: "/mock/bot/src/lib.rs" }, diffs: [{ path: "/mock/bot/src/lib.rs", old: before, new: next }] };
      tool("e" + mid, "in_progress", edit); await sleep(500);
      files.set("src/lib.rs", next);
      event("file/changed", { path: "src/lib.rs", version: hash(next), text: next, by: "agent" });
      tool("e" + mid, "completed", edit);
      const check = { run, title: "mtx scenario check scenarios/*.json", kind: "execute", summary: "Run scenario check", runSummary: edit.runSummary, input: { command: "mtx scenario check scenarios/*.json", description: "Check the scenarios still load" } };
      tool("c" + mid, "in_progress", check); await sleep(500);
      tool("c" + mid, "completed", { ...check, output: "scenarios/practice-101.json: ok, 5 steps\nscenarios/practice-102.json: ok, 10 steps" });
      await stream(reply, "agent", `I set \`DIP_PCT\` to **${Number(number[1]).toFixed(1)}** in \`src/lib.rs\`: the strategy now waits for a ${Number(number[1]).toFixed(1)}% fall before buying.\n\nPress play to see where it buys on this scenario. A deeper threshold skips the small dips, and misses the shocks that stop short of it.`);
    } else if (!cancelled && images.length) {
      await stream(reply, "agent", `I can see ${images.length === 1 ? "the screenshot" : `${images.length} screenshots`}. A real agent would read the market off the chart, write a scenario that plays it, and sketch a strategy for it; this mock only shows how that turn looks.`);
      say(`m${++mid}`, "studio", "mtx refused a scenario:\n- scenarios/from-screenshot.json: a 5% move needs two steps, and the next waypoint comes one step later", true);
      await stream(`m${++mid}`, "agent", "Fixed: the waypoint after the 5% move now waits for it to land.");
    } else if (!cancelled) {
      await stream(reply, "agent", "This is the studio's mock agent: it only knows one trick. Ask it to *buy at a 2% dip* and it edits `DIP_PCT` in `src/lib.rs`.\n\n```rust\nconst DIP_PCT: f64 = 2.0;\n```\n\nWith a real agent (Claude Code or Codex over ACP), the message goes with the file, the scenario and the step on screen.");
    }
    pick.busy = false;
    return {};
  }

  async function handle(command, a) {
    switch (command) {
      case "initialize": return { workspace: "/mock/bot", task: "flash-crash-spcx", files: [...files.keys()].sort(), scenarios: scenarios(), defaultScenario: "practice-101", agent: pick, session: status() };
      case "session/history": return { events: session ? session.events : [] };
      case "composer/history": return { messages };
      case "file/read": {
        if (!files.has(a.path)) throw reject(`${a.path}: no such file`);
        return { text: files.get(a.path), version: hash(files.get(a.path)) };
      }
      case "file/write": {
        const current = files.get(a.path);
        if (a.baseVersion && current !== undefined && hash(current) !== a.baseVersion) throw reject("conflict", { conflict: true, version: hash(current), text: current });
        if (a.path.startsWith("scenarios/") && a.path.endsWith(".json")) {
          try { const sc = JSON.parse(a.text); if (!Array.isArray(sc.path) || !sc.duration_slots) throw new Error("missing field `path` or `duration_slots`"); }
          catch (e) {
            const at = Number((String(e.message).match(/position (\d+)/) || [])[1] || 0);
            const before = a.text.slice(0, at).split("\n");
            throw reject(`${a.path} isn't a scenario: ${e.message}`, { diagnostics: [{ file: a.path, line: before.length, column: before.at(-1).length + 1, endLine: before.length, endColumn: before.at(-1).length + 2, severity: "error", message: String(e.message) }] });
          }
        }
        files.set(a.path, a.text);
        return { version: hash(a.text) };
      }
      case "scenario/read": {
        if (!files.has(`scenarios/${a.name}.json`)) throw reject(`no scenario ${a.name}`);
        const sc = read(a.name), lv = levels(sc);
        const real = session && session.scenario === a.name;
        const start = real ? session.sim.start : START;
        return { scenario: sc, steps: lv.length, slots_per_step: 2, start_mark_usd: start, start_mark_estimated: !real, plan: lv.map((level_pct, k) => ({ k, level_pct, mark_usd: Math.round(start * (1 + level_pct / 100) * 100) / 100 })), takers: [] };
      }
      case "build": { const b = await build(); return { ok: b.ok, diagnostics: b.diagnostics }; }
      case "launch": return launch(a);
      case "restart": if (!session) throw reject("no session to restart"); return launch(session.args);
      case "continue": case "pause": case "next": case "terminate": case "setBreakpoints": case "setSpeed": {
        const s = session;
        if (!s) throw reject("no session: launch one");
        if (command === "setBreakpoints") s.bps = a;
        if (command === "setSpeed") s.ms = a.msPerStep ?? 0;
        if (command === "pause" && s.state === "running") s.pausing = true;
        if (command === "terminate" && s.state !== "terminated") { clearTimeout(s.timer); s.state = "terminated"; event("terminated", { scenario: s.scenario, pnl_usd: s.sim.steps[Math.max(0, s.k)].account.pnl_usd, bound_pnl_usd: 1124.39, reference_pnl_usd: read(s.scenario).reference_pnl_usd ?? null, disqualified: null, stopped: "terminated", not_graded: "the session was stopped" }); }
        if ((command === "continue" || command === "next") && s.state === "stopped") {
          s.once = command === "next"; s.state = "running";
          if (command === "continue") event("continued", {});
          s.timer = setTimeout(() => tick(s), 0);
        }
        return status();
      }
      case "agents/list": return {
        selected: pick,
        agents: [
          { id: "claude-code", name: "Claude Code", available: true, reason: null, adapter: "claude-agent-acp", via: "npx", models: [{ id: "", name: "default" }, { id: "claude-opus-5-5", name: "claude-opus-5-5" }, { id: "claude-sonnet-5-5", name: "claude-sonnet-5-5" }] },
          { id: "codex", name: "Codex", available: true, reason: null, adapter: "codex-acp", via: "path", models: [{ id: "", name: "default" }] },
          { id: "gemini", name: "Gemini CLI", available: false, reason: "gemini isn't on PATH", models: [] },
        ],
      };
      case "agents/select": pick = { ...pick, agent: a.agent, model: a.model || null }; return pick;
      case "composer/send": return compose(a.text, a.images || []);
      case "composer/cancel": cancelled = true; return {};
      default: throw reject(`unknown command ${command}`);
    }
  }

  return {
    post(request) {
      handle(request.command, request.arguments || {}).then(
        (body) => send({ type: "response", request_seq: request.seq, command: request.command, success: true, body }),
        (error) => send({ type: "response", request_seq: request.seq, command: request.command, success: false, message: error.message, body: error.body || {} }),
      );
    },
  };
}
