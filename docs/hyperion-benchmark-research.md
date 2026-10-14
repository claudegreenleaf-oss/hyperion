# What makes a browser feel fast, and how to benchmark the Hyperion patches

Date: 2026-10-13. Scope: a personal Chromium fork (Hyperion, on Helium) running on a fanless M4 MacBook Air with roughly 25 ms RTT to Cloudflare. Its patches so far: speculative prefetch and prerender (speculation rules, hover triggers, a local Markov next-click predictor), loading predictors (preconnect, LCP-critical-path preload), a cache patch that skips revalidation for hash-named assets, and soon a prerender governor and tab-switch warm-up.

A note on evidence quality. Sources marked "(fetched)" in the Sources section were opened and read during this research. Sources marked "(recalled)" are cited from memory and their URLs and exact numbers should be spot-checked before anyone quotes them. Histogram names in section 3 are mostly recalled, so confirm each one exists in the local build at chrome://histograms before building tooling around it.

## 1. Human perception: which latencies people notice

### The classic response-time limits

The three limits that nearly all UI latency guidance descends from are about 0.1 s, 1 s and 10 s. Nielsen's summary attributes them to Miller (1968) and Card, Robertson and Mackinlay (1991). At 0.1 s the system feels instantaneous and the user feels they are directly manipulating the thing. At 1 s the user notices the delay but their flow of thought is not interrupted. At 10 s attention is lost and the user needs a progress indicator. These are derived from decades of lab HCI work, not from web browsing specifically, and they are order-of-magnitude anchors rather than sharp cliffs. The practical reading for a browser is that anything that lands under about 100 ms after the input is judged "instant", and that the interesting region for a loading patch is the gap between roughly 100 ms and 1 s, where a click stops feeling like a direct action and starts feeling like a wait.

Two caveats from the HCI literature matter for this benchmark. First, thresholds depend on the input type. Direct manipulation (dragging, scrolling, touch) is far less tolerant than discrete commands: studies of touch latency (Ng et al., UIST 2012, recalled) found people detect and are bothered by delays of a few tens of milliseconds, and well below 10 ms for dragging. Second, people detect small differences in latency even when they cannot name an absolute threshold, so a 30 ms change on a 150 ms interaction can be perceptible in a side-by-side comparison even though it would never show up in a satisfaction survey.

### Perceived page-load speed versus measured metrics

The strongest direct evidence is SpeedPerception (Gao, Dey, Ahammad, 2017, fetched). Participants watched pairs of page-load videos and chose which loaded faster, using the Internet Retailer Top 500. Standard metrics such as onLoad and TTFB agreed with the majority human choice in under 60% of comparisons, barely better than a coin flip. A simple three-variable model built from visual-progress features explained the majority choice with about 87% accuracy. The authors also found that people judge relative speed well before the visualComplete event occurs, meaning that perception is dominated by the early and middle portion of the visual progression, not the tail. Eyeorg (Varvello et al., 2016, recalled and also fetched as a search hit) is the companion crowdsourcing platform and reaches a similar conclusion: above-the-fold visual progress predicts human judgment much better than network-level events.

Other relevant findings, mostly recalled and worth verifying if quoted: Akamai and Google industry studies show conversion and bounce correlate with load time, but those are correlational and mix perception with site quality; Mozilla and Google both moved toward visual-progress metrics (Speed Index, then FCP and LCP) because onLoad did not track perception. Core Web Vitals thresholds (LCP at most 2.5 s good, above 4.0 s poor; INP at most 200 ms good, above 500 ms poor, both at the 75th percentile of field loads, fetched) are field-population thresholds chosen to be achievable across a wide range of devices. The INP page explicitly says they balance achievability against device variability, so they are not psychophysical limits and should not be used as targets for a fast laptop on a home connection.

### Which moments matter most

There is no single study that ranks these for desktop browser users, so the following ranking combines the response-time limits, the SpeedPerception findings, and engineering judgment. It should be treated as a hypothesis, not a measured fact.

1. Click (or Enter) to first visual change. This is the moment a user decides whether the browser "reacted". Anything past about 100 ms starts to feel like lag, and the move from "the old page is still there" to "the new page is visibly arriving" is the strongest single cue. Prerender activation turns this into a near-instant swap, which is why it is the highest-value effect the fork can have.
2. Click to usable. This is when the page is readable and interactive enough that the user starts acting on it. LCP is the nearest standard proxy, but it is a proxy. For prerendered pages the relevant clock starts at activation.
3. Tab switching. It is a discrete command with direct visual feedback, so the 100 ms limit applies cleanly. A stale or blank frame after a switch is highly noticeable, and Chromium itself treats it as a first-class metric (see section 3).
4. Scrolling smoothness. This is direct manipulation, so dropped frames are noticed even at small rates. The fork's loading patches should not regress it, but they do not aim to improve it, so it is a guardrail, not a headline.
5. Back and forward. The back/forward cache already makes these near-instant for eligible pages, and a patch that evicts bfcache entries (for example, memory pressure from extra prerenders) would make them noticeably worse. This is a guardrail for the prerender governor.
6. Omnibox typing. Keystroke-to-suggestion latency is a direct-input case. The fork does not touch it, so treat it as a guardrail only.
7. Cold start. Matters once per session, so lower weight per occurrence, but the fork's extra initialization (predictor database load, for instance) should not push it measurably.

## 2. Which metrics correlate with perceived speed

Better correlates, with the evidence available:

- First visual change and visual-progress metrics (Speed Index, visual completeness curves, time to a given completeness such as 85%). SpeedPerception and Eyeorg both find that visual progress beats network and DOM events. Speed Index integrates the visual-completeness curve over time and is computed from a screen video, so it works across browsers and needs no page cooperation.
- FCP and LCP. FCP marks the first content painted; LCP marks the largest text or image block painted in the viewport (fetched, with its limitations below). They are the best of the standardized, in-page metrics and are what Chromium reports in the field.
- INP for post-load interaction. It is the worst-or-near-worst interaction latency over a page visit and is the Core Web Vitals responsiveness metric.
- Dropped-frame percentage and frame-time percentiles for scrolling and animation (see section 3 for the Chromium smoothness metric).

Known poor or misleading correlates:

- onLoad, DOMContentLoaded and TTFB on their own (under 60% agreement with humans in SpeedPerception).
- Total bytes or request counts, which say nothing about the order of visual arrival.
- LCP has specific blind spots (fetched from web.dev): the largest element is not always the most important, iframes are ignored, elements that later change size or position do not create new candidates, text in a blocked web font is not eligible until it renders, and cross-origin images without Timing-Allow-Origin report coarsened render time in Chrome 133 and later. For prerendered pages, LCP must be measured from activationStart or it will look artificially negative or zero. Bfcache restores are not reported by the LCP API at all.
- Speedometer and MotionMark. Speedometer measures JavaScript and DOM framework throughput and MotionMark measures graphics throughput. Neither measures loading, navigation, or speculation. They are useful only as regression guards against accidental CPU or memory side effects of the patches, not as evidence that the patches help.

A consequence for this project: a speculation patch that makes the click-to-paint time near zero will look spectacular on activation-relative LCP and FCP, but a fair comparison must also count the cost the user did not see: wasted CPU, bandwidth, and memory when the prediction is wrong. The benchmark therefore needs a "wrong prediction" arm.

## 3. How Chromium measures these

Treat all names in this section as leads to confirm in the local build, since histogram names change.

### Page-load and navigation

- PageLoad.PaintTiming.NavigationToFirstContentfulPaint and PageLoad.PaintTiming.NavigationToLargestContentfulPaint2 (recalled): the standard FCP and LCP histograms. For prerendered pages Chromium reports paint timing relative to activation; the web.dev LCP page states that prerendered pages should be measured from activationStart (fetched).
- In-page, the same information is available through PerformanceNavigationTiming (which exposes activationStart for prerendered documents), PerformancePaintTiming, and the largest-contentful-paint and event-timing PerformanceObservers. For a benchmark harness these are easier and more controllable than UMA.
- Prerender2 activation: the Prerender2 code in content/browser/preloading/prerender records activation time split by trigger type and embedder, and records which navigation parameters differed when an activation failed (found via search; the 2022 commit message notes roughly 2% of activations were failing from parameter mismatch before a dedicated cancellation reason was added). The preloading prediction and attempt metrics under Preloading.* (recalled) report whether a speculation was triggered, ready, and used. Read the README at content/browser/preloading/prerender/README.md in the Chromium tree for the current list.

### Tab switching

- Browser.Tabs.TotalSwitchDuration and its finer-grained variants (a 2019 commit added variants that record whether the target tab was loaded or frozen; fetched via search). The recorder in content/common/tab_switch_time_recorder.cc begins a trace event at switch start and computes the delta to the presentation-feedback timestamp of the first presented frame, so it measures what the user actually sees rather than an internal event.
- MPArch.RWH_TabSwitchPaintDuration is the older metric used by the tab_switching perf benchmark: time from "tab requested to be shown" to first paint.

### Input and smoothness

- Event.Latency.* (recalled), for example scroll begin and scroll update latency and end-to-end event latency histograms: input timestamp to frame presentation.
- Graphics.Smoothness.PercentDroppedFrames3.* (recalled): percent of dropped frames per animation sequence, which is Chromium's headline smoothness metric.

### Startup

- Startup.FirstWebContents.NonEmptyPaint3 and related Startup.* histograms (recalled).

### How to read them in a local build

- chrome://histograms shows all UMA histograms recorded in the current browser process, even when metrics reporting is off. It is the quickest way to confirm a name and see a distribution after a scripted run. Reload the page or use the "refresh" behavior between runs, or start fresh profiles, since values accumulate.
- Tracing: chrome://tracing or Perfetto, via the DevTools Tracing domain or crossbench's perfetto probe. Tab-switch and navigation events appear as trace events.
- --enable-logging=stderr --v=1 prints verbose logs. This is noisy and slows the browser, so use it for debugging, not for measurement.
- Chrome DevTools Protocol: Page.lifecycleEvent, Performance.getMetrics, Tracing.start, and Runtime.evaluate for PerformanceObserver entries. Driving a Hyperion build with Puppeteer, Playwright or raw CDP is the most controllable approach.
- Chromium's own tooling: crossbench (fetched; supports Chrome, Firefox, Safari and Edge, benchmarks including speedometer, jetstream and a loading benchmark whose pages are given by URL or config, a --repeat flag, results aggregated as min, max and average per story in cb.results.json, network modes of LIVE, WPR replay, or LOCAL file server, optional traffic shaping such as 3G presets, and probes including perfetto, profiling, v8.log and trace_processor). The Telemetry and Pinpoint infrastructure behind chromeperf is where Chromium runs these at scale. I could not verify whether crossbench's loading benchmark manages cache state or how it handles interaction scripts, so check its README before relying on either.

## 4. Benchmark methodology

### Record and replay

- web_page_replay_go (catapult) records live traffic into an archive and serves it back with an injected certificate, applying optional shaping. Chromium needs the flags that make it trust the replay certificates and resolve all hosts to the replay server (the catapult docs list the exact flags). Mahimahi (Netravali et al., USENIX ATC 2015, recalled) is the research alternative: it records and replays HTTP traffic while preserving multi-origin server structure, and emulates link rate and delay with composable shells. Its paper argues that single-server replay, as done by earlier tools, distorts results because real pages touch many origins with different RTTs, which matters for preconnect measurements.
- Replay is deterministic, which is exactly what is wanted to isolate patch effects, and also exactly what hides things: dynamic content, personalization, A/B test scripts, ads, and time-dependent requests either break or get frozen. Prerendering and speculation rules still work against a replayed site, but the speculation rules must be present in the recorded HTML or injected by the fork, and any same-site or cross-origin restriction (cross-origin prerender is limited, and cross-origin iframes are not rendered during prerender until activation, fetched from Chrome's prerender docs) will apply exactly as in live.
- Replay pitfalls: (a) loopback RTT is near zero, so the benefit of preconnect, prefetch and prerender is hugely understated unless RTT is injected; (b) the archive must preserve response headers, since the hash-named asset cache patch changes behavior based on cache headers; (c) archives go stale, so re-record on a schedule and keep archive versions with results; (d) some sites detect the replay certificate or block recording.
- A sensible split: use replay with shaped RTT for the primary, low-variance A/B comparisons, and run a smaller live-network confirmation pass on the winners, because live is the real target.

### Network shaping

Set RTT to the user's real conditions: 25 ms RTT to the edge (so add roughly 25 ms of delay in total, split per direction), with bandwidth well above the page's needs, for example 100 Mbit/s down. Add a second profile that is slower (for example 100 ms RTT and 20 Mbit/s) because patches that look like noise at 25 ms often show up there, and a patch that only helps on slow links is worth knowing about even if it does not help at home. On macOS, shaping options are the Network Link Conditioner, pfctl with dummynet, or the shaping built into web_page_replay_go and crossbench. Verify the shaping with a ping or a trivial fetch before every session and log the measured RTT with the results.

### Statistical power and design

- Page-load times on a stable machine under replay typically have a coefficient of variation of roughly 5 to 15% per run (an assumption to be measured with an A/A test, see below). To detect a 5% shift at 80% power with a two-sided 5% test at CV 10%, the standard two-sample formula n = 2 (z_alpha/2 + z_beta)^2 (CV / delta)^2 gives n of about 63 runs per arm. Paired designs (below) reduce this substantially if runs are correlated, which interleaving makes them.
- Interleave the arms (ABBA or randomized block order) so that drift in temperature, background load, and time of day hits both equally. Never run all of A then all of B.
- Randomize site order and the A/B order within each block. Use a fresh browser profile (or a profile restored from a snapshot) per run for the cold arm.
- Run A/A tests first: the same build against itself, same protocol. This is the only honest way to measure the noise floor of this particular laptop and this protocol, and it gives the minimum detectable effect directly.
- Report medians and percentiles, not means, because page-load distributions are right-skewed. Use per-site paired ratios (B divided by A) so that heavy and light sites contribute equally, and aggregate on the log scale.

### Warm versus cold cache

Cold cache and warm cache are different questions and judge different patches. The hash-named asset cache patch matters only for warm-cache repeat visits, where it saves revalidation round trips (at 25 ms RTT that is about 25 ms per revalidated asset on the critical path, often overlapped). Predictors (preconnect, LCP preload) matter most on first and cold loads. Speculation matters on both. Always label each result with its cache state and run each patch under the states that matter to it. "Warm" must be defined operationally: for example, one priming load, then close the page, wait a fixed interval, then measure; and also include a "stale" warm state where cached entries are past freshness, since that is where skipping revalidation actually changes behavior.

### Scripting realistic interaction so speculation is exercised

Speculation patches only do anything if the harness behaves like a person. Synthetic navigation (page.goto, or typing a URL) bypasses hover triggers and next-click prediction entirely, which would make the benchmark show zero effect from the fork's headline features. So:

- Navigate by real click events on real links, via CDP Input.dispatchMouseEvent or OS-level input, not element.click() in JavaScript, since hover and pointer-down eagerness rules key off pointer events. Chrome's documented desktop eagerness is: moderate fires after about 200 ms of hover, eager after about 10 ms of hover, conservative on pointer or touch down (fetched from Chrome's prerender docs).
- Insert a human-like dwell: move the pointer to the link, hover for a sampled dwell (for example lognormal with a median of 400 ms and a spread covering 150 ms to 1.5 s), then click. Sample the dwell from a recorded distribution if possible. Include short dwells (under 200 ms) so the hover-trigger threshold is genuinely tested, and also immediate click cases where prerender cannot finish.
- The Markov predictor needs history. Train it with a fixed, scripted browsing history from a fixed seed before the measured runs, and include a "novel link" condition where the click target was never seen so the predictor cannot help. Report hit and miss runs separately and also as a weighted mix at a stated hit rate.
- Limits to respect: Chrome caps concurrent speculations (2 each for eager, moderate and conservative with FIFO eviction; up to 10 prerenders and 50 prefetches for immediate), and blocks speculation under Save-Data, Energy Saver with low battery, memory pressure, in background tabs, or with "Preload pages" off (fetched). The harness must run with the browser in the foreground, on AC power, with Preload pages enabled, or the patches silently do nothing. Check this at the start of each session by confirming that at least one prerender actually activated.

### Pitfalls on a fanless M4 MacBook Air

- Thermal throttling and clock changes. There is no fan, so sustained load heats the chassis and the SoC will reduce clocks. Mitigate with short blocks, cool-down gaps (a fixed idle period or a wait until the thermal state is nominal; `pmset -g therm` and `sudo powermetrics --samplers thermal` can show the state), randomized interleaving so heat affects both arms, and by recording the temperature or thermal state with each run. Discard or flag runs where throttling is reported.
- Plug in AC power, disable Low Power Mode, and keep battery above a stable level, since Chromium itself changes speculation behavior on Energy Saver.
- Quit other apps, disable Spotlight indexing and Time Machine during runs, turn off Wi-Fi roaming and automatic updates, and keep the screen at a fixed brightness and resolution since compositor cost varies.
- P-core versus E-core scheduling on Apple silicon adds noise; this machine runs an auto-demote daemon (taskpolicy to E-cores) that must be disabled for the Hyperion benchmark processes.
- Page-load runs on this machine should be on the primary display at a fixed window size, since headless and headful differ and tab-switch and paint timing only exist in headful.
- Process warm-up effects: discard the first N runs per session, and always include the A/A control in the same session.

## 5. Recommended benchmark design

### Ground rules

- One decision per patch. Each patch is built so it can be toggled at runtime (a feature flag or switch) where possible, so the A and B arms are the same binary with one switch flipped. That removes build-to-build differences. If a patch cannot be toggled, build two binaries and alternate them.
- A/A control in every session. A patch is judged against the observed A/A noise, not against a theoretical zero.
- Sites: 12 to 16 recorded sites (WPR archives) chosen to cover content types the user actually browses: a news article page with a list of other articles, a search results page, a documentation site, a social or forum thread, an e-commerce listing to product page, a Wikipedia-like article with dense links, a webmail-like single-page app, and a few heavy third-party pages. Each should have at least one realistic "next click" with a scripted link. Mark each site as hash-named-asset-heavy or not, since the cache patch only helps those. Add 3 to 4 live sites for confirmation runs.

### Ranked measurements

1. Click-to-first-contentful-paint after a predicted click (judges the speculation rules, hover triggers, the Markov predictor, and later the prerender governor). Collect with CDP: timestamp of the dispatched mouse-down or click, then PerformancePaintTiming for the new document, using activationStart-relative time when the document was prerendered, plus the activation flag from PerformanceNavigationTiming. Also collect click-to-LCP. Run on the hit arm and the miss arm separately and report a mix. This is the primary metric of the benchmark.
2. Click-to-visually-complete or Speed Index from screen capture (judges all loading patches end to end, and is the guard against metrics that look better only because they are measured from a different clock). Record the display at 60 fps with a screen recorder or a CDP screencast (Page.startScreencast) and compute first visual change, 85% visual completeness, and Speed Index offline. Use it on a subset (6 sites) because it is more expensive to process.
3. Warm-cache repeat-visit load time (judges the cache patch for hash-named assets). Collect load duration, count of conditional requests and 304 responses via CDP Network events (the number of revalidation round trips is a direct, low-noise mechanism metric), and LCP. Run three cache states: cold, warm-fresh, and warm-stale. The expected effect is visible first in the revalidation request count, which should drop to near zero for hash-named assets, and only secondarily in the time.
4. Cold first-load FCP and LCP with shaped RTT (judges preconnect and the LCP-critical-path preload). Collect FCP, LCP, and DNS, connect and TLS timings from Resource Timing for the LCP resource. Include the 100 ms RTT profile to give the effect room above the noise.
5. Tab-switch latency (judges the tab-switch warm-up). Collect Browser.Tabs.TotalSwitchDuration and its loaded/frozen variants by diffing chrome://histograms snapshots (or via the tracing probe), with the workload being a scripted switch between N tabs (for example 8 tabs, 5 of them backgrounded for more than a few minutes so some are frozen or discarded). Run 30 switches per arm per tab state. Because UMA histograms are bucketed, prefer the trace events or CDP-visible timings when the effect is small.
6. Guardrails (a regression in any of these vetoes a patch even if the headline metric wins): wasted work on a wrong prediction (extra bytes transferred, CPU time of the renderer processes and browser process measured with powermetrics or ps sampling during a miss run, and resident memory); back/forward navigation time and bfcache hit rate; scroll dropped-frame percentage (Graphics.Smoothness.PercentDroppedFrames3); and a Speedometer 3 run with `--repeat` of 10 as a coarse CPU and memory side-effect check.

### Run counts

- Primary comparisons (items 1, 3, 4): 30 paired runs per site per arm per condition at minimum, with 12 sites. That is 360 pairs per condition, which is large enough to detect effects well below 5% at the aggregate level even if single-site power is modest. Interleave pairs and randomize site order.
- Visual-completeness (item 2): 10 paired runs per site for 6 sites.
- Tab-switch (item 5): 30 switches per tab state per arm, repeated across 3 separate sessions to capture session-to-session variation.
- A/A: 20 paired runs per site on the first session and 10 per site on every later session, to monitor drift.
- Budget the time: if one run takes about 8 seconds including cool-down, 360 pairs is about 1.6 hours per condition, so schedule the full suite as overnight batches with thermal gaps.

### Decision rule: "beats noise"

1. For each site and condition, compute the paired log ratio log(B_i / A_i) for every interleaved pair.
2. Aggregate across sites with a mixed-effects model, or more simply compute the per-site median log ratio and then the median across sites.
3. Compute a 95% bootstrap confidence interval of the aggregate, resampling pairs within site and then sites (a hierarchical bootstrap with at least 10,000 resamples).
4. Run a Wilcoxon signed-rank test on the per-pair log ratios as a nonparametric check, with Holm correction across the patches and metrics tested in the same round.
5. A patch is kept only if all hold: the 95% interval lies entirely on the improving side of zero; the effect is larger than the A/A minimum detectable effect (the 95th percentile of the absolute aggregate effect seen in A/A comparisons, with a floor of 3%); the effect is practically meaningful for the metric (at least about 20 to 30 ms on the click-to-paint measurement, since smaller shifts are below what a person would notice, per the section 1 discussion); and no guardrail regresses beyond its own noise floor.
6. A patch that fails the rule is reverted, as the project already intends. A patch that is neutral on the headline metric but costs CPU or memory on a wrong prediction is reverted on the guardrail alone.
7. Re-run any keeper once on the live-network confirmation set before final adoption. If the live result contradicts the replay result, trust live and investigate the replay setup.

### Suggested order of work

1. Build the harness (CDP driver with realistic pointer events, WPR archives, shaping, thermal logging).
2. Run the A/A baseline for a week of sessions to establish the noise floor and check drift.
3. Evaluate patches one at a time against the baseline, starting with the speculation stack (measurement 1), then the cache patch (3), then predictors (4); add the prerender governor and tab-switch warm-up when they land (1 and 6, then 5).

## Sources

Fetched and read during this research:

- Gao, Dey, Ahammad, "Perceived Performance of Webpages In the Wild: Insights from Large-scale Crowdsourcing of Above-the-Fold QoE" (SpeedPerception), 2017: https://arxiv.org/abs/1704.01220 and PDF https://arxiv.org/pdf/1704.01220 ; CCR version: https://ccronline.sigcomm.org/2017/ccr-october-2017/perceived-performance-of-top-retail-webpages-in-the-wild/ (only the abstract-level findings were seen via search results, not the full text)
- Nielsen, "Response Times: The 3 Important Limits" (attributing Miller 1968 and Card et al. 1991): https://www.nngroup.com/articles/response-times-3-important-limits/
- web.dev, Interaction to Next Paint: https://web.dev/articles/inp
- web.dev, Largest Contentful Paint: https://web.dev/articles/lcp
- Chrome for Developers, Prerender pages in Chrome: https://developer.chrome.com/docs/web-platform/prerender-pages (the last portion of the page, which includes measurement guidance, was not read)
- Crossbench (Chromium): https://chromium.googlesource.com/crossbench/
- Chromium tab switch time recorder (as surfaced by search; verify against current tree): https://chromium.googlesource.com/chromium/src.git/+/029d1ba4094de57a99dfd77306a1582588b3cc34/content/common/tab_switch_time_recorder.cc
- Chromium commit adding granularity to Browser.Tabs.TotalSwitchDuration (as surfaced by search): https://chromium.googlesource.com/chromium/src.git/+/5f06ca3e6676ee033795e2757fc855f706c503cc
- Chromium tab_switching perf benchmark (older revision, mentions MPArch.RWH_TabSwitchPaintDuration): https://chromium.googlesource.com/chromium/src/+/b78379a2/tools/perf/benchmarks/tab_switching.py
- Chromium Prerender2 README (as surfaced by search): https://chromium.googlesource.com/chromium/src/+/7a69db2f55d7472bf5648f1a6158366c01b917f8/content/browser/preloading/prerender/README.md
- Chromium commit on activation parameter mismatch cancellation: https://chromium.googlesource.com/chromium/src/+/aeec5df139bb4555d5b5c2044ac1f682aa724899
- Eyeorg: https://arxiv.org/pdf/1902.02865

Cited from memory, not opened here (verify before quoting):

- Miller, "Response time in man-computer conversational transactions," AFIPS 1968; Card, Robertson, Mackinlay, "The information visualizer," CHI 1991.
- Ng, Lepinski, Wigdor, Sanders, Dietz, "Designing for Low-Latency Direct-Touch Input," UIST 2012.
- Varvello et al., "Eyeorg: A Platform for Crowdsourcing Web Quality of Experience Measurements," CoNEXT 2016.
- Netravali et al., "Mahimahi: Accurate Record-and-Replay for HTTP," USENIX ATC 2015: https://www.usenix.org/conference/atc15/technical-session/presentation/netravali
- catapult web_page_replay_go: https://chromium.googlesource.com/catapult/+/HEAD/web_page_replay_go/ (the README URL I tried returned 404; find the current path in the catapult tree)
- Chromium histogram names Event.Latency.*, Graphics.Smoothness.PercentDroppedFrames3.*, PageLoad.PaintTiming.*, Startup.FirstWebContents.NonEmptyPaint3, Preloading.*: check in chrome://histograms and tools/metrics/histograms/ in the Chromium tree.
- Speculation rules specification (WICG): https://wicg.github.io/nav-speculation/speculation-rules.html
