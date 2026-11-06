// The guide pages' interactive figures ("labs").
//
// A lab is written in a guide's Markdown as a ```lab block holding JSON: a
// model name, its controls (sliders and choices) and presets. build.mjs turns
// it into a <figure class="mt-lab"> carrying that JSON. This file renders
// every lab on the page: the controls, the readouts, and one or more charts
// that share their x axis and crosshair.
//
// A model is a pure function from the controls' values to what to draw:
//
//   MT.register("perp.payoff", (p) => ({
//     stats: [{ label, value, tone?, hint? }],
//     note: "plain text under the readouts",
//     panels: [{
//       title, height?,
//       x: { label, format }, y: { label, format, zero? },
//       series: [{ name, points: [[x, y], ...], color?, fill?: "sign", dash? }],
//       rules:  [{ axis: "x" | "y", at, label, tone? }],
//       bands:  [{ axis: "x" | "y", from, to, tone, label? }],
//       dots:   [{ x, y, label, tone? }],
//     }],
//   }));
//
// Models know nothing about the DOM, so a new instrument only writes models.
// See specs/site/guides/README.md.

(() => {
  const SVG = "http://www.w3.org/2000/svg";
  const models = {};

  // Formats, by name, for axes, readouts and tooltips. Percentages are
  // fractions: 0.05 is 5%.
  const group = (v, digits) =>
    Math.abs(v).toLocaleString("en-US", { minimumFractionDigits: digits, maximumFractionDigits: digits });
  const FORMATS = {
    num: (v) => group(v, Math.abs(v) < 10 && v % 1 ? 2 : 0).replace(/^/, v < 0 ? "−" : ""),
    usd: (v) => `${v < 0 ? "−" : ""}$${group(v, Math.abs(v) >= 1000 ? 0 : 2)}`,
    usd0: (v) => `${v < 0 ? "−" : ""}$${group(v, 0)}`,
    "+usd": (v) => `${v > 0 ? "+" : v < 0 ? "−" : ""}$${group(v, Math.abs(v) >= 1000 ? 0 : 2)}`,
    price: (v) => `$${group(v, 2)}`,
    pct: (v) => `${v < 0 ? "−" : ""}${group(v * 100, 1)}%`,
    pct0: (v) => `${v < 0 ? "−" : ""}${group(v * 100, 0)}%`,
    "+pct": (v) => `${v > 0 ? "+" : v < 0 ? "−" : ""}${group(v * 100, 1)}%`,
    bp: (v) => `${v < 0 ? "−" : ""}${group(v, Math.abs(v) % 1 ? 2 : 0)} bp`,
    x: (v) => `${group(v, v % 1 ? 1 : 0)}×`,
    h: (v) => `${group(v, v % 1 ? 1 : 0)} h`,
    d: (v) => `${group(v, v % 1 ? 1 : 0)} d`,
  };
  const format = (spec) => (typeof spec === "function" ? spec : FORMATS[spec] ?? FORMATS.num);

  /** Round tick values covering [min, max]. */
  function ticks(min, max, count) {
    const raw = (max - min) / count;
    const mag = 10 ** Math.floor(Math.log10(raw));
    const err = raw / mag;
    const step = (err >= 7.5 ? 10 : err >= 3.5 ? 5 : err >= 1.5 ? 2 : 1) * mag;
    const out = [];
    for (let v = Math.ceil(min / step) * step; v <= max + step * 1e-9; v += step) out.push(+v.toPrecision(12));
    return out;
  }

  function el(name, attrs = {}, parent) {
    const node = document.createElementNS(SVG, name);
    for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
    parent?.appendChild(node);
    return node;
  }
  function html(tag, cls, parent, text) {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined) node.textContent = text;
    parent?.appendChild(node);
    return node;
  }

  /** The point of a sorted series nearest to x. */
  function nearest(points, x) {
    let lo = 0;
    let hi = points.length - 1;
    while (hi - lo > 1) {
      const mid = (lo + hi) >> 1;
      if (points[mid][0] < x) lo = mid;
      else hi = mid;
    }
    return Math.abs(points[lo][0] - x) <= Math.abs(points[hi][0] - x) ? points[lo] : points[hi];
  }

  const colorOf = (series, i) => `var(--mt-${series.color ?? `s${i + 1}`})`;
  const toneOf = (tone) => `var(--mt-${tone ?? "muted"})`;

  /**
   * Draw one panel into `box`. Returns its crosshair: show(x) and hide(), so
   * the lab can keep its panels' crosshairs together.
   */
  function drawPanel(box, panel, onHover, uid) {
    box.replaceChildren();
    if (panel.title) html("div", "mt-panel-title", box, panel.title);
    const frame = html("div", "mt-frame", box);
    const W = Math.max(frame.clientWidth || box.clientWidth || 600, 280);
    const H = panel.height ?? (W < 500 ? 200 : 240);
    const m = { top: panel.y.label ? 24 : 14, right: 14, bottom: 34, left: 58 };
    const iw = W - m.left - m.right;
    const ih = H - m.top - m.bottom;
    const fx = format(panel.x.format);
    const fy = format(panel.y.format);

    // Domains: the data, the rules, and the finite ends of the bands.
    const xs = [];
    const ys = [];
    for (const s of panel.series) for (const [x, y] of s.points) xs.push(x), ys.push(y);
    for (const r of panel.rules ?? []) (r.axis === "x" ? xs : ys).push(r.at);
    for (const d of panel.dots ?? []) xs.push(d.x), ys.push(d.y);
    if (panel.y.zero !== false) ys.push(0);
    for (const b of panel.bands ?? [])
      for (const v of [b.from, b.to]) if (Number.isFinite(v) && b.axis === "y") ys.push(v);
    let [x0, x1] = [panel.x.min ?? Math.min(...xs), panel.x.max ?? Math.max(...xs)];
    let [y0, y1] = [panel.y.min ?? Math.min(...ys), panel.y.max ?? Math.max(...ys)];
    if (x1 === x0) x1 = x0 + 1;
    if (y1 === y0) (y0 -= 1), (y1 += 1);
    const pad = (y1 - y0) * 0.08;
    if (panel.y.min === undefined) y0 -= pad;
    if (panel.y.max === undefined) y1 += pad;
    const X = (v) => m.left + ((v - x0) / (x1 - x0)) * iw;
    const Y = (v) => m.top + (1 - (v - y0) / (y1 - y0)) * ih;
    const clampX = (v) => Math.min(Math.max(v, x0), x1);
    const clampY = (v) => Math.min(Math.max(v, y0), y1);

    const svg = el("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H, role: "img" }, frame);
    el("title", {}, svg).textContent = panel.title ?? panel.y.label ?? "";
    const defs = el("defs", {}, svg);
    const hatch = el("pattern", { id: `${uid}-hatch`, width: 6, height: 6, patternUnits: "userSpaceOnUse", patternTransform: "rotate(45)" }, defs);
    el("line", { x1: 0, y1: 0, x2: 0, y2: 6, class: "mt-hatch" }, hatch);
    const clip = (id, y, h) => el("rect", { x: m.left, y, width: iw, height: Math.max(h, 0) }, el("clipPath", { id }, defs));
    const yZero = Y(clampY(0));
    clip(`${uid}-plot`, m.top, ih);
    clip(`${uid}-above`, m.top, yZero - m.top);
    clip(`${uid}-below`, yZero, m.top + ih - yZero);

    // Bands, behind everything.
    for (const b of panel.bands ?? []) {
      const lo = Math.max(Math.min(b.from, b.to), b.axis === "x" ? x0 : y0);
      const hi = Math.min(Math.max(b.from, b.to), b.axis === "x" ? x1 : y1);
      if (hi <= lo) continue;
      const r =
        b.axis === "x"
          ? { x: X(lo), y: m.top, width: X(hi) - X(lo), height: ih }
          : { x: m.left, y: Y(hi), width: iw, height: Y(lo) - Y(hi) };
      el("rect", { ...r, class: `mt-band mt-band-${b.tone ?? "muted"}` }, svg);
      if (b.tone === "danger") el("rect", { ...r, fill: `url(#${uid}-hatch)`, class: "mt-band-hatch" }, svg);
      // A band too thin for its label leaves it to the rule beside it.
      if (b.label && r.height >= 18 && r.width >= 120) {
        const t = el("text", { class: "mt-band-label", x: r.x + 6, y: r.y + 14 }, svg);
        t.textContent = b.label;
      }
    }

    // Grid and axes.
    const yt = ticks(y0, y1, H < 220 ? 3 : 4);
    for (const v of yt) {
      el("line", { x1: m.left, x2: m.left + iw, y1: Y(v), y2: Y(v), class: v === 0 ? "mt-zero" : "mt-grid" }, svg);
      el("text", { x: m.left - 8, y: Y(v) + 4, class: "mt-tick", "text-anchor": "end" }, svg).textContent = fy(v);
    }
    for (const v of ticks(x0, x1, Math.max(2, Math.floor(iw / 90)))) {
      el("line", { x1: X(v), x2: X(v), y1: m.top + ih, y2: m.top + ih + 4, class: "mt-axis" }, svg);
      el("text", { x: X(v), y: m.top + ih + 18, class: "mt-tick", "text-anchor": "middle" }, svg).textContent = fx(v);
    }
    el("line", { x1: m.left, x2: m.left + iw, y1: m.top + ih, y2: m.top + ih, class: "mt-axis" }, svg);
    if (panel.x.label)
      el("text", { x: m.left + iw, y: H - 2, class: "mt-axis-label", "text-anchor": "end" }, svg).textContent = panel.x.label;
    if (panel.y.label)
      el("text", { x: 4, y: 11, class: "mt-axis-label" }, svg).textContent = panel.y.label;

    // Series: a signed fill under the line if asked, then the line.
    const plot = el("g", { "clip-path": `url(#${uid}-plot)` }, svg);
    panel.series.forEach((s, i) => {
      if (s.points.length < 2) return;
      const d = s.points.map(([x, y], j) => `${j ? "L" : "M"}${X(x).toFixed(1)},${Y(y).toFixed(1)}`).join("");
      if (s.fill === "sign") {
        const area = `${d}L${X(s.points.at(-1)[0])},${yZero}L${X(s.points[0][0])},${yZero}Z`;
        el("path", { d: area, class: "mt-fill-profit", "clip-path": `url(#${uid}-above)` }, plot);
        el("path", { d: area, class: "mt-fill-loss", "clip-path": `url(#${uid}-below)` }, plot);
      }
      const line = el("path", { d, class: "mt-line" }, plot);
      line.style.stroke = colorOf(s, i);
      if (s.dash) line.setAttribute("stroke-dasharray", "5 4");
    });

    // Rules: labelled reference lines, staggered so labels don't collide.
    let lastLabelX = -Infinity;
    let stagger = 0;
    const rules = [...(panel.rules ?? [])].sort((a, b) => a.at - b.at);
    for (const r of rules) {
      if (r.axis === "x") {
        if (r.at < x0 || r.at > x1) continue;
        const x = X(r.at);
        el("line", { x1: x, x2: x, y1: m.top, y2: m.top + ih, class: "mt-rule" }, svg).style.stroke = toneOf(r.tone);
        stagger = x - lastLabelX < 90 ? stagger + 1 : 0;
        lastLabelX = x;
        const anchor = x > m.left + iw - 80 ? "end" : "start";
        const t = el("text", { x: x + (anchor === "end" ? -4 : 4), y: m.top + 12 + stagger * 14, class: "mt-rule-label", "text-anchor": anchor }, svg);
        t.textContent = r.label;
      } else {
        if (r.at < y0 || r.at > y1) continue;
        const y = Y(r.at);
        el("line", { x1: m.left, x2: m.left + iw, y1: y, y2: y, class: "mt-rule" }, svg).style.stroke = toneOf(r.tone);
        el("text", { x: m.left + iw - 4, y: y - 4, class: "mt-rule-label", "text-anchor": "end" }, svg).textContent = r.label;
      }
    }

    // Dots: labelled events, such as a liquidation.
    for (const d of panel.dots ?? []) {
      const cx = X(clampX(d.x));
      const cy = Y(clampY(d.y));
      const c = el("circle", { cx, cy, r: 5, class: "mt-dot" }, svg);
      c.style.fill = toneOf(d.tone);
      if (d.label) {
        const anchor = cx > m.left + iw - 100 ? "end" : "start";
        const t = el("text", { x: cx + (anchor === "end" ? -9 : 9), y: cy - 8, class: "mt-dot-label", "text-anchor": anchor }, svg);
        t.textContent = d.label;
      }
    }

    // Legend, when there is more than one series.
    if (panel.series.length > 1) {
      const legend = html("div", "mt-legend", box);
      panel.series.forEach((s, i) => {
        const item = html("span", "mt-legend-item", legend);
        const key = html("span", `mt-key${s.dash ? " mt-key-dash" : ""}`, item);
        key.style.borderColor = colorOf(s, i);
        item.append(s.name);
      });
      box.insertBefore(legend, frame);
    }

    // The crosshair and tooltip.
    const cross = el("g", { class: "mt-cross", visibility: "hidden" }, svg);
    const hair = el("line", { y1: m.top, y2: m.top + ih }, cross);
    const marks = panel.series.map((s, i) => {
      const c = el("circle", { r: 4 }, cross);
      c.style.fill = colorOf(s, i);
      return c;
    });
    const tip = html("div", "mt-tip", frame);
    tip.hidden = true;
    const hit = el("rect", { x: m.left, y: m.top, width: iw, height: ih, class: "mt-hit", tabindex: 0 }, svg);

    function show(xv) {
      if (xv === null || xv < x0 || xv > x1) return hide();
      const px = X(xv);
      cross.setAttribute("visibility", "visible");
      hair.setAttribute("x1", px);
      hair.setAttribute("x2", px);
      tip.replaceChildren();
      html("div", "mt-tip-head", tip, `${panel.x.label ? `${panel.x.label}: ` : ""}${fx(xv)}`);
      panel.series.forEach((s, i) => {
        if (!s.points.length) return;
        const [, yv] = nearest(s.points, xv);
        marks[i].setAttribute("cx", px);
        marks[i].setAttribute("cy", Y(clampY(yv)));
        const row = html("div", "mt-tip-row", tip);
        const key = html("span", "mt-key", row);
        key.style.borderColor = colorOf(s, i);
        html("strong", "", row, fy(yv));
        html("span", "mt-tip-name", row, s.name);
      });
      tip.hidden = false;
      const left = px + 12 + tip.offsetWidth > W ? px - 12 - tip.offsetWidth : px + 12;
      tip.style.left = `${left}px`;
      tip.style.top = `${m.top}px`;
    }
    function hide() {
      cross.setAttribute("visibility", "hidden");
      tip.hidden = true;
    }
    const toX = (event) => {
      const rect = svg.getBoundingClientRect();
      const px = ((event.clientX - rect.left) / rect.width) * W;
      return x0 + ((px - m.left) / iw) * (x1 - x0);
    };
    hit.addEventListener("pointermove", (e) => onHover(toX(e)));
    hit.addEventListener("pointerdown", (e) => onHover(toX(e)));
    hit.addEventListener("pointerleave", () => onHover(null));
    hit.addEventListener("blur", () => onHover(null));
    let keyX = null;
    hit.addEventListener("keydown", (e) => {
      if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
      e.preventDefault();
      const step = (x1 - x0) / 50;
      keyX = Math.min(Math.max((keyX ?? (x0 + x1) / 2) + (e.key === "ArrowLeft" ? -step : step), x0), x1);
      onHover(keyX);
    });
    return { show, hide };
  }

  /** A data table of every panel, sampled, so no value needs hovering. */
  function drawTable(details, panels) {
    const body = details.querySelector(".mt-table-body");
    body.replaceChildren();
    for (const panel of panels) {
      const series = panel.series.filter((s) => s.points.length);
      if (!series.length) continue;
      const fx = format(panel.x.format);
      const fy = format(panel.y.format);
      const table = html("table", "", body);
      if (panel.title) html("caption", "", table, panel.title);
      const head = html("tr", "", html("thead", "", table));
      html("th", "", head, panel.x.label ?? "x");
      for (const s of series) html("th", "", head, s.name);
      const tbody = html("tbody", "", table);
      const base = series[0].points;
      const stride = Math.max(1, Math.floor(base.length / 12));
      for (let i = 0; i < base.length; i += stride) {
        const row = html("tr", "", tbody);
        const xv = base[i][0];
        html("td", "", row, fx(xv));
        for (const s of series) html("td", "", row, fy(nearest(s.points, xv)[1]));
      }
    }
  }

  function control(spec, state, onChange) {
    const wide = spec.type === "choice" && spec.options.length > 3;
    // A choice is a group of buttons: in a <label>, clicking its text would press the first.
    const wrap = html(spec.type === "choice" ? "div" : "label", `mt-control mt-control-${spec.type ?? "range"}${wide ? " mt-control-wide" : ""}`);
    const head = html("span", "mt-control-head", wrap);
    html("span", "mt-control-label", head, spec.label);
    if (spec.type === "choice") {
      const group = html("span", "mt-choice", wrap);
      group.setAttribute("role", "radiogroup");
      const buttons = spec.options.map(([value, label]) => {
        const b = html("button", "", group, label);
        b.type = "button";
        b.setAttribute("role", "radio");
        b.addEventListener("click", () => onChange(spec.id, value));
        return [value, b];
      });
      const sync = () => {
        for (const [value, b] of buttons) b.setAttribute("aria-checked", String(state[spec.id] === value));
      };
      if (spec.hint) html("span", "mt-control-hint", wrap, spec.hint);
      return { node: wrap, sync };
    }
    const out = html("output", "mt-control-value", head);
    const input = html("input", "", wrap);
    Object.assign(input, { type: "range", min: spec.min, max: spec.max, step: spec.step ?? "any" });
    input.addEventListener("input", () => onChange(spec.id, Number(input.value)));
    if (spec.hint) html("span", "mt-control-hint", wrap, spec.hint);
    const f = format(spec.format);
    const sync = () => {
      input.value = state[spec.id];
      out.textContent = f(state[spec.id]);
    };
    return { node: wrap, sync };
  }

  function mount(figure) {
    const config = JSON.parse(figure.querySelector("script[type='application/json']").textContent);
    const model = models[config.model];
    const root = figure.querySelector(".mt-lab-body");
    if (!model) {
      html("p", "mt-error", root, `Unknown model ${config.model}.`);
      return;
    }
    const uid = `mt${Math.random().toString(36).slice(2, 8)}`;
    const defaults = Object.fromEntries(config.controls.map((c) => [c.id, c.value]));
    const state = { ...defaults, ...(config.fixed ?? {}) };
    let hoverX = null;
    let crosshairs = [];
    let activePreset = null;

    const presetRow = config.presets?.length ? html("div", "mt-presets", root) : null;
    const presetNote = config.presets?.length ? html("p", "mt-preset-note", root) : null;
    const presetButtons = (config.presets ?? []).map((preset) => {
      const b = html("button", "", presetRow, preset.label);
      b.type = "button";
      b.addEventListener("click", () => {
        Object.assign(state, defaults, preset.set);
        activePreset = preset;
        update();
      });
      return [preset, b];
    });
    const controls = html("div", "mt-controls", root);
    const syncs = config.controls.map((spec) => {
      const c = control(spec, state, (id, value) => {
        state[id] = value;
        activePreset = null;
        update();
      });
      controls.appendChild(c.node);
      return c.sync;
    });
    const stats = html("div", "mt-stats", root);
    const note = html("p", "mt-note", root);
    const panelsBox = html("div", "mt-panels", root);
    const details = html("details", "mt-table", root);
    html("summary", "", details, "Show the data as a table");
    html("div", "mt-table-body", details);

    const hover = (x) => {
      hoverX = x;
      for (const c of crosshairs) x === null ? c.hide() : c.show(x);
    };

    let result;
    function draw() {
      const panels = result.panels ?? [];
      while (panelsBox.children.length < panels.length) html("div", "mt-panel", panelsBox);
      while (panelsBox.children.length > panels.length) panelsBox.lastChild.remove();
      crosshairs = panels.map((p, i) => drawPanel(panelsBox.children[i], p, hover, `${uid}-${i}`));
      if (hoverX !== null) hover(hoverX);
      if (details.open) drawTable(details, panels);
    }
    function update() {
      for (const sync of syncs) sync();
      for (const [preset, b] of presetButtons) b.setAttribute("aria-pressed", String(preset === activePreset));
      if (presetNote) presetNote.textContent = activePreset?.note ?? config.presetsHint ?? "Pick a case, or move the sliders.";
      result = model(state);
      stats.replaceChildren();
      for (const s of result.stats ?? []) {
        const tile = html("div", `mt-stat${s.tone ? ` mt-stat-${s.tone}` : ""}`, stats);
        html("div", "mt-stat-label", tile, s.label);
        const value = html("div", "mt-stat-value", tile);
        if (s.tone) html("span", "mt-stat-icon", value, { good: "▲ ", bad: "▼ ", warn: "⚠ " }[s.tone] ?? "");
        value.append(typeof s.value === "number" ? format(s.format)(s.value) : s.value);
        if (s.hint) html("div", "mt-stat-hint", tile, s.hint);
      }
      note.textContent = result.note ?? "";
      note.hidden = !result.note;
      draw();
    }
    details.addEventListener("toggle", () => details.open && drawTable(details, result.panels ?? []));
    let width = 0;
    new ResizeObserver(() => {
      if (Math.abs(panelsBox.clientWidth - width) < 2) return;
      width = panelsBox.clientWidth;
      if (result) draw();
    }).observe(panelsBox);
    update();
  }

  const range = (from, to, n) => Array.from({ length: n }, (_, i) => from + ((to - from) * i) / (n - 1));

  window.MT = {
    /** Make a model available to the page's labs. */
    register: (name, fn) => (models[name] = fn),
    /** Format a value with a named format, as the labs do. */
    format: (spec, v) => format(spec)(v),
    /** n evenly spaced values from `from` to `to`. */
    range,
  };
  document.addEventListener("DOMContentLoaded", () => document.querySelectorAll("figure.mt-lab").forEach(mount));
})();
