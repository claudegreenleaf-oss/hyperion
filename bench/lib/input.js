import { sleep, nowEpoch, withTimeout } from './common.js';

const send = (cdp, method, params) => withTimeout(cdp.send(method, params), 10000, method);

// Finds the anchor whose resolved href matches, scrolls it into view, and returns a point
// inside it that actually hit-tests to the anchor.
export async function locateLink(page, href, rng) {
  const info = await withTimeout(page.evaluate(
    (href, rx, ry) => {
      const strip = (u) => {
        try {
          const x = new URL(u, location.href);
          x.hash = '';
          return x.href;
        } catch {
          return u;
        }
      };
      const want = strip(href);
      const cands = [...document.querySelectorAll('a[href]')].filter((a) => strip(a.href) === want && a.target !== '_blank');
      for (const a of cands) {
        a.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
        const rects = [...a.getClientRects()].filter((r) => r.width > 4 && r.height > 4);
        for (const r of rects) {
          const tries = [[rx, ry], [0.5, 0.5], [0.25, 0.5], [0.75, 0.5], [0.5, 0.2], [0.5, 0.8], [0.1, 0.1], [0.9, 0.9], [0.1, 0.9], [0.9, 0.1]];
          for (const [fx, fy] of tries) {
            const x = r.left + r.width * (0.15 + 0.7 * fx);
            const y = r.top + r.height * (0.25 + 0.5 * fy);
            if (x < 2 || y < 2 || x > innerWidth - 2 || y > innerHeight - 2) continue;
            const el = document.elementFromPoint(x, y);
            const hit = el && el.closest('a');
            if (hit && (hit === a || strip(hit.href) === want)) return { x, y, w: r.width, h: r.height, text: (a.textContent || '').trim().slice(0, 80) };
          }
        }
      }
      return null;
    },
    href,
    rng.next(),
    rng.next(),
  ), 15000, 'locateLink');
  return info;
}

async function move(cdp, x, y, buttons = 0) {
  await send(cdp, 'Input.dispatchMouseEvent', { type: 'mouseMoved', x, y, button: 'none', buttons, pointerType: 'mouse' });
}

// Moves along a curved, eased path with small jitter, sampled at roughly 125 Hz.
async function glide(cdp, from, to, rng, durationMs) {
  const dx = to.x - from.x;
  const dy = to.y - from.y;
  const dist = Math.hypot(dx, dy) || 1;
  const nx = -dy / dist;
  const ny = dx / dist;
  const bend = rng.range(-0.18, 0.18) * dist;
  const c1 = { x: from.x + dx * 0.3 + nx * bend, y: from.y + dy * 0.3 + ny * bend };
  const c2 = { x: from.x + dx * 0.7 + nx * bend * 0.5, y: from.y + dy * 0.7 + ny * bend * 0.5 };
  const steps = Math.max(8, Math.round(durationMs / 8));
  const t0 = performance.now();
  for (let i = 1; i <= steps; i++) {
    const u = i / steps;
    const e = u * u * (3 - 2 * u);
    const m = 1 - e;
    let x = m * m * m * from.x + 3 * m * m * e * c1.x + 3 * m * e * e * c2.x + e * e * e * to.x;
    let y = m * m * m * from.y + 3 * m * m * e * c1.y + 3 * m * e * e * c2.y + e * e * e * to.y;
    if (i < steps) {
      x += rng.normal() * 0.6;
      y += rng.normal() * 0.6;
    }
    await move(cdp, x, y);
    const wait = t0 + (durationMs * i) / steps - performance.now();
    if (wait > 1) await sleep(wait);
  }
}

// Samples a hover dwell: lognormal around 330 ms, clipped to 150 to 600 ms.
export function sampleDwell(rng) {
  const d = Math.exp(Math.log(330) + 0.4 * rng.normal());
  return Math.min(600, Math.max(150, d));
}

// Real-input click: pointer glides to the link, rests for a human-like dwell, presses,
// holds, releases. Returns the timestamps (epoch ms, node clock) of each phase.
export async function humanClick(page, cdp, href, rng, { dwellMs, startPoint } = {}) {
  const target = await locateLink(page, href, rng);
  if (!target) throw new Error(`link not found or not clickable: ${href}`);
  const from = startPoint || { x: rng.range(150, 1100), y: rng.range(120, 780) };
  await move(cdp, from.x, from.y);
  await sleep(120);
  const dwell = dwellMs ?? sampleDwell(rng);
  const travel = rng.range(260, 620);
  const tMoveStart = nowEpoch();
  await glide(cdp, from, target, rng, travel);
  const tArrive = nowEpoch();
  await sleep(dwell);
  const hold = rng.range(55, 115);
  const tDown = nowEpoch();
  await send(cdp, 'Input.dispatchMouseEvent', { type: 'mousePressed', x: target.x, y: target.y, button: 'left', buttons: 1, clickCount: 1, pointerType: 'mouse' });
  await sleep(hold);
  const tUp = nowEpoch();
  await send(cdp, 'Input.dispatchMouseEvent', { type: 'mouseReleased', x: target.x, y: target.y, button: 'left', buttons: 0, clickCount: 1, pointerType: 'mouse' });
  return { tMoveStart, tArrive, tDown, tUp, dwell, hold, travel, point: { x: target.x, y: target.y }, linkText: target.text };
}
