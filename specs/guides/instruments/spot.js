// Models for the spot guide's labs (spot.md), drawn by specs/site/guides/mt.js.
// A teaching model: the order book is a flat ladder of equal levels around a
// $100 mid, and fees are a flat rate per trade. A venue profile defines the
// real book and fees.

/** Walk a flat ladder: `levels` of `depth` SPCX every `step` dollars, from `best`. */
function walk(size, best, step, depth, dir) {
  let left = size;
  let cost = 0;
  let price = best;
  let i = 0;
  while (left > 1e-9) {
    const take = Math.min(left, depth);
    price = best + dir * step * i;
    cost += take * price;
    left -= take;
    i += 1;
  }
  return { avg: size > 0 ? cost / size : best, last: price, total: cost };
}

MT.register("spot.book", (p) => {
  const mid = 100;
  const dir = p.side === "buy" ? 1 : -1;
  const half = p.spread / 100 / 2;
  const best = mid + dir * half;
  const step = 0.05;
  const fee = p.fee / 10000;
  const market = (q) => walk(q, best, step, p.depth, dir);
  const top = Math.max(p.size * 1.6, p.depth * 4);
  const xs = MT.range(0.5, top, 160);
  const m = market(p.size);
  const slip = (dir * (m.avg - mid)) / mid; // vs mid, against you
  const fees = m.total * fee;
  const pay = p.side === "buy";
  const limitPrice = mid - dir * half; // resting on your own side of the book
  const stats =
    p.order === "market"
      ? [
          { label: pay ? "You pay, on average" : "You receive, on average", value: MT.format("price", m.avg), hint: `per SPCX, for ${MT.format("num", p.size)} SPCX` },
          { label: "Spread and slippage", value: MT.format("bp", Math.round(slip * 1e5) / 10), tone: slip > 0.002 ? "bad" : slip > 0.0005 ? "warn" : "good", hint: `vs the $100 mid: ${MT.format("usd", Math.abs(m.avg - mid) * p.size)}` },
          { label: "Fees", value: fees, format: "usd", hint: `${MT.format("bp", p.fee)} of ${MT.format("usd0", m.total)}` },
          { label: pay ? "Total cost" : "Total received", value: pay ? m.total + fees : m.total - fees, format: "usd" },
          { label: "Last price you reached", value: MT.format("price", m.last), hint: (() => { const n = Math.ceil(p.size / p.depth - 1e-9); return `${MT.format("num", n)} level${n === 1 ? "" : "s"} of the book`; })() },
        ]
      : [
          { label: pay ? "Your bid" : "Your ask", value: MT.format("price", limitPrice), hint: "resting on the book" },
          { label: "If it fills", value: pay ? p.size * limitPrice * (1 + fee) : p.size * limitPrice * (1 - fee), format: "usd", hint: `you ${pay ? "pay" : "receive"}, fees included` },
          { label: "Saved vs a market order", value: Math.abs(m.avg - limitPrice) * p.size, format: "usd", hint: "the spread and slippage you don't cross" },
          { label: "Fills only if", value: pay ? "a seller comes to you" : "a buyer comes to you", tone: "warn", hint: "and the price may move away first" },
        ];
  return {
    stats,
    note:
      p.order === "market" && p.size > p.depth * 20
        ? `Your order eats ${MT.format("num", Math.ceil(p.size / p.depth))} levels of the book: the last SPCX ${pay ? "costs" : "sells for"} ${MT.format("price", m.last)}. Selling a holding this size in one go is what the selling-a-large-position guide is about.`
        : "",
    panels: [
      {
        x: { label: `SPCX you ${pay ? "buy" : "sell"} at once`, format: "num" },
        y: { label: "Price per SPCX", format: "price", zero: false },
        series: [
          { name: "Average price", points: xs.map((q) => [q, market(q).avg]) },
          { name: "Last SPCX's price", points: xs.map((q) => [q, market(q).last]), color: "s2", dash: true },
        ],
        rules: [
          { axis: "y", at: mid, label: "Mid: $100" },
          { axis: "x", at: p.size, label: `You: ${MT.format("num", p.size)} SPCX` },
        ],
        dots: [{ x: p.size, y: m.avg, tone: "s1" }],
      },
    ],
  };
});

MT.register("spot.compare", (p) => {
  const entry = 100;
  const { deposit, leverage, exit, days } = p;
  const mm = p.maintenance / 100;
  const rate = p.funding / 10000;
  const sol = deposit / entry;
  const units = (deposit * leverage) / entry;
  const funding = rate * units * entry * 24 * days; // longs pay when positive
  const liq = (entry * (1 - 1 / leverage)) / (1 - mm);
  const spot = (x) => sol * x;
  const perp = (x) => (x <= liq ? 0 : Math.max(0, deposit + units * (x - entry) - funding));
  const xs = MT.range(40, 160, 241);
  return {
    stats: [
      { label: "Spot: you hold", value: `${MT.format("num", sol)} SPCX`, hint: "bought with your $1,000" },
      { label: `Spot, if SPCX goes to ${MT.format("price", exit)}`, value: spot(exit), format: "usd0", tone: spot(exit) >= deposit ? "good" : "bad", hint: "never liquidated, never pays funding" },
      { label: `Perp at ${MT.format("x", leverage)}`, value: perp(exit), format: "usd0", tone: perp(exit) === 0 ? "bad" : perp(exit) >= deposit ? "good" : "bad", hint: exit <= liq ? "liquidated on the way" : `long ${MT.format("num", units)} SPCX, after funding` },
      { label: `Funding over ${MT.format("d", days)}`, value: -funding, format: "+usd", hint: "the perp's cost of holding; spot has none" },
      { label: "Perp liquidated below", value: liq < 0.01 ? "only at $0" : MT.format("price", liq) },
    ],
    panels: [
      {
        x: { label: "Where SPCX goes", format: "price" },
        y: { label: "What your $1,000 becomes", format: "usd0" },
        series: [
          { name: "Spot: 10 SPCX", points: xs.map((x) => [x, spot(x)]) },
          { name: `Perp at ${MT.format("x", leverage)}`, points: xs.map((x) => [x, perp(x)]), color: "s2" },
        ],
        rules: [
          { axis: "x", at: entry, label: "Today: $100" },
          { axis: "y", at: deposit, label: "Your $1,000" },
        ],
        dots: [
          { x: exit, y: spot(exit), tone: "s1" },
          { x: exit, y: perp(exit), tone: "s2" },
        ],
      },
    ],
  };
});
