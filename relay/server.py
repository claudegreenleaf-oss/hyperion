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
import re
import time
from collections import OrderedDict
from urllib.parse import urlsplit

import httpx
import lxml.html
import pypdfium2 as pdfium
import trafilatura
import zstandard
from curl_cffi import requests as cffi
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import Response
from starlette.routing import Route

SEARXNG = os.environ.get("RELAY_SEARXNG", "http://127.0.0.1:8855")
# Another relay on a different (residential) IP that takes pages this one is blocked from.
WORKER = os.environ.get("RELAY_WORKER", "")
MAX_BODY = 3 * 1024 * 1024
PAGE_TTL = 6 * 3600
SEARCH_TTL = 30 * 60
UA = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36"
)
TEXT_TYPES = ("text/html", "application/xhtml+xml", "text/plain", "application/pdf")

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


BLOCKED = (403, 429, 503)
SE_SITES = {"stackoverflow.com": "stackoverflow", "superuser.com": "superuser", "serverfault.com": "serverfault",
            "askubuntu.com": "askubuntu", "mathoverflow.net": "mathoverflow.net"}
SE_QUESTION = re.compile(r"^/questions/(\d+)")


def _blocked(status: int, headers) -> bool:
    return status in BLOCKED or headers.get("cf-mitigated") == "challenge"


async def _get_httpx(url):
    async with client.stream("GET", url) as r:
        body = bytearray()
        if r.status_code < 400:
            async for chunk in r.aiter_bytes():
                body += chunk
                if len(body) > MAX_BODY:
                    break
        return r.status_code, r.headers, bytes(body), str(r.url), r.num_bytes_downloaded


async def _get_browserlike(url):
    # Chrome's TLS and HTTP/2 fingerprint gets past bot filters that reject plain clients.
    async with cffi.AsyncSession(impersonate="chrome", timeout=10, allow_redirects=True) as sess:
        r = await sess.get(url, headers={"Accept-Language": "en-US,en;q=0.9"})
        body = r.content[:MAX_BODY] if r.status_code < 400 else b""
        return r.status_code, r.headers, body, str(r.url), len(r.content)


def _pdf_text(data: bytes) -> str:
    doc = pdfium.PdfDocument(data)
    try:
        parts = []
        for i in range(min(len(doc), 40)):
            page = doc[i]
            parts.append(page.get_textpage().get_text_range())
        return "\n\n".join(parts)
    finally:
        doc.close()


def _se_html_to_text(html: str) -> str:
    # Stack Exchange bodies are HTML fragments; keep code blocks fenced.
    root = lxml.html.fromstring(f"<div>{html}</div>")
    for pre in root.iter("pre"):
        pre.text = "\n```\n" + pre.text_content().strip("\n") + "\n```\n"
        for child in list(pre):
            pre.remove(child)
    return re.sub(r"\n{3,}", "\n\n", root.text_content()).strip()


async def _stack_exchange(url):
    u = urlsplit(url)
    host = u.netloc.lower().removeprefix("www.")
    site = SE_SITES.get(host) or (host.removesuffix(".stackexchange.com") if host.endswith(".stackexchange.com") else None)
    m = SE_QUESTION.match(u.path)
    if not site or not m:
        return None
    qid = m.group(1)
    api = "https://api.stackexchange.com/2.3"
    common = {"site": site, "filter": "withbody"}
    q, a = await asyncio.gather(
        client.get(f"{api}/questions/{qid}", params=common),
        client.get(f"{api}/questions/{qid}/answers", params={**common, "sort": "votes", "order": "desc", "pagesize": 5}),
    )
    items = q.json().get("items") if q.status_code == 200 else None
    if not items:
        return None
    question = items[0]
    parts = [f"# {lxml.html.fromstring(question['title']).text_content()}",
             f"(score {question.get('score', 0)})", _se_html_to_text(question.get("body", ""))]
    for ans in (a.json().get("items", []) if a.status_code == 200 else []):
        mark = "accepted answer" if ans.get("is_accepted") else "answer"
        parts.append(f"## {mark}, score {ans.get('score', 0)}\n\n" + _se_html_to_text(ans.get("body", "")))
    return {"title": parts[0][2:], "text": "\n\n".join(parts), "status": 200, "final_url": url,
            "wire_bytes": len(q.content) + len(a.content), "via": "stackexchange-api"}


async def _from_worker(url):
    async with httpx.AsyncClient(timeout=20, headers={"Accept-Encoding": "zstd, gzip"}) as c:
        r = await c.post(f"{WORKER}/v1/fetch", json={"urls": [url], "max_chars": 200000, "deadline_ms": 15000})
        p = r.json()["pages"][0]
    p["via"] = "worker:" + p.get("via", "direct")
    return p


async def _convert(rec, body: bytes, ctype: str, encoding: str | None):
    if ctype == "application/pdf" or body[:5] == b"%PDF-":
        async with EXTRACT_SLOTS:
            text = await asyncio.to_thread(_pdf_text, body)
        rec.setdefault("title", "")
    elif ctype == "text/plain":
        text = body.decode(encoding or "utf-8", errors="replace")
        rec.setdefault("title", "")
    elif ctype and not ctype.startswith(TEXT_TYPES):
        rec["error"] = f"unsupported content-type {ctype}"
        return
    else:
        html = body.decode(encoding or "utf-8", errors="replace")
        async with EXTRACT_SLOTS:
            text, rec["title"] = await asyncio.to_thread(_extract, html, rec["final_url"])
    rec["text"] = text
    if not text.strip():
        # Nothing server-rendered to extract: the page builds itself with JavaScript.
        rec["error"] = "no extractable text (needs javascript)"


async def _load(url: str) -> dict:
    """Tiered fetch: site API adapters, then a plain client, then a Chrome-fingerprinted
    client for bot filters, then the residential worker (a different IP) when one is set."""
    t0 = time.monotonic()
    rec = {"url": url}
    try:
        adapted = await _stack_exchange(url)
        if adapted:
            rec.update(adapted)
        else:
            status, headers, body, final, wire = await _get_httpx(url)
            via = "direct"
            if _blocked(status, headers):
                status, headers, body, final, wire = await _get_browserlike(url)
                via = "browserlike"
            if _blocked(status, headers) and WORKER:
                rec.update(await _from_worker(url))
            else:
                rec.update({"status": status, "final_url": final, "wire_bytes": wire, "via": via})
                if status >= 400:
                    rec["error"] = f"http {status}"
                else:
                    ctype = headers.get("content-type", "").split(";")[0].strip().lower()
                    m = re.search(r"charset=([\w-]+)", headers.get("content-type", ""))
                    await _convert(rec, body, ctype, m.group(1) if m else None)
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


async def _pages(urls, deadline_s, max_chars, use_cache=True):
    out, tasks = {}, {}
    for u in urls:
        cached = pages.get(u) if use_cache else None
        if cached is not None:
            out[u] = dict(cached, cached=True)
        else:
            tasks[u] = _page_task(u) if use_cache else asyncio.create_task(_load(u))
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


async def _search(q: str, n: int, use_cache=True):
    key = (q, n)
    hit = searches.get(key) if use_cache else None
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
    # "cache": false measures a cold search (used by the benchmark).
    use_cache = d.get("cache", True) is not False
    try:
        results, cached = await _search(q, n, use_cache)
    except Exception as e:
        return _reply(request, {"error": f"search failed: {type(e).__name__}: {e}"[:300]}, 502)
    t_search = time.monotonic()
    got = await _pages([r["url"] for r in results[:fetch]], max(0.3, deadline - (t_search - t0)), max_chars, use_cache) if fetch else []
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
    got = await _pages(urls, deadline, max_chars, d.get("cache", True) is not False)
    return _reply(request, {"pages": got, "timing": {"total_ms": round((time.monotonic() - t0) * 1000)}})


async def healthz(request: Request):
    return _reply(request, {"ok": True, "pages_cached": len(pages.d), "inflight": len(inflight)})


app = Starlette(routes=[
    Route("/v1/search", search, methods=["POST"]),
    Route("/v1/fetch", fetch, methods=["POST"]),
    Route("/healthz", healthz),
])
