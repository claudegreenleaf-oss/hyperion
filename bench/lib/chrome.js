import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import puppeteer from 'puppeteer-core';
import { sleep } from './common.js';

export const WPR_HOST = '10.77.0.1';
export const WPR_HTTP = 8080;
export const WPR_HTTPS = 8443;

// Preferences written into every fresh profile. network_prediction_options 0 is "Standard
// preloading", which is the "Preload pages: on" setting. Both arms get the same value, so
// the harness never measures the effect of the Preload pages default itself.
export function writeProfile(dir, { forcePreload }) {
  fs.mkdirSync(path.join(dir, 'Default'), { recursive: true });
  const prefs = {
    browser: { has_seen_welcome_page: true, check_default_browser: false },
    profile: { exit_type: 'Normal', exited_cleanly: true },
    session: { restore_on_startup: 5 },
    ...(forcePreload ? { net: { network_prediction_options: 0 } } : {}),
  };
  fs.writeFileSync(path.join(dir, 'Default', 'Preferences'), JSON.stringify(prefs));
}

export function chromeArgs({ profileDir, spkiList, sandbox, extra = [] }) {
  const args = [
    `--user-data-dir=${profileDir}`,
    '--remote-debugging-port=0',
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-component-update',
    '--disable-sync',
    '--password-store=basic',
    '--use-mock-keychain',
    '--disable-search-engine-choice-screen',
    '--disable-quic',
    '--window-size=1280,900',
    '--window-position=0,0',
    // Send every host to the replay server and remap the default ports to its ports.
    `--host-resolver-rules=MAP * ${WPR_HOST},EXCLUDE localhost`,
    `--testing-fixed-http-port=${WPR_HTTP}`,
    `--testing-fixed-https-port=${WPR_HTTPS}`,
    `--ignore-certificate-errors-spki-list=${spkiList}`,
    ...extra,
  ];
  if (!sandbox) args.push('--no-sandbox');
  args.push('about:blank');
  return args;
}

// Spawns Chrome pinned to a fixed core set. The child gets its own process group so that
// cleanup signals exactly this tree and nothing else.
export async function launchChrome({ bin, cores, args, env = {} }) {
  const cmd = cores ? 'taskset' : bin;
  const full = cores ? ['-c', cores, bin, ...args] : args;
  const proc = spawn(cmd, full, {
    detached: true,
    stdio: ['ignore', 'ignore', 'pipe'],
    env: { ...process.env, ...env },
  });
  let stderrTail = '';
  const ws = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`chrome did not report a DevTools endpoint: ${stderrTail.slice(-400)}`)), 30000);
    proc.stderr.on('data', (d) => {
      stderrTail = (stderrTail + d.toString()).slice(-4000);
      const m = /DevTools listening on (ws:\/\/\S+)/.exec(stderrTail);
      if (m) {
        clearTimeout(timer);
        resolve(m[1]);
      }
    });
    proc.on('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`chrome exited early (${code}): ${stderrTail.slice(-400)}`));
    });
  });
  proc.stderr.on('data', () => {}); // keep draining
  const exited = new Promise((r) => proc.on('exit', r));
  const browser = await puppeteer.connect({ browserWSEndpoint: ws, defaultViewport: null, protocolTimeout: 120000 });
  return {
    proc,
    browser,
    kill() {
      try {
        process.kill(-proc.pid, 'SIGKILL');
      } catch {}
    },
    async close() {
      try {
        await Promise.race([browser.close(), sleep(8000)]);
      } catch {}
      const done = await Promise.race([exited.then(() => true), sleep(8000).then(() => false)]);
      if (!done) {
        try {
          process.kill(-proc.pid, 'SIGKILL');
        } catch {}
        await Promise.race([exited, sleep(3000)]);
      }
    },
  };
}

export async function chromeVersion(bin) {
  return new Promise((resolve) => {
    const p = spawn(bin, ['--version'], { stdio: ['ignore', 'pipe', 'ignore'] });
    let out = '';
    p.stdout.on('data', (d) => (out += d));
    p.on('exit', () => resolve(out.trim()));
    p.on('error', () => resolve('unknown'));
  });
}
