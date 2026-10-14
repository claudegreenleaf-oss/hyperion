# Hyperion A/B benchmark harness

This harness compares two Chromium builds (a stock build and a Hyperion build) on recorded sites with injected network latency, driven by real mouse input, and reports paired statistics against an A/A noise floor. It follows `docs/hyperion-benchmark-research.md`. It runs on a6000-box and must not be run on the Mac.

## What it does

Sites are recorded once with web_page_replay_go and replayed from an archive, so both arms see identical bytes. Chrome runs inside a network namespace (`hbench`) joined to the host by a veth pair, and `tc netem` adds 12.5 ms in each direction (25 ms round trip, 100 Mbit/s) on that pair only. Loopback and every other interface are untouched. Chrome resolves every host to the replay server with `--host-resolver-rules` and trusts its certificate through `--ignore-certificate-errors-spki-list`. A second profile, `p100`, uses 50 ms each way and 20 Mbit/s.

Each run starts a fresh profile in `/dev/shm`, with Preload pages written to Standard so speculation is allowed. It then does the following:

1. Training, identical for both arms: it loads the site's start page and really clicks each trained link, `--train` times (default 3), so a history-driven predictor has something to learn. The last link of every site is marked novel and is never clicked during training. The browser cache is cleared afterwards, and history and predictor state are kept.
2. Cold load of the start page (cold FCP and LCP).
3. A reading pause, then a real-input click: `Input.dispatchMouseEvent` moves the pointer along a curved path, rests on the link for a dwell of 150 to 600 ms, presses, holds for 55 to 115 ms and releases. The path, dwell and pause are seeded per pair so both arms get the same motion. The measured navigation never uses `page.goto` or `element.click()`.
4. Reading the destination page's paint, LCP and navigation entries, then a warm-cache repeat load of the start page.

Click-to-first-paint and click-to-LCP are computed on one clock: `performance.timeOrigin + max(paint time, activationStart)` of the destination, minus the harness timestamp of the mouse release (the click). A prerendered page therefore reports the time until activation, not a negative or stale paint time. The mouse-press based variants are stored as `down_fcp` and `down_lcp`. The node clock and the page clock agreed within about 0.5 ms in a measurement on this box.

Per run the JSONL row records whether the click was served by a prerender activation (`activationStart > 0`) or a prefetch (`deliveryType` equal to `navigational-prefetch`), the Preload DevTools events seen, bytes received during the start load and during the idle plus click phase (from the namespace interface counter, so speculative traffic is included), CPU milliseconds across all browser processes for the same two phases, and the renderer process count. The warm repeat visit records the request count, conditional requests and 304 responses.

## One-time setup (already done on a6000-box)

`./setup.sh` installs Go into `~/hyperion-bench/go`, builds web_page_replay_go from the catapult repo, installs Chrome for Testing into `~/hyperion-bench/cft`, and runs `npm install`. The catapult checkout needs one file removed because it does not compile at HEAD, and the script does that. Privileged steps (namespace, tc) use sudo. Either configure passwordless sudo for the `ip` and `tc` commands or put the password in `~/hyperion-bench/.sudo-pass` with mode 600. The repository copy of the harness is synced with:

    rsync -a --exclude node_modules --exclude results --exclude .run bench/ nvcell@a6000-box:hyperion-bench/

## Recording sites

`candidates.json` lists the sites to try. Recording discovers up to three same-origin article-like links per site (or uses the `selector` given for a site), checks each is clickable with real input, and clicks them twice so the archive holds the destinations. Sites that block the recorder or have too few clickable links are dropped. It writes `sites.json` and the archive. Only the box can record, because the replay server must see the live network.

    cd ~/hyperion-bench
    CFT=$PWD/$(ls cft/chrome/*/chrome-linux64/chrome)
    systemd-run --user --unit=hyperion-bench-record --collect --working-directory=$PWD \
      -p StandardOutput=file:$PWD/record.log -p StandardError=file:$PWD/record.log \
      ./bench.sh record --bin $CFT --archive $PWD/archives/sites.wprgo --sites candidates.json --out sites.json

Stop any unit with `systemctl --user stop <unit name>`. Archives go stale, so re-record on a schedule and keep the archive and `sites.json` with the results they produced.

## Running an A/B session

The stock and Hyperion binaries are passed as arguments. Cores default to 56-63 for Chrome, 54-55 for the driver and 52-53 for the replay server and display; pick cores that are quiet and set them with `--chrome-cores`, `--driver-cores` and `--server-cores`. A preflight runs first: both arms must show a prerender activation or prefetch hit on a local synthetic site with speculation rules, otherwise the session aborts, because with Preload pages silently off every patch would measure as zero.

    cd ~/hyperion-bench
    S=$HOME/hyperion-cr/src/out/BenchStock/chrome
    H=$HOME/hyperion-cr/src/out/BenchHyp/chrome
    systemd-run --user --unit=hyperion-bench-ab1 --collect --working-directory=$PWD \
      -p StandardOutput=file:$PWD/ab1.log -p StandardError=file:$PWD/ab1.log \
      ./bench.sh run --stock $S --hyp $H --archive $PWD/archives/sites.wprgo --sites sites.json \
      --profile p25 --pairs 30 --aa-pairs 20 --train 3 --out results/ab1-p25.jsonl

Useful options: `--aa-pairs N` (A/A pairs per site, stock against stock, default 10), `--site-limit N` or `--site-filter REGEX`, `--cooldown SECONDS` (between runs, default 2), `--block-cooldown SECONDS` (between ABBA blocks, default 4), `--warmup N` (discarded runs, default 2), `--restart-after-train` (relaunch the browser after training so sockets and DNS are cold; use it only if the Hyperion navigation model persists to disk), `--no-force-preload` (do not write the Preload pages preference), `--sandbox` (keep the Chrome sandbox; the default passes `--no-sandbox` because Ubuntu's AppArmor blocks unprivileged user namespaces for unpackaged binaries), and `--no-preflight`. Add `--profile p100` for the slow profile. Progress is in the log file. Stop with `systemctl --user stop hyperion-bench-ab1`; the script removes the namespace, replay server and display it started, by pidfile and name only. `bench.sh selftest --stock S --hyp H` runs only the preflight, and `bench.sh check` verifies the namespace and prints the measured round trip.

Order and pairing: runs are grouped in ABBA blocks (A B B A over two consecutive pair indices) per site, sites are shuffled each round, and an A/A block (stock against stock) is interleaved in the same session. The default is 30 pairs per site. At about 30 seconds per run, 14 sites with 30 A/B pairs and 20 A/A pairs is roughly 14 hours, so run it overnight.

If the Hyperion build injects its own speculation rules, sites without rules will show speculation only in the Hyperion arm, which is the point of the comparison. For a fair attribution to one patch, build two binaries that differ only in that patch.

## Analysis

    python3 analyze.py results/ab1-p25.jsonl --boot 10000

For each metric and condition (all clicks, trained links, novel links) it computes the per-pair log ratio of arm B over arm A, aggregates as the median across sites of per-site medians, and builds a 95 percent interval with a hierarchical bootstrap (resample sites, then pairs within each drawn site). It runs a Wilcoxon signed-rank test on the pooled pair ratios with Holm correction across every row in the table, and computes the A/A minimum detectable effect as the 95th percentile of the bootstrap absolute effect on the stock-versus-stock pairs, with a floor of 3 percent. The verdict is `BETTER` only if the interval is entirely on the improving side, the Holm p value is below 0.05, the effect exceeds the A/A minimum, and for click metrics the median per-pair gain is at least 25 ms (`--min-ms`). `WORSE` is the mirror image. Anything else is `noise`, or `small` when it is significant but under the thresholds. Negative effects mean the Hyperion build is faster or lighter. A second table shows how often each arm was served by prerender or prefetch.

Times below 1 ms are floored at 1 ms before taking ratios, and count and size metrics use `log((B+1)/(A+1))`. Timing metrics are `click_fcp`, `click_lcp`, `down_fcp`, `down_lcp`, `start_fcp`, `start_lcp`, `warm_fcp`, `warm_lcp` and `warm_load`; guardrails are `bytes_start`, `bytes_click`, `cpu_start_ms`, `cpu_click_ms`, `renderers_end`, `warm_reqs`, `warm_cond` and `warm_304`.

## Known limits

- Prerender only works if the harness uses Puppeteer's own tab-target session. A session created with `page.createCDPSession()` makes Chrome refuse every prerender with `PrerenderingDisabledByDevTools`, which would silently hide the headline effect. The harness uses `page._client()`, an internal Puppeteer API, so `puppeteer-core` is pinned to 24.20.0.
- Replay archives contain whatever the recorder saw, including Cloudflare or consent walls; WPR does not revalidate, so the warm-cache conditional-request counts are mostly zero unless the recorded responses carry validators.
- Tab-switch latency, scroll smoothness, back/forward and Speed Index from screen capture (items 2, 5 and 6 of the research doc) are not implemented here.
- Timings are only as quiet as the pinned cores. Check `uptime` and the load on the chosen cores before a session; a Chromium build running on the box inflates every number.
