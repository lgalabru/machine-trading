import { MarginType } from "@ellipsis-labs/rise";
import { loadOrCreate, phoenix, send } from "./lib.ts";

await phoenix.exchange.ready();
const strat = await loadOrCreate("strategy");
const mm = await loadOrCreate("market-maker");
const deposits: [typeof strat, bigint][] = [[strat, 2_000_000_000n], [mm, 50_000_000_000n]];
for (const [s, amt] of deposits) {
  const traderAddr = await phoenix.pda.trader?.(s.address as any, 0, 0).catch?.(() => null);
  try {
    const reg = await phoenix.ixs.buildRegisterTrader({ authority: s.address as any, marginType: MarginType.Cross });
    await send(s, [reg as any], `register ${s.address.slice(0, 6)}`);
  } catch (e) { console.log("register:", (e as Error).message); }
  const dep = await phoenix.ixs.buildDepositIxs({ authority: s.address as any, amount: amt });
  await send(s, dep.instructions as any, `deposit ${Number(amt) / 1e6} USDC ${s.address.slice(0, 6)}`);
}
