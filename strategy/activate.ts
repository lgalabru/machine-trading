import { rawRpc } from "./lib.ts";
const scenario = {
  id: crypto.randomUUID(), name: "Onboard strategy trader", description: "Grant the strategy trader full capabilities", tags: ["phoenix"],
  overrides: [{
    id: crypto.randomUUID(), templateId: "phoenix-trader-capabilities", account: { pubkey: "FrbTAmqhCLwFzmEqgbAQcUWSLbZD2emNXxdwi69kRVqR" },
    values: Object.fromEntries(["PlaceLimitOrder", "PlaceMarketOrder", "RiskIncreasingTrade", "RiskReducingTrade", "DepositCollateral", "WithdrawCollateral"].map((k) => [k, "true"])),
    scenarioRelativeSlot: 1, enabled: true, fetchBeforeUse: false, label: "activate strategy",
  }],
};
console.log(await rawRpc("surfnet_registerScenario", [scenario]));
