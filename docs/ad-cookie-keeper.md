# Ad cookie keeper

The ad cookie keeper re-sets Meta's `_fbp` and `_fbc` cookies and TikTok's `_ttp` cookie from the server, with a lifetime of 390 days. Safari caps cookies that a script sets at 7 days, so without this the ad platforms lose the visitor after a week. The server also creates `_fbc` from a `fbclid` in the landing page's URL, so a Meta click is remembered from the first hit.

It is part of the GA4 client (`ga4_client`), so it runs on every GA4 hit and needs no admin card or environment variable.

## What it does

On each hit where ad storage is granted:

- **`_fbp` and `_ttp`** are re-set with the value the browser already has.
- **`_fbc`** is re-set with the value the browser already has. When the hit's page URL (`dl`) has a `fbclid` query parameter and the stored `_fbc` is missing or belongs to another click, a new `_fbc` is created as `fb.1.<milliseconds>.<fbclid>`.

Each cookie is set with `Domain=auto`, `Path=/`, `Max-Age=33696000` (390 days), `Secure` and `SameSite=Lax`.

Cookies the hit doesn't carry are not created, except `_fbc` from `fbclid` as described above.

## Consent

- The cookies are set only when **ad storage is granted**. The server reads `gcd` first: its first letter is `ad_storage` (`t`, `n`, `r` and `v` mean granted; `p`, `q`, `m` and `u` mean denied). When `gcd` is missing or has no such letter, the server falls back to `gcs`: `G110` and `G111` mean ad storage is granted, and `G100` and `G101` mean it is denied.
- A denied signal, or a hit with no signal at all, leaves the ad cookies alone. The server doesn't set them and doesn't delete any the browser already has.
- The keeper doesn't depend on analytics storage, so it works even when `extendGaCookie` is skipped for an analytics denial.

## Settings

| Setting | Default | Where | What it does |
| --- | --- | --- | --- |
| `keepAdCookies` | `true` | GA4 client `data` | Set it to `false` to turn the keeper off. |

The GA4 client takes its data from a container file, as `extendGaCookie` does (see [Containers](../README.md#containers)):

```json
{ "name": "GA4", "template": "ga4_client", "data": { "keepAdCookies": false } }
```

Without a container file the server uses the default, so the keeper is on.

## How to verify

Send a hit with a Meta and a TikTok cookie, ad consent granted and a `fbclid` on the page URL. Use `gcs` for the consent here, since the hit then carries no `gcd`:

```sh
curl -i "http://localhost:8080/g/collect?v=2&tid=G-XXXXXXX&cid=123.456&en=page_view&gcs=G111&dl=https%3A%2F%2Fexample.com%2F%3Ffbclid%3DabcDEF" \
  -H "Cookie: _fbp=fb.1.1700000000000.1234567890; _ttp=abc123"
```

The response has a `Set-Cookie` header for `_fbp`, `_ttp` and a new `_fbc` that starts with `fb.1.` and ends in `.abcDEF`. Check the `Max-Age` is `33696000`.

Then check the consent handling. With `gcs=G100` (ad storage denied) the same request gets no `Set-Cookie` headers for these cookies.

To check the feature in the code, run the sandbox tests with `cargo test`. They cover the cookie values, the consent cases and the `fbclid` handling.
