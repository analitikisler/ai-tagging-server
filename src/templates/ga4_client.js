// GA4 client: claims GA4 Measurement Protocol v2 hits sent by gtag.js, runs
// the container once per event and answers 204 when every tag has finished.
//
// Fields:
//   extendGaCookie  Re-set the _ga cookie (and the property's _ga_<id> session
//                   cookie) from the server with a two-year lifetime, so
//                   browsers that cap script-set cookies (Safari) keep the same
//                   client id. On unless set to false. Skipped when the hit says
//                   analytics_storage is denied (Consent Mode gcs=G1x0).
//   keepAdCookies   Re-set Meta's _fbp and _fbc and TikTok's _ttp cookies
//                   from the server with a 390-day lifetime (Safari caps
//                   script-set ones at 7 days), and create _fbc from ?fbclid
//                   on the landing hit. On unless set to false.
//   allowedMeasurementIds
//                   GA4 measurement ids (G-…) this server accepts hits for.
//                   Hits for any other property are left unclaimed, so the
//                   server can't be used to send traffic to someone else's.
//                   ['*'] accepts every id, and so does an empty or missing
//                   list (the server warns about that at startup).
//   siteDomains     Domains whose pages may send hits, subdomains included,
//                   e.g. ['example.com']. Default: the server's own host.
//   originCheck     'enforce', 'log' (default) or 'off'. A browser hit from
//                   this site carries an Origin or Referer on one of the site
//                   domains. With 'enforce' other hits get
//                   403 and are not forwarded; with 'log' they are forwarded
//                   and logged, so you can see what enforcing would drop.
//   keepCookies     Cookie groups to re-set from the server with their
//                   platform's lifetime, so Safari's 7-day cap on cookies set
//                   by scripts doesn't cut them short: google_analytics,
//                   google_ads, meta, tiktok, pinterest, microsoft, snapchat,
//                   linkedin, or a cookie's own name (kept a year, only
//                   when analytics storage isn't denied either). Ad cookies,
//                   these and keepAdCookies' alike, are only re-set when the
//                   hit says ad_storage is granted (Consent Mode gcd, or gcs
//                   when gcd is missing); a hit with no consent signal leaves
//                   them alone.
//   clickIdAliases  Click ids to restore from another parameter name, e.g.
//                   { "gclid": "gc_id" }: a page_location with ?gc_id=… and no
//                   gclid gets gclid=… added. Safari and Brave strip known
//                   click ids from links in some modes; an alias set in the
//                   ad's URL survives.
//   visitorInfo     true adds what the request says about the visitor to
//                   each event, under keys tags can use but that aren't sent
//                   anywhere unless a transformation copies them:
//                   x-sst-country, x-sst-region, x-sst-city,
//                   x-sst-postal-code (from the proxy's location headers),
//                   x-sst-device (mobile, tablet or desktop), x-sst-browser,
//                   x-sst-os, x-sst-ai-source (chatgpt, perplexity, claude…
//                   when the visitor came from an AI assistant) and
//                   x-sst-ai-agent (when an AI agent browses for someone).
//   botHandling     'drop' (default) answers hits from bots 204 without
//                   forwarding them. 'flag' forwards every hit and adds
//                   x-sst-bot (true when x-sst-bot-score is 50 or more),
//                   x-sst-bot-score (0-100) and x-sst-bot-reason, for
//                   triggers to use.
//
// Bots: crawlers, headless browsers, automation tools and HTTP libraries
// (curl, python, Go, Java, Node…), or hits with no user agent. CORS headers
// are only sent back to this site's own origins.
const claimRequest = require('claimRequest');
const decodeUriComponent = require('decodeUriComponent');
const extractEventsFromMpv2 = require('extractEventsFromMpv2');
const getCookieValues = require('getCookieValues');
const getRequestHeader = require('getRequestHeader');
const getRequestQueryParameter = require('getRequestQueryParameter');
const getTimestampMillis = require('getTimestampMillis');
const isRequestMpv2 = require('isRequestMpv2');
const logToConsole = require('logToConsole');
const makeString = require('makeString');
const parseUrl = require('parseUrl');
const returnResponse = require('returnResponse');
const runContainer = require('runContainer');
const setCookie = require('setCookie');
const setResponseHeader = require('setResponseHeader');
const setResponseStatus = require('setResponseStatus');

if (!isRequestMpv2()) return;
const allowedIds = data.allowedMeasurementIds || [];
const anyId = !allowedIds.length || allowedIds.indexOf('*') >= 0;
const tid = getRequestQueryParameter('tid') || '';
if (!anyId && allowedIds.indexOf(tid) < 0) return;
claimRequest();
setResponseHeader('vary', 'origin');

const finish = (status) => {
  setResponseStatus(status);
  returnResponse();
};

// The host part of an absolute URL, lower-cased; '' when there is none.
const hostOf = (url) => {
  const m = /^https?:\/\/([^/?#:]+)/i.exec(url || '');
  return m ? m[1].toLowerCase() : '';
};
const ownHost = (getRequestHeader('host') || '').split(':')[0].toLowerCase();
const siteDomains = (data.siteDomains && data.siteDomains.length ? data.siteDomains : [ownHost])
  .map((d) => d.toLowerCase())
  .filter((d) => !!d);
const isSite = (host) =>
  !!host && siteDomains.some((d) => host === d || host.slice(-d.length - 1) === '.' + d);

const origin = getRequestHeader('origin');
const originHost = hostOf(origin);
const refererHost = hostOf(getRequestHeader('referer'));
if (origin && isSite(originHost)) {
  setResponseHeader('access-control-allow-origin', origin);
  setResponseHeader('access-control-allow-credentials', 'true');
}

const mode = data.originCheck || 'log';
if (mode !== 'off' && !isSite(originHost) && !isSite(refererHost)) {
  logToConsole('origin_check=' + (mode === 'enforce' ? 'rejected' : 'would_reject') +
    ' origin=' + (originHost || '-') + ' referer=' + (refererHost || '-'));
  if (mode === 'enforce') {
    finish(403);
    return;
  }
}

const userAgent = getRequestHeader('user-agent') || '';
// "bot" only as a word or before / - ; so phone names like Cubot still count.
const BOTS = /\bbot\b|bot[\/;-]|crawl|spider|slurp|headless|curl|wget|python|go-http|java\/|okhttp|axios|node-fetch|undici|libwww|httpclient|scrapy|phantomjs|selenium|puppeteer|playwright/i;

// 100 for a known bot or no user agent; otherwise points for what real
// browsers send and this request lacks.
const botCheck = () => {
  if (!userAgent) return { score: 100, reason: 'no_user_agent' };
  if (BOTS.test(userAgent)) return { score: 100, reason: 'user_agent' };
  let score = 0;
  const reasons = [];
  if (!getRequestHeader('accept-language')) {
    score += 40;
    reasons.push('no_accept_language');
  }
  if (/Chrome\/\d/.test(userAgent) && !getRequestHeader('sec-fetch-mode')) {
    score += 40;
    reasons.push('no_fetch_metadata');
  }
  if (/headless/i.test(getRequestHeader('sec-ch-ua') || '')) {
    score += 60;
    reasons.push('headless');
  }
  return { score: score > 100 ? 100 : score, reason: reasons.join(',') };
};
const bot = botCheck();
const flagBots = data.botHandling === 'flag';
if (!flagBots && bot.score >= 100) {
  logToConsole('bot_dropped');
  finish(204);
  return;
}

const gcs = getRequestQueryParameter('gcs') || '';
const analyticsDenied = gcs.length === 4 && gcs.charAt(3) === '0';
// Consent Mode v2: gcd's first letter is ad_storage (t/n/r/v granted,
// p/q/m/u denied); gcs ("G1" + ad storage + analytics storage) is the
// fallback. A hit with no consent signal leaves ad cookies alone.
const adStorageGranted = () => {
  const c = (getRequestQueryParameter('gcd') || '').replace(/[^a-z]/g, '').charAt(0);
  if (c && 'tnrv'.indexOf(c) >= 0) return true;
  if (c && 'pqmu'.indexOf(c) >= 0) return false;
  return /^G11[01]/.test(gcs);
};
const adsGranted = adStorageGranted();
const DAY = 86400;
// Re-sets a cookie from the server, with `value` or the one the browser sent.
const keep = (name, days, value) => {
  const v = value || getCookieValues(name, true)[0];
  if (v) {
    setCookie(name, v, {
      domain: 'auto',
      path: '/',
      'max-age': days * DAY,
      secure: true,
      sameSite: 'Lax',
    }, true);
  }
};
// Each platform's own cookies and how long the platform keeps them.
const COOKIE_GROUPS = {
  google_ads: { days: 90, names: ['_gcl_aw', '_gcl_gb', '_gcl_dc', '_gcl_au'] },
  meta: { days: 390, names: ['_fbp', '_fbc'] },
  tiktok: { days: 390, names: ['_ttp'] },
  pinterest: { days: 365, names: ['_epik', '_pin_unauth'] },
  microsoft: { days: 390, names: ['_uetmsclkid', '_uetvid'] },
  snapchat: { days: 390, names: ['_scid'] },
  linkedin: { days: 30, names: ['li_fat_id'] },
};
// Meta's _fbc: a new ?fbclid on the page replaces it, else it's kept.
const keepFbc = (days) => {
  const page = parseUrl(getRequestQueryParameter('dl') || '');
  const raw = page && page.searchParams ? makeString(page.searchParams.fbclid || '') : '';
  // A click id is letters, digits, _ - and .; anything else (a ; would
  // start a cookie attribute) isn't one.
  const fbclid = /^[A-Za-z0-9_.-]{1,512}$/.test(raw) ? raw : undefined;
  const fbc = getCookieValues('_fbc', true)[0];
  if (fbclid && (!fbc || fbc.split('.').slice(3).join('.') !== fbclid)) {
    keep('_fbc', days, 'fb.1.' + getTimestampMillis() + '.' + fbclid);
  } else if (fbc) {
    keep('_fbc', days, fbc);
  }
};
const keepCookies = data.keepCookies || [];
if ((data.extendGaCookie !== false || keepCookies.indexOf('google_analytics') >= 0) && !analyticsDenied) {
  keep('_ga', 730);
  if (tid.indexOf('G-') === 0) keep('_ga_' + tid.substring(2), 730);
}
// Meta and TikTok are kept unless keepAdCookies is false; keepCookies adds
// the others and cookies by name.
const groups = keepCookies.filter((g) => g !== 'google_analytics');
if (data.keepAdCookies !== false) {
  ['meta', 'tiktok'].forEach((g) => {
    if (groups.indexOf(g) < 0) groups.push(g);
  });
}
groups.forEach((name) => {
  const group = COOKIE_GROUPS[name];
  if (!group) {
    if (adsGranted && !analyticsDenied) keep(name, 365);
  } else if (adsGranted && name === 'meta') {
    keep('_fbp', group.days);
    keepFbc(group.days);
  } else if (adsGranted) {
    group.names.forEach((n) => keep(n, group.days));
  }
});

// page_location with each aliased click id restored, e.g. ?gc_id=X becomes
// ?gc_id=X&gclid=X. Parameters already present are left as they are.
const aliases = data.clickIdAliases || {};
const restoreClickIds = (url) => {
  if (!url) return url;
  const hashAt = url.indexOf('#');
  const base = hashAt < 0 ? url : url.substring(0, hashAt);
  const fragment = hashAt < 0 ? '' : url.substring(hashAt);
  const queryAt = base.indexOf('?');
  if (queryAt < 0) return url;
  const values = {};
  base.substring(queryAt + 1).split('&').forEach((p) => {
    const k = p.split('=')[0];
    if (k && values[k] === undefined) values[k] = p.substring(k.length + 1);
  });
  const added = [];
  for (const real in aliases) {
    const v = values[aliases[real]];
    if (values[real] === undefined && v) added.push(real + '=' + v);
  }
  return added.length ? base + '&' + added.join('&') + fragment : url;
};

// Percent-decoding that keeps the raw value when it isn't valid.
const decode = (v) => {
  try {
    return decodeUriComponent(v);
  } catch (e) {
    return v;
  }
};

// What the request tells about the visitor, for data.visitorInfo.
const header = (name) => (getRequestHeader(name) || '').trim();
const firstHeader = (names, encoded) => {
  for (let i = 0; i < names.length; i++) {
    const v = header(names[i]);
    if (v && v !== 'XX') return encoded ? decode(v) : v;
  }
  return undefined;
};
const ua = userAgent;
const device = /iPad|Tablet|PlayBook|Silk/i.test(ua) || (/Android/i.test(ua) && !/Mobile/i.test(ua))
  ? 'tablet'
  : /Mobi|iPhone|iPod|Android|Windows Phone/i.test(ua) || header('sec-ch-ua-mobile') === '?1' ? 'mobile' : 'desktop';
const browser = /Edg\//.test(ua) ? 'Edge' : /OPR\/|Opera/.test(ua) ? 'Opera' : /SamsungBrowser/.test(ua) ? 'Samsung Internet'
  : /Firefox\/|FxiOS/.test(ua) ? 'Firefox' : /Chrome\/|CriOS/.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : undefined;
const os = /Windows/.test(ua) ? 'Windows' : /iPhone|iPad|iPod/.test(ua) ? 'iOS' : /Android/.test(ua) ? 'Android'
  : /CrOS/.test(ua) ? 'ChromeOS' : /Mac OS X|Macintosh/.test(ua) ? 'macOS' : /Linux/.test(ua) ? 'Linux' : undefined;
// Our edge proxy's headers (city percent-encoded), then the hosts' own.
const geo = {
  'x-sst-country': firstHeader(['x-sst-geo-country', 'cf-ipcountry', 'x-vercel-ip-country', 'cloudfront-viewer-country', 'x-appengine-country']),
  'x-sst-region': firstHeader(['x-sst-geo-region', 'cf-region-code', 'x-vercel-ip-country-region', 'cloudfront-viewer-country-region', 'x-appengine-region']),
  'x-sst-city': firstHeader(['x-sst-geo-city', 'x-vercel-ip-city'], true) || firstHeader(['cf-ipcity', 'cloudfront-viewer-city', 'x-appengine-city']),
  'x-sst-postal-code': firstHeader(['x-sst-geo-postal-code', 'cf-postal-code', 'x-vercel-ip-postal-code', 'cloudfront-viewer-postal-code']),
};
// AI agents browsing for someone, by user agent or a signed agent header.
const AI_AGENTS = [
  ['chatgpt', /ChatGPT-User|ChatGPT Agent|OAI-SearchBot|GPTBot/i],
  ['perplexity', /Perplexity-User|PerplexityBot/i],
  ['claude', /Claude-User|Claude-Web|ClaudeBot|Claude-SearchBot/i],
  ['meta_ai', /meta-externalagent|meta-externalfetcher/i],
  ['mistral', /MistralAI-User/i],
  ['duckduckgo', /DuckAssistBot/i],
  ['amazon', /Amazonbot/i],
  ['bytedance', /Bytespider/i],
];
const signedAgent = header('signature-agent');
let aiAgent;
AI_AGENTS.forEach((a) => {
  if (!aiAgent && a[1].test(ua)) aiAgent = a[0];
});
if (!aiAgent && /chatgpt\.com/i.test(signedAgent)) aiAgent = 'chatgpt';
// AI assistants people come from: the referrer's host, or utm_source.
const AI_SOURCES = {
  'chatgpt.com': 'chatgpt', 'chat.openai.com': 'chatgpt', 'perplexity.ai': 'perplexity', 'claude.ai': 'claude',
  'gemini.google.com': 'gemini', 'bard.google.com': 'gemini', 'copilot.microsoft.com': 'copilot',
  'chat.deepseek.com': 'deepseek', 'meta.ai': 'meta_ai', 'grok.com': 'grok', 'chat.mistral.ai': 'mistral',
  'you.com': 'you', 'poe.com': 'poe',
};
const aiSourceOf = (host) => {
  if (!host) return undefined;
  for (const domain in AI_SOURCES) {
    if (host === domain || host.slice(-domain.length - 1) === '.' + domain) return AI_SOURCES[domain];
  }
  return undefined;
};
const utmSource = (url) => {
  const m = /[?&]utm_source=([^&#]*)/.exec(url || '');
  return m ? decode(m[1]).toLowerCase() : '';
};

// A batched hit's lines can carry their own tid; drop events for other ids.
const events = extractEventsFromMpv2().filter(
  (event) => anyId || allowedIds.indexOf(event['x-ga-measurement_id']) >= 0,
);
events.forEach((event) => {
  event.page_location = restoreClickIds(event.page_location);
  if (data.visitorInfo === true) {
    for (const k in geo) if (geo[k]) event[k] = geo[k];
    event['x-sst-device'] = device;
    if (browser) event['x-sst-browser'] = browser;
    if (os) event['x-sst-os'] = os;
    const source = aiSourceOf(hostOf(event.page_referrer)) || aiSourceOf(utmSource(event.page_location));
    if (source) event['x-sst-ai-source'] = source;
    if (aiAgent) event['x-sst-ai-agent'] = aiAgent;
  }
  if (flagBots) {
    event['x-sst-bot'] = bot.score >= 50;
    event['x-sst-bot-score'] = bot.score;
    if (bot.reason) event['x-sst-bot-reason'] = bot.reason;
  }
});
if (flagBots && bot.score >= 50) logToConsole('bot_flagged score=' + bot.score);
let pending = events.length;

if (pending === 0) finish(204);
events.forEach((event) => {
  runContainer(event, () => {
    pending--;
    if (pending === 0) finish(204);
  });
});
