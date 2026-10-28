import { MarginType } from "@ellipsis-labs/rise";
import { lamports } from "@solana/kit";
import { loadOrCreate, phoenix, rpc, send } from "./lib.ts";

const strat = await loadOrCreate("strategy");
const mm = await loadOrCreate("market-maker");
console.log({ strategy: strat.address, marketMaker: mm.address });
for (const s of [strat, mm]) {
  const bal = await rpc.getBalance(s.address).send();
  if (bal.value < 1_000_000_000n) { await rpc.requestAirdrop(s.address, lamports(10_000_000_000n)).send(); }
}
await new Promise((r) => setTimeout(r, 1500));
