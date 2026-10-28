import { createPhoenixClient } from "@ellipsis-labs/rise";
import {
  createSolanaRpc, createKeyPairSignerFromBytes, generateKeyPairSigner, pipe,
  createTransactionMessage, setTransactionMessageFeePayerSigner,
  setTransactionMessageLifetimeUsingBlockhash, appendTransactionMessageInstructions,
  signTransactionMessageWithSigners, getBase64EncodedWireTransaction, getSignatureFromTransaction,
  type KeyPairSigner, type Instruction,
} from "@solana/kit";
import { getComputeUnitLimitIx } from "./cu.ts";
import fs from "node:fs";

export const RPC_URL = "http://127.0.0.1:8899";
export const rpc = createSolanaRpc(RPC_URL);
export const phoenix = createPhoenixClient({ rpcUrl: RPC_URL, ws: false });

export async function loadOrCreate(name: string): Promise<KeyPairSigner> {
  const p = new URL(`./keys/${name}.json`, import.meta.url);
  if (fs.existsSync(p)) return createKeyPairSignerFromBytes(new Uint8Array(JSON.parse(fs.readFileSync(p, "utf8"))));
  const kp = await generateKeyPairSigner(true);
  const pkcs8 = new Uint8Array(await crypto.subtle.exportKey("pkcs8", kp.keyPair.privateKey));
  const secret = pkcs8.slice(-32);
  const pub = new Uint8Array(await crypto.subtle.exportKey("raw", kp.keyPair.publicKey));
  fs.mkdirSync(new URL("./keys/", import.meta.url), { recursive: true });
  fs.writeFileSync(p, JSON.stringify([...secret, ...pub]));
  return createKeyPairSignerFromBytes(new Uint8Array([...secret, ...pub]));
}

export async function send(signer: KeyPairSigner, ixs: Instruction[], label: string) {
  const { value: bh } = await rpc.getLatestBlockhash().send();
  const msg = pipe(
    createTransactionMessage({ version: 0 }),
    (m) => setTransactionMessageFeePayerSigner(signer, m),
    (m) => setTransactionMessageLifetimeUsingBlockhash(bh, m),
    (m) => appendTransactionMessageInstructions([getComputeUnitLimitIx(1_400_000), ...ixs], m),
  );
  const tx = await signTransactionMessageWithSigners(msg);
  const sig = getSignatureFromTransaction(tx);
  const wire = getBase64EncodedWireTransaction(tx);
  const sim = await rpc.simulateTransaction(wire, { encoding: "base64", sigVerify: false }).send();
  if (sim.value.err) {
    console.error(`✗ ${label} simulation failed`, JSON.stringify(sim.value.err, (_, v) => typeof v === "bigint" ? v.toString() : v));
    console.error((sim.value.logs ?? []).slice(-25).join("\n"));
    throw new Error(`${label} failed`);
  }
  await rpc.sendTransaction(wire, { encoding: "base64", skipPreflight: true }).send();
  for (let i = 0; i < 40; i++) {
    const { value } = await rpc.getSignatureStatuses([sig]).send();
    const s = value[0];
    if (s?.err) throw new Error(`${label} failed on-chain: ${JSON.stringify(s.err)}`);
    if (s?.confirmationStatus === "confirmed" || s?.confirmationStatus === "finalized") { console.log(`✓ ${label} ${sig}`); return sig; }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`${label} not confirmed`);
}

export async function rawRpc(method: string, params: unknown[]) {
  const r = await fetch(RPC_URL, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }) });
  const j = await r.json();
  if (j.error) throw new Error(`${method}: ${JSON.stringify(j.error)}`);
  return j.result;
}
