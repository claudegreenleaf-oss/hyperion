// One trial of the "headless browser" arm: what a browser-driving MCP server
// (chrome-devtools-mcp, playwright-mcp) costs. Cold browser, load the results page, then
// load the chosen result pages in parallel tabs and read their text. Prints one JSON line.
// Usage: node browser_arm.js '<json {chrome, serp, urls, timeout_ms}>'
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import puppeteer from 'puppeteer-core';

const cfg = JSON.parse(process.argv[2]);
const t0 = performance.now();
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'agent-browser-'));
const out = { pages: [] };
let browser;
try {
  browser = await puppeteer.launch({
    executablePath: cfg.chrome,
    headless: true,
    userDataDir: dir,
    args: ['--no-first-run', '--no-default-browser-check', '--disable-component-update', '--no-sandbox'],
  });
  const load = async (url) => {
    const page = await browser.newPage();
    const t = performance.now();
    try {
      await page.goto(url, { waitUntil: 'load', timeout: cfg.timeout_ms });
      const chars = await page.evaluate(() => (document.body ? document.body.innerText.length : 0));
      return { url, chars, ms: Math.round(performance.now() - t) };
    } catch (e) {
      return { url, chars: 0, ms: Math.round(performance.now() - t), error: String(e.message || e).slice(0, 160) };
    }
  };
  out.serp = await load(cfg.serp);
  out.serp_ms = Math.round(performance.now() - t0);
  out.pages = await Promise.all(cfg.urls.map(load));
} catch (e) {
  out.error = String(e.message || e).slice(0, 200);
} finally {
  if (browser) await browser.close().catch(() => {});
  fs.rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
out.ms = Math.round(performance.now() - t0);
console.log(JSON.stringify(out));
