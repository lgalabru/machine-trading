// Models for the perpetual guide's labs (perp.md). Each is a pure function
// from a lab's control values to what it draws; specs/site/training/mt.js
// draws it. The numbers are a teaching model, not a venue's: margin is
// isolated to the one position, and funding accrues every hour at a fixed
// rate. A venue profile defines the real mark, margin and funding.

/** Where a position's equity falls to its maintenance margin. */
function liquidationPrice(side, entry, leverage, maintenance) {
  return side > 0
    ? (entry * (1 - 1 / leverage)) / (1 - maintenance)
    : (entry * (1 + 1 / leverage)) / (1 + maintenance);
}

const sideOf = (p) => (p.side === "short" ? -1 : 1);

MT.register("perp.payoff", (p) => {
  const s = sideOf(p);
  const { entry, deposit, leverage, exit } = p;
  const mm = p.maintenance / 100;
  const units = (deposit * leverage) / entry;
  const liq = liquidationPrice(s, entry, leverage, mm);
  const liquidated = (price) => (s > 0 ? price <= liq : price >= liq);
  // What the deposit is worth if SPCX ends at `price`, with the perp, and
  // without it: holding the shares when long, holding dollars when short.
  // The figure assumes the price goes there directly.
  const withPerp = (price) => (liquidated(price) ? 0 : deposit + s * units * (price - entry));
  const without = (price) => (s > 0 ? (deposit / entry) * price : deposit);
  const xs = MT.range(50, 150, 401);
  const have = withPerp(exit);
  const plain = without(exit);
  const name = s > 0 ? "Buying SPCX with it" : "Keeping the dollars";
  return {
    stats: [
      { label: "Your position", value: `${s > 0 ? "long" : "short"} ${MT.format("num", Math.round(units * 10) / 10)} SPCX`, hint: `worth ${MT.format("usd0", units * entry)}, on your ${MT.format("usd0", deposit)}` },
      { label: "Each $1 SPCX moves", value: `${MT.format("usd", units)} to you`, hint: `${s > 0 ? "up" : "down"} is a gain` },
      { label: `If SPCX goes to ${MT.format("price", exit)}`, value: have, format: "usd0", tone: liquidated(exit) ? "bad" : have >= deposit ? "good" : "bad", hint: liquidated(exit) ? "liquidated: the margin is gone" : `${MT.format("+pct", have / deposit - 1)} on your dollars` },
      { label: name, value: plain, format: "usd0", hint: s > 0 ? `${MT.format("num", deposit / entry)} SPCX, no leverage` : "you'd have exactly what you started with" },
      { label: "Liquidated if SPCX reaches", value: liq < 0.01 ? "only at $0" : MT.format("price", liq), tone: Math.abs(liq / entry - 1) < 0.1 ? "bad" : undefined, hint: `a ${MT.format("pct", Math.abs(liq / entry - 1))} move ${s > 0 ? "down" : "up"}` },
    ],
    note: leverage * mm >= 1 ? `At ${MT.format("x", leverage)} the margin is below the ${MT.format("num", p.maintenance)}% maintenance margin: the venue would refuse the position, or liquidate it at once.` : "",
    panels: [
      {
        x: { label: "Where SPCX goes", format: "price" },
        y: { label: `What your ${MT.format("usd0", deposit)} becomes`, format: "usd0" },
        series: [
          { name: "With the perp", points: xs.map((x) => [x, withPerp(x)]) },
          { name, points: xs.map((x) => [x, without(x)]), color: "muted", dash: true },
        ],
        rules: [
          { axis: "x", at: entry, label: "Today: $100" },
          { axis: "x", at: liq, label: "Liquidation", tone: "danger" },
          { axis: "y", at: deposit, label: "What you put in" },
        ],
        bands: [{ axis: "x", from: s > 0 ? -Infinity : liq, to: s > 0 ? liq : Infinity, tone: "danger", label: "Liquidated" }],
        dots: [
          { x: exit, y: have, label: MT.format("usd0", have), tone: "s1" },
          { x: exit, y: plain, tone: "muted" },
        ],
      },
    ],
  };
});

MT.register("perp.leverage", (p) => {
  const mm = p.maintenance / 100;
  const move = p.daily / 100;
  const levels = MT.range(1, 50, 197);
  // Past 1 ÷ maintenance, a position is liquidated as soon as it opens.
  const longDist = (L) => Math.max(0, 1 - liquidationPrice(1, 1, L, mm));
  const shortDist = (L) => Math.max(0, liquidationPrice(-1, 1, L, mm) - 1);
  const chosen = longDist(p.leverage);
  // The leverage at which an ordinary day's move liquidates a long.
  const fatal = levels.find((L) => longDist(L) <= move);
  return {
    stats: [
      { label: "Long is liquidated after", value: MT.format("pct", chosen), hint: "a fall of this much", tone: chosen <= move ? "bad" : chosen <= 3 * move ? "warn" : "good" },
      { label: "Short is liquidated after", value: MT.format("pct", shortDist(p.leverage)), hint: "a rise of this much" },
      { label: "Typical days to survive", value: chosen <= move ? "less than 1" : MT.format("num", Math.floor((chosen / move) ** 2)), hint: "a random walk's rough reach (√days)" },
      { label: "Leverage an ordinary day ends", value: fatal ? MT.format("x", Math.round(fatal * 10) / 10) : "over 50×", hint: `a ${MT.format("pct", move)} day, long` },
    ],
    note: p.leverage * mm >= 1 ? `At ${MT.format("x", p.leverage)} the margin is below the ${MT.format("num", p.maintenance)}% maintenance margin: the venue would refuse the position, or liquidate it at once.` : "",
    panels: [
      {
        x: { label: "Leverage", format: "x" },
        y: { label: "Move to liquidation", format: "pct0", min: 0, max: Math.min(1, Math.max(0.6, move * 2)) },
        series: [
          { name: "Long (price falls)", points: levels.map((L) => [L, longDist(L)]) },
          { name: "Short (price rises)", points: levels.map((L) => [L, shortDist(L)]), color: "s2" },
        ],
        rules: [
          { axis: "x", at: p.leverage, label: `You: ${MT.format("x", p.leverage)}` },
          { axis: "y", at: move, label: "An ordinary day's move", tone: "danger" },
        ],
        bands: [{ axis: "y", from: 0, to: move, tone: "danger", label: "Liquidated by an ordinary day" }],
        dots: [{ x: p.leverage, y: chosen, tone: "s1" }],
      },
    ],
  };
});

MT.register("perp.funding", (p) => {
  const s = sideOf(p);
  const notional = p.size * p.price;
  const rate = p.rate / 10000; // per hour, from basis points
  const hours = p.days * 24;
  // Positive funding: longs pay shorts. What this position receives:
  const perHour = -s * rate * notional;
  const pts = MT.range(0, hours, Math.min(400, hours + 1)).map((h) => [h / 24, perHour * h]);
  const total = perHour * hours;
  const apr = rate * 24 * 365;
  const payer = rate === 0 ? "nobody pays" : rate > 0 ? "longs pay shorts" : "shorts pay longs";
  return {
    stats: [
      { label: "Who pays", value: payer },
      { label: "Per day, you", value: perHour * 24, format: "+usd", tone: perHour > 0 ? "good" : perHour < 0 ? "bad" : undefined },
      { label: `Over ${MT.format("d", p.days)}`, value: total, format: "+usd", tone: total > 0 ? "good" : total < 0 ? "bad" : undefined, hint: `${MT.format("+pct", total / notional)} of notional` },
      { label: "Rate, per year", value: MT.format("pct", apr), hint: `${MT.format("bp", p.rate)} an hour × 8,760` },
    ],
    note:
      perHour < 0
        ? `Holding costs you ${MT.format("usd", -perHour * 24)} a day even if the price never moves. To break even on funding alone the price must move ${MT.format("pct", -total / notional)} your way over the ${MT.format("d", p.days)}.`
        : perHour > 0
          ? "You're paid to hold, but only while the crowd stays on the other side. The rate can flip quickly."
          : "",
    panels: [
      {
        x: { label: "Days held", format: "d" },
        y: { label: "Funding received", format: "+usd" },
        series: [{ name: "Funding received", points: pts, fill: "sign" }],
      },
    ],
  };
});

// Price paths for the scenario lab, deterministic so every reader sees the
// same market. Each returns the price, relative to 1, at hour t of 48.
function noise(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296 - 0.5;
  };
}
const PATHS = {
  rally: (t) => 1 + 0.12 * (t / 48),
  chop: (t) => 1 + 0.025 * Math.sin(t / 2.2) + 0.015 * Math.sin(t / 0.9),
  bleed: (t) => 1 - 0.09 * (t / 48),
  "flash-crash": (t) => (t < 16 ? 1 : t < 17 ? 1 - 0.22 * (t - 16) : 0.78 + 0.16 * (1 - Math.exp(-(t - 17) / 5))),
  squeeze: (t) => (t < 20 ? 1 - 0.01 * (t / 20) : t < 23 ? 0.99 + 0.07 * (t - 20) : 1.2 - 0.06 * (1 - Math.exp(-(t - 23) / 8))),
};

MT.register("perp.scenario", (p) => {
  const s = sideOf(p);
  const entry = 100;
  const mm = p.maintenance / 100;
  const margin = (p.size * entry) / p.leverage;
  const rate = p.funding / 10000;
  const jitter = noise(7 + p.path.length);
  const dt = 0.25;
  const price = [];
  const pnl = [];
  const drawdown = [];
  let funding = 0;
  let peak = 0;
  let worst = 0;
  let liquidatedAt = null;
  let breachedAt = null;
  let closed = null; // the PnL locked in at liquidation
  let walk = 0;
  for (let t = 0; t <= 48 + 1e-9; t += dt) {
    walk += jitter() * 0.004;
    const px = entry * (PATHS[p.path](t) + (t === 0 ? 0 : walk));
    price.push([t, px]);
    if (closed === null) {
      if (t > 0) funding -= s * rate * p.size * px * dt;
      const value = s * p.size * (px - entry) + funding;
      // Liquidated once equity reaches the maintenance margin.
      if (p.size > 0 && margin + value <= mm * p.size * px) {
        closed = -margin;
        liquidatedAt = [t, px];
      }
    }
    const value = closed ?? s * p.size * (px - entry) + funding;
    pnl.push([t, value]);
    peak = Math.max(peak, value);
    const dd = peak - value;
    drawdown.push([t, dd]);
    worst = Math.max(worst, dd);
    if (breachedAt === null && dd > p.limit) breachedAt = [t, dd];
  }
  const final = pnl.at(-1)[1];
  const overCap = p.size > p.cap;
  const tolerated = p.size > 0 ? p.limit / p.size : Infinity;
  const stats = [
    { label: "PnL at the end", value: final, format: "+usd", tone: final >= 0 ? "good" : "bad" },
    { label: "Deepest drawdown", value: worst, format: "usd", tone: worst > p.limit ? "bad" : worst > 0.7 * p.limit ? "warn" : "good", hint: `limit ${MT.format("usd0", p.limit)}` },
    { label: "Position cap", value: overCap ? "broken" : "kept", tone: overCap ? "bad" : "good", hint: `${MT.format("num", p.size)} of ${MT.format("num", p.cap)} allowed` },
    { label: "Move the limit tolerates", value: Number.isFinite(tolerated) ? MT.format("price", tolerated) : "any", hint: Number.isFinite(tolerated) ? `${MT.format("pct", tolerated / entry)} against you, from the peak` : "no position" },
    { label: "Funding", value: funding, format: "+usd", hint: `${MT.format("bp", p.funding)} an hour` },
  ];
  const events = [];
  if (overCap) events.push(`the position is over the mandate's cap of ${MT.format("num", p.cap)} from the first slot`);
  if (breachedAt) events.push(`the drawdown passes ${MT.format("usd0", p.limit)} at hour ${MT.format("num", breachedAt[0])}`);
  if (liquidatedAt) events.push(`the venue liquidates the position at hour ${MT.format("num", liquidatedAt[0])}, at ${MT.format("price", liquidatedAt[1])}, and the margin is gone`);
  const note = events.length
    ? `In this run ${events.join("; ")}. ${overCap || breachedAt ? "The benchmark's grader would disqualify the session." : ""}`.trim()
    : "The mandate holds for the whole run.";
  const liq = liquidationPrice(s, entry, p.leverage, mm);
  return {
    stats,
    note,
    panels: [
      {
        title: "Price",
        height: 190,
        x: { label: "Hour", format: "h" },
        y: { label: "Mark price", format: "price", zero: false },
        series: [{ name: "Mark price", points: price }],
        rules: [
          { axis: "y", at: entry, label: "Entry" },
          ...(p.size > 0 && Math.abs(liq / entry - 1) < 0.4 ? [{ axis: "y", at: liq, label: "Liquidation", tone: "danger" }] : []),
        ],
        dots: liquidatedAt ? [{ x: liquidatedAt[0], y: liquidatedAt[1], label: "Liquidated", tone: "danger" }] : [],
      },
      {
        title: "Your PnL",
        height: 170,
        x: { label: "Hour", format: "h" },
        y: { label: "PnL", format: "+usd" },
        series: [{ name: "PnL", points: pnl, fill: "sign" }],
      },
      {
        title: "Drawdown from the running peak",
        height: 170,
        x: { label: "Hour", format: "h" },
        y: { label: "Drawdown", format: "usd0", min: 0, max: Math.max(worst, p.limit) * 1.15 },
        series: [{ name: "Drawdown", points: drawdown, color: "s2" }],
        rules: [{ axis: "y", at: p.limit, label: "max_drawdown_usd", tone: "danger" }],
        bands: [{ axis: "y", from: p.limit, to: Infinity, tone: "danger", label: "Mandate broken" }],
        dots: breachedAt ? [{ x: breachedAt[0], y: breachedAt[1], label: "Breach", tone: "danger" }] : [],
      },
    ],
  };
});
