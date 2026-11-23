"""For each detector-fired trigger in a window: txs touching W/R after the trigger in the same slot
(failed / no-op / successful), distinct signers, and fee/tip of the winner. -> data/competition_<window>.json"""
import sys, os, json, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import replay
D = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "data")
W, R = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"
out = []
for win in sys.argv[1:]:
    blocks = replay.load(os.path.join(D, win + ".jsonl"))
    fired = json.load(open(os.path.join(D, "detector_%s.json" % win)))["fired"]
    base_failed = [sum(1 for t in b["txs"] if t["err"] and set(t["touch"]) & {W, R}) for b in blocks.values()]
    for f in fired:
        b = blocks[f["slot"]]
        after = [t for t in b["txs"] if t["idx"] > f["idx"] and set(t["touch"]) & {W, R}]
        failed = [t for t in after if t["err"]]
        out.append(dict(window=win, slot=f["slot"], trig_idx=f["idx"], trigger=f["trigger"], gap=f["gap"],
                        n_after=len(after), n_failed_after=len(failed), n_failed_signers=len({t["signer"] for t in failed}),
                        failed_signers=collections.Counter(t["signer"][:8] for t in failed).most_common(6),
                        failed_fee_q=sorted(round(t["fee"], 6) for t in failed)[-3:],
                        failed_tip_max=max([t["tip"] for t in failed] or [0]),
                        winner=f["next_signer"], winner_dpos=(f["next_idx"] - f["idx"]) if f["next_slot"] == f["slot"] else None,
                        winner_fee=f["next_fee"], winner_tip=f["next_tip"], winner_gross=f["next_gross"]))
    print(win, "baseline failed W/R txs per slot: mean", round(sum(base_failed) / len(base_failed), 2))
for o in out: print({k: o[k] for k in ("slot", "trigger", "gap", "n_failed_after", "n_failed_signers", "failed_signers", "winner", "winner_dpos", "winner_fee", "winner_tip", "winner_gross")})
json.dump(out, open(os.path.join(D, "competition_whales.json"), "w"), indent=1)
