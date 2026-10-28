import { loadOrCreate, phoenix, send } from "./lib.ts";
await phoenix.exchange.ready();
for (const [n, amt] of [["strategy", 2_000_000_000n], ["market-maker", 50_000_000_000n]] as const) {
  const s = await loadOrCreate(n);
  const dep = await phoenix.ixs.buildDepositIxs({ authority: s.address as any, amount: amt });
  await send(s, dep.instructions as any, `deposit ${Number(amt) / 1e6} USDC (${n})`);
}
