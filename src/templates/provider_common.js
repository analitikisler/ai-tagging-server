// Shared by the ad-platform tags (Meta, TikTok, Pinterest, Snapchat, Reddit,
// LinkedIn, X). The server puts
// this in front of each provider template, so the names below are in scope
// there. Tag `data` holds the provider's settings, plus `data.enrich` when an
// enrichment endpoint is configured and `data.eventUserData` when the event's
// own user data may be used.
const getEventData = require('getEventData');
const getCookieValues = require('getCookieValues');
const sendHttpRequest = require('sendHttpRequest');
const sha256Sync = require('sha256Sync');
const getTimestampMillis = require('getTimestampMillis');
const encodeUriComponent = require('encodeUriComponent');
const parseUrl = require('parseUrl');
const makeNumber = require('makeNumber');
const makeString = require('makeString');
const templateDataStorage = require('templateDataStorage');
const logToConsole = require('logToConsole');
const JSON = require('JSON');
const Math = require('Math');
const Object = require('Object');

// A value that is already a SHA-256 hex digest is sent as it is; anything
// else is trimmed and lower-cased first, as the platforms require.
const isHash = (v) => /^[a-f0-9]{64}$/.test(makeString(v));
const hash = (v) => {
  if (v === undefined || v === null || v === '') return undefined;
  const s = makeString(v).trim();
  return isHash(s.toLowerCase()) ? s.toLowerCase() : sha256Sync(s.toLowerCase(), { outputEncoding: 'hex' });
};
// Phone numbers are hashed as digits only, country code first.
const hashPhone = (v) => {
  if (v === undefined || v === null || v === '') return undefined;
  const s = makeString(v).trim();
  if (isHash(s.toLowerCase())) return s.toLowerCase();
  const digits = s.replace(/[^0-9]/g, '');
  return digits ? sha256Sync(digits, { outputEncoding: 'hex' }) : undefined;
};
const clean = (obj) => {
  const out = {};
  Object.keys(obj).forEach((k) => {
    if (obj[k] !== undefined && obj[k] !== null && obj[k] !== '') out[k] = obj[k];
  });
  return out;
};
const listOf = (v) => (v === undefined ? undefined : [v]);

// A setting that pairs GA4 event names with the platform's own ids, such as
// "purchase=123, generate_lead=456", as an object.
const eventMap = (text) => {
  const out = {};
  makeString(text || '')
    .split(',')
    .forEach((pair) => {
      const at = pair.indexOf('=');
      const k = at > 0 ? pair.substring(0, at).trim() : '';
      const v = at > 0 ? pair.substring(at + 1).trim() : '';
      if (k && v) out[k] = v;
    });
  return out;
};

// Consent Mode v2. `gcd` carries a letter per signal, in the order
// ad_storage, analytics_storage, ad_user_data, ad_personalization (for
// example "13r3r3r2r5l1"); each letter says whether the signal was granted or
// denied by default and after the banner. `gcs` ("G1" + ad storage + analytics
// storage, such as "G101") only knows the first two and is used when `gcd` is
// missing. Each signal is 'granted', 'denied' or undefined (not sent).
const GCD_SIGNALS = ['ad_storage', 'analytics_storage', 'ad_user_data', 'ad_personalization'];
const GCD_GRANTED = 'tnrv';
const GCD_DENIED = 'pqmu';
const consentState = () => {
  const out = {};
  const gcd = getEventData('x-ga-gcd');
  const letters = typeof gcd === 'string' ? gcd.replace(/[^a-z]/g, '') : '';
  GCD_SIGNALS.forEach((name, i) => {
    const c = letters.charAt(i);
    if (c && GCD_GRANTED.indexOf(c) !== -1) out[name] = 'granted';
    else if (c && GCD_DENIED.indexOf(c) !== -1) out[name] = 'denied';
  });
  const gcs = getEventData('x-ga-gcs');
  if (typeof gcs === 'string' && /^G1[01][01]/.test(gcs)) {
    if (!out.ad_storage) out.ad_storage = gcs.charAt(2) === '1' ? 'granted' : 'denied';
    if (!out.analytics_storage) out.analytics_storage = gcs.charAt(3) === '1' ? 'granted' : 'denied';
  }
  return out;
};

// Ad platforms get the visitor's data only with both ad_storage and
// ad_user_data granted. A denied signal always stops the send; a signal the
// hit doesn't carry stops it too, unless the tag's "send_without_consent"
// setting is on (for sites that don't use Consent Mode). Returns the skip
// reason, such as "consent:ad_user_data=denied", or undefined to send.
const adConsentBlock = () => {
  const state = consentState();
  const needed = ['ad_storage', 'ad_user_data'];
  for (let i = 0; i < needed.length; i++) {
    const v = state[needed[i]];
    if (v === 'denied') return 'consent:' + needed[i] + '=denied';
    if (v === undefined && data.send_without_consent !== 'true') return 'consent:' + needed[i] + '=missing';
  }
  return undefined;
};

// Ends the tag without sending, and says why in the log line and counters.
const skip = (reason) => data.gtmOnSkipped(reason);

const now = getTimestampMillis();
const pageUrl = getEventData('page_location');
const queryParam = (name) => {
  const u = pageUrl && parseUrl(pageUrl);
  return u && u.searchParams ? u.searchParams[name] : undefined;
};
const numberOrUndefined = (v) => (v === undefined || v === null || v === '' ? undefined : makeNumber(v));

// The GA4 ecommerce items of this event (`pr1`, `pr2`, … on the hit), each
// with `id`, `name`, `brand`, `category`, `price` and `quantity` (1 when not
// sent), plus their total quantity. Empty when the event has no items.
const eventItems = () => {
  const raw = getEventData('items');
  const list = raw && typeof raw === 'object' && typeof raw.length === 'number' ? raw : [];
  const items = [];
  list.forEach((i) => {
    if (!i || typeof i !== 'object') return;
    const id = i.item_id === undefined ? undefined : makeString(i.item_id);
    const price = numberOrUndefined(i.price);
    const qty = numberOrUndefined(i.quantity);
    items.push({
      id: id,
      name: i.item_name,
      brand: i.item_brand,
      category: i.item_category,
      price: price === price ? price : undefined,
      quantity: qty === qty && qty > 0 ? Math.round(qty) : 1,
    });
  });
  let total = 0;
  items.forEach((i) => (total += i.quantity));
  return { list: items, ids: items.filter((i) => i.id).map((i) => i.id), total: total };
};

// Asks the site's own server who this visitor is: GET <url>?<key>=<id> with
// an optional secret header. It answers JSON such as
// { "email": "a@b.com", "phone": "+49...", "first_name": "...", "last_name": "...",
//   "city": "...", "state": "...", "zip": "...", "country": "de", "external_id": "..." }.
// Values may be plain or already SHA-256 hashed. Answers are kept for five
// minutes in storage the ad tags share, so all of them use one lookup. A slow or failing endpoint never stops the conversion from being
// sent: the tag just goes without the extra fields.
const ENRICH_TTL_MS = 300000;
const lookupExtra = (done) => {
  const cfg = data.enrich;
  const keyName = cfg && cfg.key === 'client_id' ? 'client_id' : 'user_id';
  const id = cfg && getEventData(keyName);
  if (!cfg || !cfg.url || !id) {
    done({});
    return;
  }
  const cacheKey = 'enrich:' + keyName + ':' + id;
  const cached = templateDataStorage.getItemCopy(cacheKey);
  if (cached && now - cached.t < ENRICH_TTL_MS) {
    done(cached.v);
    return;
  }
  const headers = { accept: 'application/json' };
  if (cfg.headerName && cfg.headerValue) headers[cfg.headerName] = cfg.headerValue;
  const url = cfg.url + (cfg.url.indexOf('?') === -1 ? '?' : '&') + keyName + '=' + encodeUriComponent(id);
  sendHttpRequest(url, { method: 'GET', headers: headers, timeout: 2000 }).then(
    (res) => {
      const body = res.statusCode >= 200 && res.statusCode < 300 ? JSON.parse(res.body) : undefined;
      const v = body && typeof body === 'object' ? body : {};
      templateDataStorage.setItemCopy(cacheKey, { t: now, v: v });
      done(v);
    },
    () => done({})
  );
};

// The user data an event carries, in sGTM's `user_data` shape, with the same
// keys as an enrichment answer: email_address, phone_number, external_id,
// address.first_name … and their sha256_ forms. A sha256_ value that isn't
// a hex digest is left out rather than hashed again.
const digest = (v) => {
  const s = v === undefined || v === null ? '' : makeString(v).trim().toLowerCase();
  return isHash(s) ? s : undefined;
};
const eventUserData = () => {
  const u = getEventData('user_data');
  if (!u || typeof u !== 'object') return {};
  let a = u.address || {};
  if (a.length !== undefined) a = a[0] || {};
  if (typeof a !== 'object') a = {};
  return clean({
    email: digest(u.sha256_email_address) || u.email_address || u.email,
    phone: digest(u.sha256_phone_number) || u.phone_number || u.phone,
    external_id: u.external_id,
    first_name: digest(a.sha256_first_name) || a.first_name,
    last_name: digest(a.sha256_last_name) || a.last_name,
    city: a.city,
    state: a.region,
    zip: a.postal_code,
    country: a.country,
  });
};

// The visitor's details for the ad tags. An event a server source made
// (`x-sst-source`, e.g. the Shopify order webhook) carries the buyer's own
// details, which win over the enrichment endpoint's answer. User data a page
// sent is used only with the tag's switch for it (`data.eventUserData`), and
// the endpoint's answer wins over it.
const fetchExtra = (done) => {
  const server = !!getEventData('x-sst-source');
  const own = server || data.eventUserData ? eventUserData() : {};
  lookupExtra((x) => {
    const [weak, strong] = server ? [x || {}, own] : [own, x || {}];
    const out = {};
    Object.keys(weak).forEach((k) => (out[k] = weak[k]));
    Object.keys(strong).forEach((k) => {
      const v = strong[k];
      if (v !== undefined && v !== null && v !== '') out[k] = v;
    });
    done(out);
  });
};

const finish = (res) => {
  if (res.statusCode >= 200 && res.statusCode < 300) data.gtmOnSuccess();
  else data.gtmOnFailure();
};

// A tag without its required settings fails with a log line saying which.
const missing = (names) => {
  const unset = names.filter((n) => !data[n]);
  if (!unset.length) return false;
  logToConsole('not sent: ' + unset.join(' and ') + ' not set');
  data.gtmOnFailure();
  return true;
};
