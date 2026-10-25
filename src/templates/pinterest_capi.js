// Pinterest Conversions API tag: sends GA4 events to Pinterest from the server.
// Settings (tag `data`): ad_account_id, access_token, test_mode ("true" sends to
// Pinterest's test endpoint). GA4 events with no Pinterest name are skipped, and
// so are events whose Consent Mode state doesn't grant both ad_storage and
// ad_user_data. Pinterest deduplicates against the browser tag by event name
// plus `event_id`.
const NAMES = {
  page_view: 'page_visit',
  search: 'search',
  add_to_cart: 'add_to_cart',
  purchase: 'checkout',
  sign_up: 'signup',
  generate_lead: 'lead',
};

if (missing(['ad_account_id', 'access_token'])) {
  return;
}

const eventName = getEventData('event_name');
const pinName = NAMES[eventName];
if (!pinName) {
  skip('unmapped_event');
  return;
}
const blocked = adConsentBlock();
if (blocked) {
  skip(blocked);
  return;
}

fetchExtra((x) => {
  const value = numberOrUndefined(getEventData('value'));
  const items = eventItems();
  const customData = clean({
    value: value === undefined ? undefined : makeString(value),
    currency: getEventData('currency'),
    order_id: getEventData('transaction_id'),
    content_ids: items.ids.length ? items.ids : undefined,
    contents: items.list.length
      ? items.list.map((i) =>
          clean({
            id: i.id,
            item_name: i.name,
            item_category: i.category,
            item_brand: i.brand,
            item_price: i.price === undefined ? undefined : makeString(i.price),
            quantity: i.quantity,
          })
        )
      : undefined,
    num_items: items.list.length ? items.total : undefined,
  });
  const payload = {
    data: [
      clean({
        event_name: pinName,
        action_source: 'web',
        event_time: Math.floor(now / 1000),
        event_id: getEventData('event_id'),
        event_source_url: pageUrl,
        user_data: clean({
          client_ip_address: getEventData('ip_override'),
          client_user_agent: getEventData('user_agent'),
          click_id: queryParam('epik'),
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
    'https://api.pinterest.com/v5/ad_accounts/' + encodeUriComponent(data.ad_account_id) + '/events' +
    (data.test_mode === 'true' ? '?test=true' : '');
  sendHttpRequest(
    url,
    { method: 'POST', headers: { 'content-type': 'application/json', authorization: 'Bearer ' + data.access_token } },
    JSON.stringify(payload)
  ).then(finish, data.gtmOnFailure);
});
