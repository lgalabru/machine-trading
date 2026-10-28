import { loadOrCreate, phoenix, send } from "./lib.ts";
await phoenix.exchange.ready();
const s = await loadOrCreate("strategy");
const ix = await phoenix.ixs.buildCancelAll({ authority: s.address, symbol: "SOL" } as any);
await send(s, [ix as any], "cancel test bids");
