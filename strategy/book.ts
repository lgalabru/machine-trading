import { phoenix } from "./lib.ts";
await phoenix.exchange.ready();
const j = (x: unknown) => JSON.stringify(x, (_, v) => typeof v === "bigint" ? v.toString() : v);
const ob: any = await phoenix.rpc.markets.getOrderbook("SOL" as any);
console.log("ob keys", Object.keys(ob));
console.log(j(ob).slice(0, 1200));
const sp: any = await phoenix.rpc.markets.getSplineCollection("SOL" as any);
console.log("spline keys", Object.keys(sp));
console.log(j(sp).slice(0, 1500));
