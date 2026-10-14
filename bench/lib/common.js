import fs from 'node:fs';

export function withTimeout(promise, ms, label = 'operation') {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, rej) => {
      timer = setTimeout(() => rej(new Error(`timeout after ${ms} ms: ${label}`)), ms);
    }),
  ]).finally(() => clearTimeout(timer));
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Seeded PRNG so that both arms of a pair see the same mouse path, dwell and idle time.
export function hashString(s) {
  let h = 2166136261 >>> 0;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619) >>> 0;
  }
  return h >>> 0;
}

export function makeRng(seedString) {
  let a = hashString(String(seedString)) || 1;
  const next = () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const rng = {
    next,
    range: (lo, hi) => lo + (hi - lo) * next(),
    normal: () => {
      const u = Math.max(next(), 1e-12);
      const v = next();
      return Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * v);
    },
    shuffle: (arr) => {
      const out = arr.slice();
      for (let i = out.length - 1; i > 0; i--) {
        const j = Math.floor(next() * (i + 1));
        [out[i], out[j]] = [out[j], out[i]];
      }
      return out;
    },
  };
  return rng;
}

// Epoch milliseconds with sub-millisecond resolution, on the same clock family as
// performance.timeOrigin + performance.now() inside the page.
export const nowEpoch = () => performance.timeOrigin + performance.now();

export function readNum(path) {
  try {
    return Number(fs.readFileSync(path, 'utf8').trim());
  } catch {
    return null;
  }
}

export function parseArgs(argv, spec) {
  const out = {};
  for (const [k, v] of Object.entries(spec)) out[k] = v.default;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith('--')) throw new Error(`unexpected argument ${a}`);
    const key = a.slice(2);
    const s = spec[key];
    if (!s) throw new Error(`unknown option --${key}`);
    if (s.type === 'bool') out[key] = true;
    else {
      const v = argv[++i];
      if (v === undefined) throw new Error(`--${key} needs a value`);
      out[key] = s.type === 'number' ? Number(v) : v;
    }
  }
  return out;
}

// Runs a function or expression in the page's main world through raw Runtime.evaluate on the
// page's own CDP session. Puppeteer's page.evaluate waits on its execution-context tracking,
// which never sees the document a prerender activation swaps in.
export async function evalIn(page, fn, ...args) {
  const expression = typeof fn === 'string' ? fn : `(${fn})(${args.map((a) => JSON.stringify(a)).join(',')})`;
  const r = await page._client().send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error(`evaluate: ${r.exceptionDetails.exception?.description || r.exceptionDetails.text}`);
  return r.result.value;
}
