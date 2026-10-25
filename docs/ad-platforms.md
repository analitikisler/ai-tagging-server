# More ad platforms: Snapchat, Reddit, LinkedIn and X

Four conversion tags sit next to Meta, TikTok and Pinterest on `/admin`: **Snapchat Conversions API**, **Reddit Conversions API**, **LinkedIn Conversions API** and **X Conversions API**. Each card has the same Save, on/off and "Send a test event" controls as the others, and each one saves to the integrations file, so ids and tokens change without a deploy.

This guide covers, for each platform, where to find the credentials, what to enter on its card, how consent is handled and how to check that events arrive. Set `INTEGRATIONS_FILE` and `ADMIN_TOKEN` first, as described in the [README](../README.md#ad-platforms-meta-tiktok-pinterest).

## Behaviour shared by all four

- **Consent.** A tag sends only when the hit grants both `ad_storage` and `ad_user_data` (read from `gcd`, with `gcs` as the fallback for ad storage). A denied signal skips the event with `consent:<signal>=denied`. A hit with no signals is skipped with `consent:<signal>=missing`, unless "Send when the hit has no consent signals" is on. That switch is meant for sites without Consent Mode and for Shopify orders, and it never sends a visitor who denied consent.
- **Event names.** Each platform has its own event names, and GA4 events are mapped to them. An event with no mapping is skipped with `unmapped_event`.
- **Matching.** Emails, phone numbers and names are normalised and hashed (SHA-256) before they leave the server. Enrichment-endpoint answers and Shopify order `user_data` are used when present.
- **Skips and counters.** A skipped event is reported as `skipped`, not as a success, with its reason in the request's log line. `/system` counts skips apart from successes and failures.
- **Secrets.** Access tokens are never shown again after saving. Leave a secret field empty to keep the saved value.

## Snapchat Conversions API

Sends events to Snapchat's Conversions API with the `snapchat_capi` template.

**Credentials.** In Snapchat Ads Manager, open Events Manager, select your Pixel, then go to Conversions API and generate a token.

**Card settings.**

| Field | Required | Notes |
| --- | --- | --- |
| Pixel ID | yes | The pixel id of the Snapchat pixel. |
| Access token | yes | Stored as a secret. |
| Test mode | no | Sends to Snap's validate endpoint, which checks events without counting them. Turn it off to go live. |
| Send when the hit has no consent signals | no | See the shared behaviour above. |

**Events.** `page_view` → `PAGE_VIEW`, `view_item` → `VIEW_CONTENT`, `search` → `SEARCH`, `add_to_cart` → `ADD_CART`, `add_to_wishlist` → `ADD_TO_WISHLIST`, `begin_checkout` → `START_CHECKOUT`, `add_payment_info` → `ADD_BILLING`, `purchase` → `PURCHASE`, `sign_up` → `SIGN_UP`.

**What is sent.** The `_scid` cookie as `sc_cookie1`, the `ScCid` click id (or `sccid`) as `sc_click_id`, and the hashed email, phone, names, address and external id when available. Snapchat deduplicates on the event name plus `event_id`.

**Verify.** Turn on Test mode, save, and press "Send a test event" on the card. Each test event is validated by Snap and not counted. The card shows the outcome of the test hit. When the check passes, turn Test mode off.

## Reddit Conversions API

Sends events to the Reddit Conversions API v3 with the `reddit_capi` template.

**Credentials.** In Reddit Ads, open Events Manager, select your pixel, then go to Conversions API and generate an access token.

**Card settings.**

| Field | Required | Notes |
| --- | --- | --- |
| Pixel ID | yes | Starts with `a2_` or `t2_`, shown next to the pixel in Events Manager. |
| Access token | yes | Stored as a secret. |
| Test ID | no | From Events Manager, under Test events. While set, events only show there. Clear it to go live. |
| Send when the hit has no consent signals | no | See the shared behaviour above. |

**Events.** `page_view` → `PAGE_VISIT`, `view_item` → `VIEW_CONTENT`, `search` → `SEARCH`, `add_to_cart` → `ADD_TO_CART`, `add_to_wishlist` → `ADD_TO_WISHLIST`, `purchase` → `PURCHASE`, `generate_lead` → `LEAD`, `sign_up` → `SIGN_UP`.

**What is sent.** The `_rdt_uuid` cookie as the visitor uuid, the `rdt_cid` query parameter (or the `_rdt_cid` cookie) as the click id, and the hashed email, phone, external id and IP. Reddit deduplicates on `conversion_id`, which is the GA4 `event_id`. The Reddit pixel has to send the same value as its `conversionId`, or the two will count twice.

**Verify.** Save a Test ID, press "Send a test event", and check the event under Test events in Events Manager. Clear the Test ID when you go live.

## LinkedIn Conversions API

Sends conversions to the LinkedIn Conversions API with the `linkedin_capi` template.

**Credentials.** In Campaign Manager, go to Data, then Sources, and open the Conversions API. Create one conversion rule per event you want to send, with the Conversions API as its source. Generate an access token with the `rw_conversions` scope.

**Card settings.**

| Field | Required | Notes |
| --- | --- | --- |
| Access token | yes | Needs the `rw_conversions` scope. Stored as a secret. |
| Conversion rules | yes | Each GA4 event with the id of its conversion rule, separated by commas, for example `purchase=12345, generate_lead=67890`. Events not listed are skipped. |
| Send when the hit has no consent signals | no | See the shared behaviour above. |

**Matching.** LinkedIn needs the visitor's email (from enrichment or Shopify order data) or the `li_fat_id` click id. The `li_fat_id` is read from the query string or the cookie, and it is only there when enhanced conversion tracking is on in the Insight Tag. A conversion with neither is skipped with `no_user_id`.

**Deduplication.** Sent as `eventId`, which is the GA4 `event_id`.

**Verify with care.** LinkedIn has no test mode. "Send a test event" sends a real conversion for the chosen event, so pick an event whose conversion rule isn't used for bidding while you check, or test on a staging property. Confirm the conversion in Campaign Manager.

## X Conversions API

Sends conversions to the X Ads API with the `x_capi` template. Requests are signed with OAuth 1.0a.

**Credentials.** Get the pixel id and the event ids from Ads Manager, under Tools, then Events Manager. For the keys, create an app with Ads API access at developer.x.com. The access token and token secret must belong to a user with access to the ad account.

**Card settings.**

| Field | Required | Notes |
| --- | --- | --- |
| Pixel ID | yes | The base pixel id, such as `o8z1j`. |
| Event ids | yes | Each GA4 event with the id of its X event, separated by commas, for example `purchase=tw-o8z1j-o8z2m, add_to_cart=tw-o8z1j-o8z3a`. Events not listed are skipped. |
| API key (consumer key) | yes | From your X app. |
| API key secret | yes | Stored as a secret. |
| Access token | yes | Stored as a secret. |
| Access token secret | yes | Stored as a secret. |
| Send when the hit has no consent signals | no | See the shared behaviour above. |

**Matching.** X needs the `twclid` click id (from the query string or the `_twclid` cookie), an email or a phone number. Without any of them the event is skipped with `no_user_id`. The IP address and user agent are sent alongside.

**Deduplication.** Sent as `conversion_id`, which is the GA4 `event_id`.

**Verify with care.** X has no test mode either. "Send a test event" sends a real conversion for the chosen event. Check it in Events Manager, and use a staging property before you go live.

## Checking the cards

On `/admin`, each card's "Send a test event" button sends one test event for a chosen event name: `purchase`, `page_view`, `view_item`, `add_to_cart`, `begin_checkout`, `generate_lead` or `sign_up`. The test event carries an email (`admin-test@example.com`) so LinkedIn and X can match it, and its test consent state grants both ad signals, so it passes the consent check. The card shows the outcome, and the event also appears in the recent events log. An event that is not in the card's event list is skipped with `unmapped_event`, so add it to the list first.

The test event checks the settings, the tokens and the path to the platform. It does not check that your site sends real hits with consent. For that, watch the recent events log while you use the site.
