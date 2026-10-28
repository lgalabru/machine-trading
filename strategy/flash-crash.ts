// Flash-crash liquidity provider on Phoenix SOL-PERP.
//
// Thesis: liquidation cascades overshoot. A bot that rests a ladder of post-only bids
// below the market buys the forced selling at a discount, then sells the rebound with
// a take-profit ask above its average entry. A stop-loss caps the case where the dip
// keeps going.
//
// The market is driven by a Surfpool scenario registered at startup:
//   slot CRASH_SLOT    phoenix-market-move SOL -CRASH_PCT, then phoenix-market-uncross
//   slot RECOVER_SLOT  phoenix-market-move SOL back to its starting mark, then uncross
import { Side } from "@ellipsis-labs/rise";
import { address } from "@solana/kit";
import { loadOrCreate, phoenix, rawRpc, rpc, send } from "./lib.ts";

const SYMBOL = "SOL";
const LADDER = [
  { offsetPct: 3, size: 5 },
  { offsetPct: 5, size: 5 },
  { offsetPct: 7, size: 5 },
];
const TAKE_PROFIT_PCT = 3;
const STOP_LOSS_PCT = 10;
const CRASH_PCT = 8;
const CRASH_SLOT = 40;
const RECOVER_SLOT = 280;
const RECOVER = process.env.RECOVER !== "0";
const USDC_ATA = address("DCxBCsErpWAPTk7oNAkHafjBTLZEz6sqS7idC8HoiUJ2");

await phoenix.exchange.ready();
const bot = await loadOrCreate("strategy");
const usd = (lots: bigint | number) => Number(lots) / 1e6;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const bbo = async () => (await phoenix.rpc.hawkeye.viewBbo({ symbol: SYMBOL } as any)).returnData!.decoded as any;
const margin = async () => (await phoenix.rpc.hawkeye.viewMargin({ authority: bot.address } as any)).returnData!.decoded as any;
const asset = async () => (await phoenix.rpc.hawkeye.viewMarginForAsset({ authority: bot.address, symbol: SYMBOL } as any)).returnData!.decoded as any;
const walletUsdc = async () => Number((await rpc.getTokenAccountBalance(USDC_ATA).send()).value.amount) / 1e6;
const position = async () => Number((await asset()).baseLots) / 100;
const markUsd = async () => Number((await bbo()).markPriceTicks) / 100;
const limit = async (side: Side, priceUsd: number, baseUnits: number) =>
  phoenix.ixs.buildPlacePostOnlyOrder({
    authority: bot.address, symbol: SYMBOL,
    orderPacket: await phoenix.orderPackets.buildLimitOrderPacket({ symbol: SYMBOL, side, priceUsd, baseUnits } as any),
  } as any) as any;
const marketSell = async (baseUnits: number, priceLimitUsd: number) =>
  phoenix.ixs.buildPlaceMarketOrder({
    authority: bot.address, symbol: SYMBOL,
    orderPacket: await phoenix.orderPackets.buildMarketOrderPacket({ symbol: SYMBOL, side: Side.Ask, baseUnits, priceLimitUsd } as any),
  } as any) as any;

// 1. Fund the Phoenix account with the working capital, then snapshot starting equity.
const CAPITAL = 5_000_000_000n;
const have = BigInt((await margin()).collateralQuoteLots);
if (have < CAPITAL) {
  const dep = await phoenix.ixs.buildDepositIxs({ authority: bot.address as any, amount: CAPITAL - have });
  await send(bot, dep.instructions as any, `deposit ${usd(CAPITAL - have)} USDC`);
}
const start = { wallet: await walletUsdc(), collateral: usd((await margin()).collateralQuoteLots) };
const q0 = await bbo();
const markTicks0 = Number(q0.markPriceTicks);
const mark0 = markTicks0 / 100;
console.log(`start: wallet ${start.wallet} USDC, Phoenix collateral ${start.collateral} USDC, SOL mark $${mark0}`);

// 2. Rest the bid ladder.
const levels = LADDER.map((l) => ({ ...l, price: +(mark0 * (1 - l.offsetPct / 100)).toFixed(2) }));
await send(bot, await Promise.all(levels.map((l) => limit(Side.Bid, l.price, l.size))),
  `ladder bids ${levels.map((l) => `${l.size}@${l.price}`).join(", ")}`);

// 3. Register the market scenario.
const crashTicks = Math.round(markTicks0 * (1 - CRASH_PCT / 100));
const override = (slot: number, templateId: string, values: object, label: string) => ({
  id: crypto.randomUUID(), templateId, values, label, scenarioRelativeSlot: slot, enabled: true, fetchBeforeUse: false,
  account: { pubkey: "2nHGAaEw3D5dd4hVueaUNoygkQFmoeKqRQWnSPqSMFUC" },
});
const scenario = {
  id: crypto.randomUUID(),
  name: RECOVER ? "SOL flash crash and recovery" : "SOL crash, no recovery",
  description: `SOL drops ${CRASH_PCT}% to $${crashTicks / 100}${RECOVER ? `, then recovers to $${mark0}` : " and stays there"}`,
  tags: ["phoenix", "flash-crash"],
  overrides: [
    override(CRASH_SLOT, "phoenix-market-move", { symbol: SYMBOL, target_ticks: String(crashTicks) }, `SOL -${CRASH_PCT}%`),
    override(CRASH_SLOT, "phoenix-market-uncross", { symbol: SYMBOL }, "uncross after crash"),
    ...(RECOVER ? [
      override(RECOVER_SLOT, "phoenix-market-move", { symbol: SYMBOL, target_ticks: String(markTicks0) }, "SOL recovers"),
      override(RECOVER_SLOT, "phoenix-market-uncross", { symbol: SYMBOL }, "uncross after recovery"),
    ] : []),
  ],
};
await rawRpc("surfnet_registerScenario", [scenario]);
console.log(`scenario "${scenario.name}" registered: crash at +${CRASH_SLOT} slots${RECOVER ? `, recovery at +${RECOVER_SLOT}` : ""}`);

// 4. Wait for the crash to fill the ladder, then post the take-profit.
let pos = 0;
for (let i = 0; i < 120 && pos === 0; i++) { await sleep(500); pos = await position(); }
if (pos === 0) throw new Error("ladder never filled");
await sleep(1500); // let the uncross finish filling deeper levels
const a = await asset();
pos = Number(a.baseLots) / 100;
// entryPrice is quote lots per base lot: 1e-6 USDC per 0.01 SOL.
const entry = Number(a.entryPriceQuoteLotsPerBaseLot) / 1e4;
const tp = +(entry * (1 + TAKE_PROFIT_PCT / 100)).toFixed(2);
const stop = entry * (1 - STOP_LOSS_PCT / 100);
console.log(`filled: long ${pos} SOL @ avg $${entry.toFixed(4)} (mark $${await markUsd()}) → take-profit ask ${pos}@${tp}, stop $${stop.toFixed(2)}`);
await send(bot, [await limit(Side.Ask, tp, pos)], `take-profit ask ${pos}@${tp}`);

// 5. Manage the position until flat: the rebound lifts the ask, or the stop closes it.
const deadline = Date.now() + (RECOVER ? 120_000 : 20_000);
while ((pos = await position()) > 0) {
  const m = await markUsd();
  if (m <= stop || Date.now() > deadline) {
    console.log(`${m <= stop ? "stop hit" : "no rebound before deadline"} at mark $${m}: closing ${pos} SOL at market`);
    await send(bot, [await phoenix.ixs.buildCancelAll({ authority: bot.address, symbol: SYMBOL } as any) as any], "cancel resting orders");
    await send(bot, [await marketSell(pos, +(m * 0.98).toFixed(2))], `market sell ${pos} SOL`);
    break;
  }
  await sleep(500);
}
console.log(`flat at mark $${await markUsd()}`);

// 6. Cancel unfilled bids, withdraw everything, report.
await send(bot, [await phoenix.ixs.buildCancelAll({ authority: bot.address, symbol: SYMBOL } as any) as any], "cancel leftover orders");
const end = await margin();
const pnl = usd(end.collateralQuoteLots) - start.collateral;
const w = await phoenix.ixs.buildWithdrawIxs({ authority: bot.address as any, amount: BigInt(end.withdrawableCollateralQuoteLots) });
await send(bot, w.instructions as any, `withdraw ${usd(end.withdrawableCollateralQuoteLots)} USDC`);
await sleep(1000);
const finalWallet = await walletUsdc();
console.log(`\ntrading PnL on Phoenix: ${pnl >= 0 ? "+" : ""}${pnl.toFixed(6)} USDC (after fees)`);
console.log(`wallet: ${start.wallet + start.collateral} USDC before → ${finalWallet} USDC after (${(finalWallet - start.wallet - start.collateral).toFixed(6)})`);
