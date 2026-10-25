// Google scripts client: serves gtm.js and gtag.js from this server, so the
// page loads them from its own domain instead of www.googletagmanager.com.
// Browsers and blockers that refuse Google's domain (Safari private
// browsing among them) then still load the tags.
//
// Fields:
//   allowedIds    Container and tag ids this server will serve, for example
//                 ["GTM-ABC123", "G-XYZ789"]. Requests for any other id are
//                 left unclaimed, so the server can't be used as an open proxy.
//   cacheSeconds  How long a fetched script is reused. Default 900.
//
// gtm.js and gtag.js load further scripts (gtag.js for a GA4 tag inside a
// container, gtag/destination, the preview bootstrap) from the host in their
// "3" setting, www.googletagmanager.com. That host is rewritten to this
// server's public address, the forwarded host plus the proxy's path prefix
// (x-forwarded-prefix; none when the server runs at the root), so those
// scripts come through here too.
const claimRequest = require('claimRequest');
const getRequestHeader = require('getRequestHeader');
const getRequestMethod = require('getRequestMethod');
const getRequestPath = require('getRequestPath');
const getRequestQueryParameter = require('getRequestQueryParameter');
const getRequestQueryString = require('getRequestQueryString');
const getTimestampMillis = require('getTimestampMillis');
const JSON = require('JSON');
const returnResponse = require('returnResponse');
const sendHttpGet = require('sendHttpGet');
const setResponseBody = require('setResponseBody');
const setResponseHeader = require('setResponseHeader');
const setResponseStatus = require('setResponseStatus');
const templateDataStorage = require('templateDataStorage');

const UPSTREAM = {
  '/gtm.js': 'https://www.googletagmanager.com/gtm.js',
  '/gtag/js': 'https://www.googletagmanager.com/gtag/js',
  '/gtag/destination': 'https://www.googletagmanager.com/gtag/destination',
  '/debug/bootstrap': 'https://www.googletagmanager.com/debug/bootstrap',
};
// When gtm.js runs first-party it opens Google's service worker helper on this
// path (/_/service_worker/<version>/sw_iframe.html, which registers sw.js next
// to it). Only those two files are served, fetched without the query string:
// the page reads its parameters itself.
const SERVICE_WORKER = /^\/_\/service_worker\/([0-9a-z]{1,16})\/(sw_iframe\.html|sw\.js)$/;
const MAX_CACHED = 20;
// Query parameters Google's script URLs use. Others, and values that aren't
// short plain tokens, are dropped and the rest sorted, so random parameters
// can't bypass the cache and make every request go to Google.
const KEPT_PARAMS = ['id', 'l', 'cx', 'gtm', 'gtm_auth', 'gtm_preview', 'gtm_cookies_win', 'src', 'cond'];

if (getRequestMethod() !== 'GET') return;
const path = getRequestPath();
const worker = SERVICE_WORKER.exec(path);
let url;
if (worker) {
  url = 'https://www.googletagmanager.com/static/service_worker/' + worker[1] + '/' + worker[2];
} else {
  const upstream = UPSTREAM[path];
  if (!upstream) return;
  const id = getRequestQueryParameter('id') || '';
  if ((data.allowedIds || []).indexOf(id) < 0) return;
  const query = getRequestQueryString()
    .split('&')
    .filter((p) => {
      const kv = p.split('=');
      return KEPT_PARAMS.indexOf(kv[0]) >= 0 && kv.length === 2 && /^[A-Za-z0-9_.:-]{1,64}$/.test(kv[1]);
    })
    .sort()
    .join('&');
  url = upstream + '?' + query;
}
claimRequest();
const contentType = path.slice(-5) === '.html' ? 'text/html' : 'application/javascript';
const ttl = (data.cacheSeconds || 900) * 1000;
const now = getTimestampMillis();

const host = getRequestHeader('host');
const scriptHost = host ? host + (getRequestHeader('x-forwarded-prefix') || '') : '';
const firstParty = (body) =>
  scriptHost
    ? body.split('"3":"www.googletagmanager.com"').join('"3":' + JSON.stringify(scriptHost))
    : body;

const respond = (status, body) => {
  setResponseStatus(status);
  if (status === 200) {
    setResponseHeader('content-type', contentType + '; charset=utf-8');
    // Only this site may frame the service worker page.
    if (contentType === 'text/html') {
      setResponseHeader('content-security-policy', "frame-ancestors 'self'");
    }
    setResponseHeader('cache-control', 'public, max-age=' + (ttl / 1000));
  }
  setResponseBody(status === 200 ? firstParty(body || '') : '');
  returnResponse();
};

const cached = templateDataStorage.getItemCopy(url);
if (cached && now - cached.at < ttl) {
  respond(200, cached.body);
} else {
  sendHttpGet(url, { timeout: 5000 }).then((res) => {
    if (res.statusCode === 200) {
      // Query strings vary (gtag.js adds version parameters), so keep the
      // number of cached scripts bounded.
      const keys = templateDataStorage.getItemCopy('keys') || [];
      if (keys.indexOf(url) < 0) {
        if (keys.length >= MAX_CACHED) {
          templateDataStorage.clear();
          keys.length = 0;
        }
        keys.push(url);
        templateDataStorage.setItemCopy('keys', keys);
      }
      templateDataStorage.setItemCopy(url, { at: now, body: res.body });
      respond(200, res.body);
    } else if (cached) {
      respond(200, cached.body);
    } else {
      respond(res.statusCode || 502, '');
    }
  }, () => {
    if (cached) respond(200, cached.body);
    else respond(502, '');
  });
}
