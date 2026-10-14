#!/usr/bin/env node
// Reads the JSONL written by the preflight run and fails unless every arm showed at least
// one prerender activation or prefetch hit, which proves Preload pages is really on.
import fs from 'node:fs';

const file = process.argv[2];
const rows = fs.readFileSync(file, 'utf8').trim().split('\n').map((l) => JSON.parse(l)).filter((r) => r.type === 'run');
const arms = {};
for (const r of rows) {
  const a = (arms[r.arm] ||= { runs: 0, prerender: 0, prefetch: 0, failed: 0, binary: r.bin });
  a.runs++;
  if (!r.ok) a.failed++;
  else if (r.spec === 'prerender') a.prerender++;
  else if (r.spec === 'prefetch') a.prefetch++;
}
let bad = false;
for (const [arm, a] of Object.entries(arms)) {
  const good = a.prerender + a.prefetch > 0;
  console.log(`preflight arm ${arm}: ${a.runs} runs, ${a.prerender} prerender activations, ${a.prefetch} prefetch hits, ${a.failed} failed -> ${good ? 'PASS' : 'FAIL'}`);
  if (!good) bad = true;
}
if (!rows.length) bad = true;
process.exit(bad ? 1 : 0);
