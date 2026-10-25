// GA4 tag: forwards the event's original hit to Google, adding the visitor's
// IP (_uip) and user agent (_ua), which the browser can't send itself.
// Forwarding the hit as sent means no parameter is lost in a mapping.
// Transformations change event data, not the hit, so their changes are
// carried into it: a parameter whose event value changed is rewritten, one
// whose value was removed is dropped, and a new top-level value is added as
// an event parameter. An event no transformation changed goes as it was.
//
// Fields:
//   endpoint        Collection URL. Default https://www.google-analytics.com/g/collect
//   stripUserData   true drops plain user-provided data (`ep.user_data.*` and
//                   `epn.user_data.*` whose last key isn't `sha256_…`) before
//                   forwarding. SHA-256 digests and the Google tag's `em`
//                   envelope are kept. Default false.
const decodeUriComponent = require('decodeUriComponent');
const encodeUriComponent = require('encodeUriComponent');
const getAllEventData = require('getAllEventData');
const getType = require('getType');
const makeNumber = require('makeNumber');
const makeString = require('makeString');
const sendHttpRequest = require('sendHttpRequest');

const event = getAllEventData();
const raw = event['x-sst-raw-hit'];
// Events that didn't come from a GA4 hit, such as Shopify orders, have
// nothing to forward; the browser already sent its own purchase.
if (!raw) {
  data.gtmOnSkipped('not_a_ga4_hit');
  return;
}

// Plain user data: a user_data parameter whose last key is neither a digest
// (`sha256_…`) nor the tag's own bookkeeping (`_tag_mode`).
const plainUserData = (p) => {
  const key = p.split('=')[0];
  if (key.indexOf('ep.user_data.') !== 0 && key.indexOf('epn.user_data.') !== 0) return false;
  const last = key.split('.').pop();
  return last.indexOf('sha256_') !== 0 && last.indexOf('_') !== 0;
};

// Hit parameters that event data keys come from, as the GA4 client maps them.
const STANDARD = { en: 'event_name', uid: 'user_id', dl: 'page_location', dr: 'page_referrer', dt: 'page_title', cu: 'currency' };
// Event keys the client builds from the hit or the request, never added back.
const BUILT = ['event_name', 'client_id', 'user_id', 'page_location', 'page_referrer', 'page_title', 'language',
  'screen_resolution', 'currency', 'ga_session_id', 'ga_session_number', 'engagement_time_msec', 'ip_override',
  'user_agent', 'user_data'];
const decode = (v) => decodeUriComponent(v.split('+').join(' '));
const scalar = (v) => ['string', 'number', 'boolean'].indexOf(getType(v)) >= 0;
const param = (prefix, name, v) => prefix + name + '=' + encodeUriComponent(makeString(v));
const seen = {};
const synced = [];
raw.split('&').forEach((p) => {
  const at = p.indexOf('=');
  const k = at < 0 ? p : p.substring(0, at);
  const v = at < 0 ? '' : decode(p.substring(at + 1));
  const userData = k.indexOf('ep.user_data.') === 0 || k.indexOf('epn.user_data.') === 0;
  const numeric = k.indexOf('epn.') === 0;
  const name = STANDARD[k] || (userData ? null : k.indexOf('ep.') === 0 ? k.substring(3) : numeric ? k.substring(4) : null);
  if (!name) { synced.push(p); return; }
  seen[name] = true;
  const now = event[name];
  if (now === undefined || now === null) { if (k !== 'en') return; synced.push(p); return; }
  if (!scalar(now) || makeString(now) === v || (numeric && makeNumber(v) === now)) { synced.push(p); return; }
  if (STANDARD[k]) synced.push(k + '=' + encodeUriComponent(makeString(now)));
  else synced.push(param(getType(now) === 'number' ? 'epn.' : 'ep.', name, now));
});
const standardParam = {};
for (const k in STANDARD) standardParam[STANDARD[k]] = k;
for (const key in event) {
  if (seen[key] || key.indexOf('x-') === 0 || !scalar(event[key])) continue;
  if (standardParam[key]) synced.push(standardParam[key] + '=' + encodeUriComponent(makeString(event[key])));
  else if (BUILT.indexOf(key) < 0) synced.push(param(getType(event[key]) === 'number' ? 'epn.' : 'ep.', key, event[key]));
}

const params = synced
  .filter((p) => p.indexOf('_uip=') !== 0 && p.indexOf('_ua=') !== 0)
  .filter((p) => !(data.stripUserData === true && plainUserData(p)));
if (event.ip_override) params.push('_uip=' + encodeUriComponent(event.ip_override));
if (event.user_agent) params.push('_ua=' + encodeUriComponent(event.user_agent));

const endpoint = data.endpoint || 'https://www.google-analytics.com/g/collect';
const headers = {};
if (event.user_agent) headers['user-agent'] = event.user_agent;
if (event.ip_override) headers['x-forwarded-for'] = event.ip_override;

sendHttpRequest(endpoint + '?' + params.join('&'), { method: 'POST', headers: headers }, '')
  .then((res) => {
    if (res.statusCode >= 200 && res.statusCode < 300) data.gtmOnSuccess();
    else data.gtmOnFailure();
  }, data.gtmOnFailure);
