# Hyperion search relay

Agent web search for slow, lossy links. An agent's search normally costs its own link
DNS, TCP, TLS and redirect round trips plus megabytes of subresources for every page it
reads. Here, one request to a relay with a fast uplink runs the search (self-hosted
SearXNG), fetches the top results in parallel, extracts their main text (trafilatura), and
returns everything as one zstd-compressed JSON bundle: one round trip and a few kilobytes.

## Pieces

- `server.py`: the relay (Starlette + httpx). `POST /v1/search {q, n, fetch, max_chars,
  deadline_ms}` and `POST /v1/fetch {urls, max_chars, deadline_ms}`. Pages still loading at
  the deadline finish into a cache, so a follow-up fetch is usually instant.
- `mcp_client.py`: thin stdio MCP server for Claude Code and other agents, with the tools
  `web_search` and `read_pages`. It holds one keep-alive connection to the relay and does no
  heavy work itself.
- Fetching is tiered: Stack Exchange question pages go through the official API (Stack
  Overflow puts every non-browser client behind a Cloudflare JavaScript challenge); other
  pages try a plain client, then a Chrome TLS fingerprint (curl_cffi), then a worker relay on
  a6000-box's residential IP (`RELAY_WORKER`), since some sites (deepwiki on Vercel) refuse
  datacenter IPs. PDFs are extracted with pypdfium2 (first 40 pages).
- `deploy/`: the SearXNG settings (secret removed) and systemd units used on scotus-vps.

## Deployment (scotus-vps)

- `/opt/hyperion-relay/{searxng,relay}` with separate venvs; units `hyperion-searxng`
  (127.0.0.1:8855) and `hyperion-relay` (tailnet 100.104.232.110:8790), both inside
  `hyperion-relay.slice` (1.1 GB memory, 1.5 CPUs) so SCOTUS prod keeps its headroom.
- SearXNG egresses through the local Cloudflare WARP SOCKS proxy (127.0.0.1:40000).
- Engines: Yahoo (weight 3) and Bing (weight 0.5). Google answers "access denied", DuckDuckGo
  and Qwant CAPTCHA, Brave and Wikipedia rate-limit, and Mojeek and Startpage do not load.
  Bing alone mangles natural-language queries ("how does X" returns pages about "does").

Register the MCP client:

    claude mcp add --scope user hyperion-search -- uv run --quiet --script relay/mcp_client.py
