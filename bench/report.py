"""Summarize a bench batch: each model's skill per task, with intervals.

Reads every trial under a batch directory (`just bench` writes one per run),
and for each task and model reports:

- **skill**: total PnL over the reference's, on the batch's hidden set
  (shared by every trial: one secret per batch), averaged over runs;
- **reward**: the bound-scaled Harbor reward;
- a 90% bootstrap interval for each, drawing the hidden sessions (shared by
  all models) and each model's runs;
- which gaps between models the intervals don't resolve;
- **cost**: what a run's model calls cost, mean per run (the key proxy logs
  each call's price; the Hermes adapter adds a trial's up into result.json's
  `agent_result.cost_usd`), and each model's total over the batch. A mean
  over runs that didn't all report a cost is shown as "-", never as a part.

A trial with no reward is an infrastructure failure: it's listed as void and
left out, never scored 0. Its cost still counts in the model's total.

Usage: python3 report.py <batch dir> [--json out.json]
"""

import argparse
import glob
import json
import os
import random
from collections import defaultdict

DRAWS = 2000
LEVEL = 0.90


def trials(batch):
    for path in glob.glob(os.path.join(batch, "**", "result.json"), recursive=True):
        with open(path) as f:
            result = json.load(f)
        if "trial_name" not in result:
            continue
        agent = (result.get("config") or {}).get("agent") or {}
        info = result.get("agent_info") or {}
        # The model without its route (`custom/openai/gpt-6.1-sol` through a
        # router), as the review page and the published tables name it.
        model = (agent.get("model_name") or info.get("name") or "").split("/")[-1]
        # A model run outside its provider's own harness is its own row, by
        # the name the harness gives itself (`hermes`, not the import path
        # `mtx_hermes:Hermes` the config holds).
        harness = info.get("name") or agent.get("name")
        if harness:
            harness = harness.split(":")[-1].lower()
        if harness not in (None, "claude-code", "codex"):
            model = f"{model} · {harness}"
        task = (result.get("task_name") or "").split("/")[-1]
        report_path = os.path.join(os.path.dirname(path), "verifier", "report.json")
        report = None
        if os.path.exists(report_path):
            with open(report_path) as f:
                report = json.load(f)
        yield task, model, result, report


def interval(values):
    values = sorted(values)
    lo = values[int((1 - LEVEL) / 2 * len(values))]
    hi = values[int((1 + LEVEL) / 2 * len(values)) - 1]
    return lo, hi


def cost_of(result):
    """A trial's model calls' cost in USD, or None when it reported none."""
    cost = (result.get("agent_result") or {}).get("cost_usd")
    return float(cost) if isinstance(cost, (int, float)) and not isinstance(cost, bool) else None


def mean_cost(costs):
    """Mean over runs, only if every run reported a cost."""
    if not costs or any(c is None for c in costs):
        return None
    return sum(costs) / len(costs)


def usd(value):
    return "-" if value is None else f"${value:,.2f}"


def skill_of(runs, sessions):
    """Mean over runs of Σ bot PnL / Σ reference PnL, on `sessions`."""
    out = []
    for run in runs:
        # A disqualified run is 0, and its report may stop at the session
        # that disqualified it.
        if run["disqualified"]:
            out.append(0.0)
            continue
        bot = sum(run["pnl"][i] for i in sessions)
        ref = sum(run["ref"][i] for i in sessions)
        out.append(bot / ref if ref > 0 else 0.0)
    return sum(out) / len(out)


def reward_of(runs, sessions):
    """Mean over runs of the bound-scaled reward on `sessions`; a v2 task's
    run (no bound: its sessions report 0) counts its verifier's v2 score,
    which this report doesn't recompute on a resample."""
    out = []
    for run in runs:
        if run["disqualified"]:
            out.append(0.0)
            continue
        if run["v2"]:
            out.append(run["headline_reward"])
            continue
        bot = sum(run["pnl"][i] for i in sessions)
        bound = sum(run["bound"][i] for i in sessions)
        out.append(min(max(bot / bound, 0.0), 1.0) if bound > 0 else 0.0)
    return sum(out) / len(out)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("batch")
    parser.add_argument("--json")
    args = parser.parse_args()
    try:
        with open(os.path.join(args.batch, "batch.json")) as f:
            unguarded = json.load(f).get("guarded") is False
    except (OSError, ValueError):
        unguarded = False
    if unguarded:
        print("\nNOT GUARDED: native harnesses ran on guarded tasks (guarded: false); a screen, not for publication")

    runs = defaultdict(lambda: defaultdict(list))
    costs = defaultdict(lambda: defaultdict(list))
    void = []
    for task, model, result, report in trials(args.batch):
        costs[task][model].append(cost_of(result))
        rewards = (result.get("verifier_result") or {}).get("rewards")
        if not rewards or not report:
            void.append(f"{task} {model} {result['trial_name']}")
            continue
        scenarios = report["scenarios"]
        runs[task][model].append({
            "pnl": [s["pnl_usd"] for s in scenarios],
            "ref": [s.get("reference_pnl_usd") or 0.0 for s in scenarios],
            "bound": [s["bound_pnl_usd"] for s in scenarios],
            "disqualified": any(s.get("disqualified") for s in scenarios),
            "set": report.get("eval_seed_sha256"),
            # Scored by v2 (the atomic-arb tasks): its reward is the
            # verifier's score; there is no bound to scale by.
            "v2": report.get("headline") == "v2",
            "headline_reward": float(rewards.get("reward") or 0.0),
        })

    summary = {}
    rng = random.Random(0)
    for task in sorted(runs):
        models = runs[task]
        sets = {run["set"] for m in models.values() for run in m}
        # Every run grades the batch's set; a disqualified one may stop early.
        n = max(len(run["pnl"]) for runs in models.values() for run in runs)
        v2 = any(run["v2"] for m in models.values() for run in m)
        print(f"\n{task}: {n} hidden sessions" + (" (v2: reward is the verifier's score)" if v2 else "") + (
            "" if len(sets) == 1 else f" (WARNING: {len(sets)} different hidden sets)"))
        draws = defaultdict(list)
        for _ in range(DRAWS):
            sessions = [rng.randrange(n) for _ in range(n)]
            for model, model_runs in models.items():
                resampled = [rng.choice(model_runs) for _ in model_runs]
                draws[model].append((skill_of(resampled, sessions), reward_of(resampled, sessions)))
        summary[task] = {}
        print(f"  {'model':28} {'runs':>4}  {'skill':>6} {'90% interval':>16}  {'reward':>6} {'90% interval':>16}"
              f"  {'cost/run':>9}")
        everyone = list(range(n))
        for model in sorted(models, key=lambda m: -skill_of(models[m], everyone)):
            skill = skill_of(models[model], everyone)
            reward = reward_of(models[model], everyone)
            s_lo, s_hi = interval([d[0] for d in draws[model]])
            r_lo, r_hi = interval([d[1] for d in draws[model]])
            cost = mean_cost(costs[task][model])
            summary[task][model] = {
                "runs": len(models[model]), "skill": skill, "skill_interval": [s_lo, s_hi],
                "reward": reward, "reward_interval": [r_lo, r_hi], "cost_usd_per_run": cost,
            }
            print(f"  {model:28} {len(models[model]):>4}  {skill:6.2f} ({s_lo:6.2f}, {s_hi:6.2f})"
                  f"  {reward:6.3f} ({r_lo:6.3f}, {r_hi:6.3f})  {usd(cost):>9}")
        names = sorted(models)
        unresolved = []
        for i, a in enumerate(names):
            for b in names[i + 1:]:
                gap = [x[0] - y[0] for x, y in zip(draws[a], draws[b])]
                lo, hi = interval(gap)
                if lo <= 0 <= hi:
                    unresolved.append(f"{a} vs {b} ({lo:+.2f} to {hi:+.2f})")
        if unresolved:
            print("  not resolved: " + "; ".join(unresolved))
    by_model = cost_by_model(costs)
    if by_model:
        print(f"\ncost by model (every trial, void ones too)\n  {'model':28} {'trials':>6} {'total':>10} {'per trial':>10}")
        for model, row in sorted(by_model.items(), key=lambda kv: -(kv[1]["total_usd"] or 0)):
            print(f"  {model:28} {row['trials']:>6} {usd(row['total_usd']):>10} {usd(row['per_trial_usd']):>10}")
    if void:
        print("\nvoid (no reward; re-run, don't score): " + ", ".join(void))
    if args.json:
        with open(args.json, "w") as f:
            json.dump({"tasks": summary, "cost_by_model": by_model, "void": void,
                       **({"guarded": False} if unguarded else {})}, f, indent=2)


def cost_by_model(costs):
    """Each model's trials, total cost and mean per trial over the batch;
    total and mean are None unless every trial reported a cost."""
    out = {}
    for per_model in costs.values():
        for model, values in per_model.items():
            out.setdefault(model, []).extend(values)
    return {
        model: {
            "trials": len(values),
            "total_usd": None if any(v is None for v in values) else sum(values),
            "per_trial_usd": mean_cost(values),
        }
        for model, values in out.items()
    }


if __name__ == "__main__":
    main()
