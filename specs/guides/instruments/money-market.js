// Models for the money-market guide's labs (money-market.md), drawn by
// specs/site/guides/mt.js. The collateral's limits (74% max LTV, 80%
// liquidation LTV) are Kamino's SOL reserve's at the time of writing, used
// for an illustrative SPCX; the rate curve is Kamino's USDC reserve's. Debt
// is in USDC, whose borrow factor is 1.

const UNITS = 10; // SPCX deposited as collateral
const MAX_LTV = 0.74;
const LIQ_LTV = 0.8;
const BORROW_APY = 0.05;

MT.register("mm.borrow", (p) => {
  const debt = p.borrow * (1 + BORROW_APY) ** (p.months / 12);
  const value = (price) => UNITS * price;
  const ltv = (price) => (debt > 0 ? debt / value(price) : 0);
  const now = ltv(p.price);
  const health = debt > 0 ? (LIQ_LTV * value(p.price)) / debt : Infinity;
  const liqPrice = debt / (UNITS * LIQ_LTV);
  const room = 1 - liqPrice / p.price;
  const canBorrow = Math.max(0, MAX_LTV * value(p.price) - debt);
  const xs = MT.range(40, 140, 201);
  return {
    stats: [
      { label: "Collateral", value: value(p.price), format: "usd0", hint: `10 SPCX at ${MT.format("price", p.price)}` },
      { label: "Debt", value: debt, format: "usd", hint: p.months ? `${MT.format("usd0", p.borrow)} plus ${p.months} months of interest` : "what you borrowed" },
      { label: "LTV", value: MT.format("pct", now), tone: now >= LIQ_LTV ? "bad" : now >= MAX_LTV ? "warn" : "good", hint: "debt ÷ collateral" },
      { label: "Health", value: Number.isFinite(health) ? MT.format("num", Math.round(health * 100) / 100) : "no debt", tone: health <= 1 ? "bad" : health < 1.2 ? "warn" : "good", hint: "liquidation LTV ÷ LTV" },
      { label: "Liquidated below", value: debt > 0 ? MT.format("price", liqPrice) : "never", hint: debt > 0 ? (room > 0 ? `SPCX ${MT.format("pct", room)} lower` : "already there") : "" },
      { label: "Can still borrow", value: canBorrow, format: "usd0", hint: "up to the 74% max LTV" },
    ],
    note: now >= LIQ_LTV ? "At or past the 80% liquidation LTV: anyone may now repay part of your debt and take your SPCX, with a bonus." : "",
    panels: [
      {
        x: { label: "SPCX price", format: "price" },
        y: { label: "LTV", format: "pct0", min: 0, max: 1.2 },
        series: [{ name: "LTV", points: xs.map((x) => [x, Math.min(1.2, ltv(x))]) }],
        rules: [
          { axis: "x", at: p.price, label: `SPCX: ${MT.format("price", p.price)}` },
          { axis: "y", at: MAX_LTV, label: "Max LTV: no more borrowing", tone: "warn", align: "start" },
          { axis: "y", at: LIQ_LTV, label: "Liquidation LTV", tone: "danger" },
        ],
        bands: [{ axis: "y", from: LIQ_LTV, to: Infinity, tone: "danger", label: "Liquidatable" }],
        dots: [{ x: p.price, y: Math.min(1.2, now), tone: "s1" }],
      },
    ],
  };
});

// Kamino's USDC reserve: 0% at no utilization, 5.51% a year at 95%, 36.34% at 100%.
const CURVE = [
  [0, 0],
  [0.95, 0.0551],
  [1, 0.3634],
];
const TAKE = 0.1; // the protocol's share of the interest borrowers pay
function borrowRate(u) {
  for (let i = 1; i < CURVE.length; i++) {
    const [u0, r0] = CURVE[i - 1];
    const [u1, r1] = CURVE[i];
    if (u <= u1) return r0 + ((r1 - r0) * (u - u0)) / (u1 - u0);
  }
  return CURVE.at(-1)[1];
}

MT.register("mm.rates", (p) => {
  const u = p.utilization / 100;
  const borrow = borrowRate(u);
  const supply = borrow * u * (1 - TAKE);
  const xs = MT.range(0, 1, 201);
  return {
    stats: [
      { label: "Borrowers pay", value: MT.format("pct", borrow), hint: "a year, on what they owe" },
      { label: "Lenders earn", value: MT.format("pct", supply), hint: "borrow rate × utilization × 90%" },
      { label: "Your $1,000 supplied earns", value: 1000 * supply, format: "usd", hint: "a year" },
      { label: "A $500 borrow costs", value: 500 * borrow, format: "usd", hint: "a year" },
      { label: "Left to withdraw", value: MT.format("pct", 1 - u), tone: u >= 0.95 ? "bad" : u >= 0.9 ? "warn" : "good", hint: "of the pool, if every lender asked" },
    ],
    note: u >= 0.99 ? "With almost everything lent out, lenders can't withdraw until borrowers repay, and the rate climbs to pull them back." : "",
    panels: [
      {
        x: { label: "Utilization: borrowed ÷ supplied", format: "pct0" },
        y: { label: "Rate a year", format: "pct0", min: 0 },
        series: [
          { name: "Borrow rate", points: xs.map((x) => [x, borrowRate(x)]), color: "s2" },
          { name: "Supply rate", points: xs.map((x) => [x, borrowRate(x) * x * (1 - TAKE)]) },
        ],
        rules: [
          { axis: "x", at: u, label: `Now: ${MT.format("pct0", u)}` },
          { axis: "x", at: 0.95, label: "Kink", tone: "warn" },
        ],
        bands: [{ axis: "x", from: 0.95, to: 1, tone: "warn" }],
      },
    ],
  };
});
