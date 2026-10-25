// Reddit Conversions API (v3) tag: sends GA4 events to Reddit from the server.
// Settings (tag `data`): pixel_id, access_token, test_event_code (optional,
// sent as Reddit's `test_id`). GA4 events with no Reddit name are skipped, and
// so are events whose Consent Mode state doesn't grant both ad_storage and
// ad_user_data. Reddit deduplicates against the browser pixel by
// `conversion_id`: the GA4 `event_id`, which the pixel must send as its
// conversionId.
const NAMES = {
  page_view: 'PAGE_VISIT',
  view_item: 'VIEW_CONTENT',
  search: 'SEARCH',
  add_to_cart: 'ADD_TO_CART',
  add_to_wishlist: 'ADD_TO_WISHLIST',
  purchase: 'PURCHASE',
  generate_lead: 'LEAD',
  sign_up: 'SIGN_UP',
};
// Reddit takes a value only on these, and an item count on the first three.
const WITH_ITEMS = ['ADD_TO_CART', 'ADD_TO_WISHLIST', 'PURCHASE'];
const WITH_VALUE = ['ADD_TO_CART', 'ADD_TO_WISHLIST', 'PURCHASE', 'LEAD', 'SIGN_UP'];

if (!data.pixel_id || !data.access_token) {
  data.gtmOnFailure();
  return;
}

const eventName = getEventData('event_name');
const redditName = NAMES[eventName];
if (!redditName) {
  skip('unmapped_event');
  return;
}
const blocked = adConsentBlock();
if (blocked) {
  skip(blocked);
  return;
}

// Reddit's browser pixel keeps its visitor id in _rdt_uuid; the click id
// arrives as ?rdt_cid and the pixel keeps it in _rdt_cid.
fetchExtra((x) => {
  const items = eventItems();
  const withValue = WITH_VALUE.indexOf(redditName) !== -1;
  const withItems = WITH_ITEMS.indexOf(redditName) !== -1;
  const products = items.list.filter((i) => i.id).map((i) => clean({ id: i.id, name: i.name, category: i.category }));
  const metadata = clean({
    conversion_id: getEventData('event_id') || getEventData('transaction_id'),
    currency: withValue ? getEventData('currency') : undefined,
    value: withValue ? numberOrUndefined(getEventData('value')) : undefined,
    item_count: withItems && items.list.length ? items.total : undefined,
    products: products.length ? products : undefined,
  });
  const event = clean({
    event_at: now,
    action_source: 'WEBSITE',
    click_id: queryParam('rdt_cid') || getCookieValues('_rdt_cid')[0],
    type: { tracking_type: redditName },
    metadata: Object.keys(metadata).length ? metadata : undefined,
    user: clean({
      ip_address: hash(getEventData('ip_override')),
      user_agent: getEventData('user_agent'),
      uuid: getCookieValues('_rdt_uuid')[0],
      external_id: hash(x.external_id || getEventData('user_id')),
      email: hash(x.email),
      phone_number: hashPhone(x.phone),
    }),
  });
  const body = clean({ test_id: data.test_event_code, events: [event] });
  sendHttpRequest(
    'https://ads-api.reddit.com/api/v3/pixels/' + encodeUriComponent(data.pixel_id) + '/conversion_events',
    { method: 'POST', headers: { 'content-type': 'application/json', authorization: 'Bearer ' + data.access_token } },
    JSON.stringify({ data: body })
  ).then(finish, data.gtmOnFailure);
});
