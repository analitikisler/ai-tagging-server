// X (Twitter) Conversions API tag: sends GA4 events to X from the server.
// Settings (tag `data`): pixel_id, event_ids ("purchase=tw-abc12-def34, ...":
// each GA4 event with the id of the X event it counts for, from Events
// Manager), consumer_key, consumer_secret, access_token, access_token_secret
// (the developer app's keys and its access token, which signs every request
// with OAuth 1.0a). GA4 events with no X event are skipped, and so are events
// whose Consent Mode state doesn't grant both ad_storage and ad_user_data, and
// visitors X has no way to match (no twclid, email or phone). X deduplicates
// against the browser pixel by `conversion_id`: the GA4 `event_id`.
const generateRandom = require('generateRandom');
const API = 'https://ads-api.twitter.com/12/measurement/conversions/';

if (
  !data.pixel_id ||
  !data.event_ids ||
  !data.consumer_key ||
  !data.consumer_secret ||
  !data.access_token ||
  !data.access_token_secret
) {
  data.gtmOnFailure();
  return;
}

const eventName = getEventData('event_name');
const xEvent = eventMap(data.event_ids)[eventName];
if (!xEvent) {
  skip('unmapped_event');
  return;
}
const blocked = adConsentBlock();
if (blocked) {
  skip(blocked);
  return;
}

// UTF-8 bytes of a string.
const bytesOf = (s) => {
  const out = [];
  for (let i = 0; i < s.length; i++) {
    let c = s.charCodeAt(i);
    if (c >= 0xd800 && c < 0xdc00 && i + 1 < s.length) {
      c = 0x10000 + ((c - 0xd800) << 10) + (s.charCodeAt(++i) - 0xdc00);
    }
    if (c < 0x80) out.push(c);
    else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 63));
    else if (c < 0x10000) out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
    else out.push(0xf0 | (c >> 18), 0x80 | ((c >> 12) & 63), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
  }
  return out;
};

// SHA-1 of a byte list, as a byte list. The sandbox only has SHA-256, and
// OAuth 1.0a signs with HMAC-SHA1.
const sha1 = (msg) => {
  const m = msg.slice();
  const bits = m.length * 8;
  m.push(0x80);
  while (m.length % 64 !== 56) m.push(0);
  for (let i = 7; i >= 0; i--) m.push(i > 3 ? 0 : (bits >>> (i * 8)) & 255);
  let h0 = 0x67452301;
  let h1 = 0xefcdab89;
  let h2 = 0x98badcfe;
  let h3 = 0x10325476;
  let h4 = 0xc3d2e1f0;
  const rotl = (x, n) => (x << n) | (x >>> (32 - n));
  const w = [];
  for (let off = 0; off < m.length; off += 64) {
    for (let i = 0; i < 16; i++) {
      w[i] = (m[off + 4 * i] << 24) | (m[off + 4 * i + 1] << 16) | (m[off + 4 * i + 2] << 8) | m[off + 4 * i + 3];
    }
    for (let i = 16; i < 80; i++) w[i] = rotl(w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16], 1);
    let a = h0;
    let b = h1;
    let c = h2;
    let d = h3;
    let e = h4;
    for (let i = 0; i < 80; i++) {
      let f;
      let k;
      if (i < 20) {
        f = (b & c) | (~b & d);
        k = 0x5a827999;
      } else if (i < 40) {
        f = b ^ c ^ d;
        k = 0x6ed9eba1;
      } else if (i < 60) {
        f = (b & c) | (b & d) | (c & d);
        k = 0x8f1bbcdc;
      } else {
        f = b ^ c ^ d;
        k = 0xca62c1d6;
      }
      const t = (rotl(a, 5) + f + e + k + w[i]) | 0;
      e = d;
      d = c;
      c = rotl(b, 30);
      b = a;
      a = t;
    }
    h0 = (h0 + a) | 0;
    h1 = (h1 + b) | 0;
    h2 = (h2 + c) | 0;
    h3 = (h3 + d) | 0;
    h4 = (h4 + e) | 0;
  }
  const out = [];
  [h0, h1, h2, h3, h4].forEach((h) => out.push((h >>> 24) & 255, (h >>> 16) & 255, (h >>> 8) & 255, h & 255));
  return out;
};

const hmacSha1 = (key, text) => {
  let k = bytesOf(key);
  if (k.length > 64) k = sha1(k);
  while (k.length < 64) k.push(0);
  const inner = sha1(k.map((b) => b ^ 0x36).concat(bytesOf(text)));
  return sha1(k.map((b) => b ^ 0x5c).concat(inner));
};

const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
const base64 = (bytes) => {
  let out = '';
  for (let i = 0; i < bytes.length; i += 3) {
    const n = (bytes[i] << 16) | ((bytes[i + 1] || 0) << 8) | (bytes[i + 2] || 0);
    out += B64.charAt((n >> 18) & 63) + B64.charAt((n >> 12) & 63);
    out += i + 1 < bytes.length ? B64.charAt((n >> 6) & 63) : '=';
    out += i + 2 < bytes.length ? B64.charAt(n & 63) : '=';
  }
  return out;
};

// RFC 3986 percent-encoding, which OAuth 1.0a requires.
const pct = (s) =>
  encodeUriComponent(makeString(s))
    .split('!').join('%21')
    .split("'").join('%27')
    .split('(').join('%28')
    .split(')').join('%29')
    .split('*').join('%2A');

// The Authorization header for a POST with a JSON body (which isn't signed).
const oauthHeader = (url, nonce, timestamp) => {
  const params = {
    oauth_consumer_key: data.consumer_key,
    oauth_nonce: nonce,
    oauth_signature_method: 'HMAC-SHA1',
    oauth_timestamp: timestamp,
    oauth_token: data.access_token,
    oauth_version: '1.0',
  };
  const keys = Object.keys(params).sort();
  const base = 'POST&' + pct(url) + '&' + pct(keys.map((k) => pct(k) + '=' + pct(params[k])).join('&'));
  params.oauth_signature = base64(hmacSha1(pct(data.consumer_secret) + '&' + pct(data.access_token_secret), base));
  return 'OAuth ' + Object.keys(params).sort().map((k) => pct(k) + '="' + pct(params[k]) + '"').join(', ');
};

// ISO 8601 in UTC, such as 2026-10-15T18:15:24.000Z.
const isoTime = (ms) => {
  const pad = (n, w) => {
    let s = makeString(n);
    while (s.length < w) s = '0' + s;
    return s;
  };
  const days = Math.floor(ms / 86400000);
  const rest = ms - days * 86400000;
  // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
  const z = days + 719468;
  const era = Math.floor(z / 146097);
  const doe = z - era * 146097;
  const yoe = Math.floor((doe - Math.floor(doe / 1460) + Math.floor(doe / 36524) - Math.floor(doe / 146096)) / 365);
  const doy = doe - (365 * yoe + Math.floor(yoe / 4) - Math.floor(yoe / 100));
  const mp = Math.floor((5 * doy + 2) / 153);
  const day = doy - Math.floor((153 * mp + 2) / 5) + 1;
  const month = mp < 10 ? mp + 3 : mp - 9;
  const year = yoe + era * 400 + (month <= 2 ? 1 : 0);
  return (
    pad(year, 4) + '-' + pad(month, 2) + '-' + pad(day, 2) + 'T' +
    pad(Math.floor(rest / 3600000), 2) + ':' + pad(Math.floor(rest / 60000) % 60, 2) + ':' +
    pad(Math.floor(rest / 1000) % 60, 2) + '.' + pad(rest % 1000, 3) + 'Z'
  );
};

// The X pixel keeps the ad click's ?twclid in a _twclid cookie, as JSON.
const cookieClickId = () => {
  const raw = getCookieValues('_twclid')[0];
  if (!raw) return undefined;
  if (raw.charAt(0) !== '{') return raw;
  const parsed = JSON.parse(raw);
  return parsed && parsed.twclid ? makeString(parsed.twclid) : undefined;
};

fetchExtra((x) => {
  const identifiers = [];
  const twclid = queryParam('twclid') || cookieClickId();
  if (twclid) identifiers.push({ twclid: twclid });
  const email = hash(x.email);
  if (email) identifiers.push({ hashed_email: email });
  const phone = hashPhone(x.phone);
  if (phone) identifiers.push({ hashed_phone_number: phone });
  if (!identifiers.length) {
    skip('no_user_id');
    return;
  }
  // IP and user agent only help next to one of the identifiers above.
  const ipUa = clean({ ip_address: getEventData('ip_override'), user_agent: getEventData('user_agent') });
  if (ipUa.ip_address && ipUa.user_agent) identifiers.push(ipUa);
  const items = eventItems();
  const conversion = clean({
    conversion_time: isoTime(now),
    event_id: xEvent,
    identifiers: identifiers,
    conversion_id: getEventData('event_id') || getEventData('transaction_id'),
    value: numberOrUndefined(getEventData('value')),
    price_currency: getEventData('currency'),
    number_items: items.list.length ? items.total : undefined,
    contents: items.list.length
      ? items.list.map((i) =>
          clean({ content_id: i.id, content_name: i.name, content_price: i.price, num_items: i.quantity })
        )
      : undefined,
  });
  const url = API + encodeUriComponent(data.pixel_id);
  const nonce = makeString(now) + generateRandom(100000000, 999999999) + generateRandom(100000000, 999999999);
  sendHttpRequest(
    url,
    {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        authorization: oauthHeader(url, nonce, makeString(Math.floor(now / 1000))),
      },
    },
    JSON.stringify({ conversions: [conversion] })
  ).then(finish, data.gtmOnFailure);
});
