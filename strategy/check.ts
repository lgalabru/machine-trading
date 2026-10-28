import { phoenix } from "./lib.ts";
await phoenix.exchange.ready();
const ob: any = await phoenix.rpc.markets.getOrderbook("SOL" as any);
console.log("fees", ob.header.defaultTakerFeeMicro, ob.header.defaultMakerFeeMicro);
const b: any = (await phoenix.rpc.hawkeye.viewBbo({ symbol: "SOL" } as any)).returnData.decoded;
console.log("bid", Number(b.bestBidTicks) / 100, "ask", Number(b.bestAskTicks) / 100, "mark", Number(b.markPriceTicks) / 100);
