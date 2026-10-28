import { Side } from "@ellipsis-labs/rise";
import { loadOrCreate, phoenix, send } from "./lib.ts";
await phoenix.exchange.ready();
const strat = await loadOrCreate("strategy");
const mm = await loadOrCreate("market-maker");
const SIZE = 10, ROUNDS = 3, EDGE = 0.05;
const bbo = async () => { const d: any = (await phoenix.rpc.hawkeye.viewBbo({ symbol: "SOL" } as any)).returnData.decoded; return { bid: Number(d.bestBidTicks) / 100, ask: Number(d.bestAskTicks) / 100, mark: Number(d.markPriceTicks) / 100 }; };
const margin = async (a: string) => (await phoenix.rpc.hawkeye.viewMargin({ authority: a } as any) as any).returnData?.decoded;
const j = (x: unknown) => JSON.stringify(x, (_, v) => typeof v === "bigint" ? v.toString() : v);
console.log("strategy margin before", j(await margin(strat.address)));

for (let r = 1; r <= ROUNDS; r++) {
  const q = await bbo();
  const bidPx = +(q.bid + EDGE).toFixed(2), askPx = +(q.ask - EDGE).toFixed(2);
  console.log(`\nround ${r}: book ${q.bid}/${q.ask} mark ${q.mark} → strategy quotes ${bidPx}/${askPx}`);
  const bidPkt = await phoenix.orderPackets.buildLimitOrderPacket({ symbol: "SOL", side: Side.Bid, priceUsd: bidPx, baseUnits: SIZE } as any);
  const askPkt = await phoenix.orderPackets.buildLimitOrderPacket({ symbol: "SOL", side: Side.Ask, priceUsd: askPx, baseUnits: SIZE } as any);
  await send(strat, [
    await phoenix.ixs.buildPlacePostOnlyOrder({ authority: strat.address, symbol: "SOL", orderPacket: bidPkt } as any) as any,
    await phoenix.ixs.buildPlacePostOnlyOrder({ authority: strat.address, symbol: "SOL", orderPacket: askPkt } as any) as any,
  ], `strategy post-only bid ${SIZE}@${bidPx} + ask ${SIZE}@${askPx}`);
  const sell = await phoenix.orderPackets.buildMarketOrderPacket({ symbol: "SOL", side: Side.Ask, baseUnits: SIZE, priceLimitUsd: bidPx } as any);
  await send(mm, [await phoenix.ixs.buildPlaceMarketOrder({ authority: mm.address, symbol: "SOL", orderPacket: sell } as any) as any], `flow sells ${SIZE} into strategy bid`);
  const buy = await phoenix.orderPackets.buildMarketOrderPacket({ symbol: "SOL", side: Side.Bid, baseUnits: SIZE, priceLimitUsd: askPx } as any);
  await send(mm, [await phoenix.ixs.buildPlaceMarketOrder({ authority: mm.address, symbol: "SOL", orderPacket: buy } as any) as any], `flow buys ${SIZE} from strategy ask`);
}
console.log("\nstrategy margin after", j(await margin(strat.address)));
