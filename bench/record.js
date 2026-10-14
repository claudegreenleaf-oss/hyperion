#!/usr/bin/env node
// Records sites through web_page_replay_go (running in record mode, see bench.sh record).
// Input: a candidates file with {name, start, category} entries. For entries without links,
// same-origin article-like links are discovered. Output: sites.json used by run.js.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { sleep, makeRng, parseArgs, withTimeout } from './lib/common.js';
import { writeProfile, chromeArgs, launchChrome } from './lib/chrome.js';
import { humanClick, locateLink } from './lib/input.js';
import { waitForNavigation } from './lib/measure.js';

const cfg = parseArgs(process.argv.slice(2), {
  bin: { default: '' }, sites: { default: 'candidates.json' }, out: { default: 'sites.json' }, links: { type: 'number', default: 3 },
  'spki-file': { default: path.join(os.homedir(), 'hyperion-bench/catapult/web_page_replay_go/wpr_public_hash.txt') },
  'chrome-cores': { default: '' }, only: { default: '' }, sandbox: { type: 'bool', default: false }, archive: { default: '' },
});
const spki = fs.readFileSync(cfg['spki-file'], 'utf8').split(/\s+/).filter(Boolean).join(',');
const cands = JSON.parse(fs.readFileSync(cfg.sites, 'utf8')).sites.filter((s) => !cfg.only || new RegExp(cfg.only).test(s.name));
process.on('unhandledRejection', (e) => console.error('unhandled rejection (ignored):', String(e && e.message ? e.message : e).slice(0, 200)));
const log = (...a) => console.error(new Date().toISOString().slice(11, 19), ...a);

const DISCOVER = (start, want, selector) => {
  const base = new URL(start);
  const bad = /(log.?in|sign.?in|sign.?up|cart|checkout|account|subscribe|logout|register|donate|\/search|#|\.pdf$|\.zip$|\/tag\/|\/category\/|\/author\/|mailto:)/i;
  const out = [];
  const seen = new Set([base.href.replace(/#.*/, '')]);
  for (const a of document.querySelectorAll(selector || 'a[href]')) {
    let u;
    try { u = new URL(a.href); } catch { continue; }
    if (u.origin !== base.origin || a.target === '_blank') continue;
    const href = u.href.replace(/#.*/, '');
    if (seen.has(href) || bad.test(u.pathname + u.search)) continue;
    if (u.pathname === base.pathname && u.search === base.search) continue;
    if (!selector && a.closest('header, nav, footer, aside, [role=navigation], [role=banner]')) continue;
    const r = a.getBoundingClientRect();
    const txt = (a.textContent || '').trim().replace(/\s+/g, ' ');
    if (r.width < (selector ? 20 : 40) || r.height < 10 || r.top > 2600 || r.bottom < 0) continue;
    if (!selector && (txt.split(' ').length < 3 || txt.length < 14)) continue;
    const st = getComputedStyle(a);
    if (st.visibility === 'hidden' || st.display === 'none') continue;
    seen.add(href);
    out.push({ href, text: txt.slice(0, 70), top: r.top });
  }
  return out.slice(0, want * 4);
};

const result = [];
const writeOut = () => fs.writeFileSync(cfg.out, JSON.stringify({ meta: { recorded: new Date().toISOString(), archive: cfg.archive }, sites: result }, null, 2));

async function main() {
  const profileDir = fs.mkdtempSync('/dev/shm/hb-rec-');
  writeProfile(profileDir, { forcePreload: true });
  const chrome = await launchChrome({ bin: cfg.bin, cores: cfg['chrome-cores'], args: chromeArgs({ profileDir, spkiList: spki, sandbox: cfg.sandbox }) });
  try {
    let page = (await chrome.browser.pages())[0];
    const cdp = { send: (m, p) => page._client().send(m, p) };
    const recordSite = async (site) => {
      log('recording', site.name, site.start);
      await page.goto(site.start, { waitUntil: 'load', timeout: 60000 });
      await sleep(3500);
      let links = site.links;
      if (!links || !links.length) {
        const found = await withTimeout(page.evaluate(DISCOVER, site.start, cfg.links, site.selector || ''), 15000, 'discover');
        links = [];
        const probe = makeRng('probe|' + site.name);
        for (const f of found) {
          if (links.length >= cfg.links) break;
          if (await locateLink(page, f.href, probe)) links.push({ href: f.href, text: f.text });
        }
      }
      if (links.length < 2) {
        log('  skipped, only', links.length, 'links');
        return;
      }
      links = links.map((l, i) => ({ ...l, novel: i === links.length - 1 }));
      const rng = makeRng('record|' + site.name);
      let ok = 0;
      for (let pass = 0; pass < 2; pass++) {
        for (const l of links) {
          try {
            await page.goto(site.start, { waitUntil: 'load', timeout: 60000 });
            await sleep(1500);
            const before = page.url();
            await humanClick(page, cdp, l.href, rng, { dwellMs: 300 });
            await waitForNavigation(page, before, 40000);
            await sleep(3000);
            if (pass === 0) ok++;
          } catch (e) {
            log('  link failed', l.href, String(e.message).slice(0, 100));
          }
        }
      }
      if (ok >= 2) {
        result.push({ ...site, links });
        writeOut();
      }
      else log('  dropped,', ok, 'links clicked');
    };
    for (const site of cands) {
      try {
        await withTimeout(recordSite(site), 300000, `site ${site.name}`);
      } catch (e) {
        log('  site failed', String(e.message).slice(0, 120));
        // A hung page cannot be trusted any more: open a fresh tab and drop the old one.
        try {
          const np = await chrome.browser.newPage();
          const old = page;
          page = np;
          old.close().catch(() => {});
        } catch {}
      }
    }
  } finally {
    await chrome.close();
    fs.rmSync(profileDir, { recursive: true, force: true });
  }
  writeOut();
  log('wrote', cfg.out, result.length, 'sites');
}
main().catch((e) => {
  console.error(e);
  process.exit(1);
});
