// TikTok Events API tag: sends GA4 events to TikTok from the server.
// Settings (tag `data`): pixel_code, access_token, test_event_code (optional).
// GA4 events with no TikTok name are skipped, and so are events whose Consent
// Mode state doesn't grant both ad_storage and ad_user_data. TikTok
// deduplicates against the browser pixel by event name plus `event_id`: send
// the same value from both.
const NAMES = {
  view_item: 'ViewContent',
  add_to_cart: 'AddToCart',
  add_to_wishlist: 'AddToWishlist',
  begin_checkout: 'InitiateCheckout',
  add_payment_info: 'AddPaymentInfo',
  purchase: 'CompletePayment',
  generate_lead: 'SubmitForm',
  sign_up: 'CompleteRegistration',
  search: 'Search',
};

if (missing(['pixel_code', 'access_token'])) {
  return;
}

const eventName = getEventData('event_name');
const ttName = NAMES[eventName];
if (!ttName) {
  skip('unmapped_event');
  return;
}
const blocked = adConsentBlock();
if (blocked) {
  skip(blocked);
  return;
}

// TikTok answers 200 with a non-zero `code` when it refuses an event.
const finishTikTok = (res) => {
  let body;
  if (res.statusCode >= 200 && res.statusCode < 300) {
    body = JSON.parse(res.body || 'null');
  }
  if (body && typeof body === 'object' && body.code) {
    logToConsole('refused: code ' + body.code + ' ' + makeString(body.message || ''));
    data.gtmOnFailure();
    return;
  }
  finish(res);
};

// TikTok's browser pixel sets _ttp; the click id arrives as ?ttclid.
fetchExtra((x) => {
  const items = eventItems();
  const properties = clean({
    value: numberOrUndefined(getEventData('value')),
    currency: getEventData('currency'),
    order_id: getEventData('transaction_id'),
    content_type: items.list.length ? 'product' : undefined,
    contents: items.list.length
      ? items.list.map((i) =>
          clean({
            content_id: i.id,
            content_name: i.name,
            content_category: i.category,
            brand: i.brand,
            price: i.price,
            quantity: i.quantity,
          })
        )
      : undefined,
  });
  const payload = {
    event_source: 'web',
    event_source_id: data.pixel_code,
    data: [
      clean({
        event: ttName,
        event_time: Math.floor(now / 1000),
        event_id: getEventData('event_id'),
        user: clean({
          ip: getEventData('ip_override'),
          user_agent: getEventData('user_agent'),
          ttp: getCookieValues('_ttp')[0],
          ttclid: queryParam('ttclid'),
          external_id: hash(x.external_id || getEventData('user_id')),
          email: hash(x.email),
          phone_number: hashPhone(x.phone),
        }),
        page: clean({ url: pageUrl, referrer: getEventData('page_referrer') }),
        properties: Object.keys(properties).length ? properties : undefined,
      }),
    ],
  };
  if (data.test_event_code) payload.test_event_code = data.test_event_code;
  sendHttpRequest(
    'https://business-api.tiktok.com/open_api/v1.3/event/track/',
    { method: 'POST', headers: { 'content-type': 'application/json', 'access-token': data.access_token } },
    JSON.stringify(payload)
  ).then(finishTikTok, data.gtmOnFailure);
});
