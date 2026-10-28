import { phoenix, rpc } from "./lib.ts";
await phoenix.exchange.ready();
const ob: any = await phoenix.rpc.markets.getOrderbook("SOL" as any);
const slot = await rpc.getSlot().send();
console.log("slot", slot, "asks", ob.asks.length, "bids", ob.bids.length);
console.log("sample ask", JSON.stringify(ob.asks[0], (_, v) => typeof v === "bigint" ? v.toString() : v));
const lvl = (side: any[]) => side.map((o: any) => {
  const p = Number(o.priceInTicks ?? o.price ?? o[0]?.priceInTicks ?? o.orderId?.priceInTicks);
  const q = Number(o.numBaseLots ?? o.baseLots ?? o.size ?? o[1]?.numBaseLots ?? o.order?.numBaseLots ?? o.restingOrder?.numBaseLots);
  return [p, q];
});
const a = lvl(ob.asks).sort((x, y) => x[0] - y[0]); const b = lvl(ob.bids).sort((x, y) => y[0] - x[0]);
console.log("best asks", a.slice(0, 5), "best bids", b.slice(0, 5));
let cum = 0; for (const [p, q] of a) { if (p > 11450) break; cum += q; }
console.log("resting ask SOL up to $114.50:", cum / 100, " highest ask", a.at(-1));
const sp: any = await phoenix.rpc.markets.getSplineCollection("SOL" as any);
for (const s of sp.splines) console.log("spline", s.trader.slice(0,6), "mid", s.midPrice, "userUpdate", s.userUpdateSlot, "askTotal", s.askRegions.reduce((t: number, r: any) => t + Number(r.totalSize) - Number(r.filledSize), 0) / 100, "maxAskOffset", s.askRegions.at(-1)?.endOffset, "lifespan", s.askRegions[0]?.lifespan);
console.log(JSON.stringify(await phoenix.rpc.hawkeye.viewBbo({ symbol: "SOL" } as any).then((r: any) => r.returnData.decoded)));
