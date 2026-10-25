// Snapchat Conversions API tag: sends GA4 events to Snap from the server.
// Settings (tag `data`): pixel_id, access_token, test_mode ("true" sends to
// Snap's validate endpoint, which checks the event without counting it).
// GA4 events with no Snap name are skipped, and so are events whose Consent
// Mode state doesn't grant both ad_storage and ad_user_data. Snap
// deduplicates against the browser pixel by event name plus `event_id`.
const NAMES = {
  page_view: 'PAGE_VIEW',
  view_item: 'VIEW_CONTENT',
  search: 'SEARCH',
  add_to_cart: 'ADD_CART',
  add_to_wishlist: 'ADD_TO_WISHLIST',
  begin_checkout: 'START_CHECKOUT',
  add_payment_info: 'ADD_BILLING',
  purchase: 'PURCHASE',
  sign_up: 'SIGN_UP',
};

if (!data.pixel_id || !data.access_token) {
  data.gtmOnFailure();
  return;
}

const eventName = getEventData('event_name');
const snapName = NAMES[eventName];
if (!snapName) {
  skip('unmapped_event');
  return;
}
const blocked = adConsentBlock();
if (blocked) {
  skip(blocked);
  return;
}

// Snap's browser pixel sets _scid; the click id arrives as ?ScCid.
fetchExtra((x) => {
  const items = eventItems();
  const value = numberOrUndefined(getEventData('value'));
  const customData = clean({
    value: value,
    currency: getEventData('currency'),
    order_id: getEventData('transaction_id'),
    content_ids: items.ids.length ? items.ids : undefined,
    contents: items.list.length
      ? items.list.map((i) => clean({ id: i.id, quantity: i.quantity, item_price: i.price }))
      : undefined,
    num_items: items.list.length ? items.total : undefined,
  });
  const payload = {
    data: [
      clean({
        event_name: snapName,
        action_source: 'WEB',
        event_time: Math.floor(now / 1000),
        event_id: getEventData('event_id'),
        event_source_url: pageUrl,
        user_data: clean({
          client_ip_address: getEventData('ip_override'),
          client_user_agent: getEventData('user_agent'),
          sc_cookie1: getCookieValues('_scid')[0],
          sc_click_id: queryParam('ScCid') || queryParam('sccid'),
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
  const url =
    'https://tr.snapchat.com/v3/' + encodeUriComponent(data.pixel_id) + '/events' +
    (data.test_mode === 'true' ? '/validate' : '') +
    '?access_token=' + encodeUriComponent(data.access_token);
  sendHttpRequest(url, { method: 'POST', headers: { 'content-type': 'application/json' } }, JSON.stringify(payload)).then(
    finish,
    data.gtmOnFailure
  );
});
