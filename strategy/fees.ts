import { rawRpc } from "./lib.ts";
const scenario = { id: crypto.randomUUID(), name: "SOL maker rebate", description: "taker 3.5 bps, maker -0.5 bps", tags: ["phoenix"],
  overrides: [{ id: crypto.randomUUID(), templateId: "phoenix-market-fees", account: { pubkey: "71Si24E4uc3oCaPbPZTozC1ptSNNqygjjebxSmErSsC2" },
    values: { defaultTakerFeeMicro: 350, defaultMakerFeeMicro: -50 }, scenarioRelativeSlot: 1, enabled: true, fetchBeforeUse: false, label: "SOL maker rebate" }] };
console.log(await rawRpc("surfnet_registerScenario", [scenario]));
