import { address, type Instruction } from "@solana/kit";
export function getComputeUnitLimitIx(units: number): Instruction {
  const data = new Uint8Array(5); data[0] = 2; new DataView(data.buffer).setUint32(1, units, true);
  return { programAddress: address("ComputeBudget111111111111111111111111111111"), accounts: [], data };
}
