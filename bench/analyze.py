#!/usr/bin/env python3
"""Analyze Hyperion benchmark JSONL results.

For every metric and condition this computes the paired log ratio per interleaved pair,
a hierarchical bootstrap 95 percent interval (resample sites, then pairs within each
drawn site), a Wilcoxon signed-rank test with Holm correction, the A/A minimum
detectable effect from the stock-versus-stock pairs of the same session, and a verdict.

Usage: analyze.py results/*.jsonl [--boot 10000] [--min-ms 25] [--json out.json]
"""
import argparse
import json
import math
import sys
from collections import defaultdict

import numpy as np
from scipy import stats

# name -> (kind, practical threshold in ms or None). All metrics are lower-is-better.
TIME = ["click_fcp", "click_lcp", "down_fcp", "down_lcp", "start_fcp", "start_lcp", "warm_fcp", "warm_lcp", "warm_load"]
COUNT = ["warm_reqs", "warm_cond", "warm_304", "bytes_start", "bytes_click", "cpu_start_ms", "cpu_click_ms", "renderers_end"]
PRACTICAL = {"click_fcp", "click_lcp"}
MDE_FLOOR = 0.03


def load(paths):
    runs, sessions = [], []
    for p in paths:
        with open(p) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    o = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if o.get("type") == "session":
                    sessions.append(o)
                elif o.get("type") == "run" and not o.get("warmup"):
                    runs.append(o)
    return runs, sessions


def build_pairs(runs):
    """Group runs into pairs keyed by (session, site, kind, rep). Returns list of dicts."""
    by = defaultdict(dict)
    failed = 0
    for r in runs:
        if not r.get("ok"):
            failed += 1
            by[(r["session"], r["site"], r["kind"], r["rep"])]["bad"] = True
            continue
        by[(r["session"], r["site"], r["kind"], r["rep"])][r["arm"]] = r
    pairs = []
    for (sess, site, kind, rep), d in by.items():
        if d.get("bad"):
            continue
        x, y = d.get("A"), d.get("B" if kind == "AB" else "A2")
        if x is None or y is None:
            continue
        pairs.append({"session": sess, "site": site, "kind": kind, "rep": rep, "x": x, "y": y})
    return pairs, failed


def value(run, metric):
    v = run["m"].get(metric)
    if v is None:
        return None
    return float(v)


def pair_stat(pair, metric):
    a, b = value(pair["x"], metric), value(pair["y"], metric)
    if a is None or b is None:
        return None
    if metric in TIME:
        a, b = max(a, 1.0), max(b, 1.0)
        return math.log(b / a), a, b
    return math.log((b + 1.0) / (a + 1.0)), a, b


def conditions(pair):
    """Condition labels a pair belongs to."""
    link = pair["x"]["link"]["type"]
    prof = pair["x"].get("profile", "?")
    return [f"{prof}/all", f"{prof}/{link}"]


def hier_boot(site_arrays, nboot, rng):
    """Hierarchical bootstrap of the median-across-sites of per-site median log ratios."""
    s = len(site_arrays)
    nmax = max(len(a) for a in site_arrays)
    r = np.full((s, nmax), np.nan)
    n = np.zeros(s, dtype=int)
    for i, a in enumerate(site_arrays):
        r[i, : len(a)] = a
        n[i] = len(a)
    out = np.empty((nboot, s))
    chunk = 2000
    for lo in range(0, nboot, chunk):
        hi = min(nboot, lo + chunk)
        b = hi - lo
        for j in range(s):
            sj = rng.integers(0, s, size=b)
            nj = n[sj]
            u = rng.random((b, nmax))
            idx = np.minimum((u * nj[:, None]).astype(int), nj[:, None] - 1)
            vals = r[sj[:, None], idx]
            vals = np.where(np.arange(nmax)[None, :] < nj[:, None], vals, np.nan)
            out[lo:hi, j] = np.nanmedian(vals, axis=1)
    return np.median(out, axis=1)


def aggregate(by_site):
    return float(np.median([np.median(v) for v in by_site.values()]))


def holm(pvals):
    order = sorted(range(len(pvals)), key=lambda i: pvals[i])
    m = len(pvals)
    adj = [0.0] * m
    running = 0.0
    for rank, i in enumerate(order):
        running = max(running, min(1.0, (m - rank) * pvals[i]))
        adj[i] = running
    return adj


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    ap.add_argument("--boot", type=int, default=10000)
    ap.add_argument("--min-ms", type=float, default=25.0, help="practical threshold for click metrics")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--json", default="")
    ap.add_argument("--metrics", default="", help="comma list to restrict")
    args = ap.parse_args()
    rng = np.random.default_rng(args.seed)

    runs, sessions = load(args.files)
    pairs, failed = build_pairs(runs)
    if not pairs:
        print("no complete pairs found", file=sys.stderr)
        sys.exit(1)
    metrics = [m for m in TIME + COUNT if not args.metrics or m in args.metrics.split(",")]

    # Collect per-metric, per-condition, per-kind site->array of log ratios and raw values.
    store = defaultdict(lambda: defaultdict(lambda: {"r": defaultdict(list), "x": [], "y": [], "d": []}))
    for p in pairs:
        for m in metrics:
            st = pair_stat(p, m)
            if st is None:
                continue
            lr, a, b = st
            for c in conditions(p):
                e = store[(m, c)][p["kind"]]
                e["r"][p["site"]].append(lr)
                e["x"].append(a)
                e["y"].append(b)
                e["d"].append(a - b)

    rows = []
    for (m, c), kinds in sorted(store.items()):
        ab = kinds.get("AB")
        if not ab or not ab["x"]:
            continue
        sites = {k: np.array(v) for k, v in ab["r"].items()}
        arrays = list(sites.values())
        effect = aggregate(sites)
        boot = hier_boot(arrays, args.boot, rng)
        lo, hi = np.percentile(boot, [2.5, 97.5])
        allr = np.concatenate(arrays)
        nz = allr[allr != 0]
        p = float(stats.wilcoxon(nz).pvalue) if len(nz) >= 6 else 1.0
        aa = kinds.get("AA")
        mde, aa_n = None, 0
        if aa and aa["x"]:
            aa_sites = {k: np.array(v) for k, v in aa["r"].items()}
            aaboot = hier_boot(list(aa_sites.values()), args.boot, rng)
            mde = max(math.log1p(MDE_FLOOR), float(np.percentile(np.abs(aaboot), 95)))
            aa_n = len(aa["x"])
        rows.append({
            "metric": m, "cond": c, "pairs": len(allr), "sites": len(sites),
            "med_a": float(np.median(ab["x"])), "med_b": float(np.median(ab["y"])),
            "med_diff": float(np.median(ab["d"])),
            "effect": effect, "lo": float(lo), "hi": float(hi), "p": p,
            "mde": mde, "aa_pairs": aa_n,
        })
    adj = holm([r["p"] for r in rows])
    for r, a in zip(rows, adj):
        r["p_holm"] = a
        floor = math.log1p(MDE_FLOOR)
        mde = r["mde"] if r["mde"] is not None else floor
        practical = True
        if r["metric"] in PRACTICAL:
            practical = abs(r["med_diff"]) >= args.min_ms
        sig_better = r["hi"] < 0 and r["p_holm"] < 0.05
        sig_worse = r["lo"] > 0 and r["p_holm"] < 0.05
        big = abs(r["effect"]) > mde
        if sig_better and big and practical:
            v = "BETTER"
        elif sig_worse and big:
            v = "WORSE"
        elif (r["hi"] < 0 or r["lo"] > 0) and r["p_holm"] < 0.05:
            v = "small" if not big else "small(<%dms)" % args.min_ms
        else:
            v = "noise"
        r["verdict"] = v + ("" if r["mde"] is not None else " (no A/A)")

    # ---- print
    meta = sessions[0] if sessions else {}
    print(f"files: {len(args.files)}  sessions: {len(sessions)}  complete pairs: {len(pairs)} "
          f"(AB {sum(p['kind']=='AB' for p in pairs)}, AA {sum(p['kind']=='AA' for p in pairs)})  failed runs: {failed}")
    if meta:
        print(f"stock: {meta.get('stock_version','?')}   hyp: {meta.get('hyp_version','?')}   {meta.get('label','')}")
    print("negative effect = B faster/lighter than A. MDE = A/A 95th percentile |effect|, floor 3 percent.")
    print()
    hdr = f"{'metric':<14}{'condition':<14}{'pairs':>6}{'sites':>6}{'median A':>10}{'median B':>10}{'effect % [95% CI]':>26}{'MDE %':>8}{'p(Holm)':>9}  verdict"
    print(hdr)
    print("-" * len(hdr))
    pct = lambda x: 100 * math.expm1(x)
    for r in rows:
        eff = f"{pct(r['effect']):+6.1f} [{pct(r['lo']):+6.1f},{pct(r['hi']):+6.1f}]"
        mde = f"{pct(r['mde']):6.1f}" if r["mde"] is not None else "   n/a"
        print(f"{r['metric']:<14}{r['cond']:<14}{r['pairs']:>6}{r['sites']:>6}{r['med_a']:>10.1f}{r['med_b']:>10.1f}{eff:>26}{mde:>8}{r['p_holm']:>9.3f}  {r['verdict']}")

    # Speculation outcomes per arm: how often the click was served by prerender or prefetch.
    print()
    print("speculation outcome by arm (share of measured clicks):")
    cnt = defaultdict(lambda: defaultdict(int))
    for p in pairs:
        for arm in ("x", "y"):
            run = p[arm]
            key = (p["kind"], run["arm"], run["link"]["type"])
            cnt[key][run["spec"]] += 1
            cnt[key]["n"] += 1
    print(f"{'pair kind':<10}{'arm':<5}{'link':<9}{'n':>5}{'prerender':>11}{'prefetch':>10}{'none':>8}")
    for key in sorted(cnt):
        d = cnt[key]
        n = d["n"]
        print(f"{key[0]:<10}{key[1]:<5}{key[2]:<9}{n:>5}{100*d['prerender']/n:>10.0f}%{100*d['prefetch']/n:>9.0f}%{100*d['none']/n:>7.0f}%")

    if args.json:
        with open(args.json, "w") as f:
            json.dump(rows, f, indent=1)


if __name__ == "__main__":
    main()
