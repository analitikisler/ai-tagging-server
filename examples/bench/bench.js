// Load test for any server-side tagging endpoint: this one, TagPiper or
// Google's own. Sends GA4 page_view hits with a fixed number of connections
// in flight and prints throughput and latency percentiles.
//
//   node examples/bench/bench.js <url> [--requests 5000] [--concurrency 32] [--measurement-id G-XXXX] [--origin https://example.com]
//
// <url> is the collect endpoint, e.g. http://localhost:8080/g/collect. Only
// the hits' round trip is measured, including whatever the server does
// before answering; point it at a staging property, since every hit counts.

const http = require('node:http');
const https = require('node:https');

const args = process.argv.slice(2);
const url = args.find((a) => !a.startsWith('--') && /^https?:/.test(a));
const opt = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : fallback;
};
if (!url) {
  console.error('usage: node bench.js <url> [--requests N] [--concurrency N] [--measurement-id G-XXXX] [--origin URL]');
  process.exit(2);
}
const total = Number(opt('requests', 5000));
const concurrency = Number(opt('concurrency', 32));
const tid = opt('measurement-id', 'G-TEST000001');
const origin = opt('origin', 'https://example.com');

const target = new URL(url);
const lib = target.protocol === 'https:' ? https : http;
const agent = new lib.Agent({ keepAlive: true, maxSockets: concurrency });

function hit(i) {
  const q = new URLSearchParams({
    v: '2', tid, cid: `${1000 + (i % 500)}.${1700000000 + i}`, en: 'page_view',
    dl: `${origin}/page-${i % 20}`, dt: 'Bench', sid: String(1700000000 + (i % 500)), sct: '1', seg: '1',
  });
  const u = new URL(target);
  q.forEach((v, k) => u.searchParams.set(k, v));
  return new Promise((resolve) => {
    const t0 = process.hrtime.bigint();
    const req = lib.get(u, { agent, headers: { origin, 'user-agent': 'Mozilla/5.0 Bench' } }, (res) => {
      res.resume();
      res.on('end', () => resolve({ ms: Number(process.hrtime.bigint() - t0) / 1e6, status: res.statusCode }));
    });
    req.on('error', () => resolve({ ms: Number(process.hrtime.bigint() - t0) / 1e6, status: 0 }));
  });
}

(async () => {
  let next = 0;
  const results = [];
  const worker = async () => {
    while (next < total) results.push(await hit(next++));
  };
  // Warm up connections and caches without counting.
  await Promise.all(Array.from({ length: Math.min(concurrency, 16) }, (_, i) => hit(i)));
  const start = process.hrtime.bigint();
  await Promise.all(Array.from({ length: concurrency }, worker));
  const secs = Number(process.hrtime.bigint() - start) / 1e9;

  const ms = results.map((r) => r.ms).sort((a, b) => a - b);
  const pct = (p) => ms[Math.min(ms.length - 1, Math.floor(ms.length * p))].toFixed(1);
  const statuses = {};
  for (const r of results) statuses[r.status] = (statuses[r.status] || 0) + 1;
  console.log(JSON.stringify({
    url, requests: results.length, concurrency, seconds: +secs.toFixed(2),
    requests_per_second: Math.round(results.length / secs),
    latency_ms: { mean: +(ms.reduce((a, b) => a + b, 0) / ms.length).toFixed(1), p50: +pct(0.5), p95: +pct(0.95), p99: +pct(0.99), max: +ms[ms.length - 1].toFixed(1) },
    statuses,
  }, null, 2));
  agent.destroy();
})();
