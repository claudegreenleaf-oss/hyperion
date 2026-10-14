import fs from 'node:fs';
import { sleep, readNum, withTimeout } from './common.js';

// Runs inside the page. Everything is read from buffered performance entries after the
// fact, so nothing is injected before navigation and prerendered documents work the same
// way as ordinary ones.
export const PAGE_METRICS = `(async () => {
  const nav = performance.getEntriesByType('navigation')[0] || {};
  const paints = performance.getEntriesByType('paint');
  const lcps = await new Promise((res) => {
    const es = [];
    const po = new PerformanceObserver((l) => es.push(...l.getEntries()));
    try { po.observe({ type: 'largest-contentful-paint', buffered: true }); } catch (e) { res([]); return; }
    setTimeout(() => { po.disconnect(); res(es); }, 60);
  });
  const fcp = paints.find((p) => p.name === 'first-contentful-paint');
  const fp = paints.find((p) => p.name === 'first-paint');
  const last = lcps[lcps.length - 1];
  return {
    url: location.href,
    readyState: document.readyState,
    prerendering: !!document.prerendering,
    timeOrigin: performance.timeOrigin,
    activationStart: nav.activationStart || 0,
    deliveryType: nav.deliveryType === undefined ? null : nav.deliveryType,
    navType: nav.type || null,
    transferSize: nav.transferSize === undefined ? null : nav.transferSize,
    responseStart: nav.responseStart === undefined ? null : nav.responseStart,
    loadEventEnd: nav.loadEventEnd === undefined ? null : nav.loadEventEnd,
    fcp: fcp ? fcp.startTime : null,
    fp: fp ? fp.startTime : null,
    lcp: last ? last.startTime : null,
    lcpSize: last ? last.size : null,
    lcpUrl: last ? (last.url || '') : null,
    lcpTag: last && last.element ? last.element.tagName : null,
    rulesScripts: document.querySelectorAll('script[type="speculationrules"]').length,
    now: performance.timeOrigin + performance.now(),
  };
})()`;

export async function readPage(page, retries = 20) {
  let lastErr;
  for (let i = 0; i < retries; i++) {
    try {
      return await withTimeout(page.evaluate(PAGE_METRICS), 10000, 'readPage');
    } catch (e) {
      lastErr = e;
      await sleep(100);
    }
  }
  throw lastErr;
}

// Polls cheaply until the page URL differs from fromUrl and the document is complete.
export async function waitForNavigation(page, fromUrl, timeoutMs = 25000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try {
      const s = await withTimeout(page.evaluate(() => location.href + '\n' + document.readyState + '\n' + (document.prerendering ? 1 : 0)), 8000, 'poll');
      const [href, rs, pr] = s.split('\n');
      if (href !== fromUrl && rs === 'complete' && pr === '0') return href;
    } catch {}
    await sleep(60);
  }
  throw new Error(`navigation from ${fromUrl} did not complete`);
}

// Browser-wide process info through a browser-level CDP session.
export async function processSample(browserSession) {
  try {
    const { processInfo } = await withTimeout(browserSession.send('SystemInfo.getProcessInfo'), 8000, 'processInfo');
    let cpu = 0;
    let renderers = 0;
    for (const p of processInfo) {
      cpu += p.cpuTime;
      if (p.type === 'renderer') renderers++;
    }
    return { cpuMs: cpu * 1000, renderers, processes: processInfo.length };
  } catch {
    return { cpuMs: null, renderers: null, processes: null };
  }
}

export function ifaceBytes(iface) {
  return readNum(`/sys/class/net/${iface}/statistics/rx_bytes`);
}

// Collects Preload domain events (speculation lifecycle) for one run. Puppeteer swaps the
// page's primary session on some navigations, so listeners are re-attached whenever the
// session object changes. Call refresh() after navigations.
export function attachPreloadLog(page) {
  const events = [];
  const t0 = performance.now();
  let attached = null;
  const push = (kind) => (e) => {
    const key = e.key || {};
    events.push({
      k: kind,
      t: Math.round(performance.now() - t0),
      u: (key.url || '').slice(0, 120),
      a: key.action || null,
      s: e.status,
      d: e.prerenderStatus || e.prefetchStatus || null,
    });
  };
  const refresh = async () => {
    const c = page._client();
    if (c === attached) return;
    attached = c;
    c.on('Preload.prefetchStatusUpdated', push('prefetch'));
    c.on('Preload.prerenderStatusUpdated', push('prerender'));
    c.on('Preload.ruleSetUpdated', (e) => events.push({ k: 'ruleset', t: Math.round(performance.now() - t0), u: (e.ruleSet?.url || '').slice(0, 120), s: 'seen' }));
    await withTimeout(c.send('Preload.enable'), 5000, 'Preload.enable').catch(() => {});
  };
  return { events, refresh };
}

export function loadSites(file) {
  const j = JSON.parse(fs.readFileSync(file, 'utf8'));
  return j.sites;
}
