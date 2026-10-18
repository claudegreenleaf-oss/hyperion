"""Answer-first layer over the relay's search: pick the sentence that answers, then ask a
small model for a one-line answer.

The model is the Qwen already serving Murmur and the voice assistant on a6000-box (vLLM,
OpenAI-compatible, tailnet only): about 0.14 s for a one-line answer. opencode's free models
were the first choice, but their free tier refuses every request that is not OpenCode's own
unmodified agent, and that agent took over 40 s.

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

LLM = os.environ.get("RELAY_LLM", "")  # OpenAI-compatible base URL, e.g. https://a6000-box...:8993
MODEL = os.environ.get("RELAY_LLM_MODEL", "qwen")
MODEL_BUDGET_S = float(os.environ.get("RELAY_MODEL_BUDGET_S", "1.2"))

STOP = set("""a an the of to in on for at by with from and or but is are was were be been being do does did
how what when where which who whom whose why can could should would will shall may might must it its this that
these those there their they them i me my we our you your he she his her as if than then so not no yes vs
about into over under up down out just only also any some all each per get got many much have has had
use used using way ways""".split())
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


def _units(text: str) -> list[str]:
    out = []
    for raw in SENT_SPLIT.split(text[:20000]):
        u = clean(raw)
        if len(u) >= 3:
            out.append(u)
    return out


def best_sentences(q: str, pages: list[dict], k: int = 5) -> list[dict]:
    """Score windows of the fetched pages against the query. A window is a sentence or line
    plus its short neighbours, so a heading like "November 1" travels with the line under it.
    Score: share of query terms covered (rarer terms weigh more), a bonus when the query wants
    a number and the window has one (a penalty when it has none), and small preferences for
    higher-ranked pages and for brevity."""
    qt = list(dict.fromkeys(terms(q)))
    if not qt:
        return []
    wins = []
    for pi, p in enumerate(pages):
        us = _units(p.get("text") or "")
        for j, u in enumerate(us):
            if len(u) < 20 and not DIGIT.search(u):
                continue
            parts = [u]
            if j > 0 and len(us[j - 1]) <= 90:
                parts.insert(0, us[j - 1])
            if j + 1 < len(us) and len(us[j + 1]) <= 90:
                parts.append(us[j + 1])
            w = " · ".join(parts)
            if 20 <= len(w) <= 480:
                wins.append((pi, w, set(terms(w))))
    if not wins:
        return []
    n = len(wins)
    df = {t: sum(1 for _, _, st in wins if t in st) for t in qt}
    weight = {t: 1.0 + math.log((n + 1) / (df[t] + 1)) for t in qt}
    total = sum(weight.values())
    wants_num = bool(WANTS_NUMBER.search(q))
    scored = []
    for pi, w, st in wins:
        cover = sum(weight[t] for t in qt if t in st) / total
        if cover == 0:
            continue
        score = cover
        if wants_num:
            score += 0.25 if DIGIT.search(w) else -0.15
        score -= 0.04 * pi
        score -= max(0, len(w) - 260) / 1500
        scored.append({"score": round(score, 3), "text": w, "i": pi})
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


_llm = httpx.AsyncClient(http2=True, timeout=httpx.Timeout(5.0, connect=1.0)) if LLM else None
REPLY_REF = re.compile(r"\s*\[(\d)\]\s*\.?\s*$")
SYSTEM = ("You answer web search questions from the numbered excerpts you are given. Reply with only the answer, "
          "at most 15 words, no preamble, then the number of the excerpt it came from in brackets, like [2]. "
          "If the excerpts do not clearly answer the question, or the answer needs more than one sentence, "
          "reply exactly NEEDS_PAGE.")


async def model_answer(q: str, excerpts: list[dict]) -> dict | None:
    """One-line answer from the model, or None when it is slow or unavailable."""
    if not _llm or not excerpts:
        return None
    user = f"Question: {q}\n\nExcerpts:\n" + "\n".join(
        f"[{n + 1}] ({e['host']}) {e['text']}" for n, e in enumerate(excerpts))
    t0 = time.monotonic()
    try:
        r = await asyncio.wait_for(_llm.post(f"{LLM}/v1/chat/completions", json={
            "model": MODEL, "max_tokens": 48, "temperature": 0,
            "chat_template_kwargs": {"enable_thinking": False},
            "messages": [{"role": "system", "content": SYSTEM}, {"role": "user", "content": user}],
        }), MODEL_BUDGET_S)
        reply = (r.json()["choices"][0]["message"]["content"] or "").strip()
    except (asyncio.TimeoutError, httpx.HTTPError, KeyError, IndexError, ValueError):
        return None
    ms = round((time.monotonic() - t0) * 1000)
    if not reply or "NEEDS_PAGE" in reply:
        return {"easy": False, "ms": ms}
    m = REPLY_REF.search(reply)
    ref = int(m.group(1)) - 1 if m else 0
    short = REPLY_REF.sub("", reply).strip().strip('"').strip()
    if not short or len(short) > 160:
        return {"easy": False, "ms": ms}
    return {"easy": True, "short": short, "ref": ref if 0 <= ref < len(excerpts) else 0, "ms": ms}


COMPUTE = re.compile(r"^\s*(what('s| is)\s+)?(\d[\d,.\s]*\s*([-+*/x^%]|plus|minus|times|divided by|percent of)\s*\d|"
                     r"(the\s+)?(square root|cube root|log|sin|cos|tan)\b|\d[\d,.]*\s*(°?[fc]|degrees|miles|km|kg|lbs?|pounds|"
                     r"feet|ft|meters|inches|cups|ounces|oz|liters|gallons)\s+(in|to)\s+)", re.I)


def is_computation(q: str) -> bool:
    return bool(COMPUTE.search(q))


async def _chat(system: str, user: str, max_tokens: int, budget: float) -> str | None:
    if not _llm:
        return None
    try:
        r = await asyncio.wait_for(_llm.post(f"{LLM}/v1/chat/completions", json={
            "model": MODEL, "max_tokens": max_tokens, "temperature": 0,
            "chat_template_kwargs": {"enable_thinking": False},
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
        }), budget)
        return (r.json()["choices"][0]["message"]["content"] or "").strip()
    except (asyncio.TimeoutError, httpx.HTTPError, KeyError, IndexError, ValueError):
        return None


REWRITE = ("Turn the spoken question into a short web search query of 3 to 9 words. Speech recognition may have "
           "misheard words: fix them when the context makes the intended word clear (college admissions: REA, EA, "
           "ED, RD, Common App, supplemental essays; school: AP, SAT, FAFSA). Keep names and numbers. "
           "Reply with only the query.")


async def search_query(spoken: str) -> str | None:
    """A clean search query from a spoken question (about 0.15 s)."""
    out = await _chat(REWRITE, spoken, 24, 0.8)
    if not out or len(out) > 120 or "\n" in out:
        return None
    return out.strip().strip('"').strip()


async def direct_answer(q: str) -> str | None:
    """Arithmetic and unit conversions need no search."""
    out = await _chat("Answer the arithmetic or unit conversion with only the result, at most 10 words. "
                      "If it is not arithmetic or a unit conversion, reply exactly NO.", q, 24, 0.8)
    if not out or out.upper().startswith("NO") or len(out) > 80:
        return None
    return out


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
