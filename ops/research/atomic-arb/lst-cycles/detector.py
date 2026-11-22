"""Negative-cycle detector for LST / stable / mid-cap pool graphs.

Graph: nodes are mints; each directed edge is one way through one venue:
  kind "cpv"   : constant-product on virtual reserves (r_in, r_out) with fee f
                 (CP pools exactly; CLMM / Whirlpool / DLMM locally, inside the current range)
  kind "linear": fixed rate with capacity (stake-pool DepositSol / WithdrawSol at NAV, prop AMMs, RFQ)
Rule: a cycle from SOL of depth <= MAX_DEPTH is a candidate when the product of marginal rates net of fees
exceeds 1 + MIN_EDGE_BPS (Bellman-Ford over w = -log(rate*(1-f)) finds the negative cycles; we enumerate
all simple cycles <= depth through the start node, which is exact and cheap at this graph size).
Each candidate is sized by golden-section search on input, profit = out(in) - in - tx_cost.
Usage: python3 -I detector.py <graph.json>  (see make_graph() for the schema)
"""
import itertools, json, math, sys

MAX_DEPTH = 4
MIN_EDGE_BPS = 0.5

def out_amount(e, x):
    if x <= 0: return 0.0
    f = e.get("fee", 0.0)
    if e["kind"] == "cpv":
        xe = x * (1 - f)
        return e["r_out"] * xe / (e["r_in"] + xe)
    if e["kind"] == "linear":
        return min(x, e.get("cap_in", float("inf"))) * e["rate"] * (1 - f)
    raise ValueError(e["kind"])

def marginal(e):
    if e["kind"] == "cpv": return e["r_out"] / e["r_in"] * (1 - e.get("fee", 0))
    return e["rate"] * (1 - e.get("fee", 0))

def bellman_ford_negative(nodes, edges, src):
    """Classic BF on w=-log(marginal); returns True if a negative cycle is reachable from src (used as a fast gate)."""
    dist = {n: math.inf for n in nodes}; dist[src] = 0.0
    for _ in range(len(nodes) - 1):
        ch = False
        for e in edges:
            w = -math.log(marginal(e))
            if dist[e["from"]] + w < dist[e["to"]] - 1e-15:
                dist[e["to"]] = dist[e["from"]] + w; ch = True
        if not ch: break
    return any(dist[e["from"]] - math.log(marginal(e)) < dist[e["to"]] - 1e-12 for e in edges)

def cycles_from(edges, src, depth):
    adj = {}
    for e in edges: adj.setdefault(e["from"], []).append(e)
    out = []
    def dfs(node, path, seen):
        if len(path) >= depth: return
        for e in adj.get(node, []):
            if e["to"] == src and path:
                out.append(path + [e]); continue
            if e["to"] == src and not path: continue
            if e["to"] in seen: continue
            dfs(e["to"], path + [e], seen | {e["to"]})
    dfs(src, [], {src})
    return out

def chain(cyc, x):
    for e in cyc: x = out_amount(e, x)
    return x

def size(cyc, max_in, tx_cost):
    lo, hi = 0.0, max_in
    g = (math.sqrt(5) - 1) / 2
    f = lambda x: chain(cyc, x) - x
    a, b = hi - g * (hi - lo), lo + g * (hi - lo)
    for _ in range(120):
        if f(a) > f(b): hi = b
        else: lo = a
        a, b = hi - g * (hi - lo), lo + g * (hi - lo)
    x = (lo + hi) / 2
    return x, f(x) - tx_cost

def detect(graph, src="SOL", depth=MAX_DEPTH, max_in=5000.0, tx_cost=0.00002, min_edge_bps=MIN_EDGE_BPS):
    edges = graph["edges"]; nodes = {e["from"] for e in edges} | {e["to"] for e in edges}
    hits = []
    gate = bellman_ford_negative(nodes, edges, src)
    for cyc in cycles_from(edges, src, depth):
        if len(cyc) < 2: continue
        m = math.exp(sum(math.log(marginal(e)) for e in cyc))
        edge_bps = (m - 1) * 1e4
        if edge_bps <= min_edge_bps: continue
        x, p = size(cyc, max_in, tx_cost)
        hits.append({"route": [f'{e["from"]}>{e["to"]}@{e.get("venue","")}:{e.get("pool","")[:6]}' for e in cyc],
                     "edge_bps": round(edge_bps, 3), "size_in": round(x, 4), "profit": round(p, 6)})
    hits.sort(key=lambda h: -h["profit"])
    return {"negative_cycle_gate": gate, "hits": hits}

if __name__ == "__main__":
    g = json.load(open(sys.argv[1]))
    print(json.dumps(detect(g), indent=1))
