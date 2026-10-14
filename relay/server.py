"""Hyperion agent search relay.

Runs next to a fast uplink. One request from a slow client becomes a SearXNG query plus
parallel fetches of the top results, all at datacenter latency; the client gets back a
single compressed bundle of extracted text instead of paying DNS, TCP, TLS, redirects and
megabytes of subresources per page over its own link.

Pages still loading at the deadline keep loading in the background, so a follow-up
/v1/fetch for them is usually served from cache.
"""

import asyncio
import gzip
import json
import os
import time
from collections import OrderedDict
from urllib.parse import urlsplit

import httpx
import trafilatura
import zstandard
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import Response
from starlette.routing import Route

SEARXNG = os.environ.get("RELAY_SEARXNG", "http://127.0.0.1:8855")
MAX_BODY = 3 * 1024 * 1024
PAGE_TTL = 6 * 3600
SEARCH_TTL = 30 * 60
UA = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36"
)
TEXT_TYPES = ("text/html", "application/xhtml+xml", "text/plain")

# Extraction is CPU-bound and the box is shared with SCOTUS prod, so cap concurrency.
EXTRACT_SLOTS = asyncio.Semaphore(2)


class TTLCache:
    def __init__(self, size, ttl):
        self.size, self.ttl, self.d = size, ttl, OrderedDict()

    def get(self, k):
        v = self.d.get(k)
        if v is None or time.monotonic() - v[0] > self.ttl:
            self.d.pop(k, None)
            return None
        self.d.move_to_end(k)
        return v[1]

    def put(self, k, v):
        self.d[k] = (time.monotonic(), v)
        self.d.move_to_end(k)
        while len(self.d) > self.size:
            self.d.popitem(last=False)


pages = TTLCache(2000, PAGE_TTL)
searches = TTLCache(500, SEARCH_TTL)
inflight: dict[str, asyncio.Task] = {}

client = httpx.AsyncClient(
    http2=True,
    follow_redirects=True,
    max_redirects=6,
    timeout=httpx.Timeout(8.0, connect=3.0),
    limits=httpx.Limits(max_connections=64, max_keepalive_connections=32, keepalive_expiry=120),
    headers={
        "User-Agent": UA,
        "Accept": "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5",
        "Accept-Language": "en-US,en;q=0.9",
    },
)


def _extract(html: str, url: str) -> tuple[str, str]:
    text = trafilatura.extract(
        html, url=url, output_format="markdown", include_comments=False,
        include_tables=True, include_images=False, include_links=False, deduplicate=True,
    ) or ""
    meta = trafilatura.extract_metadata(html, default_url=url)
    return text, (meta.title if meta and meta.title else "")


async def _load(url: str) -> dict:
    t0 = time.monotonic()
    rec = {"url": url}
    try:
        async with client.stream("GET", url) as r:
            ctype = r.headers.get("content-type", "").split(";")[0].strip().lower()
            rec["status"] = r.status_code
            rec["final_url"] = str(r.url)
            if r.status_code >= 400:
                rec["error"] = f"http {r.status_code}"
                return rec
            if ctype and not ctype.startswith(TEXT_TYPES):
                rec["error"] = f"unsupported content-type {ctype}"
                return rec
            body = bytearray()
            async for chunk in r.aiter_bytes():
                body += chunk
                if len(body) > MAX_BODY:
                    break
            rec["wire_bytes"] = r.num_bytes_downloaded
            html = body.decode(r.encoding or "utf-8", errors="replace")
        if ctype == "text/plain":
            text, title = html, ""
        else:
            async with EXTRACT_SLOTS:
                text, title = await asyncio.to_thread(_extract, html, str(rec["final_url"]))
        rec["title"] = title
        rec["text"] = text
        if not text.strip():
            # Nothing server-rendered to extract: the page builds itself with JavaScript.
            rec["error"] = "no extractable text (needs javascript)"
    except Exception as e:  # network errors, decode errors, extractor failures
        rec["error"] = f"{type(e).__name__}: {e}"[:200]
    finally:
        rec["fetch_ms"] = round((time.monotonic() - t0) * 1000)
    # Cache answers that will not change on retry; transient network failures and 5xx do not count.
    err = rec.get("error", "")
    if not err or 400 <= rec.get("status", 0) < 500 or err.startswith(("unsupported", "no extractable")):
        pages.put(url, rec)
    return rec


def _page_task(url: str) -> asyncio.Task:
    t = inflight.get(url)
    if t is None:
        t = asyncio.create_task(_load(url))
        inflight[url] = t
        t.add_done_callback(lambda _t: inflight.pop(url, None))
    return t


async def _pages(urls, deadline_s, max_chars):
    out, tasks = {}, {}
    for u in urls:
        cached = pages.get(u)
        if cached is not None:
            out[u] = dict(cached, cached=True)
        else:
            tasks[u] = _page_task(u)
    if tasks:
        await asyncio.wait(list(tasks.values()), timeout=deadline_s)
    for u, t in tasks.items():
        # asyncio.wait does not cancel, so a page past the deadline keeps loading into the cache.
        out[u] = t.result() if t.done() else {"url": u, "error": "still loading at deadline; fetch again shortly"}
    result = []
    for u in urls:
        r = dict(out[u])
        text = r.get("text") or ""
        r["chars"] = len(text)
        if len(text) > max_chars:
            r["text"] = text[:max_chars]
            r["truncated"] = True
        result.append(r)
    return result


async def _search(q: str, n: int):
    key = (q, n)
    hit = searches.get(key)
    if hit is not None:
        return hit, True
    r = await client.get(f"{SEARXNG}/search", params={"q": q, "format": "json", "language": "en"}, timeout=6.0)
    r.raise_for_status()
    d = r.json()
    seen, results = set(), []
    for x in d.get("results", []):
        u = x.get("url")
        if not u or u in seen:
            continue
        seen.add(u)
        results.append({"title": x.get("title", ""), "url": u, "snippet": (x.get("content") or "")[:300]})
        if len(results) >= n:
            break
    searches.put(key, results)
    return results, False


def _reply(request: Request, obj, status=200) -> Response:
    raw = json.dumps(obj, ensure_ascii=False, separators=(",", ":")).encode()
    ae = request.headers.get("accept-encoding", "")
    headers = {"Content-Type": "application/json", "X-Raw-Bytes": str(len(raw))}
    if "zstd" in ae:
        body = zstandard.ZstdCompressor(level=9).compress(raw)
        headers["Content-Encoding"] = "zstd"
    elif "gzip" in ae:
        body = gzip.compress(raw, 6)
        headers["Content-Encoding"] = "gzip"
    else:
        body = raw
    return Response(body, status_code=status, headers=headers)


def _int(d, k, default, lo, hi):
    try:
        return max(lo, min(hi, int(d.get(k, default))))
    except (TypeError, ValueError):
        return default


async def search(request: Request):
    t0 = time.monotonic()
    d = await request.json()
    q = str(d.get("q", "")).strip()
    if not q:
        return _reply(request, {"error": "missing q"}, 400)
    n = _int(d, "n", 8, 1, 20)
    fetch = _int(d, "fetch", 3, 0, 8)
    max_chars = _int(d, "max_chars", 6000, 200, 100000)
    deadline = _int(d, "deadline_ms", 5000, 500, 30000) / 1000
    try:
        results, cached = await _search(q, n)
    except Exception as e:
        return _reply(request, {"error": f"search failed: {type(e).__name__}: {e}"[:300]}, 502)
    t_search = time.monotonic()
    got = await _pages([r["url"] for r in results[:fetch]], max(0.3, deadline - (t_search - t0)), max_chars) if fetch else []
    return _reply(request, {
        "query": q,
        "results": results,
        "pages": got,
        "timing": {"search_ms": round((t_search - t0) * 1000), "total_ms": round((time.monotonic() - t0) * 1000),
                   "search_cached": cached},
    })


async def fetch(request: Request):
    t0 = time.monotonic()
    d = await request.json()
    urls = [u for u in d.get("urls", []) if isinstance(u, str) and urlsplit(u).scheme in ("http", "https")][:10]
    if not urls:
        return _reply(request, {"error": "missing urls"}, 400)
    max_chars = _int(d, "max_chars", 12000, 200, 200000)
    deadline = _int(d, "deadline_ms", 8000, 500, 30000) / 1000
    got = await _pages(urls, deadline, max_chars)
    return _reply(request, {"pages": got, "timing": {"total_ms": round((time.monotonic() - t0) * 1000)}})


async def healthz(request: Request):
    return _reply(request, {"ok": True, "pages_cached": len(pages.d), "inflight": len(inflight)})


app = Starlette(routes=[
    Route("/v1/search", search, methods=["POST"]),
    Route("/v1/fetch", fetch, methods=["POST"]),
    Route("/healthz", healthz),
])
