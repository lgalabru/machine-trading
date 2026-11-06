// Models for the arbitrage guide's labs (arbitrage.md), drawn by
// specs/site/guides/mt.js. A teaching model: slippage is linear in size,
// margin is isolated to the perp leg, and funding accrues every hour.

MT.register("arb.spread", (p) => {
  const price = 100;
  // Fees on both venues, per SPCX.
  const feeUnit = 2 * (p.fee / 10000) * price;
  // Each venue's price moves k dollars against you per SPCX you trade, so the
  // average unit costs k × size / 2 on each venue, k × size on both.
  const k = p.depth / 100 / 10;
  const edge = p.gap - feeUnit;
  const profit = (q) => q * edge - k * q * q;
  const best = edge > 0 ? edge / (2 * k) : 0;
  const breakEven = edge > 0 ? edge / k : 0;
  const mine = profit(p.size);
  const xs = MT.range(0, 400, 201);
  return {
    stats: [
      { label: "Gap, per SPCX", value: p.gap, format: "usd" },
      { label: "Fees, per SPCX", value: feeUnit, format: "usd", hint: `${MT.format("bp", p.fee)} on each venue` },
      { label: "Best size", value: best > 0 ? `${MT.format("num", Math.round(best))} SPCX` : "don't trade", hint: best > 0 ? `for ${MT.format("usd", profit(best))}` : "the fees are bigger than the gap", tone: best > 0 ? "good" : "bad" },
      { label: `Trading ${MT.format("num", p.size)} SPCX`, value: mine, format: "+usd", tone: mine > 0 ? "good" : mine < 0 ? "bad" : undefined, hint: breakEven > 0 ? `loses money past ${MT.format("num", Math.round(breakEven))} SPCX` : "" },
    ],
    panels: [
      {
        x: { label: "SPCX traded", format: "num" },
        y: { label: "Profit", format: "+usd" },
        series: [{ name: "Profit", points: xs.map((q) => [q, profit(q)]), fill: "sign" }],
        rules: [
          { axis: "x", at: p.size, label: `You: ${MT.format("num", p.size)}` },
          ...(best > 0 ? [{ axis: "x", at: best, label: "Best size", tone: "good" }] : []),
        ],
        dots: [{ x: p.size, y: mine, tone: "s1" }],
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

// SPCX over 30 days (720 hours), relative to 1.
const PATHS = {
  flat: () => 1,
  rally: (h) => 1 + 0.3 * (h / 720),
  crash: (h) => 1 - 0.3 * (h / 720),
  spike: (h) => (h < 240 ? 1 : h < 264 ? 1 + 0.25 * ((h - 240) / 24) : h < 288 ? 1.25 - 0.25 * ((h - 264) / 24) : 1),
};

MT.register("arb.carry", (p) => {
  const { capital, entry, leverage } = p;
  const mm = p.maintenance / 100;
  const rate = p.rate / 10000;
  // Equal legs: q SPCX bought spot, q SPCX short on the perp, from the capital.
  const q = capital / (entry * (1 + 1 / leverage));
  const margin = (q * entry) / leverage;
  const jitter = noise(11 + p.path.length);
  const price = [];
  const spot = [];
  const short = [];
  const fund = [];
  const total = [];
  const equity = [];
  const required = [];
  let walk = 0;
  let funding = 0;
  let liquidated = null;
  let closest = Infinity;
  for (let h = 0; h <= 720; h += 2) {
    if (h > 0) walk += jitter() * 0.006;
    const px = entry * (PATHS[p.path](h) + walk);
    price.push([h / 24, px]);
    let shortLeg;
    if (liquidated === null) {
      if (h > 0) funding += rate * q * px * 2;
      shortLeg = q * (entry - px);
      const eq = margin + shortLeg + funding;
      const need = mm * q * px;
      closest = Math.min(closest, eq - need);
      equity.push([h / 24, eq]);
      required.push([h / 24, need]);
      if (eq <= need) liquidated = { day: h / 24, px, funding };
    }
    if (liquidated !== null) {
      // The leg is closed and its margin, funding included, is gone.
      shortLeg = -margin - liquidated.funding;
      if (equity.at(-1)[0] !== h / 24) {
        equity.push([h / 24, 0]);
        required.push([h / 24, 0]);
      }
    }
    const spotLeg = q * (px - entry);
    spot.push([h / 24, spotLeg]);
    short.push([h / 24, shortLeg]);
    fund.push([h / 24, liquidated ? liquidated.funding : funding]);
    total.push([h / 24, spotLeg + shortLeg + (liquidated ? liquidated.funding : funding)]);
  }
  const end = total.at(-1)[1];
  const earned = fund.at(-1)[1];
  const liqPrice = (entry * (1 + 1 / leverage)) / (1 + mm);
  const note = liquidated
    ? `The short leg is liquidated on day ${MT.format("num", Math.round(liquidated.day * 10) / 10)}, at ${MT.format("price", liquidated.px)}, and its ${MT.format("usd0", margin)} of margin is gone. From then on you hold ${MT.format("num", Math.round(q * 10) / 10)} SPCX unhedged: your result follows SPCX's price, the bet this trade was meant to avoid.`
    : earned >= 0
      ? `Both legs stand all month. SPCX's moves cancel, and the result is the funding collected${Math.abs(end - earned) > 1 ? ", give or take the legs' small mismatch" : ""}.`
      : "Both legs stand, but funding ran the wrong way: the short paid it every hour.";
  return {
    stats: [
      { label: "Each leg", value: `${MT.format("num", Math.round(q * 10) / 10)} SPCX`, hint: `${MT.format("usd0", q * entry)} spot, ${MT.format("usd0", margin)} margin for the short` },
      { label: "Funding collected", value: earned, format: "+usd", tone: earned > 0 ? "good" : earned < 0 ? "bad" : undefined },
      { label: "After 30 days", value: end, format: "+usd", tone: end > 0 ? "good" : end < 0 ? "bad" : undefined, hint: `${MT.format("+pct", end / capital)} on your ${MT.format("usd0", capital)}` },
      {
        label: "The short leg",
        value: liquidated ? "liquidated" : "survived",
        tone: liquidated ? "bad" : closest < 0.3 * margin ? "warn" : "good",
        hint: liquidated ? `on day ${MT.format("num", Math.round(liquidated.day))}` : `liquidated above about ${MT.format("price", liqPrice)}`,
      },
    ],
    note,
    panels: [
      {
        title: "SPCX",
        height: 170,
        x: { label: "Day", format: "d" },
        y: { label: "SPCX", format: "price", zero: false },
        series: [{ name: "SPCX", points: price }],
        rules: [
          { axis: "y", at: entry, label: "Entry" },
          ...(liqPrice < entry * 1.45 ? [{ axis: "y", at: liqPrice, label: "Short leg liquidated", tone: "danger" }] : []),
        ],
        dots: liquidated ? [{ x: liquidated.day, y: liquidated.px, label: "Liquidated", tone: "danger" }] : [],
      },
      {
        title: "Your PnL, leg by leg",
        height: 220,
        x: { label: "Day", format: "d" },
        y: { label: "PnL", format: "+usd" },
        series: [
          { name: "Spot SPCX", points: spot },
          { name: "Short perp", points: short, color: "s2" },
          { name: "Funding", points: fund, color: "s3" },
          { name: "Total", points: total, color: "ink", dash: true },
        ],
      },
      {
        title: "The short leg's margin",
        height: 170,
        x: { label: "Day", format: "d" },
        y: { label: "Equity on the perp venue", format: "usd0" },
        series: [
          { name: "Equity", points: equity, color: "s2" },
          { name: "Maintenance margin", points: required, color: "muted", dash: true },
        ],
      },
    ],
  };
});
