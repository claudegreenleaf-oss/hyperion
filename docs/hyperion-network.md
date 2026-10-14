# Hyperion network and loading changes

Research basis and rationale for the network-layer patches in `patches/hyperion/`. Written
2026-10-12 against Chromium 155.0.8059.39 / Helium 0.19.2.1.

## Where the time goes on this machine

Measured from the Mac on the home Comcast line with `curl -w` (HTTP/2, cold connections):

| Phase | Typical cost |
| --- | --- |
| DNS, cache miss (local forwarder) | 38 to 66 ms |
| DNS, cache hit | 2 ms |
| TCP connect | about 25 ms (RTT to Cloudflare 26 ms) |
| TLS handshake | 40 to 110 ms |
| Server think time to first byte | 30 to 180 ms |

So a cold origin costs roughly 100 to 250 ms before the first response byte, and every new
host a page discovers pays the DNS and handshake again. Bandwidth is not the constraint.
The work therefore targets round trips on the critical path, plus starting work before the
user's click lands.

## The structural problem: Helium runs Chromium with its experiments removed

Two inherited decisions turn off most of Chromium's modern loading machinery:

1. `bromite/disable-fetching-field-trials.patch` removes Finch, Chrome's remote experiment
   system. Chromium ships new network and loading features `DISABLED_BY_DEFAULT` and
   enables them per population through Finch. Without Finch, Hyperion only ever sees code
   defaults. `testing/variations/fieldtrial_testing_config.json` at the pinned tag lists the
   configurations Google tests on Mac, including their parameters, which is the reference
   used below.
2. `inox-patchset/modify-default-prefs.patch` sets `NetworkPredictionOptions::kDefault` to
   `kDisabled` ("Preload pages: off"). That single preference disables preconnect, the
   loading predictor, and all speculation rules, so it gates every other change here.
   Bundled uBlock Origin then sets the same preference off again through
   `chrome.privacy.network.networkPredictionEnabled` (its "Disable pre-fetching" option).

## Patches

### `preload-pages-standard`
Restores Chrome's default of Standard preloading. Extended is deliberately not used: it
enables cross-site prefetch through Google's private prefetch proxy, which this build
does not have.

### `ublock-allow-prefetch`
Flips uBlock Origin's `prefetchingDisabled` default to `false`, so the extension stops
overriding the preference above.

### `net-finch-parity` (DNS and connection setup)
Hard-codes the Mac experiment arms from the field trial config that change timing and
ordering only, never the bytes of the TLS or QUIC handshake (decision 11 of the spec):

| Feature | Value | What it removes from the critical path |
| --- | --- | --- |
| `OptimisticDnsForTcp`, `use_stale_connectors=true` | on | Connects to a stale cached address while revalidating (RFC 8767 serve-stale idea, applied to connection setup), removing the 40 to 60 ms DNS miss on repeat hosts |
| `OptimisticDnsForQuic` | on | Same for HTTP/3 |
| `EnableIntermediateDnsResults` | on | Starts connecting on the first of A, AAAA or HTTPS records instead of waiting for all |
| `AsyncDnsQuicJob`, `AsyncDnsQuicJobFastFail=true` | on | QUIC session setup uses the service-endpoint resolver path, no serialized DNS |
| `AdjustIPv6FallbackTime`, `fallback_time=100ms` | on | Happy Eyeballs fallback delay 300 to 100 ms |
| `IPv6FallbackBasedOnRTT` (1.5x RTT, 50 to 1500 ms) | on | Fallback timer scales with measured RTT |
| `AdjustQuicSlowTimerDelay` 100 ms, `QuicSlowTimerBasedOnRTT` | on | Races TCP sooner when QUIC is slow to handshake |
| `EarlyCookieLoadOnPreconnect` | on | Cookie store loads during preconnect, not on the first request |
| `IgnoreQuicCryptoConfigMemoryPressure` | on | macOS memory pressure no longer flushes QUIC resumption state |
| `IgnoreMemoryPressureForSslClientSessionCache` | on | Same for TLS session tickets, so resumed handshakes survive pressure events, which are frequent on a 16 GB machine |
| `QuicSocketSendBufferSize` | 1 MiB | Matches the tested arm |

Excluded on purpose because they change handshake bytes: `EnableTLS13EarlyData` (adds
`early_data` to the ClientHello), `QuicLongerIdleConnectionTimeout` (changes the
`max_idle_timeout` transport parameter), QUIC `connection_options`, and Helium's
`split-clienthello` and QUIC v2 flags.

### `loading-predictors` (history-driven critical path)
The academic line of work on client-side dependency prediction (Polaris NSDI'16, Vroom
SIGCOMM'17, Oblique and Alohamora NSDI'21) shows that learning a page's critical resources
and fetching them before the HTML parser discovers them saves roughly one HTML
time-to-first-byte per navigation. Gains in those papers are measured on high-RTT mobile
links and shrink at 25 ms. Chromium implements a client-only version of this as the LCP
Critical Path Predictor (LCPP), which learns per host the LCP element, its fonts, and its
subresources from the user's own browsing. It is entirely disabled by default on desktop.

Enabled: `LCPCriticalPathPredictor` (with image priority adjustment, including
`<img>` elements), `LCPPAutoPreconnectLcpOrigin`, `LCPPFontURLPredictor`,
`LCPPDeferUnusedPreload`, `LCPPPrefetchSubresource`, `HttpDiskCachePrewarming`,
`LCPTimingPredictorPrerender2`, `Prerender2EarlyDocumentLifecycleUpdate`, and
`LoadingPredictorPrefetch` on desktop (subresource type `js_css`, since images are already
handled by LCPP priority and are the largest waste on a misprediction).

### `speculation-inject` (navigation before the click)
Prefetch and prerender hide a whole navigation on a correct prediction, which is the only
change here whose payoff is the full page load rather than one round trip. Most sites ship no
speculation rules. Chromium has a browser-injected rules mechanism (`AutoSpeculationRules`)
whose configuration normally arrives through Finch. This patch gives it a built-in default:
on every `https` page that has not opted out, same-origin links are prefetched on hover
(`moderate`) and prerendered on pointerdown (`conservative`). It excludes paths that often
perform actions on GET (log out, delete, remove, unsubscribe, cart, checkout), downloads,
`rel=nofollow` and `target=_blank`. Chromium's own limits still apply: two non-immediate
prerenders and the prefetch active-set limit.

### `cache-content-addressed` (new code)
Facebook measured that revalidations of content-addressed resources always return 304
(https://engineering.fb.com/2017/01/26/web/this-browser-tweak-saved-60-of-requests-to-facebook/,
IETF 97 `immutable` slides), and Vesuna et al. (USENIX ATC'16) showed that critical-path
objects are the ones a cache most often fails to help with. Bundlers put a content hash
in the file name of exactly those render-blocking scripts and stylesheets, so the URL
itself proves the bytes cannot have changed. Many hosts still send `max-age=0,
must-revalidate` on them (Netlify's default), which costs a round trip per dependency
level on every revisit.

`net::HttpUtil::IsLikelyContentAddressedUrl` accepts https URLs with a static-asset
extension whose stem has a hex digest (8+ chars with a digit) or a base64url digest (8 to
16 chars, mixed case, a digit, no run of four lowercase letters, so words like
`Html5Player` do not match). `HttpCache::Transaction::IsFreshContentAddressedResponse`
then skips validation for GET 200 responses with a validator, younger than 30 days, not
marked `no-cache`/`no-store`. Blink's `FreshnessLifetime` mirrors the rule, otherwise
the memory cache would send its own conditional request. Explicit reloads
(`LOAD_VALIDATE_CACHE`) still revalidate. Covered by `HttpUtilTest.IsLikelyContentAddressedUrl`;
the existing `HttpCache*` (392) and Blink resource and memory-cache (349) suites pass.

### `navigation-model` (new code)
Hover prefetch only starts once the pointer reaches a link, leaving 100 to 300 ms of the
hover-to-click gap as the whole head start. Predicting the click from history starts the
fetch when the page settles instead. The research line is Padmanabhan and Mogul (1996,
predictive prefetching from access dependencies) and the Markov and PPM predictors that
followed; Deshpande and Karypis (SDM 2001, ACM TOIT 2004) showed that pruning
low-support states and backing off between model orders keeps accuracy while cutting
model size by up to 90%. No published number exists for Chrome's own model, and Helium
removes it anyway (`ungoogled-chromium/disable-ai.patch` empties
`OnMLModelExecutionTimerFired`), while leaving the path from a score to a preloading
decision intact.

`HyperionNavigationModel` (owned by `NavigationPredictorKeyedService`) counts page views
and same-origin link clicks per page, keyed by origin plus path and by a template where
ID-like path segments become `*`. The score is a smoothed first-order estimate
`clicks / (views + 2)`, blended with the template estimate by `views / (views + 3)`, and is
zero until a link has two recorded clicks. `NavigationPredictor` records the view and
each click, scores the page's anchors 300 ms and 2 s after they are reported (the
injected speculation rules exist only after parsing, and the decider enacts only
predictions that match a registered candidate), and passes the top two to
`PreloadingDecider::OnPreloadingHeuristicsModelDone`. `enact_candidates` is on with a
prefetch threshold of 0.35 (a link followed on about half of visits). The decider patch
stops a model prediction from switching off the hover heuristic for every other link.

Storage is a JSON file in the profile (`Hyperion Navigation Model`, at most 4,000 pages
by 16 links, least recently used pages evicted, counts halved at 512 views), never
written off the record, and cleared on any history deletion. Built against Chromium 155
with the full Helium series on a6000-box; the 9 model tests pass (scoring, smoothing,
template back-off, cross-origin rejection, clearing, eviction bound, save and reload).

### `prerender-governor` (new code)
Chromium keeps at most 2 non-immediate prerenders (hover, pointer-down and model-predicted)
per tab and evicts the oldest when a third arrives. `navigation-model` passes its top two
predictions through that group, so the model fills it when the page settles and the first
hover throws away one of its prerenders, whatever either is worth. Two changes in
`PrerenderHostRegistry` replace the fixed count and the FIFO order.

**Capacity from memory.** The non-immediate limit is one prerender per 256 MB of available
physical memory above a 1.5 GB reserve, clamped to 2 to 6 (`GetMaxLimit`). On top of that,
Chromium's own governor scales every limit by the memory coordinator's current limit (100%,
50% under moderate pressure, 0% under critical) and evicts down to it in `OnReleaseMemory`.
That governor sits behind `StatefulMemoryPressure`, which is off in code and on by Finch for
every desktop platform, so Helium never had it; the patch turns it on, which also makes the
net and GPU caches, back/forward cache and spare renderer respond to pressure as in Chrome.

**Eviction by expected value.** The confidence that `PreloadingDecider` already computes
(the model's score, 100 for pointer triggers) stopped at `PrerendererImpl`; it now rides in
`PrerenderAttributes` to the registry. Each queued host is worth its confidence halved every
5 s for pointer triggers or every 60 s for model predictions, since a hover is intent about
the next moment and a model prediction is about the page. A full group cancels its least
valuable host, and refuses a newcomer worth less than every running host. With equal
evidence the order is still Chromium's FIFO. Tests: memory-sized limit, and eviction of a
weak prediction ahead of an older hover, plus all 95 existing prerender registry and
prerenderer tests (which needed `LCPTimingPredictorPrerender2` off in their fixtures: it
holds prerenders until an LCP signal unit tests never send).

## Not pursued, and why

- **A relay on scotus-vps (MASQUE or a split-connection proxy).** The VPS is 21 ms from home,
  about the same as Cloudflare at 26 ms, so a relay adds a hop without shortening any path.
  The independent measurement of iCloud Private Relay (PAM 2023) found up to a 60% increase in
  page load time in some cases. WatchTower (MobiSys'19) also found that proxying can slow
  loads. Revisit only if traceroutes show bad Comcast peering to specific destinations.
- **Server-dependent techniques** (Compression Dictionary Transport, 103 Early Hints, ORIGIN
  frame, 0-RTT acceptance): already supported by Chromium where sites deploy them, and a
  client cannot force them.
- **Multipath QUIC, L4S, BBRv3:** need a second path or are sender-side; downloads are
  sent by servers.
- **Renderer scheduler loosening (tight mode, delayable limits):** H2 and H3 hosts are already
  exempt from network-service delays on non-cellular links; the remaining knobs are small and
  carry regression risk. Deferred until the benchmark harness exists.

- **Proactive DNS refresh before TTL expiry** and **request-priority changes:** designed on
  2026-10-13 and judged modest to near zero. `net-finch-parity` already serves stale entries
  while refreshing, which removes most of the expired-TTL cost.

## Next

1. Record-and-replay benchmark (Mahimahi-style, 25 ms RTT) plus real-site click-to-paint, to
   measure each patch against variance. Anything that does not beat noise gets reverted.

## Sources

- WProf, NSDI'13: https://www.cs.stonybrook.edu/~arunab/papers/wprof.pdf
- Polaris, NSDI'16: https://www.cs.princeton.edu/~ravian/publications/polaris_nsdi16.pdf
- Vroom, SIGCOMM'17: https://www.cs.princeton.edu/~ravian/publications/vroom.pdf
- WatchTower, MobiSys'19: https://www.cs.princeton.edu/~ravian/publications/watchtower_mobisys19.pdf
- Oblique, NSDI'21: https://www.cs.princeton.edu/~ravian/publications/oblique_nsdi21.pdf
- Alohamora, NSDI'21: https://www.cs.princeton.edu/~ravian/publications/alohamora_nsdi21.pdf
- Upper bound on push and preload benefit: https://arxiv.org/pdf/2207.05885
- iCloud Private Relay performance, PAM 2023: https://iris.unito.it/retrieve/handle/2318/1946119/1215375/pam23.pdf
- H3 prioritization across browsers, ANRW'24: https://research.edm.uhasselt.be/~mwijnants/pdf/herbotsANRW2024.pdf
- RFC 8767 (serve-stale DNS): https://www.rfc-editor.org/rfc/rfc8767.html
- Chrome QUIC deployment notes: https://blog.chromium.org/2024/12/making-chrome-quicer.html
- 0-RTT deployment challenges, IETF 115: https://datatracker.ietf.org/meeting/115/materials/slides-115-tdd-sessa-challenges-of-0-rtt-in-ietf-quic-linked-from-deployment-talk-00
- Prerender and speculation rules: https://developer.chrome.com/docs/web-platform/prerender-pages
