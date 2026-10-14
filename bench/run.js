#!/usr/bin/env node
// Runs one benchmark session. Meant to execute inside the hbench network namespace
// (see bench.sh), with the replay server already running on 10.77.0.1.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { sleep, makeRng, nowEpoch, parseArgs, withTimeout } from './lib/common.js';
import { writeProfile, chromeArgs, launchChrome, chromeVersion } from './lib/chrome.js';
import { humanClick, sampleDwell } from './lib/input.js';
import { readPage, waitForNavigation, processSample, ifaceBytes, attachPreloadLog, loadSites } from './lib/measure.js';

const spec = {
  stock: { default: '' }, hyp: { default: '' }, sites: { default: 'sites.json' }, out: { default: '' },
  pairs: { type: 'number', default: 30 }, 'aa-pairs': { type: 'number', default: 10 },
  train: { type: 'number', default: 3 }, profile: { default: 'p25' }, 'chrome-cores': { default: '' },
  cooldown: { type: 'number', default: 2 }, 'block-cooldown': { type: 'number', default: 4 },
  warmup: { type: 'number', default: 2 }, 'site-filter': { default: '' }, 'site-limit': { type: 'number', default: 0 },
  seed: { default: 'hb1' }, settle: { type: 'number', default: 1500 }, iface: { default: 'hbv-n' },
  'spki-file': { default: path.join(os.homedir(), 'hyperion-bench/catapult/web_page_replay_go/wpr_public_hash.txt') },
  'restart-after-train': { type: 'bool', default: false }, 'no-force-preload': { type: 'bool', default: false },
  sandbox: { type: 'bool', default: false }, 'chrome-arg': { default: '' }, 'run-timeout': { type: 'number', default: 240 },
  'no-aa': { type: 'bool', default: false }, label: { default: '' },
};
const cfg = parseArgs(process.argv.slice(2), spec);
if (!cfg.stock || !cfg.hyp) throw new Error('--stock and --hyp are required');
const spki = fs.readFileSync(cfg['spki-file'], 'utf8').split(/\s+/).filter(Boolean).join(',');
const sessionId = cfg.out ? path.basename(cfg.out, '.jsonl') : new Date().toISOString().replace(/[:.]/g, '-');
const outFile = cfg.out || path.join('results', `${sessionId}.jsonl`);
fs.mkdirSync(path.dirname(outFile), { recursive: true });
const emit = (obj) => fs.appendFileSync(outFile, JSON.stringify(obj) + '\n');
process.on('unhandledRejection', (e) => console.error('unhandled rejection (ignored):', String(e && e.message ? e.message : e).slice(0, 200)));
const log = (...a) => console.error(new Date().toISOString().slice(11, 19), ...a);

let sites = loadSites(cfg.sites);
if (cfg['site-filter']) sites = sites.filter((s) => new RegExp(cfg['site-filter']).test(s.name));
if (cfg['site-limit']) sites = sites.slice(0, cfg['site-limit']);

// ---------------------------------------------------------------- one measured run
async function waitLoad(page, url) {
  await page.goto(url, { waitUntil: 'load', timeout: 45000 });
}

async function trainSite(page, cdp, site, rng, n) {
  const trained = site.links.filter((l) => !l.novel);
  for (let i = 0; i < n; i++) {
    await waitLoad(page, site.start);
    await sleep(700);
    for (let j = 0; j < trained.length; j++) {
      const before = page.url();
      await humanClick(page, cdp, trained[j].href, rng, { dwellMs: rng.range(180, 420) });
      await waitForNavigation(page, before);
      await sleep(800);
      if (j < trained.length - 1) {
        await waitLoad(page, site.start);
        await sleep(500);
      }
    }
  }
}

function classify(m) {
  if (m.activationStart > 0) return 'prerender';
  if (m.deliveryType === 'navigational-prefetch') return 'prefetch';
  return 'none';
}

async function executeRun(run, ctx = {}) {
  const { site, arm, bin } = run;
  const link = site.links[run.linkIdx];
  const seedBase = `${cfg.seed}|${site.name}|${run.rep}|${run.kind}`;
  const rngTrain = makeRng(seedBase + '|train');
  const rngClick = makeRng(seedBase + '|click');
  const rngIdle = makeRng(seedBase + '|idle');
  const profileDir = fs.mkdtempSync('/dev/shm/hb-profile-');
  writeProfile(profileDir, { forcePreload: !cfg['no-force-preload'] });
  const args = chromeArgs({ profileDir, spkiList: spki, sandbox: cfg.sandbox, extra: cfg['chrome-arg'] ? cfg['chrome-arg'].split(' ') : [] });
  let chrome = await launchChrome({ bin, cores: cfg['chrome-cores'], args });
  ctx.chrome = chrome;
  const rec = { ok: false };
  try {
    const open = async () => {
      const page = (await chrome.browser.pages())[0] || (await chrome.browser.newPage());
      // Puppeteer's own tab-target session. A session made with page.createCDPSession() attaches
      // without tab target support, and Chrome then refuses every prerender (PrerenderingDisabledByDevTools).
      const cdp = { send: (m, p) => page._client().send(m, p) };
      const bsess = await chrome.browser.target().createCDPSession();
      return { page, cdp, bsess };
    };
    let { page, cdp, bsess } = await open();

    // Training: identical scripted history for both arms. The predictor needs history.
    const nTrain = cfg.train;
    if (nTrain > 0) await trainSite(page, cdp, site, rngTrain, nTrain);
    await withTimeout(cdp.send('Network.clearBrowserCache'), 10000, 'clearCache').catch(() => {});
    if (cfg['restart-after-train'] && nTrain > 0) {
      await chrome.close();
      for (const d of ['Cache', 'Code Cache', 'GPUCache']) fs.rmSync(path.join(profileDir, 'Default', d), { recursive: true, force: true });
      chrome = await launchChrome({ bin, cores: cfg['chrome-cores'], args });
      ctx.chrome = chrome;
      ({ page, cdp, bsess } = await open());
    }
    const preload = attachPreloadLog(page);
    const preloadEvents = preload.events;
    await preload.refresh();
    await page.goto('about:blank');
    await sleep(500);

    // Cold start page load (cache cleared, history trained).
    const reqStats = { n: 0, cond: 0, s304: 0 };
    let counting = false;
    page.on('request', (r) => {
      if (!counting) return;
      reqStats.n++;
      const h = r.headers();
      if (h['if-none-match'] || h['if-modified-since']) reqStats.cond++;
    });
    page.on('response', (r) => {
      if (counting && r.status() === 304) reqStats.s304++;
    });
    const b0 = ifaceBytes(cfg.iface);
    const p0 = await processSample(bsess);
    await waitLoad(page, site.start);
    await sleep(cfg.settle);
    await preload.refresh();
    const startM = await readPage(page);
    const b1 = ifaceBytes(cfg.iface);
    const p1 = await processSample(bsess);

    // Idle on the start page like a reader would, then the measured real-input click.
    await sleep(rngIdle.range(1500, 3500));
    const dwell = sampleDwell(rngClick);
    const fromUrl = page.url();
    const click = await humanClick(page, cdp, link.href, rngClick, { dwellMs: dwell });
    const landed = await waitForNavigation(page, fromUrl);
    await preload.refresh();
    await sleep(cfg.settle);
    const dest = await readPage(page);
    const b2 = ifaceBytes(cfg.iface);
    const p2 = await processSample(bsess);

    // Repeat visit with a warm cache.
    counting = true;
    await waitLoad(page, site.start);
    await sleep(cfg.settle);
    counting = false;
    const warm = await readPage(page);

    const paintAbs = (m, v) => (v == null ? null : m.timeOrigin + Math.max(v, m.activationStart));
    const fcpAbs = paintAbs(dest, dest.fcp ?? dest.fp);
    const lcpAbs = paintAbs(dest, dest.lcp);
    const d = (x, t0) => (x == null ? null : +(x - t0).toFixed(2));
    rec.ok = true;
    rec.link = { idx: run.linkIdx, href: link.href, type: nTrain > 0 && !link.novel ? 'trained' : 'novel', text: click.linkText };
    rec.input = { dwell: +click.dwell.toFixed(1), hold: +click.hold.toFixed(1), travel: +click.travel.toFixed(1), arrive_to_down: +(click.tDown - click.tArrive).toFixed(1) };
    rec.landed = landed;
    rec.spec = classify(dest);
    rec.dest = { activationStart: dest.activationStart, deliveryType: dest.deliveryType, transferSize: dest.transferSize, fcp: dest.fcp, lcp: dest.lcp, rulesScripts: dest.rulesScripts, lcpTag: dest.lcpTag };
    rec.start_rules = startM.rulesScripts;
    rec.preload = { n: preloadEvents.length, events: preloadEvents.slice(0, 40) };
    rec.m = {
      click_fcp: d(fcpAbs, click.tUp),
      click_lcp: d(lcpAbs, click.tUp),
      down_fcp: d(fcpAbs, click.tDown),
      down_lcp: d(lcpAbs, click.tDown),
      activated: dest.activationStart > 0 ? 1 : 0,
      prefetch_hit: dest.deliveryType === 'navigational-prefetch' ? 1 : 0,
      start_fcp: startM.fcp ?? startM.fp,
      start_lcp: startM.lcp,
      start_ttfb: startM.responseStart,
      start_load: startM.loadEventEnd,
      warm_fcp: warm.fcp ?? warm.fp,
      warm_lcp: warm.lcp,
      warm_ttfb: warm.responseStart,
      warm_load: warm.loadEventEnd,
      warm_reqs: reqStats.n,
      warm_cond: reqStats.cond,
      warm_304: reqStats.s304,
      bytes_start: b1 != null && b0 != null ? b1 - b0 : null,
      bytes_click: b2 != null && b1 != null ? b2 - b1 : null,
      cpu_start_ms: p1.cpuMs != null ? +(p1.cpuMs - p0.cpuMs).toFixed(1) : null,
      cpu_click_ms: p2.cpuMs != null ? +(p2.cpuMs - p1.cpuMs).toFixed(1) : null,
      renderers_start: p1.renderers,
      renderers_end: p2.renderers,
    };
  } catch (e) {
    rec.error = String(e && e.message ? e.message : e).slice(0, 400);
  } finally {
    await chrome.close().catch(() => {});
    // Chrome's helpers can still be flushing into the profile after close(), so retry and never let cleanup kill the run.
    try {
      fs.rmSync(profileDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    } catch (e) {
      log('cleanup failed', profileDir, e.code);
    }
  }
  return rec;
}

// ---------------------------------------------------------------- schedule
function buildSchedule() {
  const rng = makeRng(cfg.seed + '|schedule');
  const blocks = [];
  const nBlocks = Math.ceil(cfg.pairs / 2);
  const aaBlocks = cfg['no-aa'] ? 0 : Math.ceil(cfg['aa-pairs'] / 2);
  for (let b = 0; b < nBlocks; b++) {
    for (const site of rng.shuffle(sites)) {
      const kinds = rng.shuffle(b < aaBlocks ? ['AB', 'AA'] : ['AB']);
      for (const kind of kinds) {
        const limit = kind === 'AB' ? cfg.pairs : cfg['aa-pairs'];
        const X = { arm: 'A', bin: cfg.stock };
        const Y = kind === 'AB' ? { arm: 'B', bin: cfg.hyp } : { arm: 'A2', bin: cfg.stock };
        const r0 = 2 * b;
        const r1 = 2 * b + 1;
        const mk = (arm, rep) => ({ site, kind, rep, ...arm, linkIdx: rep % site.links.length });
        const quartet = [mk(X, r0), mk(Y, r0)];
        if (r1 < limit) quartet.push(mk(Y, r1), mk(X, r1)); // ABBA
        blocks.push(quartet);
      }
    }
  }
  return blocks;
}

// ---------------------------------------------------------------- main
async function main() {
  const meta = {
    type: 'session', id: sessionId, label: cfg.label, started: new Date().toISOString(),
    cfg: { ...cfg }, host: os.hostname(), stock_version: await chromeVersion(cfg.stock), hyp_version: await chromeVersion(cfg.hyp),
    sites: sites.map((s) => s.name), node: process.version,
  };
  try {
    meta.iface_rx_bytes_start = ifaceBytes(cfg.iface);
  } catch {}
  emit(meta);
  log('session', sessionId, 'sites', sites.length, 'out', outFile);

  const blocks = buildSchedule();
  const total = blocks.reduce((n, b) => n + b.length, 0);
  let done = 0;
  let order = 0;
  const execWithTimeout = async (run) => {
    const ctx = {};
    let timer;
    const res = await Promise.race([
      executeRun(run, ctx),
      new Promise((r) => (timer = setTimeout(() => {
        // Kill exactly the Chrome process group this run started, then report the timeout.
        if (ctx.chrome) ctx.chrome.kill();
        r({ ok: false, error: 'run timeout' });
      }, cfg['run-timeout'] * 1000))),
    ]);
    clearTimeout(timer);
    return res;
  };

  // Warm-up runs: not analysed, they settle CPU frequency, page cache and the replay server.
  for (let i = 0; i < cfg.warmup && blocks.length; i++) {
    const w = { ...blocks[0][0], arm: 'A', bin: cfg.stock };
    log('warmup', i + 1, w.site.name);
    const rec = await execWithTimeout(w);
    emit({ type: 'run', session: sessionId, warmup: true, site: w.site.name, ...rec });
    await sleep(cfg.cooldown * 1000);
  }

  for (const quartet of blocks) {
    for (const run of quartet) {
      const t0 = Date.now();
      const rec = await execWithTimeout(run);
      emit({
        type: 'run', session: sessionId, warmup: false, order: order++, site: run.site.name, kind: run.kind,
        arm: run.arm, rep: run.rep, pair: `${run.site.name}|${run.kind}|${run.rep}`, bin: run.bin,
        profile: cfg.profile, train: cfg.train, wall_ms: Date.now() - t0, t: new Date().toISOString(), ...rec,
      });
      done++;
      const mm = rec.m || {};
      log(`${done}/${total}`, run.site.name, run.kind, run.arm, 'rep', run.rep, rec.ok ? `spec=${rec.spec} link=${rec.link.type} click_fcp=${mm.click_fcp}` : `FAIL ${rec.error}`);
      await sleep(cfg.cooldown * 1000);
    }
    await sleep(cfg['block-cooldown'] * 1000);
  }
  emit({ type: 'session_end', id: sessionId, ended: new Date().toISOString(), runs: done });
  log('done');
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
