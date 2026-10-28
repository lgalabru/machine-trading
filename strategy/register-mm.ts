import { MarginType } from "@ellipsis-labs/rise";
import { loadOrCreate, phoenix, send } from "./lib.ts";
await phoenix.exchange.ready();
const mm = await loadOrCreate("market-maker");
const reg = await phoenix.ixs.buildRegisterTrader({ authority: mm.address as any, marginType: MarginType.Cross });
await send(mm, [reg as any], "register market-maker");
