// Edge proxy: serves the tagging server on your own domain.
//
// https://example.com/k7q2/g/collect?... is forwarded to
// ORIGIN_URL/g/collect?..., with the visitor's IP, the site's host, the path
// prefix and the shared secret added. The server refuses requests without
// the secret, so it can only be reached through this proxy.
//
// With the RATE_LIMITER binding (wrangler.toml), each visitor IP gets a fixed
// number of requests per period; the rest are answered 429 here and never
// reach the server.
export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    // A short path of your own, e.g. /k7q2. Words like analytics, metrics,
    // tracking, gtm or ad in it get it blocked sooner.
    const prefix = env.PATH_PREFIX;
    if (!prefix || !prefix.startsWith('/')) {
      return new Response('set PATH_PREFIX, e.g. /k7q2', { status: 500 });
    }
    if (url.pathname !== prefix && !url.pathname.startsWith(prefix + '/')) {
      return new Response('not found', { status: 404 });
    }

    const ip = request.headers.get('cf-connecting-ip') || '';
    if (env.RATE_LIMITER) {
      const { success } = await env.RATE_LIMITER.limit({ key: ip });
      if (!success) {
        return new Response('too many requests', { status: 429, headers: { 'retry-after': '10' } });
      }
    }

    const target = new URL(env.ORIGIN_URL);
    target.pathname = url.pathname.slice(prefix.length) || '/';
    target.search = url.search;

    const headers = new Headers(request.headers);
    headers.set('x-forwarded-host', url.host);
    headers.set('x-forwarded-prefix', prefix);
    headers.set('x-sst-client-ip', ip);
    headers.set('x-proxy-secret', env.PROXY_SECRET);

    // The visitor's location from Cloudflare, for the GA4 client's
    // visitorInfo. Values a client sent under these names are dropped first.
    const geo = {
      'x-sst-geo-country': request.cf?.country,
      'x-sst-geo-region': request.cf?.regionCode,
      'x-sst-geo-city': request.cf?.city && encodeURIComponent(request.cf.city),
      'x-sst-geo-postal-code': request.cf?.postalCode,
    };
    for (const [name, value] of Object.entries(geo)) {
      headers.delete(name);
      if (value) headers.set(name, value);
    }

    const hasBody = request.method !== 'GET' && request.method !== 'HEAD';
    return fetch(target, {
      method: request.method,
      headers,
      body: hasBody ? request.body : undefined,
      redirect: 'manual',
    });
  },
};
