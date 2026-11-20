// Run with: node --test deploy/cloudflare/worker.test.mjs
// The Worker against a stand-in for the server: what it lets through, and
// what the request looks like when it gets there.
import assert from 'node:assert/strict';
import { afterEach, beforeEach, test } from 'node:test';

import worker from './worker.js';

const env = { PATH_PREFIX: '/k7q2', ORIGIN_URL: 'https://server.example', PROXY_SECRET: 'the-proxy-secret' };

// Every request the Worker sends on, in place of the network.
let forwarded;
const realFetch = globalThis.fetch;
beforeEach(() => {
  forwarded = [];
  globalThis.fetch = async (url, init) => {
    forwarded.push({ url: String(url), method: init.method, headers: new Headers(init.headers) });
    return new Response(null, { status: 204 });
  };
});
afterEach(() => {
  globalThis.fetch = realFetch;
});

const visit = (path, headers = {}, cf) => {
  const request = new Request('https://shop.example' + path, {
    headers: { 'cf-connecting-ip': '203.0.113.9', ...headers },
  });
  if (cf) Object.defineProperty(request, 'cf', { value: cf });
  return worker.fetch(request, env);
};

test('a hit is forwarded with the visitor, the site and the secret', async () => {
  const res = await visit('/k7q2/g/collect?v=2&en=page_view', { cookie: '_ga=GA1.1.1.2', 'user-agent': 'Mozilla/5.0' });
  assert.equal(res.status, 204);
  assert.equal(forwarded.length, 1);
  const { url, headers } = forwarded[0];
  assert.equal(url, 'https://server.example/g/collect?v=2&en=page_view');
  assert.equal(headers.get('x-sst-client-ip'), '203.0.113.9');
  assert.equal(headers.get('x-forwarded-host'), 'shop.example');
  assert.equal(headers.get('x-forwarded-prefix'), '/k7q2');
  assert.equal(headers.get('x-proxy-secret'), 'the-proxy-secret');
  // What the visitor's browser sent for the tags is kept.
  assert.equal(headers.get('cookie'), '_ga=GA1.1.1.2');
  assert.equal(headers.get('user-agent'), 'Mozilla/5.0');
});

test('the admin console and the counters are not served on the site', async () => {
  for (const path of ['/k7q2/admin', '/k7q2/admin/', '/k7q2/admin/api/overview', '/k7q2/Admin/api/traces', '/k7q2/system', '/k7q2/system?x=1']) {
    const res = await visit(path, { authorization: 'Bearer guess', 'x-debug-token': 'guess' });
    assert.equal(res.status, 404, path);
  }
  assert.equal(forwarded.length, 0);
  // A tracking path that only starts alike still goes through.
  assert.equal((await visit('/k7q2/administrator')).status, 204);
  assert.equal((await visit('/k7q2/systems/x')).status, 204);
  assert.equal(forwarded.length, 2);
});

test('paths outside the prefix are not forwarded', async () => {
  for (const path of ['/', '/admin', '/k7q2x/g/collect', '/g/collect']) {
    assert.equal((await visit(path)).status, 404, path);
  }
  assert.equal(forwarded.length, 0);
});

test("a visitor can't pass for the proxy or claim another address or place", async () => {
  await visit('/k7q2/g/collect?v=2', {
    'x-proxy-secret': 'forged',
    'x-sst-client-ip': '8.8.8.8',
    'x-sst-geo-country': 'ZZ',
    'x-sst-geo-city': 'Nowhere',
    'x-sst-anything': '1',
    'x-forwarded-host': 'other-tenant.example',
    'x-forwarded-prefix': '/evil',
    'x-forwarded-for': '8.8.8.8',
    'x-real-ip': '8.8.8.8',
    'true-client-ip': '8.8.8.8',
    forwarded: 'for=8.8.8.8',
    'x-vercel-ip-city': 'Nowhere',
    'x-vercel-ip-country': 'ZZ',
    'cloudfront-viewer-country': 'ZZ',
    'x-appengine-city': 'nowhere',
    'cf-ipcity': 'Nowhere',
    'cf-region-code': 'ZZ',
    'cf-postal-code': '00000',
    'cf-ipcountry': 'DE',
  });
  const { headers } = forwarded[0];
  assert.equal(headers.get('x-proxy-secret'), 'the-proxy-secret');
  assert.equal(headers.get('x-sst-client-ip'), '203.0.113.9');
  assert.equal(headers.get('x-forwarded-host'), 'shop.example');
  assert.equal(headers.get('x-forwarded-prefix'), '/k7q2');
  for (const gone of [
    'x-sst-geo-country', 'x-sst-geo-city', 'x-sst-anything', 'x-forwarded-for', 'x-real-ip', 'true-client-ip',
    'forwarded', 'x-vercel-ip-city', 'x-vercel-ip-country', 'cloudfront-viewer-country', 'x-appengine-city',
    'cf-ipcity', 'cf-region-code', 'cf-postal-code',
  ]) {
    assert.equal(headers.get(gone), null, gone);
  }
  // Cloudflare's own headers are its word, not the visitor's.
  assert.equal(headers.get('cf-connecting-ip'), '203.0.113.9');
  assert.equal(headers.get('cf-ipcountry'), 'DE');
});

test("the visitor's place comes from Cloudflare", async () => {
  await visit('/k7q2/g/collect?v=2', { 'x-sst-geo-city': 'Nowhere' }, { country: 'TR', regionCode: '34', city: 'İstanbul', postalCode: '34000' });
  const { headers } = forwarded[0];
  assert.equal(headers.get('x-sst-geo-country'), 'TR');
  assert.equal(headers.get('x-sst-geo-region'), '34');
  assert.equal(headers.get('x-sst-geo-city'), encodeURIComponent('İstanbul'));
  assert.equal(headers.get('x-sst-geo-postal-code'), '34000');
});

test('without a path prefix nothing is served', async () => {
  const res = await worker.fetch(new Request('https://shop.example/k7q2/g/collect'), { ...env, PATH_PREFIX: '' });
  assert.equal(res.status, 500);
  assert.equal(forwarded.length, 0);
});
