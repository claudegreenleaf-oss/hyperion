"""Answer-first layer over the relay's search: pick the sentence that answers, then ask a
small free model (through a local `opencode serve`) for a one-line answer.

The extracted sentence never waits on the model. The model only gets a short budget; if it
is slow or unsure, the extracted sentence stands and the caller decides whether the answer
is small enough for the notch or needs the full page.
"""

import asyncio
import math
import os
import re
import time
from urllib.parse import urlsplit

import httpx

OPENCODE = os.environ.get("RELAY_OPENCODE", "")  # e.g. http://127.0.0.1:4096
MODEL = os.environ.get("RELAY_MODEL", "opencode/nemotron-3.5-lightning-free")
AGENT = os.environ.get("RELAY_AGENT", "answer")
MODEL_BUDGET_S = float(os.environ.get("RELAY_MODEL_BUDGET_S", "1.6"))

STOP = set("""a an the of to in on for at by with from and or but is are was were be been being do does did
how what when where which who whom whose why can could should would will shall may might must it its this that
these those there their they them i me my we our you your he she his her as if than then so not no yes vs
about into over under up down out just only also any some all each per get got""".split())
QUESTION = re.compile(r"^\s*(when|what|who|whom|whose|where|which|how (many|much|long|old|far|tall|big|fast)|"
                      r"is|are|was|were|does|do|did|can|could|define|meaning of)\b", re.I)
WANTS_NUMBER = re.compile(r"\b(when|date|deadline|year|how (many|much|long|old|far|tall|big|fast)|"
                          r"price|cost|population|number|time)\b", re.I)
DIGIT = re.compile(r"\d")
SENT_SPLIT = re.compile(r"(?<=[.!?])\s+(?=[A-Z0-9\"'(])|\n+")
MD = re.compile(r"[*_`#>]+|\[(.*?)\]\([^)]*\)")
OFFICIAL_SUFFIX = (".gov", ".edu", ".mil", ".int")
OFFICIAL_HOSTS = {"developer.mozilla.org", "docs.python.org", "git-scm.com", "react.dev", "support.apple.com",
                  "developer.apple.com", "developer.chrome.com", "learn.microsoft.com", "who.int", "nasa.gov"}


def host_of(url: str) -> str:
    h = urlsplit(url).hostname or ""
    return h[4:] if h.startswith("www.") else h


def is_official(url: str) -> bool:
    h = host_of(url)
    return h.endswith(OFFICIAL_SUFFIX) or h in OFFICIAL_HOSTS or any(h.endswith("." + o) for o in OFFICIAL_HOSTS)


def terms(text: str) -> list[str]:
    return [w for w in re.findall(r"[a-z0-9]+", text.lower()) if w not in STOP and len(w) > 1]


def clean(s: str) -> str:
    s = MD.sub(lambda m: m.group(1) or "", s)
    s = s.replace("|", " ").replace(" ", " ")
    return re.sub(r"\s+", " ", s).strip()


def question_like(q: str) -> bool:
    return bool(QUESTION.match(q)) or q.strip().endswith("?")


def best_sentences(q: str, pages: list[dict], k: int = 4) -> list[dict]:
    """Score every sentence of the fetched pages against the query: share of query terms it
    covers (weighted so rarer terms count more), a bonus when the query wants a number and the
    sentence has one, and a small preference for higher-ranked pages and for brevity."""
    qt = list(dict.fromkeys(terms(q)))
    if not qt:
        return []
    sents = []
    df = {t: 0 for t in qt}
    for pi, p in enumerate(pages):
        text = p.get("text") or ""
        for raw in SENT_SPLIT.split(text[:20000]):
            s = clean(raw)
            if 25 <= len(s) <= 400:
                st = set(terms(s))
                for t in qt:
                    if t in st:
                        df[t] += 1
                sents.append((pi, s, st))
    if not sents:
        return []
    n = len(sents)
    w = {t: 1.0 + math.log((n + 1) / (df[t] + 1)) for t in qt}
    total = sum(w.values())
    wants_num = bool(WANTS_NUMBER.search(q))
    scored = []
    for pi, s, st in sents:
        cover = sum(w[t] for t in qt if t in st) / total
        if cover == 0:
            continue
        score = cover
        if wants_num and DIGIT.search(s):
            score += 0.15
        score -= 0.04 * pi
        score -= max(0, len(s) - 220) / 2000
        scored.append({"score": round(score, 3), "text": s, "i": pi})
    scored.sort(key=lambda x: -x["score"])
    out, seen = [], set()
    for c in scored:
        key = c["text"][:60].lower()
        if key not in seen:
            seen.add(key)
            out.append(c)
        if len(out) >= k:
            break
    return out


def heuristic_easy(q: str, best: dict | None) -> bool:
    return bool(best) and question_like(q) and best["score"] >= 0.62 and len(best["text"]) <= 180


_oc = httpx.AsyncClient(timeout=httpx.Timeout(10.0, connect=1.0)) if OPENCODE else None
REPLY_REF = re.compile(r"\s*\[(\d)\]\s*\.?\s*$")
SYSTEM = ("You answer web search questions from the numbered excerpts you are given. Reply with only the answer, "
          "at most 15 words, no preamble, then the number of the excerpt it came from in brackets, like [2]. "
          "If the excerpts do not clearly answer the question, or the answer needs more than one sentence, "
          "reply exactly NEEDS_PAGE.")


async def model_answer(q: str, excerpts: list[dict]) -> dict | None:
    """One-line answer from the free model, or None when it is slow, unsure or unavailable."""
    if not _oc or not excerpts:
        return None
    body_text = f"Question: {q}\n\nExcerpts:\n" + "\n".join(
        f"[{n + 1}] ({e['host']}) {e['text']}" for n, e in enumerate(excerpts))
    provider, _, model = MODEL.partition("/")
    t0 = time.monotonic()
    sid = None
    try:
        r = await asyncio.wait_for(_oc.post(f"{OPENCODE}/session", json={"title": "answer"}), MODEL_BUDGET_S)
        sid = r.json()["id"]
        left = MODEL_BUDGET_S - (time.monotonic() - t0)
        r = await asyncio.wait_for(_oc.post(f"{OPENCODE}/session/{sid}/message", json={
            "agent": AGENT, "model": {"providerID": provider, "modelID": model}, "system": SYSTEM,
            "parts": [{"type": "text", "text": body_text}],
        }), max(0.2, left))
        reply = " ".join(p.get("text", "") for p in r.json().get("parts", []) if p.get("type") == "text").strip()
    except (asyncio.TimeoutError, httpx.HTTPError, KeyError, ValueError):
        return None
    finally:
        if sid:
            asyncio.get_running_loop().create_task(_forget(sid))
    if not reply or "NEEDS_PAGE" in reply:
        return {"easy": False, "ms": round((time.monotonic() - t0) * 1000)}
    m = REPLY_REF.search(reply)
    ref = int(m.group(1)) - 1 if m else 0
    short = REPLY_REF.sub("", reply).strip().strip('"').strip()
    if not short or len(short) > 160:
        return {"easy": False, "ms": round((time.monotonic() - t0) * 1000)}
    return {"easy": True, "short": short, "ref": ref if 0 <= ref < len(excerpts) else 0,
            "ms": round((time.monotonic() - t0) * 1000)}


async def _forget(sid: str):
    try:
        await _oc.delete(f"{OPENCODE}/session/{sid}")
    except httpx.HTTPError:
        pass


class Stream:
    """Events of one answer, shared by every request for the same query (the listener's
    speculative request, the final one and the answer page's) and replayed from cache."""

    def __init__(self):
        self.events: list[dict] = []
        self.done = False
        self.cond = asyncio.Condition()
        self.t0 = time.monotonic()

    async def put(self, ev: dict, last: bool = False):
        ev["ms"] = round((time.monotonic() - self.t0) * 1000)
        async with self.cond:
            self.events.append(ev)
            self.done = self.done or last
            self.cond.notify_all()

    async def follow(self):
        i = 0
        while True:
            async with self.cond:
                await self.cond.wait_for(lambda: len(self.events) > i or self.done)
                batch, finished = self.events[i:], self.done
            for ev in batch:
                yield ev
            i += len(batch)
            if finished and i >= len(self.events):
                return
