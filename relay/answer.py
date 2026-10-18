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
SYSTEM = ("You answer spoken questions for an answer panel, using the numbered web excerpts you are given. "
          "Reply with only the answer, as short as possible, no preamble. A single fact: one line. A list (essay "
          "prompts, steps, options): one item per line, at most 8 items, at most 90 words in all. End with the number "
          "of the excerpt it came from in brackets, like [2]. Reply exactly NEEDS_PAGE only when the excerpts contain "
          "nothing that answers the question.")


async def model_answer(q: str, excerpts: list[dict]) -> dict | None:
    """One-line answer from the model, or None when it is slow or unavailable."""
    if not _llm or not excerpts:
        return None
    user = f"Question: {q}\n\nExcerpts:\n" + "\n".join(
        f"[{n + 1}] ({e['host']}) {e['text']}" for n, e in enumerate(excerpts))
    t0 = time.monotonic()
    try:
        r = await asyncio.wait_for(_llm.post(f"{LLM}/v1/chat/completions", json={
            "model": MODEL, "max_tokens": 200, "temperature": 0,
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
    if not short or len(short) > 900:
        return {"easy": False, "ms": ms}
    return {"easy": True, "short": short, "ref": ref if 0 <= ref < len(excerpts) else 0, "ms": ms}


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


ROUTE = """Route a request spoken to a Mac voice assistant. Reply with one JSON object only.
{{"kind":"command","id":"c3","args":{{"arg":"value"}},"done":"at most 4 words"}} if one of COMMANDS does it. Use its argument names; times like 2026-10-19T09:00; for on, off or a level use a set command with a value, not a toggle.
{{"kind":"open","url":"https://...","label":"Site"}} to go to a website. Installed apps are commands.
{{"kind":"answer","answer":"..."}} only for arithmetic, unit conversions, definitions or spelling (60 words max).
{{"kind":"search","query":"..."}} for any other question: a short web query, fixing misheard words ("REI" or "RAE" is REA, "supplements" is supplemental essays).
COMMANDS:
{commands}"""


async def route(spoken: str, commands: str = "", now: str = "") -> dict | None:
    """Decide in one quick model call (about 0.3 s) whether to run a Mac command, open a site,
    answer from knowledge, or search, and clean up the search query."""
    # Everything that changes per request goes in the user message, so the long instructions
    # and catalog stay an identical prefix that the model server keeps cached.
    # The model sees short aliases (c1, c2, ...) instead of ids like
    # "rc:mattisssa/spotify-player/justPlay": every output token costs about 10 ms.
    alias, lines = {}, []
    for n, line in enumerate(l for l in commands.splitlines() if "|" in l):
        cid, rest = line.split("|", 1)
        alias[f"c{n + 1}"] = cid.strip()
        lines.append(f"c{n + 1} |{rest}")
    prompt = ROUTE.format(commands="\n".join(lines) or "(none)")
    user = f"Now: {now or time.strftime('%Y-%m-%d %H:%M %A')}\nRequest: {spoken}"
    out = await _chat(prompt, user, 120, 2.5)
    if not out:
        return None
    m = re.search(r"\{.*\}", out, re.S)
    if not m:
        return None
    try:
        r = __import__("json").loads(m.group(0))
    except ValueError:
        return None
    kind = r.get("kind")
    if kind == "command" and r.get("id"):
        args = r.get("args") if isinstance(r.get("args"), dict) else {}
        cid = alias.get(str(r["id"]).strip(), str(r["id"]))
        return {"kind": "command", "id": cid, "args": {str(k): str(v) for k, v in args.items()},
                "done": str(r.get("done") or "Done")[:60]}
    if kind == "open" and str(r.get("url", "")).startswith(("https://", "http://")):
        return {"kind": "open", "url": r["url"], "label": str(r.get("label") or host_of(r["url"]))[:40]}
    if kind == "answer" and r.get("answer") and len(str(r["answer"])) <= 600:
        return {"kind": "answer", "answer": str(r["answer"]).strip()}
    if kind == "search" and r.get("query"):
        return {"kind": "search", "query": str(r["query"]).strip()[:150]}
    return None


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
