#!/usr/bin/env node
// Tiny local site used by the preflight check. Its start page ships speculation rules that
// prerender the three article pages on moderate eagerness (about 200 ms of hover), so a
// browser with Preload pages on must show a prerender activation here.
import http from 'node:http';

const port = Number(process.argv[2] || 8080);
const host = process.argv[3] || '10.77.0.3';
const para = (n) => `<p>${('Speculative loading moves work in front of the click. '.repeat(6)).trim()} Paragraph ${n}.</p>`;
const page = (title, body, extra = '') => `<!doctype html><html><head><meta charset="utf-8"><title>${title}</title>${extra}
<style>body{font:18px/1.5 sans-serif;max-width:46em;margin:2em auto;padding:0 1em}a{display:block;margin:1.2em 0;font-size:22px}</style></head>
<body><h1>${title}</h1>${body}</body></html>`;

const rules = `<script type="speculationrules">{"prerender":[{"where":{"href_matches":"/p*.html"},"eagerness":"moderate"}]}</script>`;
const routes = {
  '/': page('Selftest start', [1, 2, 3].map((i) => `<a href="/p${i}.html">Selftest article number ${i} about speculative loading</a>`).join('\n') + Array.from({ length: 8 }, (_, i) => para(i)).join('\n'), rules),
};
for (const i of [1, 2, 3]) routes[`/p${i}.html`] = page(`Selftest article ${i}`, Array.from({ length: 40 }, (_, k) => para(k)).join('\n'));

http.createServer((req, res) => {
  const body = routes[req.url.split('?')[0]];
  if (!body) {
    res.writeHead(404).end('not found');
    return;
  }
  res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' }).end(body);
}).listen(port, host, () => console.error(`selftest server on ${host}:${port}`));
