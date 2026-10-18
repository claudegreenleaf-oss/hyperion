"""Hyperion agent search relay.

Runs next to a fast uplink. One request from a slow client becomes a SearXNG query plus
parallel fetches of the top results, all at datacenter latency; the client gets back a
single compressed bundle of extracted text instead of paying DNS, TCP, TLS, redirects and
megabytes of subresources per page over its own link.

Pages still loading at the deadline keep loading in the background, so a follow-up
/v1/fetch for them is usually served from cache.
"""

import asyncio
import base64
import gzip
import json
import os
import re
import threading
import time
import urllib.parse
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
from starlette.responses import FileResponse, Response, StreamingResponse
from starlette.routing import Route

import answer as ans

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
EXTRACT_SLOTS = asyncio.Semaphore(3)


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


TITLE = re.compile(r"<title[^>]*>(.*?)</title>", re.I | re.S)


def _extract(html: str, url: str) -> tuple[str, str]:
    # fast=True skips trafilatura's fallback extractors: identical text on the pages measured,
    # 1.3 to 3x faster. The title comes from <title> rather than a second full metadata parse.
    text = trafilatura.extract(
        html, url=url, output_format="markdown", include_comments=False, include_tables=True,
        include_images=False, include_links=False, deduplicate=True, fast=True,
    ) or ""
    m = TITLE.search(html, 0, 200000)
    title = lxml.html.fromstring(m.group(1)).text_content().strip() if m and m.group(1).strip() else ""
    return text, title


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


# PDFium is not thread-safe; two extraction threads inside it at once segfault the process.
PDFIUM_LOCK = threading.Lock()


def _pdf_text(data: bytes) -> str:
    with PDFIUM_LOCK:
        return _pdf_text_locked(data)


def _pdf_text_locked(data: bytes) -> str:
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


WIKI_PAGE = re.compile(r"^([a-z\-]+)\.(?:m\.)?wikipedia\.org$")
API_UA = "hyperion-relay/0.1 (https://github.com/claudegreenleaf-oss/hyperion)"


async def _wikipedia(url):
    # The extracts API returns the article as plain text: about 0.3 s, against about 2 s to
    # fetch and extract a long article's HTML.
    u = urlsplit(url)
    m = WIKI_PAGE.match(u.netloc.lower())
    if not m or not u.path.startswith("/wiki/") or u.query:
        return None
    title = urllib.parse.unquote(u.path[len("/wiki/"):])
    if not title or ":" in title:
        return None
    r = await client.get(f"https://{m.group(1)}.wikipedia.org/w/api.php", headers={"User-Agent": API_UA}, params={
        "action": "query", "prop": "extracts", "explaintext": 1, "redirects": 1, "titles": title, "format": "json"})
    if r.status_code != 200:
        return None
    page = next(iter(r.json().get("query", {}).get("pages", {}).values()), {})
    text = page.get("extract") or ""
    if not text:
        return None
    return {"title": page.get("title", title), "text": text, "status": 200, "final_url": url,
            "wire_bytes": len(r.content), "via": "wikipedia-api"}


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
        t0 = time.monotonic()
        async with EXTRACT_SLOTS:
            t1 = time.monotonic()
            text, rec["title"] = await asyncio.to_thread(_extract, html, rec["final_url"])
        rec["slot_wait_ms"], rec["extract_ms"] = round((t1 - t0) * 1000), round((time.monotonic() - t1) * 1000)
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
        adapted = await _stack_exchange(url) or await _wikipedia(url)
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
    return [_clip(out[u], max_chars) for u in urls]


async def _searx(q: str, n: int, bang: str = "") -> list[dict]:
    r = await client.get(f"{SEARXNG}/search", params={"q": f"{bang} {q}" if bang else q, "format": "json",
                                                       "language": "en"}, timeout=6.0)
    r.raise_for_status()
    seen, results = set(), []
    for x in r.json().get("results", []):
        u = x.get("url")
        if not u or u in seen:
            continue
        seen.add(u)
        results.append({"title": x.get("title", ""), "url": u, "snippet": (x.get("content") or "")[:300]})
        if len(results) >= n:
            break
    return results


BING_UNWRAP = re.compile(r"^https?://(?:www\.)?bing\.com/ck/a\?")
bing_client = httpx.AsyncClient(
    http2=True, timeout=httpx.Timeout(5.0, connect=2.0),
    limits=httpx.Limits(max_keepalive_connections=8, keepalive_expiry=300),
    headers={"User-Agent": UA, "Accept": "text/html", "Accept-Language": "en-US,en;q=0.9"},
)


def _bing_url(href: str) -> str:
    # Result links are click-tracking redirects carrying the target as "a1" + urlsafe base64.
    if not BING_UNWRAP.match(href):
        return href
    u = urllib.parse.parse_qs(urlsplit(href).query).get("u", [""])[0]
    if not u.startswith("a1"):
        return href
    try:
        return base64.urlsafe_b64decode(u[2:] + "=" * (-len(u[2:]) % 4)).decode()
    except ValueError:
        return href


async def _bing(q: str, n: int) -> list[dict]:
    """Bing's own results page, queried directly in the en-US market: same top 3 as Yahoo in
    54 of 60 measured queries (Yahoo serves Bing's index), about 100 ms sooner. SearXNG's
    Bing engine is not used for this because it garbles natural-language queries."""
    r = await bing_client.get("https://www.bing.com/search", params={"q": q, "setlang": "en-US", "cc": "US", "mkt": "en-US"})
    r.raise_for_status()
    doc = await asyncio.to_thread(lxml.html.fromstring, r.text)
    seen, results = set(), []
    for li in doc.xpath('//li[contains(@class,"b_algo")]'):
        a = li.xpath(".//h2/a")
        if not a:
            continue
        u = _bing_url(a[0].get("href", ""))
        if not u.startswith("http") or u in seen:
            continue
        seen.add(u)
        snip = li.xpath('.//p | .//div[contains(@class,"b_caption")]')
        results.append({"title": a[0].text_content().strip(), "url": u,
                        "snippet": (snip[0].text_content().strip() if snip else "")[:300]})
        if len(results) >= n:
            break
    if not results:
        raise RuntimeError("bing returned no results (challenge page?)")
    return results


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


GOOD_CHARS = 300  # a fetched page counts toward the reply once it has this much text


def _clip(rec, max_chars):
    r = dict(rec)
    text = r.get("text") or ""
    r["chars"] = len(text)
    if len(text) > max_chars:
        r["text"] = text[:max_chars]
        r["truncated"] = True
    return r


class SearchError(Exception):
    pass


async def search(request: Request):
    t0 = time.monotonic()
    d = await request.json()
    q = str(d.get("q", "")).strip()
    if not q:
        return _reply(request, {"error": "missing q"}, 400)
    try:
        out = await _run_search(q, _int(d, "n", 8, 1, 20), _int(d, "fetch", 3, 0, 8),
                                _int(d, "max_chars", 6000, 200, 100000),
                                t0 + _int(d, "deadline_ms", 5000, 500, 30000) / 1000,
                                d.get("cache", True) is not False, t0)
    except SearchError as e:
        return _reply(request, {"error": str(e)}, 502)
    return _reply(request, out)


async def _run_search(q, n, fetch, max_chars, deadline, use_cache, t0):
    """Search, then read `fetch` result pages, optimized for time to a useful answer.

    Pages are read from the top fetch+2 results, starting the moment Yahoo answers, and the
    reply goes out as soon as `fetch` of them have usable text, in rank order, instead of
    waiting for the slowest page. Stragglers keep loading into the cache. (Speculatively
    loading Bing's top results while Yahoo works was measured and dropped: only 4 of 60 of
    them ended up in the final top 3, so it mostly took extraction slots from real pages.)
    """
    # use_cache False measures a cold search (used by the benchmark).
    tasks: dict[str, asyncio.Task] = {}

    def start(url):
        if url in tasks:
            return
        hit = pages.get(url) if use_cache else None
        if hit is not None:
            done = asyncio.get_running_loop().create_future()
            done.set_result(dict(hit, cached=True))
            tasks[url] = done
        else:
            tasks[url] = _page_task(url) if use_cache else asyncio.create_task(_load(url))

    results, engine = (searches.get((q, n)) if use_cache else None), "cache"
    search_cached = results is not None
    if results is None:
        # Race Yahoo (via SearXNG) against Bing's own page: they rank the same index almost
        # identically, so the first to answer sets the results and seeds the page fetches,
        # and either one covers for the other when it is slow, rate-limited or challenged.
        racers = {asyncio.create_task(_searx(q, n, bang="!yh"), name="yahoo"),
                  asyncio.create_task(_bing(q, n), name="bing")}
        results, err, engine = None, None, ""
        pending = racers
        while pending and results is None:
            done, pending = await asyncio.wait(pending, timeout=max(0.5, min(5.0, deadline - time.monotonic())),
                                               return_when=asyncio.FIRST_COMPLETED)
            if not done:
                break
            for t in done:
                if t.exception() is None and t.result():
                    results, engine = t.result(), t.get_name()
                    break
                err = t.exception() or err
        for t in pending:
            t.cancel()
        if not results:
            msg = f"{type(err).__name__}: {err}" if err else "no results"
            raise SearchError(f"search failed: {msg}"[:300])
        searches.put((q, n), results)
    t_search = time.monotonic()

    chosen = []
    if fetch:
        candidates = [r["url"] for r in results[:fetch + 2]]
        for u in candidates:
            start(u)

        def good():
            return [u for u in candidates if tasks[u].done() and not tasks[u].exception()
                    and len(tasks[u].result().get("text") or "") >= GOOD_CHARS]

        pending = {tasks[u] for u in candidates}
        while len(good()) < fetch and pending:
            left = deadline - time.monotonic()
            if left <= 0:
                break
            done, pending = await asyncio.wait(pending, timeout=left, return_when=asyncio.FIRST_COMPLETED)
        chosen = [_clip(tasks[u].result(), max_chars) for u in good()[:fetch]]
        if len(chosen) < fetch:
            # Say what happened to the rest so the agent can decide whether to retry them.
            for u in candidates:
                if len(chosen) >= fetch:
                    break
                t = tasks[u]
                if t.done() and len(t.result().get("text") or "") >= GOOD_CHARS:
                    continue
                rec = t.result() if t.done() and not t.exception() else {
                    "url": u, "error": "still loading at deadline; fetch again shortly"}
                chosen.append(_clip(rec, max_chars))
    return {
        "query": q,
        "results": results,
        "pages": chosen,
        "timing": {"search_ms": round((t_search - t0) * 1000), "total_ms": round((time.monotonic() - t0) * 1000),
                   "search_cached": search_cached, "engine": engine},
    }


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


answers = TTLCache(300, 10 * 60)


async def _produce(q: str, st: ans.Stream):
    """Fill one answer stream: sources, page texts, the extracted sentence, then the answer."""
    t0 = time.monotonic()
    try:
        out = await _run_search(q, 8, 3, 8000, t0 + 4.0, True, t0)
    except SearchError as e:
        await st.put({"type": "error", "error": str(e)}, last=True)
        return
    results = [{"title": r.get("title", ""), "url": r["url"], "host": ans.host_of(r["url"]),
                "snippet": r.get("content") or r.get("snippet") or "", "official": ans.is_official(r["url"])}
               for r in out["results"]]
    await st.put({"type": "sources", "results": results, "timing": out["timing"]})
    got = [p for p in out["pages"] if p.get("text")]
    await st.put({"type": "pages", "pages": [
        {"url": p["url"], "final_url": p.get("final_url", p["url"]), "title": p.get("title", ""),
         "host": ans.host_of(p.get("final_url", p["url"])), "text": p["text"]} for p in got]})
    picks = ans.best_sentences(q, got)
    best = picks[0] if picks else None
    if best:
        page = got[best["i"]]
        await st.put({"type": "passage", "text": best["text"], "score": best["score"], "url": page["url"],
                      "host": ans.host_of(page.get("final_url", page["url"])), "title": page.get("title", "")})
    excerpts = [{"text": c["text"], "host": ans.host_of(got[c["i"]]["url"]), "url": got[c["i"]]["url"]} for c in picks[:5]]
    m = await ans.model_answer(q, excerpts)
    if m and m.get("easy"):
        src = excerpts[m["ref"]]
        ev = {"type": "answer", "easy": True, "short": m["short"], "url": src["url"], "host": src["host"],
              "passage": src["text"], "by": "model", "model": ans.MODEL, "model_ms": m["ms"]}
    elif best:
        easy = ans.heuristic_easy(q, best) and m is None
        ev = {"type": "answer", "easy": easy, "short": best["text"], "url": excerpts[0]["url"],
              "host": excerpts[0]["host"], "by": "extract", "model_ms": m["ms"] if m else None}
    else:
        ev = {"type": "answer", "easy": False, "short": "", "url": results[0]["url"] if results else "",
              "host": results[0]["host"] if results else "", "by": "none"}
    await st.put(ev)
    await st.put({"type": "done"}, last=True)


async def answer(request: Request):
    """POST {q, texts?, prefetch?} -> NDJSON events: sources, pages (only with texts), passage,
    answer, done. Requests for the same query share one stream, and finished ones replay
    from cache, so the listener's speculative request warms the final one."""
    d = await request.json()
    q = re.sub(r"\s+", " ", str(d.get("q", ""))).strip()[:300]
    if not q:
        return _reply(request, {"error": "missing q"}, 400)
    key = q.lower().rstrip("?. ")
    st = answers.get(key)
    if st is None:
        st = ans.Stream()
        answers.put(key, st)
        asyncio.get_running_loop().create_task(_produce(q, st))
    if d.get("prefetch"):
        return Response(status_code=204)
    texts = bool(d.get("texts"))

    async def lines():
        async for ev in st.follow():
            if ev["type"] == "pages" and not texts:
                continue
            yield json.dumps(ev, ensure_ascii=False, separators=(",", ":")) + "\n"

    return StreamingResponse(lines(), media_type="application/x-ndjson", headers={"Cache-Control": "no-store"})


PAGE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "answer.html")


async def answer_page(request: Request):
    return FileResponse(PAGE, media_type="text/html", headers={"Cache-Control": "max-age=60"})


async def healthz(request: Request):
    return _reply(request, {"ok": True, "pages_cached": len(pages.d), "inflight": len(inflight),
                            "answers_cached": len(answers.d), "model": ans.MODEL if ans.LLM else None})


app = Starlette(routes=[
    Route("/v1/search", search, methods=["POST"]),
    Route("/v1/fetch", fetch, methods=["POST"]),
    Route("/v1/answer", answer, methods=["POST"]),
    Route("/a", answer_page),
    Route("/healthz", healthz),
])
