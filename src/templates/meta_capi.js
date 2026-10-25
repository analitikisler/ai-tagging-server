// Meta Conversions API tag: sends GA4 events to Meta from the server.
// Settings (tag `data`): pixel_id, access_token, test_event_code (optional).
// GA4 events with no Meta name are skipped, and so are events whose Consent
// Mode state doesn't grant both ad_storage and ad_user_data. Meta deduplicates
// against the browser pixel by event name plus `event_id`: send the same value
// from both.
const NAMES = {
  page_view: 'PageView',
  view_item: 'ViewContent',
  add_to_cart: 'AddToCart',
  add_to_wishlist: 'AddToWishlist',
  begin_checkout: 'InitiateCheckout',
  add_payment_info: 'AddPaymentInfo',
  purchase: 'Purchase',
  generate_lead: 'Lead',
  sign_up: 'CompleteRegistration',
  search: 'Search',
};

if (missing(['pixel_id', 'access_token'])) {
  return;
}

const eventName = getEventData('event_name');
const metaName = NAMES[eventName];
if (!metaName) {
  skip('unmapped_event');
  return;
}
const blocked = adConsentBlock();
if (blocked) {
  skip(blocked);
  return;
}

// _fbc is set by Meta's browser pixel; without it, build it from ?fbclid.
let fbc = getCookieValues('_fbc')[0];
if (!fbc && queryParam('fbclid')) fbc = 'fb.1.' + now + '.' + queryParam('fbclid');

fetchExtra((x) => {
  const items = eventItems();
  const customData = clean({
    value: numberOrUndefined(getEventData('value')),
    currency: getEventData('currency'),
    order_id: getEventData('transaction_id'),
    content_type: items.ids.length ? 'product' : undefined,
    content_ids: items.ids.length ? items.ids : undefined,
    contents: items.ids.length
      ? items.list.filter((i) => i.id).map((i) => clean({ id: i.id, quantity: i.quantity, item_price: i.price }))
      : undefined,
    num_items: items.list.length ? items.total : undefined,
  });
  const payload = {
    data: [
      clean({
        event_name: metaName,
        event_time: Math.floor(now / 1000),
        event_id: getEventData('event_id'),
        action_source: 'website',
        event_source_url: pageUrl,
        user_data: clean({
          client_ip_address: getEventData('ip_override'),
          client_user_agent: getEventData('user_agent'),
          fbp: getCookieValues('_fbp')[0],
          fbc: fbc,
          external_id: listOf(hash(x.external_id || getEventData('user_id'))),
          em: listOf(hash(x.email)),
          ph: listOf(hashPhone(x.phone)),
          fn: listOf(hash(x.first_name)),
          ln: listOf(hash(x.last_name)),
          ct: listOf(hash(x.city)),
          st: listOf(hash(x.state)),
          zp: listOf(hash(x.zip)),
          country: listOf(hash(x.country)),
        }),
        custom_data: Object.keys(customData).length ? customData : undefined,
      }),
    ],
  };
  // Tells Meta which integration sent the event.
  payload.partner_agent = 'analitikisler-tagging-server';
  if (data.test_event_code) payload.test_event_code = data.test_event_code;
  // The token goes in the body, not the URL, so it never shows up in logs.
  payload.access_token = data.access_token;
  const url = 'https://graph.facebook.com/v26.0/' + encodeUriComponent(data.pixel_id) + '/events';
  sendHttpRequest(url, { method: 'POST', headers: { 'content-type': 'application/json' } }, JSON.stringify(payload)).then(finish, data.gtmOnFailure);
});
