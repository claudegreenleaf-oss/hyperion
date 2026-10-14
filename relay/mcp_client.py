# /// script
# requires-python = ">=3.11"
# dependencies = ["mcp>=2,<3", "httpx>=0.28", "zstandard>=0.23"]
# ///
"""Thin MCP front end for the Hyperion search relay.

All the heavy work (search, page fetches, text extraction) happens on the relay; this
process only keeps one warm keep-alive connection to it and formats the reply as compact
text, so a search costs the agent's link one round trip and a few kilobytes.
"""

import os

import httpx
from mcp.server.mcpserver import MCPServer

RELAY = os.environ.get("HYPERION_RELAY", "http://100.104.232.110:8790")

mcp = MCPServer("hyperion-search")
client = httpx.Client(
    base_url=RELAY,
    headers={"Accept-Encoding": "zstd, gzip"},
    timeout=httpx.Timeout(40.0, connect=10.0),
    limits=httpx.Limits(max_keepalive_connections=4, keepalive_expiry=300),
)


def _post(path: str, body: dict) -> dict:
    r = client.post(path, json=body)
    r.raise_for_status()
    return r.json()


def _page_block(p: dict) -> str:
    head = f"## {p.get('title') or p['url']}\n{p.get('final_url') or p['url']}"
    if p.get("error"):
        return f"{head}\n[not read: {p['error']}]"
    tail = f"\n[truncated: {p['chars']} chars total; call read_pages with a larger max_chars for more]" if p.get("truncated") else ""
    return f"{head}\n\n{p.get('text', '').strip()}{tail}"


@mcp.tool()
def web_search(query: str, num_results: int = 8, read_top: int = 3, max_chars_per_page: int = 6000) -> str:
    """Search the web and read the top results in one call.

    Returns the ranked result list (title, URL, snippet) followed by the extracted main
    text of the first `read_top` results. Use read_pages for any other result.

    Args:
        query: what to search for; plain keywords or a natural question both work.
        num_results: how many results to list (1 to 20).
        read_top: how many of the top results to fetch and extract (0 to 8).
        max_chars_per_page: cap on extracted text per page.
    """
    d = _post("/v1/search", {"q": query, "n": num_results, "fetch": read_top, "max_chars": max_chars_per_page})
    if d.get("error"):
        return f"search failed: {d['error']}"
    lines = [f"# Results for: {d['query']}"]
    for i, r in enumerate(d["results"], 1):
        lines.append(f"{i}. {r['title']}\n   {r['url']}\n   {r['snippet']}")
    out = "\n".join(lines)
    if d["pages"]:
        out += "\n\n# Top pages\n\n" + "\n\n".join(_page_block(p) for p in d["pages"])
    t = d["timing"]
    return out + f"\n\n(relay: search {t['search_ms']} ms, total {t['total_ms']} ms)"


@mcp.tool()
def read_pages(urls: list[str], max_chars: int = 12000) -> str:
    """Fetch web pages and return their extracted main text (up to 10 URLs, fetched in parallel).

    Args:
        urls: http(s) URLs to read.
        max_chars: cap on extracted text per page.
    """
    d = _post("/v1/fetch", {"urls": urls, "max_chars": max_chars})
    if d.get("error"):
        return f"fetch failed: {d['error']}"
    return "\n\n".join(_page_block(p) for p in d["pages"])


if __name__ == "__main__":
    mcp.run()
