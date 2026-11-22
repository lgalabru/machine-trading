"""Fetch and decode klend Reserve accounts (current state) for every reserve named in parsed records.
usage: python3 -I fetch_reserves.py <scripts_dir> <records.jsonl> <out.json>
Offsets follow crates/runtime/src/venue/kamino/klend.rs (Reserve::decode); token name at 5032.
"""
import sys, json, base64, struct
sys.path.insert(0, sys.argv[1])
import rpc
from b58 import b58encode

def key(d, o): return b58encode(d[o:o + 32])
def u64(d, o): return struct.unpack_from("<Q", d, o)[0]
def u16(d, o): return struct.unpack_from("<H", d, o)[0]
def sf(d, o): return int.from_bytes(d[o:o + 16], "little") / 2 ** 60

def decode(d):
    return {
        "lending_market": key(d, 32), "mint": key(d, 128), "supply_vault": key(d, 160), "fee_vault": key(d, 192),
        "available": u64(d, 224), "borrowed": sf(d, 232), "price": sf(d, 248), "decimals": u64(d, 272),
        "collateral_mint": key(d, 2560), "collateral_vault": key(d, 2600),
        "protocol_liq_fee_pct": d[4871], "protocol_take_rate_pct": d[4870], "bad_debt_bonus_bps": u16(d, 4878),
        "origination_fee": u64(d, 4896) / 2 ** 60, "flash_loan_fee": u64(d, 4904) / 2 ** 60,
        "ltv_pct": d[4872], "liq_threshold_pct": d[4873],
        "min_bonus_bps": u16(d, 4874), "max_bonus_bps": u16(d, 4876),
        "borrow_factor_pct": u64(d, 5008), "deposit_limit": u64(d, 5016), "borrow_limit": u64(d, 5024),
        "name": d[5032:5064].rstrip(b"\x00").decode(errors="replace"),
        "max_twap_div_bps": u64(d, 5088), "max_age_price_s": u64(d, 5096),
        "scope_prices": key(d, 5112), "price_chain": [u16(d, 5144 + 2 * i) for i in range(4)],
        "twap_chain": [u16(d, 5152 + 2 * i) for i in range(4)],
        "farm_collateral": key(d, 64), "farm_debt": key(d, 96), "token_program": key(d, 408),
    }

recs = [json.loads(l) for l in open(sys.argv[2])]
addrs = set()
for r in recs:
    addrs.update([r["repay_reserve"], r["withdraw_reserve"]])
    addrs.update(r.get("refresh_reserves") or [])
    for f in r.get("flash_borrows") or []:
        addrs.add(f["reserve"])
addrs = sorted(addrs)
out = {}
for i in range(0, len(addrs), 100):
    chunk = addrs[i:i + 100]
    res = rpc.call("getMultipleAccounts", [chunk, {"encoding": "base64"}])
    for a, acc in zip(chunk, res["value"]):
        if not acc: continue
        d = base64.b64decode(acc["data"][0])
        if len(d) != 8624: continue
        out[a] = decode(d)
json.dump(out, open(sys.argv[3], "w"), indent=1)
print(len(addrs), "reserves,", len(out), "decoded")
