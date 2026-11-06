// Models for the selling-a-large-position guide (selling-a-large-position.md),
// drawn by specs/site/guides/mt.js. A teaching model of execution: a sale
// pushes the price down for a while (temporary impact, growing with the speed)
// and for good (permanent impact, growing with the total), while the market
// moves on its own. The numbers are illustrative, not fitted to a venue.

const Q = 30000; // SOL to sell
const P0 = 100; // the price when the sale starts
const NOTIONAL = Q * P0;
const bp = (usd) => (usd / NOTIONAL) * 10000;

MT.register("exec.speed", (p) => {
  // Temporary impact: selling at r SOL an hour costs eta * r per SOL, so a
  // sale spread evenly over T hours costs eta * Q^2 / T. At normal depth,
  // selling everything in one hour costs 1%.
  const eta = 0.01 * P0 / Q / p.depth;
  const gamma = (0.005 * P0) / Q; // permanent: the whole sale moves SOL 0.5%
  const muH = p.drift / 100 / 24; // the market's own drift, per hour
  const sigH = p.vol / 100 / Math.sqrt(24);
  const impact = (T) => (eta * Q * Q) / T + (gamma * Q * Q) / 2;
  const drift = (T) => -muH * (T / 2) * NOTIONAL;
  const expected = (T) => impact(T) + drift(T);
  const risk = (T) => sigH * Math.sqrt(T / 3) * NOTIONAL;
  const xs = MT.range(0.25, 48, 192);
  const best = xs.reduce((a, T) => (expected(T) + risk(T) < expected(a) + risk(a) ? T : a), xs[0]);
  const T = p.hours;
  return {
    stats: [
      { label: "Impact", value: impact(T), format: "usd0", hint: `${MT.format("bp", Math.round(bp(impact(T))))}: your own selling` },
      { label: "Market drift", value: drift(T), format: "+usd", hint: p.drift === 0 ? "none in this market" : p.drift < 0 ? "SOL falls while you wait" : "SOL rises while you wait: a gain" },
      { label: "Expected cost", value: expected(T), format: "usd0", tone: expected(T) <= expected(best) * 1.15 ? "good" : "warn", hint: `${MT.format("bp", Math.round(bp(expected(T))))} of $3,000,000` },
      { label: "Risk, one bad day", value: risk(T), format: "usd0", hint: "one standard deviation of the result" },
      { label: "Best, with risk counted", value: MT.format("h", Math.round(best * 4) / 4), hint: "the hours that minimise cost + risk" },
    ],
    panels: [
      {
        x: { label: "Hours to sell 30,000 SOL", format: "h" },
        y: { label: "Cost", format: "usd0", min: Math.min(0, ...xs.map(drift)) },
        series: [
          { name: "Impact", points: xs.map((x) => [x, impact(x)]) },
          { name: "Market drift", points: xs.map((x) => [x, drift(x)]), color: "s3" },
          { name: "Expected cost", points: xs.map((x) => [x, expected(x)]), color: "ink", dash: true },
          { name: "Expected cost + risk", points: xs.map((x) => [x, expected(x) + risk(x)]), color: "s2", dash: true },
        ],
        rules: [
          { axis: "x", at: T, label: `You: ${MT.format("h", T)}` },
          { axis: "x", at: best, label: "Best", tone: "good" },
        ],
      },
    ],
  };
});

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

// SOL over a 120-block session, relative to the price at the start.
const BLOCKS = 120;
const REGIMES = {
  calm: () => 1,
  fall: (t) => 1 - 0.08 * (t / BLOCKS),
  climb: (t) => 1 + 0.06 * (t / BLOCKS),
  crash: (t) => (t < 30 ? 1 : t < 35 ? 1 - 0.1 * ((t - 30) / 5) : 0.9 + 0.05 * (1 - Math.exp(-(t - 35) / 20))),
  shakeout: (t) => (t < 25 ? 1 - 0.05 * (t / 25) : 0.95 + 0.1 * ((t - 25) / (BLOCKS - 25))),
};

// Each strategy: how much it wants to sell in block t, given what it has left
// and the price it sees.
const TWAP = Q / BLOCKS;
const STRATEGIES = [
  { name: "All at once", color: "s1", steady: 0, want: (t, left) => left },
  { name: "Evenly (TWAP)", color: "s2", steady: 1, want: (t, left) => Math.min(left, TWAP) },
  { name: "Front-loaded", color: "s3", steady: 1, want: (t, left) => Math.min(left, 2 * TWAP) },
  {
    name: "Adaptive",
    color: "ink",
    steady: 0.35,
    want: (t, left, px) => {
      if (t >= BLOCKS - 20) return left / (BLOCKS - t);
      const pace = px < P0 * 0.995 ? 2 : px > P0 * 1.005 ? 0.35 : 1;
      return Math.min(left, pace * TWAP);
    },
  },
];

MT.register("exec.session", (p) => {
  const floor = P0 * (1 - p.floor / 100);
  // Square-root impact: selling q SOL in one block costs c * sqrt(q / D) per
  // SOL on average, and the last SOL twice that, where D is the book's depth.
  // At normal depth, all 30,000 at once average 3% under the mid.
  const D = 1000 * p.depth;
  const c = 3 / Math.sqrt(Q / 1000);
  const avgCost = (q) => c * Math.sqrt(q / D);
  const gamma = (0.01 * P0) / Q; // the whole sale moves SOL 1% for good
  const reader = p.reader === "on" ? 0.003 * P0 : 0; // what a front-runner takes per SOL from a steady seller: 30 bp
  const jitter = noise(5 + p.regime.length);
  const base = [];
  let walk = 0;
  for (let t = 0; t < BLOCKS; t++) {
    if (t > 0) walk += jitter() * 0.003;
    base.push(P0 * (REGIMES[p.regime](t) + walk));
  }
  const runs = STRATEGIES.map((s) => {
    let left = Q;
    let sold = 0;
    let proceeds = 0;
    const leftPts = [];
    for (let t = 0; t < BLOCKS; t++) {
      const mid = base[t] - gamma * sold;
      let q = Math.max(0, Math.min(left, s.want(t, left, mid)));
      // Never a fill below the floor: sell only down to it.
      if (mid - 2 * avgCost(q) < floor) q = Math.max(0, Math.min(q, D * ((mid - floor) / (2 * c)) ** 2));
      const readCost = t >= 10 ? reader * s.steady : 0;
      const avg = mid - avgCost(q) - readCost;
      proceeds += q * avg;
      sold += q;
      left -= q;
      leftPts.push([t, left]);
    }
    const close = base[BLOCKS - 1] - gamma * sold;
    const residual = left * close * 0.97; // what's unsold is sold later, 3% under the close
    const shortfall = bp(NOTIONAL - proceeds - residual);
    return { ...s, leftPts, shortfall, left };
  });
  const best = runs.reduce((a, r) => (r.shortfall < a.shortfall ? r : a));
  const unsold = runs.filter((r) => r.left > 1);
  return {
    stats: runs.map((r) => ({
      label: r.name,
      value: `${MT.format("num", Math.round(r.shortfall))} bp`,
      tone: r === best ? "good" : r.shortfall > best.shortfall + 50 ? "bad" : undefined,
      hint: r.left > 1 ? `${MT.format("num", Math.round(r.left))} SOL unsold` : "all sold",
    })),
    note: `Best here: ${best.name}, ${MT.format("num", Math.abs(Math.round(best.shortfall)))} bp ${best.shortfall < 0 ? "above" : "under"} the starting price.${unsold.length ? ` ${unsold.map((r) => r.name).join(" and ")} left SOL unsold, valued 3% under the close.` : ""}`,
    panels: [
      {
        title: "SOL",
        height: 180,
        x: { label: "Round", format: "num" },
        y: { label: "SOL, before your selling", format: "price", zero: false },
        series: [{ name: "SOL", points: base.map((v, t) => [t, v]) }],
        rules: [
          { axis: "y", at: P0, label: "Arrival: $100" },
          { axis: "y", at: floor, label: "Floor: min_sell_price", tone: "danger" },
        ],
      },
      {
        title: "SOL left to sell",
        height: 200,
        x: { label: "Round", format: "num" },
        y: { label: "SOL left", format: "num", min: 0 },
        series: runs.map((r) => ({ name: r.name, points: r.leftPts, color: r.color, dash: r.color === "ink" })),
      },
    ],
  };
});
