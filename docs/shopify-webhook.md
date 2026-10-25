# Shopify orders

The Shopify order webhook sends each paid Shopify order to the tags from the server, as a GA4-style `purchase`. It works without the browser: a purchase still reaches Meta, TikTok, Pinterest and the BigQuery export when the checkout page's pixels didn't fire. It appears on `/admin/integrations` as a card, **Shopify orders**, with a switch and two fields.

This guide covers the Shopify side, the admin settings, what the endpoint does with each request, and how to check that orders arrive.

Requires integrations mode (`INTEGRATIONS_FILE`). There are no new environment variables, and the webhook is not available in single-site or tenants mode.

## 1. Create the webhook in Shopify

In Shopify admin, go to **Settings > Notifications > Webhooks > Create webhook** and set:

| Field | Value |
| --- | --- |
| Event | **Order payment** (`orders/paid`) |
| Format | **JSON** |
| URL | `https://<your site>/<your path>/shopify/orders` |

The URL goes through the Cloudflare Worker, which strips your path (`PATH_PREFIX`, e.g. `/k7q2`) and adds the shared secret the server requires. So the server receives the request at `/shopify/orders`. Send the webhook to the Worker's address, not directly to the server.

## 2. Copy the signing secret

Shopify signs every webhook with a secret. It is shown under the webhook list as **"Your webhooks will be signed with ..."**. For a webhook that an app creates, the signing secret is the app's client secret.

## 3. Set it in the admin

Open `/admin/integrations`, turn on the **Shopify orders** switch, and fill in:

- **Webhook signing secret** (required): the value from step 2. Like the other secrets on the page, it is never shown again after saving. Leave the field empty to keep the saved value.
- **Product id to send** (optional): which id each line item is sent with. `variant_id` is the default and matches Shopify's own Meta catalog. Use `product_id` or `sku` if your ad catalogs use those. A line with no value for the chosen id falls back to its variant id, then its product id.

The card has no "send test event" box. To test, use the webhook's **Send test notification** in Shopify (see below).

Changes apply within a few seconds on every instance, without a deploy.

## What the server does with a request

The endpoint is `POST /shopify/orders`.

| Request | Response | Log line |
| --- | --- | --- |
| Signature missing or wrong | 401 | `shopify=bad_signature` |
| Method other than POST | 405 | |
| Topic other than `orders/paid` | 200, not run | `shopify=ignored_topic` |
| Body that isn't an order | 400 | `shopify=not_an_order` |
| Test order (`"test": true`) | 200, not run | `shopify=test_order` |
| Same order id again within a day, on the same instance | 200, not run | `shopify=duplicate` |
| Anything else | 200, tags run | |

The signature is the base64 HMAC-SHA256 of the raw request body, keyed with the signing secret, and compared with the `X-Shopify-Hmac-Sha256` header. Shopify retries deliveries that don't get a 200, so the server answers 200 to an order the tags already had.

## What each purchase contains

| Event field | From the order |
| --- | --- |
| `event_name` | `purchase` |
| `event_id`, `transaction_id` | `id` (the order id as a string) |
| `currency` | `currency` |
| `value` | `current_total_price`, else `total_price` |
| `tax` | `current_total_tax`, else `total_tax` |
| `shipping` | `total_shipping_price_set.shop_money.amount` |
| `coupon` | the first `discount_codes[].code` |
| `items` | each line item: `item_id` (per the setting above), `item_name` (`title`), `item_variant` (`variant_title`), `item_brand` (`vendor`), `price`, `quantity` |
| `ip_override` | `browser_ip`, else `client_details.browser_ip` |
| `user_agent` | `client_details.user_agent` |
| `page_location` | `https://<first SITE_DOMAINS entry>/` (the GA4 client's site domains); without one, `https://<shop>.myshopify.com/` from the `X-Shopify-Shop-Domain` header |
| `user_data` | email, phone, `external_id` (the customer id), and the billing address (or shipping, if there is none) |

Meta, TikTok and Pinterest hash `user_data`, and the enrichment endpoint, if set, only fills in fields the order doesn't have.

Each tag's behaviour for orders:

- **Consent.** Orders carry no Consent Mode signals. A tag sends an order only if its "Send when the hit has no consent signals" switch is on. Browser hits from a site using Consent Mode always carry signals, so this switch doesn't change what happens to them.
- **GA4.** The GA4 tag skips orders (`not_a_ga4_hit`), because the browser already sends the purchase to GA4.
- **BigQuery export.** The row is written, but without `user_data` and without the IP address.
- **Duplicates with the browser pixel.** To deduplicate against a browser purchase event, send the Shopify order id as the `event_id` on that event too. The platforms use it to deduplicate the two.

## 4. Check that orders arrive

1. In Shopify, open the webhook and click **Send test notification**. Test orders are signed, so the server checks the signature and answers 200, but does not run the tags (`shopify=test_order`). A 401 here means the signing secret is wrong.
2. Place a real order, or wait for one. Its request should log `shopify=` followed by a result. A run with no `shopify=` line means the tags ran.
3. Check the admin's recent events log and the counters on `/system`, which count skipped tags apart from successes and failures.
4. In Shopify, the webhook's delivery history shows each request and the status code the server returned. Anything other than 200 means the server refused the order: a 401 is a signing secret mismatch, and a 400 means the body wasn't an order.

Shopify retries a delivery that fails, so the same order can arrive more than once. The server runs it only the first time within a day.
