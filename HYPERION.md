# Hyperion

A personal speed fork of [helium-macos](https://github.com/imputnet/helium-macos), built for one
Apple Silicon Mac. It carries a small patch set on top of Helium (`patches/hyperion/`, listed at the
end of `patches/series`) so that upstream security releases keep flowing in. Builds run on
GitHub Actions only (`Build macOS binaries of Helium`, arm64 only); nothing compiles locally.

## Patches

| Patch | What it does |
| --- | --- |
| `hwconcurrency-revert` | Helium randomizes `navigator.hardwareConcurrency` downward (a 10-core M4 reports 6, 8 or 10), so multithreaded web apps size their worker pools too small. Default flipped to off; the flag still exists. |
| `ublock-allow-prefetch` | Bundled uBlock Origin ships with "Disable pre-fetching" ticked, which turns off Chromium's network prediction and with it speculation rules (prefetch and prerender). Default flipped to off. |
| `preload-pages-standard` | Helium inherits a "Preload pages: off" default that disables preconnect, the loading predictor and speculation rules. Restored to Chrome's Standard. |
| `net-finch-parity` | DNS and connection setup features Chrome enables through Finch, which Helium removes: optimistic (stale) DNS, intermediate DNS results, RTT-scaled Happy Eyeballs and QUIC race timers, early cookie load, session caches kept under memory pressure. Handshake bytes unchanged. |
| `loading-predictors` | The LCP Critical Path Predictor family plus desktop subresource prefetch from the loading predictor: learns each site's critical resources and fetches them before the parser finds them. |
| `speculation-inject` | Built-in speculation rules for pages that ship none: same-origin prefetch on hover, prerender on pointerdown, with action-like paths excluded. |
| `cache-content-addressed` | New code. Cached JS, CSS, font and image responses whose file name carries a bundler content hash (`app.3f9a1c7e.js`) are treated as fresh for 30 days even when the server sends a short `max-age` or `must-revalidate`, in both the HTTP cache and Blink's memory cache. Removes revalidation round trips that can only return 304. `no-cache` and `no-store` are still honored. Unit-tested. |

Research basis, measurements and sources: [docs/hyperion-network.md](docs/hyperion-network.md).

## Not needed

Helium's shared `flags.gn` sets `chrome_pgo_phase=0`, but the macOS CI passes `pgo=true` to
`configure_build`, which appends `chrome_pgo_phase=2`, and `clone.py -p mac-arm` fetches Google's
Apple Silicon profile. Release builds are already profile-guided.

## Roadmap

1. CPU targeting (`-mcpu=apple-m4`), measured against variance before it is kept.
2. Local install agent: stable self-signed identity, artifact hash check, bundle swap.
3. Click-to-first-paint benchmark on real sites, run against Helium, Chrome and Safari.
4. Speculation layer: rule injection, uncredentialed cross-site prerender, memory governor.
5. UI latency: animation durations, paint holding, lazy tab restore.
