// Models for the prop AMM guide's labs (prop-amm.md), drawn by specs/site/guides/mt.js.
// A teaching model: the firm quotes a fair value of $100 with a spread on each
// side, widens it by a tier for bigger trades, and by a skew as a trade uses up
// the inventory it pays from. Real firms publish their own numbers, and change
// them every slot.

const FAIR = 100;
/** The spot guide's book: 100 SPCX at every $0.05 from a $0.05 half-spread, and a 2 bp taker fee. */
const BOOK = { half: 0.05, step: 0.05, depth: 100, fee: 2 };
/** Tiers, by the trade's notional: the spread added to the whole trade, in bp. */
const TIERS = [
  { upTo: 10000, add: 0 },
  { upTo: 100000, add: 3 },
  { upTo: Infinity, add: 10 },
];
/** How hard the price leans as a trade uses up the inventory that pays it. */
const SKEW = 0.005;

const tierOf = (size, fair) => TIERS.find((t) => size * fair <= t.upTo).add;

/**
 * What the firm quotes for `size` SPCX: `dir` is +1 for your buy, -1 for your
 * sell. `inventory` is what it can pay out, in SPCX (or its worth in USDC).
 * Returns null when the trade is more than the firm holds.
 */
function quote(size, dir, { fair = FAIR, spread, inventory }) {
  const used = size / inventory;
  if (used >= 1) return null;
  const tier = tierOf(size, fair);
  const skew = (SKEW * used) / (1 - used);
  const widen = (1 + (spread + tier) / 10000) * (1 + skew);
  return { price: dir > 0 ? fair * widen : fair / widen, tier, skew, used };
}

/** A market order on the spot guide's book: average and last price, before fees. */
function book(size, dir) {
  let left = size;
  let cost = 0;
  let price = FAIR + dir * BOOK.half;
  for (let i = 0; left > 1e-9; i += 1) {
    const take = Math.min(left, BOOK.depth);
    price = FAIR + dir * (BOOK.half + BOOK.step * i);
    cost += take * price;
    left -= take;
  }
  return { avg: size > 0 ? cost / size : price, last: price };
}

/** The book's average price with its taker fee in it, to compare like with like. */
const bookNet = (size, dir) => book(size, dir).avg * (1 + (dir * BOOK.fee) / 10000);

const spcx = (n) => `${n.toLocaleString("en-US", { maximumFractionDigits: 3 })} SPCX`;
const bp = (fraction) => MT.format("bp", Math.round(fraction * 1e6) / 100);

MT.register("propamm.quote", (p) => {
  const buy = p.side === "buy";
  const dir = buy ? 1 : -1;
  const at = (spread) => (dir > 0 ? FAIR * (1 + spread / 10000) : FAIR / (1 + spread / 10000));
  // Buying spends the $1,000; selling gives up the 10 SPCX the spot guide bought.
  const cost = (spread) => (buy ? p.deposit - (p.deposit / at(spread)) * FAIR : 10 * (FAIR - at(spread)));
  const price = at(p.spread);
  const onBook = 0.7; // the spot guide's ticket: 5 bp of half-spread and a 2 bp fee
  const mine = cost(p.spread);
  return {
    stats: [
      { label: "The firm's fair value", value: MT.format("price", FAIR), hint: "what it thinks SPCX is worth" },
      { label: buy ? "The quote: you pay" : "The quote: you receive", value: MT.format("price", price), hint: `fair value ${buy ? "plus" : "less"} ${MT.format("bp", p.spread)}` },
      buy
        ? { label: "Position", value: spcx(p.deposit / price), hint: "$1,000 ÷ the quote" }
        : { label: "You receive", value: 10 * price, format: "usd", hint: "10 SPCX × the quote" },
      { label: "Cost against fair value", value: mine, format: "usd", tone: mine < onBook - 0.005 ? "good" : mine > onBook + 0.005 ? "bad" : "warn", hint: `on the spot guide's book: ${MT.format("usd", onBook)}` },
    ],
    note: p.spread >= 20 ? "A firm quotes this wide when it isn't sure of the price: it would rather not trade than trade wrong." : "",
    panels: [
      {
        x: { label: "The firm's spread, each side", format: "bp" },
        y: { label: "What the trade costs you", format: "usd", zero: true },
        series: [{ name: "Cost against fair value", points: MT.range(0, 100, 101).map((s) => [s, cost(s)]) }],
        rules: [
          { axis: "y", at: onBook, label: "The spot guide's book: $0.70" },
          { axis: "x", at: p.spread, label: `Now: ${MT.format("bp", p.spread)}` },
        ],
        dots: [{ x: p.spread, y: mine, tone: "s1" }],
      },
    ],
  };
});

MT.register("propamm.size", (p) => {
  const buy = p.side === "buy";
  const dir = buy ? 1 : -1;
  const maker = { spread: p.spread, inventory: p.inventory };
  const q = quote(p.size, dir, maker);
  const top = Math.max(p.size * 1.3, 400);
  const xs = MT.range(1, top, 240);
  const ammPoints = xs.filter((x) => x / p.inventory < 0.97).map((x) => [x, quote(x, dir, maker).price]);
  const onBook = bookNet(p.size, dir);
  const bookCost = dir * (onBook - FAIR) * p.size;
  const holds = buy ? `${MT.format("num", p.inventory)} SPCX` : MT.format("usd0", p.inventory * FAIR);
  const stats = q
    ? [
        { label: buy ? "You pay, each" : "You receive, each", value: MT.format("price", q.price), hint: `for ${MT.format("num", p.size)} SPCX, one price for all of them` },
        { label: "Spread", value: MT.format("bp", p.spread + q.tier), tone: q.tier > 0 ? "warn" : undefined, hint: q.tier > 0 ? `${MT.format("bp", p.spread)}, plus ${MT.format("bp", q.tier)} for a trade of ${MT.format("usd0", p.size * FAIR)}` : `no tier: the trade is ${MT.format("usd0", p.size * FAIR)}` },
        { label: "Inventory skew", value: bp(q.skew), tone: q.skew > 0.002 ? "bad" : q.skew > 0.0005 ? "warn" : "good", hint: `the trade takes ${(q.used * 100).toLocaleString("en-US", { maximumFractionDigits: 2 })}% of the firm's ${holds}` },
        { label: "Cost against fair value", value: dir * (q.price - FAIR) * p.size, format: "usd" },
        { label: "The same on the book", value: bookCost, format: "usd", tone: bookCost > dir * (q.price - FAIR) * p.size ? "good" : "bad", hint: `${MT.format("price", onBook)} each, fee included` },
      ]
    : [
        { label: "The firm", value: "can't pay", tone: "bad", hint: `it holds ${holds}, and you asked for ${buy ? `${MT.format("num", p.size)} SPCX` : MT.format("usd0", p.size * FAIR)}` },
        { label: "The same on the book", value: bookCost, format: "usd", hint: `${MT.format("price", onBook)} each, fee included` },
      ];
  return {
    stats,
    note: q ? "" : "A trade for more than the firm holds doesn't fill at a worse price: it doesn't fill. The swap fails, and you still have what you came with.",
    panels: [
      {
        x: { label: `SPCX you ${buy ? "buy" : "sell"} at once`, format: "num" },
        y: { label: "Price per SPCX", format: "price", zero: false },
        series: [
          { name: "Prop AMM", points: ammPoints },
          { name: "The spot guide's book, fee included", points: xs.map((x) => [x, bookNet(x, dir)]), color: "muted", dash: true },
        ],
        rules: [
          { axis: "y", at: FAIR, label: "Fair value: $100" },
          { axis: "x", at: p.size, label: `You: ${MT.format("num", p.size)} SPCX` },
        ],
        dots: q ? [{ x: p.size, y: q.price, tone: "s1" }] : [],
      },
    ],
  };
});

MT.register("propamm.landing", (p) => {
  const widen = 1 + p.spread / 10000;
  const quoted = FAIR * widen;
  const expected = p.deposit / quoted;
  const minimum = expected * (1 - p.tolerance / 10000);
  const stale = p.age > p.limit;
  const fresh = (fair) => p.deposit / (fair * widen);
  const filled = (fair) => (stale ? 0 : fresh(fair) >= minimum - 1e-12 ? fresh(fair) : 0);
  const got = filled(p.landing);
  const xs = MT.range(95, 105, 201);
  let outcome;
  let note;
  if (stale && p.stale === "revert") {
    outcome = { label: "What happens", value: "The swap fails", tone: "warn", hint: "you keep your $1,000, and you know it" };
    note = `The quote is ${p.age} slots old and the venue fills up to ${p.limit}. This venue fails the swap, so a bot sees an error and can go elsewhere.`;
  } else if (stale) {
    outcome = { label: "What happens", value: "Success, and nothing", tone: "bad", hint: "no SPCX, your $1,000 untouched, no error" };
    note = `The quote is ${p.age} slots old and the venue fills up to ${p.limit}. This venue confirms the swap and moves nothing, whatever minimum you set: a bot that reads "success" as "I hold SPCX" is wrong.`;
  } else if (got > 0) {
    const short = expected - got;
    outcome = { label: "What happens", value: `Filled: ${spcx(got)}`, tone: short > 1e-9 ? "warn" : "good", hint: `at ${MT.format("price", p.landing * widen)} each` };
    note = short > 1e-9 ? `The firm moved its quote before your swap landed. You hold ${spcx(short)} fewer than you expected, inside the ${MT.format("bp", p.tolerance)} you allowed.` : short < -1e-9 ? "The quote moved your way before your swap landed, and you got more than you expected." : "";
  } else {
    outcome = { label: "What happens", value: "The swap fails", tone: "good", hint: "your minimum output stopped it; you keep your $1,000" };
    note = `At the new quote your $1,000 buys ${spcx(fresh(p.landing))}, under your minimum. The swap fails, which is the minimum doing its job.`;
  }
  return {
    stats: [
      { label: "You expected", value: spcx(expected), hint: `$1,000 at the ${MT.format("price", quoted)} you read` },
      { label: "Minimum output", value: spcx(minimum), hint: `${MT.format("bp", p.tolerance)} under what you expected` },
      outcome,
      { label: "Against what you expected", value: stale || got === 0 ? "nothing traded" : MT.format("+usd", (got - expected) * p.landing), hint: stale || got === 0 ? "" : got < expected ? "the SPCX you're short, at the new price" : "the extra SPCX, at the new price" },
    ],
    note,
    panels: [
      {
        x: { label: "The firm's fair value when your swap lands", format: "price" },
        y: { label: "SPCX you receive for $1,000", format: "num", zero: true },
        series: [
          { name: "With your minimum output", points: xs.map((x) => [x, filled(x)]) },
          { name: "With no minimum", points: xs.map((x) => [x, stale ? 0 : fresh(x)]), color: "muted", dash: true },
        ],
        rules: [
          { axis: "x", at: FAIR, label: "When you read it: $100" },
          { axis: "y", at: minimum, label: `Minimum: ${spcx(minimum)}`, align: "start" },
        ],
        dots: [{ x: p.landing, y: got, tone: got > 0 ? "s1" : "danger", label: got > 0 ? "" : "Nothing" }],
      },
    ],
  };
});

MT.register("propamm.scenarios", (p) => {
  const firms = {
    calm: { spread: 2, inventory: 20000, says: "The firm quotes 2 bp wide and holds $2,000,000 to pay sellers." },
    wide: { spread: 60, inventory: 20000, says: "SPCX is jumping about, and the firm has widened its quote to 60 bp." },
    drained: { spread: 2, inventory: 300, says: "Sellers have taken most of the firm's dollars: it has $30,000 left to pay with." },
    dark: { dark: true, says: "The firm has stopped publishing. Its last quote is too old to fill." },
  };
  const maker = firms[p.scenario];
  const dir = -1;
  const q = maker.dark ? null : quote(p.size, dir, maker);
  const b = book(p.size, dir);
  const net = bookNet(p.size, dir);
  const ammWins = q && q.price >= net;
  const ammOk = q && q.price >= p.floor - 1e-9;
  const bookOk = b.last >= p.floor - 1e-9;
  const pickOk = ammWins ? ammOk : bookOk;
  const top = Math.max(p.size * 1.3, 400);
  const xs = MT.range(1, top, 240);
  const ammPoints = maker.dark ? [] : xs.filter((x) => x / maker.inventory < 0.97).map((x) => [x, quote(x, dir, maker).price]);
  const floor = MT.format("price", p.floor);
  return {
    stats: [
      q
        ? { label: "Prop AMM: you receive", value: MT.format("price", q.price), tone: ammOk ? "good" : "bad", hint: ammOk ? "one fill, at or above the floor" : `one fill, under the ${floor} floor` }
        : { label: "Prop AMM", value: maker.dark ? "no quote" : "can't pay", tone: "warn", hint: maker.dark ? "the swap doesn't fill" : `it holds ${MT.format("usd0", maker.inventory * FAIR)}` },
      { label: "Order book: you receive", value: MT.format("price", b.avg), tone: bookOk ? "good" : "bad", hint: bookOk ? `on average; the last fill is ${MT.format("price", b.last)}` : `on average, but the last fill is ${MT.format("price", b.last)}, under the floor` },
      { label: "The better price is on", value: ammWins ? "the prop AMM" : "the order book", hint: q ? `${MT.format("price", q.price)} against ${MT.format("price", net)} after the book's fee` : "the only one that fills" },
      { label: "Selling there", value: pickOk ? "keeps the mandate" : "breaks the mandate", tone: pickOk ? "good" : "bad", hint: pickOk ? `every fill at or above ${floor}` : `a fill under ${floor}` },
    ],
    note: `${maker.says}${!pickOk && (ammWins ? bookOk : ammOk) ? " The better price breaks the floor, and the other venue doesn't." : !pickOk ? " Neither venue can take this size above the floor in one go: sell less at a time." : ""}`,
    panels: [
      {
        x: { label: "SPCX you sell at once", format: "num" },
        y: { label: "Price per SPCX", format: "price", zero: false },
        series: [
          { name: "Prop AMM: the one fill", points: ammPoints },
          { name: "Order book: average", points: xs.map((x) => [x, book(x, dir).avg]), color: "s2" },
          { name: "Order book: last fill", points: xs.map((x) => [x, book(x, dir).last]), color: "s2", dash: true },
        ],
        rules: [
          { axis: "y", at: p.floor, label: `min_sell_price: ${floor}` },
          { axis: "x", at: p.size, label: `You: ${MT.format("num", p.size)} SPCX` },
        ],
        dots: q ? [{ x: p.size, y: q.price, tone: ammOk ? "s1" : "danger" }] : [],
      },
    ],
  };
});
