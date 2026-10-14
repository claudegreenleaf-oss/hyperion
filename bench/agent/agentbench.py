"""Agent web-search benchmark: what one "search and read the top 3 results" costs an agent
on a given link, for three ways of doing it.

  relay    one POST to the Hyperion relay (search + parallel fetch + extraction server side)
  fetch    WebFetch-style: GET a results page, then GET the 3 result pages and extract locally
  browser  browser-MCP-style: cold headless Chrome loads the results page and the 3 pages

All three read the same 3 pages per query (chosen once, untimed, by the relay), so they differ
only in transport. Runs inside the hagent namespace (netns-agent.sh); bytes are the
namespace interface's counters, so they include DNS, TLS and every subresource.

  python agentbench.py --profile bad --queries queries.txt --reps 2 --out results/agent-bad.jsonl
"""

import argparse
import asyncio
import json
import os
import random
import subprocess
import time
import urllib.parse

import httpx
import trafilatura

RELAY = os.environ.get("HYPERION_RELAY", "http://100.104.232.110:8790")
IFACE = "ha-n"
HERE = os.path.dirname(os.path.abspath(__file__))
UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36"
MIN_CHARS = 500  # a page counts as read when at least this much text came back


def counters():
    base = f"/sys/class/net/{IFACE}/statistics"
    return tuple(int(open(f"{base}/{k}").read()) for k in ("rx_bytes", "tx_bytes"))


async def arm_relay(q, urls, timeout):
    async with httpx.AsyncClient(timeout=timeout, headers={"Accept-Encoding": "zstd, gzip"}) as c:
        r = await c.post(f"{RELAY}/v1/search", json={"q": q, "n": 8, "fetch": 3, "max_chars": 6000,
                                                     "deadline_ms": int(timeout * 1000) - 2000, "cache": False})
        d = r.json()
    return [{"url": p["url"], "chars": p.get("chars", 0), "error": p.get("error")} for p in d.get("pages", [])]


async def arm_fetch(q, urls, timeout):
    async with httpx.AsyncClient(http2=True, follow_redirects=True, timeout=timeout,
                                 headers={"User-Agent": UA, "Accept-Language": "en-US,en;q=0.9"}) as c:
        serp = await c.get("https://www.bing.com/search", params={"q": q})
        serp.raise_for_status()

        async def one(u):
            try:
                r = await c.get(u)
                text = await asyncio.to_thread(trafilatura.extract, r.text, url=u) or ""
                return {"url": u, "chars": len(text)}
            except Exception as e:
                return {"url": u, "chars": 0, "error": f"{type(e).__name__}: {e}"[:160]}

        return await asyncio.gather(*(one(u) for u in urls))


async def arm_browser(q, urls, timeout, chrome):
    cfg = {"chrome": chrome, "serp": "https://www.bing.com/search?q=" + urllib.parse.quote_plus(q),
           "urls": urls, "timeout_ms": int(timeout * 1000)}
    p = await asyncio.create_subprocess_exec("node", os.path.join(HERE, "browser_arm.js"), json.dumps(cfg),
                                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, cwd=HERE)
    out, _ = await asyncio.wait_for(p.communicate(), timeout * 2 + 30)
    d = json.loads(out.decode().strip().splitlines()[-1])
    if d.get("error"):
        raise RuntimeError(d["error"])
    return d["pages"]


async def choose_urls(q):
    async with httpx.AsyncClient(timeout=60) as c:
        d = (await c.post(f"{RELAY}/v1/search", json={"q": q, "n": 8, "fetch": 0})).json()
    return [r["url"] for r in d.get("results", [])[:3]]


async def trial(arm, q, urls, args):
    rx0, tx0 = counters()
    t0 = time.monotonic()
    rec = {"arm": arm, "q": q, "profile": args.profile}
    try:
        if arm == "relay":
            pages = await asyncio.wait_for(arm_relay(q, urls, args.timeout), args.timeout + 10)
        elif arm == "fetch":
            pages = await asyncio.wait_for(arm_fetch(q, urls, args.timeout), args.timeout * 2)
        else:
            pages = await arm_browser(q, urls, args.timeout, args.chrome)
        rec["pages_read"] = sum(1 for p in pages if p.get("chars", 0) >= MIN_CHARS)
        rec["pages"] = pages
        rec["ok"] = True
    except Exception as e:
        rec["ok"] = False
        rec["pages_read"] = 0
        rec["error"] = f"{type(e).__name__}: {e}"[:300]
    rec["ms"] = round((time.monotonic() - t0) * 1000)
    rx1, tx1 = counters()
    rec["rx_bytes"], rec["tx_bytes"] = rx1 - rx0, tx1 - tx0
    return rec


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", required=True)
    ap.add_argument("--queries", default=os.path.join(HERE, "queries.txt"))
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--arms", default="relay,fetch,browser")
    ap.add_argument("--timeout", type=float, default=60.0)
    ap.add_argument("--chrome", default=os.path.expanduser("~/hyperion-bench/cft/chrome/linux-155.0.8059.79/chrome-linux64/chrome"))
    ap.add_argument("--seed", default="agent")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    queries = [l.strip() for l in open(args.queries) if l.strip() and not l.startswith("#")]
    arms = args.arms.split(",")
    rng = random.Random(f"{args.seed}|{args.profile}")
    chosen = {}
    for q in queries:
        chosen[q] = await choose_urls(q)
    with open(args.out, "a") as f:
        for rep in range(args.reps):
            for q in rng.sample(queries, len(queries)):
                for arm in rng.sample(arms, len(arms)):
                    rec = await trial(arm, q, chosen[q], args)
                    rec["rep"] = rep
                    f.write(json.dumps(rec) + "\n")
                    f.flush()
                    print(f"{args.profile} {arm:7s} {rec['ms']:6d} ms {rec['rx_bytes'] + rec['tx_bytes']:9d} B "
                          f"read {rec['pages_read']}/3 {rec.get('error', '')[:80]} | {q[:50]}", flush=True)
                    await asyncio.sleep(1.0)


if __name__ == "__main__":
    asyncio.run(main())
