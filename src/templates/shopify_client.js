// Shopify order webhook client: claims Shopify's `orders/paid` webhooks at
// POST /shopify/orders, checks Shopify's signature and runs the container
// once with a GA4-style `purchase` event built from the order, so the ad tags
// and the BigQuery export get the purchase from the server, whatever happened
// to the checkout page's pixels.
//
// Fields:
//   webhook_secret  The key Shopify signs webhooks with: the "Your webhooks
//                   will be signed with" value under Settings > Notifications
//                   > Webhooks, or the app's client secret for an app.
//   item_id         Which id each product is sent with: variant_id (default,
//                   what Shopify's own Meta catalog uses), product_id or sku.
//   siteUrl         The store's address, sent as the page the purchase
//                   happened on. Default: https://<shop>.myshopify.com/.
//
// Answers 401 to a missing or wrong signature, 400 to a body that isn't an
// order, and 200 to everything else, so Shopify doesn't retry an order the
// tags already had. Other topics, test orders and repeated deliveries of the
// same order are answered 200 and not run. Orders carry no Consent Mode
// signals: each tag's "send when the hit has no consent signals" setting
// decides whether it sends them.
const claimRequest = require('claimRequest');
const getRequestBody = require('getRequestBody');
const getRequestHeader = require('getRequestHeader');
const getRequestMethod = require('getRequestMethod');
const getRequestPath = require('getRequestPath');
const getTimestampMillis = require('getTimestampMillis');
const hmacSha256 = require('hmacSha256');
const JSON = require('JSON');
const logToConsole = require('logToConsole');
const makeNumber = require('makeNumber');
const makeString = require('makeString');
const returnResponse = require('returnResponse');
const runContainer = require('runContainer');
const setResponseStatus = require('setResponseStatus');
const sha256Sync = require('sha256Sync');
const templateDataStorage = require('templateDataStorage');

if (getRequestPath() !== '/shopify/orders') return;
claimRequest();

const finish = (status, log) => {
  if (log) logToConsole(log);
  setResponseStatus(status);
  returnResponse();
};

if (getRequestMethod() !== 'POST') {
  finish(405);
  return;
}
if (!data.webhook_secret) {
  finish(503, 'shopify=no_secret');
  return;
}

const body = getRequestBody() || '';
const given = (getRequestHeader('x-shopify-hmac-sha256') || '').trim();

const str = (v) => (v === undefined || v === null || v === '' ? undefined : makeString(v));
const num = (v) => {
  if (v === undefined || v === null || v === '') return undefined;
  const n = makeNumber(v);
  return n === n ? n : undefined;
};

// The purchase as the GA4 client would have produced it, with the buyer's
// details in sGTM's `user_data` shape for the ad tags to hash.
const toEvent = (order) => {
  const idKey = data.item_id === 'product_id' || data.item_id === 'sku' ? data.item_id : 'variant_id';
  const lines = order.line_items && typeof order.line_items.length === 'number' ? order.line_items : [];
  const items = lines.map((l) => ({
    item_id: str(l[idKey]) || str(l.variant_id) || str(l.product_id),
    item_name: str(l.title),
    item_variant: str(l.variant_title),
    item_brand: str(l.vendor),
    price: num(l.price),
    quantity: num(l.quantity) || 1,
  }));
  const customer = order.customer || {};
  const address = order.billing_address || order.shipping_address || {};
  const shipping = order.total_shipping_price_set && order.total_shipping_price_set.shop_money;
  const codes = order.discount_codes && order.discount_codes.length ? order.discount_codes : [];
  const client = order.client_details || {};
  const shop = (getRequestHeader('x-shopify-shop-domain') || '').toLowerCase();
  const id = makeString(order.id);
  return {
    event_name: 'purchase',
    // Send the order id as event_id from the browser too, and Meta and TikTok
    // count the purchase once.
    event_id: id,
    transaction_id: id,
    currency: str(order.currency),
    value: num(order.current_total_price !== undefined ? order.current_total_price : order.total_price),
    tax: num(order.current_total_tax !== undefined ? order.current_total_tax : order.total_tax),
    shipping: num(shipping && shipping.amount),
    coupon: codes.length ? str(codes[0].code) : undefined,
    items: items,
    ip_override: str(order.browser_ip || client.browser_ip),
    user_agent: str(client.user_agent),
    page_location: data.siteUrl || (shop ? 'https://' + shop + '/' : undefined),
    user_data: {
      email_address: str(order.email || order.contact_email || customer.email),
      phone_number: str(order.phone || customer.phone || address.phone),
      external_id: str(customer.id),
      address: {
        first_name: str(address.first_name || customer.first_name),
        last_name: str(address.last_name || customer.last_name),
        city: str(address.city),
        region: str(address.province_code || address.province),
        postal_code: str(address.zip),
        country: str(address.country_code),
      },
    },
    // A server source: the ad tags use this user data without their switch.
    'x-sst-source': 'shopify',
    'x-shopify-order-name': str(order.name),
    'x-shopify-shop-domain': shop || undefined,
  };
};

hmacSha256(body, data.webhook_secret, (expected) => {
  // Compares digests of both values, so the time taken says nothing about
  // how much of a guessed signature was right.
  const digest = (v) => sha256Sync(v, { outputEncoding: 'hex' });
  if (!given || digest(given) !== digest(expected)) {
    finish(401, 'shopify=bad_signature');
    return;
  }
  const topic = getRequestHeader('x-shopify-topic') || '';
  if (topic !== 'orders/paid') {
    finish(200, 'shopify=ignored_topic topic=' + topic);
    return;
  }
  const order = JSON.parse(body);
  if (!order || typeof order !== 'object' || order.id === undefined || order.id === null) {
    finish(400, 'shopify=not_an_order');
    return;
  }
  if (order.test === true) {
    finish(200, 'shopify=test_order');
    return;
  }
  // Shopify retries a delivery it didn't see answered in time. Remembered by
  // this instance for a day; tags with an event_id deduplicate on their side too.
  const now = getTimestampMillis();
  const seenKey = 'paid:' + makeString(order.id);
  const seen = templateDataStorage.getItemCopy(seenKey);
  if (seen && now - seen < 86400000) {
    finish(200, 'shopify=duplicate');
    return;
  }
  templateDataStorage.setItemCopy(seenKey, now);
  runContainer(toEvent(order), () => finish(200));
});
