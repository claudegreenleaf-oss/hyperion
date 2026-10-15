"""Summarize agent search benchmark results.

  python analyze_agent.py results/agent-run1.jsonl

Per profile and arm: median and 90th-percentile wall time, median bytes on the wire (both
directions, including DNS, TLS and subresources), pages read per search (of 3), and failures.
Then, per profile, the paired per-query ratio of each arm to the relay, with a bootstrap 95%
interval on the median ratio.
"""

import collections
import json
import random
import statistics
import sys

PROFILES = ["good", "3g", "bad", "awful"]


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


def boot_median(xs, n=2000, seed=1):
    rng = random.Random(seed)
    meds = sorted(statistics.median(rng.choices(xs, k=len(xs))) for _ in range(n))
    return meds[int(0.025 * n)], meds[int(0.975 * n)]


rows = [json.loads(l) for f in sys.argv[1:] for l in open(f)]
by = collections.defaultdict(list)
for r in rows:
    by[(r["profile"], r["arm"])].append(r)

print(f"{'profile':8s} {'arm':8s} {'n':>3s} {'median s':>9s} {'p90 s':>7s} {'median KB':>10s} {'read/3':>7s} {'fail':>5s}")
for p in PROFILES:
    for arm in ("relay", "fetch", "browser"):
        rs = by.get((p, arm))
        if not rs:
            continue
        ms = [r["ms"] / 1000 for r in rs]
        kb = [(r["rx_bytes"] + r["tx_bytes"]) / 1024 for r in rs]
        read = statistics.mean(r["pages_read"] for r in rs)
        fail = sum(1 for r in rs if not r["ok"])
        extra = ""
        srv = [r["server_ms"] / 1000 for r in rs if r.get("server_ms") is not None]
        if srv:
            link = [r["ms"] / 1000 - r["server_ms"] / 1000 for r in rs if r.get("server_ms") is not None]
            extra = f"   relay work {statistics.median(srv):.2f} s, link {statistics.median(link):.2f} s (p90 {pct(link, 0.9):.2f})"
        print(f"{p:8s} {arm:8s} {len(rs):3d} {statistics.median(ms):9.2f} {pct(ms, 0.9):7.2f} "
              f"{statistics.median(kb):10.1f} {read:7.2f} {fail:5d}{extra}")
    print()

print("paired ratios to relay (median over queries, 95% bootstrap interval)")
for p in PROFILES:
    relay = {(r["q"], r["rep"]): r for r in by.get((p, "relay"), [])}
    for arm in ("fetch", "browser"):
        t, b = [], []
        for r in by.get((p, arm), []):
            base = relay.get((r["q"], r["rep"]))
            if not base or not r["ok"] or not base["ok"]:
                continue
            t.append(r["ms"] / base["ms"])
            b.append((r["rx_bytes"] + r["tx_bytes"]) / max(1, base["rx_bytes"] + base["tx_bytes"]))
        if len(t) < 3:
            continue
        lo, hi = boot_median(t)
        blo, bhi = boot_median(b)
        print(f"  {p:6s} {arm:8s} time x{statistics.median(t):5.1f} [{lo:.1f}, {hi:.1f}]   "
              f"bytes x{statistics.median(b):6.0f} [{blo:.0f}, {bhi:.0f}]   n={len(t)}")
