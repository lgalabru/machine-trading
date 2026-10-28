import { phoenix, rpc } from "./lib.ts";
await phoenix.exchange.ready();
const ob: any = await phoenix.rpc.markets.getOrderbook("SOL" as any);
const slot = Number(await rpc.getSlot().send());
const live = (o: any) => o.order.lastValidSlot === null || Number(o.order.lastValidSlot) > slot + 1000;
for (const side of ["asks", "bids"]) {
  const persist = ob[side].filter(live).map((o: any) => [Number(o.orderId.priceInTicks) / 100, Number(o.order.numBaseLotsRemaining) / 100, o.order.traderPositionId.traderId, o.order.lastValidSlot?.toString() ?? "none"]);
  console.log(side, "total", ob[side].length, "persisting", persist.length, persist.sort((a: any, b: any) => side === "asks" ? a[0] - b[0] : b[0] - a[0]).slice(0, 12));
}
