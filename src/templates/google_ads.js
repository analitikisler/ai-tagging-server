// Google Ads conversions tag: sends conversions to Google Ads through the
// Data Manager API (events:ingest), Google's route for conversion imports
// since the Google Ads API's UploadClickConversions was retired for new
// developer tokens in June 2026.
//
// It does two jobs on every event:
// - Keeps the ad click: when the page URL has ?gclid, ?gbraid or ?wbraid,
//   it saves the click id in a first-party cookie, `_sst_ads`, for 90 days.
//   This is what sGTM's Conversion Linker does.
// - Sends the conversion: for the conversion event (`purchase` by default)
//   it sends the click id (from that cookie, the page URL, or Google's own
//   `_gcl_aw` and `_gcl_gb` cookies) and, as enhanced conversions, hashed
//   user data, with the value, currency and transaction id.
// The click is kept only when Consent Mode grants ad_storage, and a conversion
// is sent only when it grants ad_user_data too; otherwise the tag is skipped
// with the reason (send_without_consent lets hits without signals through).
//
// Settings (tag `data`): customer_id (the Google Ads account the conversion
// action is in, digits), conversion_action_id, login_customer_id (optional:
// a manager account the login goes through), client_id, client_secret and
// refresh_token (an OAuth client and a refresh token with the
// https://www.googleapis.com/auth/datamanager scope), conversion_event
// (optional, default purchase), test_mode (optional: validates without
// importing).

const setCookie = require('setCookie');

const COOKIE = '_sst_ads';
const COOKIE_DAYS = 90;
const TOKEN_KEY = 'google_ads_token';

// Dashes and spaces are common in pasted account ids.
const digits = (v) => makeString(v || '').replace(/[^0-9]/g, '');

// ISO 8601 in UTC, from milliseconds; the sandbox has no Date.
const iso = (ms) => {
  const s = Math.floor(ms / 1000);
  const days = Math.floor(s / 86400);
  const sec = s - days * 86400;
  const z = days + 719468;
  const era = Math.floor(z / 146097);
  const doe = z - era * 146097;
  const yoe = Math.floor((doe - Math.floor(doe / 1460) + Math.floor(doe / 36524) - Math.floor(doe / 146096)) / 365);
  const doy = doe - (365 * yoe + Math.floor(yoe / 4) - Math.floor(yoe / 100));
  const mp = Math.floor((5 * doy + 2) / 153);
  const d = doy - Math.floor((153 * mp + 2) / 5) + 1;
  const m = mp < 10 ? mp + 3 : mp - 9;
  const y = yoe + era * 400 + (m <= 2 ? 1 : 0);
  const p = (n) => (n < 10 ? '0' : '') + n;
  return y + '-' + p(m) + '-' + p(d) + 'T' + p(Math.floor(sec / 3600)) + ':' + p(Math.floor((sec % 3600) / 60)) + ':' + p(sec % 60) + '+00:00';
};

// The click is kept only with ad_storage granted (or no signal, with the
// tag's send_without_consent setting on).
const consent = consentState();
const storage = consent.ad_storage;
if (storage === 'denied' || (storage === undefined && data.send_without_consent !== 'true')) {
  skip('consent:ad_storage=' + (storage || 'missing'));
  return;
}

// 1. Keep the click from this page's URL.
const KINDS = ['gclid', 'gbraid', 'wbraid'];
// Click ids are letters, digits, _ - and .; anything else isn't one, and
// mustn't reach the cookie, where ; would start a new attribute.
const CLICK_ID = /^[A-Za-z0-9_.-]{1,512}$/;
let fromUrl;
KINDS.forEach((k) => {
  const v = makeString(queryParam(k) || '');
  if (!fromUrl && CLICK_ID.test(v)) fromUrl = { kind: k, id: v };
});
if (fromUrl) {
  setCookie(COOKIE, 'v1.' + fromUrl.kind + '.' + Math.floor(now / 1000) + '.' + fromUrl.id, {
    domain: 'auto',
    path: '/',
    'max-age': COOKIE_DAYS * 86400,
    secure: true,
    sameSite: 'Lax',
  }, true);
}

const conversionEvent = data.conversion_event || 'purchase';
if (getEventData('event_name') !== conversionEvent) {
  data.gtmOnSuccess();
  return;
}
if (missing(['customer_id', 'conversion_action_id', 'client_id', 'client_secret', 'refresh_token'])) {
  return;
}
// Sending the conversion also needs ad_user_data.
const blocked = adConsentBlock();
if (blocked) {
  skip(blocked);
  return;
}

// 2. The click: this page, then our cookie, then Google's web cookies
// (`GCL.<seconds>.<id>`).
const click = () => {
  if (fromUrl) return fromUrl;
  const ours = getCookieValues(COOKIE)[0];
  if (ours) {
    const parts = ours.split('.');
    if (parts.length >= 4 && parts[0] === 'v1' && KINDS.indexOf(parts[1]) >= 0) {
      return { kind: parts[1], id: parts.slice(3).join('.') };
    }
  }
  const gcl = (name, kind) => {
    const v = getCookieValues(name)[0];
    const parts = v ? v.split('.') : [];
    return parts.length >= 3 ? { kind: kind, id: parts.slice(2).join('.') } : undefined;
  };
  return gcl('_gcl_aw', 'gclid') || gcl('_gcl_gb', 'gbraid');
};

// Enhanced conversions: Google wants e-mail trimmed and lower-cased, with the
// dots before @ taken out for Gmail, and phone numbers in E.164 (with +).
const googleEmail = (v) => {
  if (v === undefined || v === null || v === '') return undefined;
  let s = makeString(v).trim().toLowerCase();
  if (isHash(s)) return s;
  const at = s.indexOf('@');
  if (at > 0) {
    const domain = s.substring(at + 1);
    if (domain === 'gmail.com' || domain === 'googlemail.com') s = s.substring(0, at).split('.').join('') + '@' + domain;
  }
  return sha256Sync(s, { outputEncoding: 'hex' });
};
const googlePhone = (v) => {
  if (v === undefined || v === null || v === '') return undefined;
  const s = makeString(v).trim();
  if (isHash(s.toLowerCase())) return s.toLowerCase();
  const d = s.replace(/[^0-9]/g, '');
  return d ? sha256Sync('+' + d, { outputEncoding: 'hex' }) : undefined;
};

// An access token from the refresh token, kept until a minute before it ends.
const withToken = (then, fail) => {
  const cached = templateDataStorage.getItemCopy(TOKEN_KEY);
  if (cached && cached.client === data.client_id && cached.until > now + 60000) {
    then(cached.token);
    return;
  }
  const body = 'grant_type=refresh_token&client_id=' + encodeUriComponent(data.client_id) +
    '&client_secret=' + encodeUriComponent(data.client_secret) +
    '&refresh_token=' + encodeUriComponent(data.refresh_token);
  sendHttpRequest('https://oauth2.googleapis.com/token', { method: 'POST', headers: { 'content-type': 'application/x-www-form-urlencoded' }, timeout: 3000 }, body).then(
    (res) => {
      const j = res.statusCode === 200 ? JSON.parse(res.body) : undefined;
      if (!j || !j.access_token) {
        logToConsole('not sent: Google refused the refresh token: ' + makeString(res.body).substring(0, 200));
        fail();
        return;
      }
      templateDataStorage.setItemCopy(TOKEN_KEY, { client: data.client_id, token: j.access_token, until: now + makeNumber(j.expires_in || 3600) * 1000 });
      then(j.access_token);
    },
    fail
  );
};

fetchExtra((x) => {
  const c = click();
  const ids = [];
  const email = googleEmail(x.email);
  const phone = googlePhone(x.phone);
  if (email) ids.push({ emailAddress: email });
  if (phone) ids.push({ phoneNumber: phone });
  // An address counts only whole: both names, the country and the postcode.
  const country = x.country ? makeString(x.country).trim().toUpperCase() : '';
  if (x.first_name && x.last_name && country.length === 2 && x.zip) {
    ids.push({ address: { givenName: hash(x.first_name), familyName: hash(x.last_name), regionCode: country, postalCode: makeString(x.zip).trim() } });
  }
  if (!c && !ids.length) {
    logToConsole('not sent: no ad click and no user data to match the conversion with');
    data.gtmOnSuccess();
    return;
  }
  const adIdentifiers = {};
  if (c) adIdentifiers[c.kind] = c.id;
  const transactionId = getEventData('transaction_id') || getEventData('event_id') || (makeString(getEventData('client_id') || 'x') + '.' + now);
  const event = clean({
    adIdentifiers: c ? adIdentifiers : undefined,
    conversionValue: numberOrUndefined(getEventData('value')),
    currency: getEventData('currency') ? makeString(getEventData('currency')).toUpperCase() : undefined,
    eventTimestamp: iso(now),
    transactionId: makeString(transactionId),
    eventSource: 'WEB',
    userData: ids.length ? { userIdentifiers: ids } : undefined,
  });
  const destination = {
    operatingAccount: { accountType: 'GOOGLE_ADS', accountId: digits(data.customer_id) },
    productDestinationId: digits(data.conversion_action_id),
  };
  if (data.login_customer_id) destination.loginAccount = { accountType: 'GOOGLE_ADS', accountId: digits(data.login_customer_id) };
  const payload = {
    destinations: [destination],
    encoding: 'HEX',
    consent: { adUserData: 'CONSENT_GRANTED', adPersonalization: 'CONSENT_GRANTED' },
    events: [event],
  };
  if (data.test_mode === true || data.test_mode === 'true') payload.validateOnly = true;
  withToken((token) => {
    sendHttpRequest('https://datamanager.googleapis.com/v1/events:ingest', {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: 'Bearer ' + token },
    }, JSON.stringify(payload)).then((res) => {
      if (res.statusCode >= 200 && res.statusCode < 300) {
        const j = JSON.parse(res.body) || {};
        if (j.fieldWarnings && j.fieldWarnings.length) logToConsole('Google warned: ' + JSON.stringify(j.fieldWarnings).substring(0, 300));
        data.gtmOnSuccess();
      } else {
        logToConsole('Google answered ' + res.statusCode + ': ' + makeString(res.body).substring(0, 300));
        data.gtmOnFailure();
      }
    }, data.gtmOnFailure);
  }, data.gtmOnFailure);
});
