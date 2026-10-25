<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="brand/assets/lockups/ai-tagging-server-dark.svg">
    <img alt="ai tagging server" src="brand/assets/lockups/ai-tagging-server-light.svg" height="44">
  </picture>
</h1>

A self-hosted, open-source server-side tagging server. Your website sends its tracking hits (GA4, ad pixels) to your own domain; this server cleans them up and forwards them to Google Analytics, Meta, TikTok, Pinterest and Google Ads from the server. It's a small Rust binary that runs Google sGTM-style templates, with an admin console and an optional AI that helps you set it up and keep it working.

> Status: early development. The GA4 client and tag, the ad platform tags and the console work end to end; the template API is still growing (see [API coverage](#api-coverage)). Expect changes before 1.0.

## New to server-side tagging?

Normally the browser sends tracking data straight to each vendor. Ad blockers and browser privacy features cut some of it, and every pixel slows the page down. With server-side tagging, the page sends one stream of hits to a server on your own domain, and that server decides what goes to which vendor. You get more complete data, fewer third-party scripts on the page, and control over what personal data leaves your server.

This project does that job like Google's server-side Tag Manager (sGTM): **clients** receive hits and turn them into events, **firing rules** decide which **tags** run for each event, and tags send the event on. Templates use Google's sandboxed JavaScript API names, so many sGTM templates work as they are. You don't need sGTM or a Google Cloud account to run it.

## What you get

- **Runs anywhere, cheaply.** One static binary (about 11 MB) that idles at about 6 MB of memory. Docker, Cloud Run, a VPS or your laptop.
- **GA4 and ad platforms built in.** GA4, Meta Conversions API, TikTok Events API, Pinterest Conversions API and Google Ads conversions, with consent respected and user data hashed.
- **Recovers lost data.** First-party Google scripts, a cookie keeper for Safari's 7-day cap, click id restore, and the AI assistants your visitors come from. See the [Features page](#features-page).
- **An admin console** to watch hits live, see why a tag fired or not, edit tags and transformations in a draft, preview it in your own browser, publish versions and undo them, get alerts when tracking breaks, and check that your website and server are connected ([Setup check](#setup-check)). In English and Turkish.
- **An optional AI** that explains a broken hit, audits the setup for wasted sends, writes transformations, sets up tags from a description and reviews changes before you publish. It works on the setup, never on the hits: every hit is handled by the server alone, so the AI adds nothing to latency or cost per hit. Everything else works without it. See [AI features](#ai-features).
- **Safe by default.** Sandboxed templates with a time and memory budget, no requests to private addresses, only your own GA4 properties accepted, and new features off until you turn them on.

## Quick start

Make an admin token (keep it: you sign in to the console with it), then run the server with Docker, accepting hits for one GA4 property (use your own `G-` id):

```sh
export ADMIN_TOKEN="$(openssl rand -hex 24)" && echo "$ADMIN_TOKEN"
docker build -t tagging-server .
docker run --rm -p 8080:8080 -e MEASUREMENT_IDS=G-XXXXXXX -e ADMIN_TOKEN tagging-server
```

Or from source, with a recent stable Rust:

```sh
cargo build --release
MEASUREMENT_IDS=G-XXXXXXX ./target/release/tagging-server
```

Then:

1. Send a test hit. It's answered `204` and forwarded to GA4 (hits from `curl`'s own user agent are treated as a bot, hence `-A`):

   ```sh
   curl -i -A 'Mozilla/5.0' "http://localhost:8080/g/collect?v=2&tid=G-XXXXXXX&cid=123.456&en=page_view&dl=https%3A%2F%2Fexample.com%2F"
   ```

2. Open `http://localhost:8080/admin`, sign in with the token, and look at the hit under **Live**.
3. Point your website at the server: in your web GTM container, set the Google tag's `server_container_url` to the server's address on your domain. For production, run it behind your own domain as described in [Deploy](#deploy-cloud-run-behind-cloudflare); the **Setup check** page tells you what's still missing.
4. To edit tags in the console, give the server a container file to work on (`CONTAINER_FILE`, see [Containers](#containers)) and a folder for history and alerts (`STATE_DIR`).

## Where to go next

- [Configuration](#configuration): every environment variable.
- [Containers](#containers) and [Transformations](#transformations): tags, triggers and cleaning up event data.
- [Ad platforms](#ad-platforms-meta-tiktok-pinterest-google-ads): Meta, TikTok, Pinterest and Google Ads.
- [Writing templates](#writing-templates) and [API coverage](#api-coverage): your own clients and tags.
- [Deploy](#deploy-cloud-run-behind-cloudflare): Cloud Run behind Cloudflare, step by step.
- [CONTRIBUTING.md](CONTRIBUTING.md) to help build it, and [SECURITY.md](SECURITY.md) to report a vulnerability.

## Under the hood

- **Sandboxed templates.** Each template run gets its own [QuickJS](https://bellard.org/quickjs/) context with no ambient access: no `eval`, no `Function` constructor, nothing global but `require` and `data`. Runs have a time budget and a memory cap, and templates from files declare the permissions they need.
- **Firing rules.** Tags fire only for the events that match their conditions.
- **Cookies and event data.** `setCookie`, `getCookieValues` and `getEventData` work, so templates that manage first-party cookies run as written.
- **Safe outgoing requests.** Sends to private, loopback, link-local and cloud-metadata addresses are refused.
- **Every CPU core.** Hits run on a pool of JavaScript workers, one runtime each.

With `DEBUG_TOKEN` set, add `__debug` to any hit and send the token in an `x-debug-token` header to get a JSON report instead of the normal response: the claiming client, the events, why each tag fired or didn't, each tag's outcome, every outgoing request and the template logs.

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `PORT` | `8080` | Port to listen on. |
| `WORKERS` | 8 × CPUs | JavaScript worker threads, each with its own runtime. A worker stays busy until its hit's sends to the platforms are answered, so this is how many hits run at once. |
| `RUN_TIMEOUT_MS` | `5000` | Time budget per request, including outgoing sends. Max 30000. |
| `JS_MEMORY_MB` | `64` | Memory cap per runtime. |
| `CONTAINER_FILE` | built-in | Path to a container JSON file (below). Without it, the server runs the GA4 client, the GA4 tag and the Google scripts client. |
| `SCRIPT_IDS` | none | Built-in container only: comma-separated GTM container and Google tag ids (`GTM-…`, `G-…`) whose `gtm.js` and `gtag.js` the server serves. |
| `MEASUREMENT_IDS` | `G-` ids in `SCRIPT_IDS` | Built-in container only: comma-separated GA4 measurement ids hits are accepted for. Hits for other properties are refused. With neither set the server won't start; `*` accepts any id on purpose. |
| `SITE_DOMAINS` | the request's host | Built-in container only: comma-separated domains whose pages may send hits, subdomains included, e.g. `example.com` when the server runs on `edge.example.com`. |
| `ORIGIN_CHECK` | `log` | Built-in container only: the GA4 client's `originCheck` (below): `enforce`, `log` or `off`. |
| `STRIP_USER_DATA` | `0` | Built-in container only: `1` sets the GA4 tag's `stripUserData` (below). |
| `KEEP_COOKIES` | none | Built-in container only: the GA4 client's `keepCookies` (below), comma-separated, e.g. `google_ads,meta`. |
| `CLICK_ID_ALIASES` | none | Built-in container only: the GA4 client's `clickIdAliases`, e.g. `gclid=gc_id,msclkid=ms_id`. |
| `VISITOR_INFO` | `0` | Built-in container only: `1` sets the GA4 client's `visitorInfo`. |
| `BOT_HANDLING` | `drop` | Built-in container only: the GA4 client's `botHandling`, `drop` or `flag`. |
| `TENANTS_FILE` | off | Path to the tenants file: turns on [multi-tenant hosting](#multi-tenant-hosting). Can't be combined with `CONTAINER_FILE`, `SCRIPT_IDS`, `MEASUREMENT_IDS` or `SITE_DOMAINS`. |
| `INTEGRATIONS_FILE` | off | Path to the integrations file: turns on [ad platforms](#ad-platforms-meta-tiktok-pinterest-google-ads) set up from the admin page. Can't be combined with `TENANTS_FILE`. |
| `ADMIN_TOKEN` | off | At least 32 characters. Turns on the admin console and API at `/admin`: the debugger and the container in every mode, plus tenant editing with `TENANTS_FILE` and ad platforms with `INTEGRATIONS_FILE`. |
| `TRACE_BUFFER` | `200` | Recent hits the admin console keeps in memory for its debugger (max 2000, `0` turns it off). Only with `ADMIN_TOKEN`. |
| `STATE_DIR` | off | A folder every instance shares (on Cloud Run, a mounted bucket) for [counts, alerts, history and pinned hits](#alerts-history-and-safe-changes). Without it they are kept in each instance's memory. |
| `SLACK_WEBHOOK_URL` | off | A Slack incoming webhook (`https://…`) that alerts are posted to. |
| `CHECK_TOKEN` | off | At least 16 characters. Lets a scheduler call `POST /admin/api/alerts/check`, and nothing else. |
| `ALERT_INTERVAL_S` | `300` | How often the server runs the alert checks itself (minimum 60, `0` turns the timer off, as on Cloud Run where a scheduler runs them). |
| `ADMIN_URL` | none | The console's address, e.g. `https://…run.app/admin`, for links in Slack messages. |
| `ANTHROPIC_API_KEY` | off | Turns on the [AI features](#ai-features) in the admin console. Needs `ADMIN_TOKEN`; without it the AI stays off with a warning. |
| `AI_MODEL` | `claude-opus-5-5` | The model the AI uses. A cheaper one, such as `claude-haiku-5-5`, costs less per call and answers less carefully. |
| `AI_DAILY_LIMIT` | `200` | Most AI calls per instance per day (UTC), so the bill can't run away. |
| `AI_BASE_URL` | `https://api.anthropic.com` | Where AI calls go, for a gateway or proxy in front of the Anthropic API. |
| `DEBUG_TOKEN` | off | Enables the `__debug` report. At least 16 characters. Accepted only in the `x-debug-token` header, so it never appears in URL logs. |
| `OUTGOING_ALLOW_PRIVATE` | `0` | `1` allows sends to private and internal addresses. Otherwise every address a name resolves to is checked when connecting, redirects aren't followed, and `HTTP_PROXY`/`HTTPS_PROXY` are ignored. |
| `SEND_RETRIES` | `1` | Extra attempts for an outgoing send after a failed connection or a 502, 503 or 504 answer (max 5). Timeouts aren't retried, and all attempts stay within the request's time budget. |
| `PROXY_SECRET` | off | When set (16+ characters), every request except `/health` must carry it in `x-proxy-secret`, so only your edge proxy can reach the server. The proxy's `x-forwarded-host` and `x-sst-client-ip` are then trusted, and the secret is removed before templates see the headers. Without it the visitor IP comes from `cf-connecting-ip`, `x-real-ip` or `x-forwarded-for`, which a client can fake unless a proxy in front of the server overwrites them. |

## Speed and monitoring

Each worker keeps one JavaScript runtime and compiles the built-in and container templates once; later hits load the compiled code instead of parsing it. Every hit still runs in fresh contexts, so a template's variables never carry over between visitors. On a 4-core machine a GA4 hit takes about 0.9 ms of JavaScript time, down from 3.6 ms, with the outgoing send to Google on top.

`GET /system` returns counters as JSON: requests by status class, in-flight and refused-as-busy requests, events, tag outcomes (success, failure, skipped, timeout), sends (ok, failed, retried) and a latency histogram with approximate p50/p95/p99. Counters start at zero when the process starts, so on Cloud Run each instance reports its own. The route needs `DEBUG_TOKEN` in the `x-debug-token` header (and `PROXY_SECRET` when one is set); without a configured token it answers 404.

`examples/bench/bench.js` load-tests any endpoint that speaks the GA4 protocol, so the same run works against this server, TagPiper or Google's own:

```sh
node examples/bench/bench.js https://your-server/g/collect --requests 5000 --concurrency 32 --measurement-id G-XXXX
```

It prints requests per second and latency percentiles. Every hit counts in the property it names, and each server's outgoing send to Google is part of what's measured, so use a staging property and run the servers from the same region.

## AI features

Set `ANTHROPIC_API_KEY` (with `ADMIN_TOKEN`) and the admin console gets an AI that works on the setup and never on the hits: no hit waits for it or pays for it. It answers only when someone clicks, and it never changes the container. A suggested tag change opens in the tag editor, filled in and not saved, for you to check and save.

- **Explain a hit.** In Live, a hit's Summary has "Explain with AI": the AI reads the trace and the container and says whether the hit went as it should, which condition stopped a tag, what a vendor's refusal means, and what to change. One call, a few cents; asking again about the same hit and container is free.
- **AI audit.** A page under Debug. The server counts the trace buffer itself (each tag's runs, sends by event, tags that never fired, several tags sending one hit to the same vendor, sends a vendor keeps refusing) and the AI picks and explains the findings: dead tags, duplicate sends, events a platform doesn't use, failing sends. What each finding saves, in sends per 1,000 hits, is worked out by the server from the counts, not by the model. One call, around 10 to 25 cents.

- **Write a transformation.** On the Transformations page, describe a change in words ("Purchase and order_complete should both be purchase", "don't send the email address to TikTok"). The AI gets the request, the container and the event keys seen in recent hits with a few example values, and writes the rule; the server loads it, and a rule that doesn't load goes back to the AI once with the error. The console shows the rule in plain words with a preview on recent hits, and saves it only when you add it. A request a rule can't express gets an answer saying what is possible instead.

- **Set up with AI.** A page under Manage. Describe the site and what to track ("a Shopify store, GA4, Meta and TikTok for purchases, Google Ads conversion action 987654 for purchases"), and the AI proposes every tag and transformation the container should have, keeping the ones you don't mention, with the steps to take on the website and in the web GTM container. The proposal must load as a container before you see it (one that doesn't goes back to the AI once with the error), the server lists the settings still to fill in, and Apply goes through the same check, history and undo as any save; a tag that keeps its name keeps its stored secrets. Leave tokens out of the description: one that looks like it holds a token or key is refused, so secrets never reach the AI. Needs `CONTAINER_FILE`.

Answers come in a fixed JSON schema and are checked before the console sees them: findings about tags the container doesn't have, trace ids the buffer doesn't hold and tag changes that don't load are dropped.

What the AI is sent: the trace as the console shows it (plain user data already hashed, secret settings `[redacted]`), without the visitor's IP address (forwarding headers, `ip_override`, `uip` and the remote address are removed), with the body cut to 8 KB and the logs to the last 100 lines, plus the container as the console shows it. The audit sends the counts, not the hits. Calls, tokens and an estimated cost since the instance started show on the Overview page and in `/admin/api/overview` under `ai`. Like traces, the AI works per instance: each instance has its own daily limit and its own counts.

## Drafts, publish and preview

With `CONTAINER_FILE` and the admin console, edits don't go live as they are saved. They collect in a **draft**, kept in the state folder, like a GTM workspace: the console shows and edits the draft, with a bar listing what it changes against live. Each edit still gets the check against recent and pinned hits.

- **Publish** puts the whole draft live as a named version, with a note, after the same check against live. A draft started before live changed (another publish, a hand edit to the file) is only published over that change when you confirm. Every instance runs the new version within a few seconds.
- **Preview** the draft in one browser: on the Setup check page, "Preview the draft" gives a code; opening the site with `?sst_check=<code>` makes that browser run the draft, and a `_sst_check` cookie keeps its next pages on the draft for 4 hours. Everyone else keeps the live container. Sends are dry unless you choose "Send for real": the draft runs fully, but nothing reaches the platforms. The live check shows that browser's hits and gives the draft's test result for a conversion. Preview hits show as `preview` in the debugger and stay out of the counts behind alerts and the setup check.
- **Discard** drops the draft. **Rollback**: History lists each published version; Undo puts live back to before one.

`POST /admin/api/container/publish` (`{ "revision", "name", "note", "force" }`, `"check": true` for the check), `DELETE /admin/api/container/draft`, and `POST /admin/api/checkup/live` with `{ "preview": true, "send": false }`. Edits through the API go to the draft the same way.

Drafts aren't environments: for separate staging and production, run two deployments, each with its own container and state.

## Features page

The console's **Features** page (under Manage) lists everything the server can do, grouped by what it's for: recovering lost data (cookie keeper, click id restore, first-party Google scripts), knowing your visitors (visitor info, AI traffic, bot handling), privacy and safety, ad platforms and transformations, safe changes (drafts, alerts, history) and the AI. Each one shows whether it's on, and opening it says how it works and where to change it. The GA4 client's own features are edited right there: into the draft with a container file, into the site's settings with tenants. With the built-in container the page shows the environment variable to set. Every feature is part of the server; none needs a paid service, and the AI is optional.

The header shows the server at a glance on every page: no hits yet, the events handled since it started, or tag errors when more than 5% of tag runs fail.

## Setup check

A website sends to this server from its web GTM container (or `gtag.js` on the page), with the Google tag's `server_container_url` pointing here; the server sends on to the platforms. The console's **Setup check** page (under Manage, with `ADMIN_TOKEN`) checks that chain and says, for each finding, which side to fix: the website and its web container, or this server.

- **From what the server has** (the last 24 hours of counts and recent hits): hits arriving, unclaimed hits, Consent Mode signals, Google's scripts served first-party; tags whose required settings are empty, still in test mode, failing (with the failing run's log line) or idle while hits arrive; and for each ad platform tag, whether its conversion event arrives with `value` and `currency` (and `transaction_id` for Google Ads, `event_id` for Meta and TikTok, a warning when their browser pixel also runs on the site), `user_data` when the tag uses it, and ad click ids reaching the server.
- **From the site,** when its address is set with this server's address on it: the page and its web GTM container (`gtm.js`, the one the page loads or the id you give) are read, and the check says whether a Google tag sends to this server (`server_container_url` or `transport_url`), whether the GA4 ids match the ones arriving, whether `gtm.js` is served first-party, and which browser pixels run. The reads only reach public addresses, redirects included.
- **Live check:** a code to open the site with (`?sst_check=<code>`). That browser's hits show on the page as they arrive, followed to later pages by its GA4 client id: each event with the values it carries and the tags that ran. When it sends a conversion (`purchase`, or a Google Ads tag's own conversion event), the page gives a **test result** for it, tag by tag: pass; warn when the tag sent it but without something it needs to count or match well (value, currency, `transaction_id` for Google Ads, `event_id` when the platform's browser pixel also runs, user data when the tag uses it, an ad click for Google Ads in this visit); or fail when the tag didn't run, failed, sent nothing or the platform refused it, with the tag's log line. This works from the first visit, so a fresh setup is checked with one test purchase. On Cloud Run the hits and the console must reach the same instance; keep the console open while you click.
- **With the AI on,** "Write my to-do list" turns the findings into steps, most important first, for the website and for this server.

The same is in `GET /admin/api/checkup` (`?refresh=true` reads the site again), `PUT /admin/api/checkup/config` (`{ "site_url", "gtm_id", "public_url" }`), `POST /admin/api/checkup/live`, `GET /admin/api/checkup/live/<code>` and `POST /admin/api/ai/checkup`.

## Alerts, history and safe changes

These need the admin console (`ADMIN_TOKEN`). They work without the AI; with it, alerts and checks get a short explanation.

**Shared state.** With `STATE_DIR`, every instance writes to and reads from one folder: on Cloud Run, the bucket the deploy script mounts at `/data`. It holds counts per quarter hour (counts only, never hit data; combined per day and kept 35 days), the container history, alerts and pinned hits. Without it, each instance keeps its own in memory, which a restart empties.

**Alerts.** Each instance counts, per site and quarter hour, events by name, `value`, `currency`, `transaction_id` and `event_id` missing from them, tag runs and failures, and sends by vendor host with their errors. A check compares the last hour with the same hour on each of the 7 days before and opens an alert for:

| Alert | When |
| --- | --- |
| Key event stopped | none in the last hour, when 10 or more are usual |
| Key event dropped | 70% below usual (a warning at 50% when 20 or more are usual) |
| Vendor refusing sends | 20% or more of 20 or more sends failed, at least twice the usual rate |
| Tag failing | 20% or more of 20 or more runs failed, threw or timed out, at least twice the usual rate |
| Value missing | `value` or `currency` on purchases, or `event_id` on key events when Meta or TikTok are sent to, missing on 20% or more |
| Unclaimed hits | 20% or more of 20 or more hits no client claimed |
| Event renamed | a name never seen before (10 or more) while one spelled the same but for case or separators falls by half (`purchase` → `Purchase`) |

Key events are `purchase`, `generate_lead`, `sign_up` and `add_to_cart` by default, set on the console's Alerts page. Drops, stops and renames wait for 3 days of history. "Usual" is the median of the earlier days; rates are pooled over them. An alert names the setup changes made in the 3 hours before it started, gets the AI's likely cause when the AI is on, and is posted to Slack (`SLACK_WEBHOOK_URL`, in English or Turkish) when it opens, when it gets worse (at most hourly) and when it resolves; a check posts at most 5 messages. Checks run every `ALERT_INTERVAL_S` on a server that keeps running, and from `POST /admin/api/alerts/check` (the deploy script's Cloud Scheduler job, every 5 minutes, with `CHECK_TOKEN`); a lock in the state folder keeps two from running at once.

**History and undo.** Every container save from the console is kept with the file as it was and what changed (tags added, deleted or changed and which fields, the order, transformations; never setting values), and every Integrations save is noted. The History page lists them, newest first; Undo puts the container back as it was before a change. The last 200 are kept. The kept files hold the container's settings, secrets included, in the state folder next to the container itself.

**Checking a change before it is saved.** Every container save from the console, and every undo, first replays recent hits (the trace buffer's, up to 200) and every pinned hit through the container as it is and as it would be, platform tags included. Nothing is sent: vendors get a canned success, and both runs of a hit see the same clock and random numbers, so what differs is what the change did. When a change alters what tags send, a dialog lists it tag by tag (a tag that stops or starts firing for an event, parameters added, removed or changed, with how many hits and an example), with the AI's review when it is on, before Save anyway. Hits keep masked user data and cookie names only, the same for both runs, and a tag that behaves differently on a vendor error won't show that here. Any save API call takes `"check": true` (`?check=true` to delete) to get this result without saving.

**Pinned test hits.** "Pin as a test hit" on a hit in Live keeps it, labelled, as a test case every check replays, wherever the traffic is: up to 50, in the state folder.

## Multi-tenant hosting

One server can serve several sites. Set `TENANTS_FILE` and each request is routed by its `Host` header (the proxy's `x-forwarded-host` when `PROXY_SECRET` is set) to a tenant with its own GA4 ids, script ids, site domains and origin check. A host no enabled tenant owns gets `404`, so the server is not an open relay. Tenants don't share template storage, and counters are kept per tenant.

```json
{
  "tenants": [
    {
      "id": "example",
      "name": "Example Shop",
      "hosts": ["example.com"],
      "measurement_ids": ["G-ABC123"],
      "script_ids": ["GTM-ABC123"],
      "site_domains": [],
      "origin_check": "log",
      "strip_user_data": false,
      "features": { "keep_cookies": ["google_ads", "meta"], "visitor_info": true },
      "enabled": true
    }
  ]
}
```

`features` sets the GA4 client's optional features for the tenant: `keep_cookies`, `click_id_aliases`, `visitor_info` and `bot_handling`, as described [below](#built-in-templates). A tenant must name at least one `G-` measurement id (`*` isn't accepted), and a host belongs to one tenant. The file is created when missing. Every instance re-reads it within a few seconds of a change, and a file that doesn't validate is ignored with a warning while the tenants already running keep serving. Each worker keeps one compiled runtime per tenant (up to 16) and rebuilds it when the tenant's settings change.

On Cloud Run the disk is ephemeral, so put the file on a mounted bucket (Cloud Storage volume mount) shared by all instances, e.g. `TENANTS_FILE=/data/tenants.json`. Each save goes through its own temporary file, so two instances saving at once never corrupt the file, but the later save wins: changes made on two instances in the same moment can lose one of them. Saved files are readable by their owner only. `TENANTS_BUCKET=<name> ./deploy/cloud-run.sh` sets this up (see [Deploy](#deploy-cloud-run-behind-cloudflare)).

### Admin console

With `ADMIN_TOKEN` set, `/admin` serves the console: a debugger for recent hits (each one's request, events, which tags fired or why not, their outcomes, outgoing requests and logs), the container in use, counters (requests, events, status classes, tag failures, tags skipped), and, with `TENANTS_FILE`, tenants to add, edit, disable and delete. It works without `TENANTS_FILE` and `INTEGRATIONS_FILE` too, for one container. The page needs the token to load any data; it keeps the token in the tab's session storage only. The same API is available for scripts, with `Authorization: Bearer <token>`:

| Method and path | Does |
| --- | --- |
| `GET /admin/api/overview` | Mode, version, uptime, workers, the `/system` counters and per-tenant counters. |
| `GET /admin/api/traces` | Recent hits, newest first. `after` and `before` (ids), `limit` (max 500), `tenant`, and `q`, text matched against the path, event and tag names, send hosts and errors. |
| `GET /admin/api/traces/<id>` | One hit: the request and the full `__debug` report, with `triggers`, each tag's firing conditions for each event. |
| `DELETE /admin/api/traces` | Empties the trace buffer. |
| `GET /admin/api/container` | The container (`?tenant=<id>` with tenants), whether it can be edited and its `revision`. Settings whose names look secret (`*token*`, `*secret*`, `*password*`, `*key`, header values) show as `[redacted]`. |
| `POST /admin/api/container/tags` | Adds a tag (`{ "revision", "tag": { "name", "template", "data", "fire_when", "enabled" } }`). |
| `PUT /admin/api/container/tags/<name>` | Replaces a tag. A different `tag.name` renames it. |
| `DELETE /admin/api/container/tags/<name>?revision=…` | Removes a tag. |
| `POST /admin/api/container/tags/order` | Sets the firing order (`{ "revision", "names": [...] }`, every tag once). |
| `PUT /admin/api/container/clients/<name>/features` | Sets a GA4 client's optional features (`{ "revision", "features": { "keep_cookies", "click_id_aliases", "visitor_info", "bot_handling" } }`); the rest of its data stays as it is. |
| `GET /admin/api/history` | Container changes and Integrations saves, newest first (`?limit=`, max 200). |
| `POST /admin/api/history/<id>/undo` | Puts the container back as it was before a change (`{ "revision" }`). |
| `GET /admin/api/pinned`, `POST`, `DELETE /admin/api/pinned/<id>` | Pinned test hits: list, pin a trace (`{ "trace", "label" }`), remove. |
| `GET /admin/api/alerts` | Open and resolved alerts, the last check and the settings. |
| `POST /admin/api/alerts/check` | Runs the alert checks. Takes the admin token or `CHECK_TOKEN`. |
| `PUT /admin/api/alerts/config` | Key events and Slack language (`{ "key_events": [...], "language": "en" \| "tr" }`). |
| `POST /admin/api/alerts/test` | Posts a test message to Slack. |
| `PUT /admin/api/container/transformations` | Replaces the transformations (`{ "revision", "transformations": [...] }`). |
| `POST /admin/api/container/transformations/preview` | What a rule (`{ "rule": {...} }`) would do to the events in the trace buffer: events matched and changed, and a few changes key by key. |
| `POST /admin/api/container/test` | Checks a tag's `fire_when` against the hits in the trace buffer without running anything: events matched, by event name and per condition. |
| `POST /admin/api/ai/explain/<id>?lang=en\|tr` | With the AI on: explains one trace (below). |
| `POST /admin/api/ai/transformation?lang=en\|tr` | With the AI on: writes a transformation from `{ "request": "..." }` and previews it (below). |
| `POST /admin/api/ai/setup?lang=en\|tr` | With the AI on: proposes a whole setup from `{ "request": "..." }`. |
| `PUT /admin/api/container/setup` | Replaces every tag and transformation (`{ "revision", "tags": [...], "transformations": [...] }`). |
| `POST /admin/api/ai/review?lang=en\|tr` | With the AI on: reviews a change's check result (`{ "edits", "replay" }`). |
| `POST /admin/api/ai/audit?lang=en\|tr` | With the AI on: audits the container against the trace buffer (`&tenant=<id>` with tenants). |
| `GET /admin/api/tenants` | Tenants and their counters. |
| `POST /admin/api/tenants` | Creates a tenant (JSON body as above). |
| `PUT /admin/api/tenants/<id>` | Replaces a tenant's settings. |
| `DELETE /admin/api/tenants/<id>` | Removes a tenant. |

With `CONTAINER_FILE`, tags can be added, changed, turned off, reordered and deleted from the console. A save rewrites the file's `tags` list (a temporary file renamed over it) and leaves every other key as written; the new container is checked like a startup first, and a change that doesn't load is refused with nothing written. Every instance re-reads the file within a few seconds of a change, so saves and hand edits apply without a restart, and a file that no longer loads is ignored with a warning. Each save sends the `revision` it was edited from and gets `409` if the file changed since. Settings shown as `[redacted]` keep their stored value when saved unchanged. The built-in container and tenants' containers can't be edited. In integrated mode the platform tags come after the file's own and are set up under Integrations.

Traces stay in the process's memory, so on Cloud Run each instance keeps its own. They drop the values of the `authorization`, `x-debug-token` and `x-proxy-secret` headers and keep only cookie names, but keep the query and up to 16 KB of the body, which can carry personal data: only people allowed to see the hits should hold the token, and `TRACE_BUFFER=0` keeps nothing. Plain user-provided data never reaches a trace or the `__debug` report: it shows as `[hashed before sending]` in events, in the request's query and body, in each event's raw hit and in forwarded URLs, and SHA-256 digests are kept. The GA4 tag forwards the hit to Google as the browser sent it, plain user data included, unless its `stripUserData` is on.

The admin routes work on any host and don't need `PROXY_SECRET`, because the edge proxy only forwards the tracking path and the token guards the rest. A wrong token is answered after half a second, and wrong tokens sent in parallel wait their turn, so an instance answers about eight guesses a second. Keep the token in Secret Manager like the other secrets, and don't route `/admin` through your Worker.

`/system` also lists the per-tenant counters, and counts hits for unknown hosts as `unknown_host`.

## Containers

A container file lists templates, clients and tags. Template paths are relative to the file. The built-in `ga4_client`, `ga4_tag` and `google_scripts_client` are always available.

```json
{
  "version": "1",
  "templates": { "purchase_logger": "templates/purchase_logger.js" },
  "clients": [
    { "name": "GA4", "template": "ga4_client", "data": { "extendGaCookie": true } }
  ],
  "tags": [
    { "name": "GA4", "template": "ga4_tag" },
    {
      "name": "Log purchases",
      "template": "purchase_logger",
      "fire_when": [{ "key": "event_name", "op": "equals", "value": "purchase" }]
    }
  ]
}
```

`fire_when` conditions all have to match. `key` is an event data key, dotted for nested values. `op` is one of `equals`, `not_equals`, `contains`, `starts_with`, `ends_with` and `matches_regex`. A tag without conditions fires for every event.

A tag with `"enabled": false` never fires and stays in the file. Tag names are unique, at most 100 characters.

A template can be a path, or `{ "file": ..., "permissions": {...} }`. A `.tpl` file exported from Google's template editor or the gallery works too: its code and its server permissions are read from the file.

See [`examples/container.json`](examples/container.json).

### Transformations

A container's `transformations` change event data on the server before tags see it, like the Transformations of Google's sGTM: fix event names and formats, add values, take out what a platform shouldn't get. Each rule runs for the events its `when` conditions match (same form as `fire_when`; none means every event) and for the tags it names in `tags` (none means every tag, including the Integrations ad platform tags), and its `steps` run in order. Rules run in their order, each on the event as the rules before it left it.

```json
"transformations": [
  {
    "name": "Normalise purchases",
    "steps": [
      { "op": "map", "key": "event_name", "table": { "Purchase": "purchase", "order_complete": "purchase" } },
      { "op": "format", "key": "value", "format": "number" },
      { "op": "set", "key": "currency", "value": "EUR", "only_if_missing": true }
    ]
  },
  {
    "name": "No email for TikTok",
    "tags": ["TikTok Events API"],
    "steps": [{ "op": "remove", "keys": ["user_data.email_address", "user_data.sha256_email_address"] }]
  }
]
```

| Step | What it does |
| --- | --- |
| `{ "op": "set", "key", "value", "only_if_missing" }` | Sets a key to any JSON value; with `only_if_missing`, only when it has none. |
| `{ "op": "copy", "from", "to" }` | Copies a value to another key. |
| `{ "op": "map", "key", "to", "table", "default", "ignore_case" }` | Looks the value up in `table` by its string form and writes the result to `to` (or back to `key`). No match keeps the value, or writes `default` when set. |
| `{ "op": "format", "key", "format" }` | `lower`, `upper`, `trim`, `number` (from `"49,90"` or `"1,234.50"`; anything else is left as it was) or `string`. |
| `{ "op": "remove", "keys" }` | Takes the keys out. |
| `{ "op": "keep", "keys" }` | Keeps only these keys and `event_name`. |

Keys are dotted for nested values (`user_data.address.city`). A rule can't change each element of an array such as `items`, do arithmetic or call other servers. A container holds at most 100 rules of at most 50 steps; a rule that names a tag the container doesn't have doesn't load, so a tag a rule names can't be deleted or renamed until the rule changes.

A tag fires on, and reads, the event as its rules left it, so a rule that maps `Purchase` to `purchase` makes a tag waiting for `purchase` fire. The client's own event is never changed: each tag gets its own copy, and the debugger shows the event as the client built it, with the rules that ran for each tag on the Triggers tab. The GA4 tag forwards the browser's hit rather than event data, so it carries the changes into the hit: a parameter whose value changed is rewritten, one whose value was removed is dropped, and a new value is added as an event parameter (`ep.` or `epn.`) or its standard parameter (`cu` for `currency`, `uid` for `user_id`, `dl`, `dr`, `dt`). An event no rule changed is forwarded exactly as sent.

With `CONTAINER_FILE`, the console's Transformations page lists the rules in plain words, turns them on and off, reorders, deletes and edits them, and previews a rule on the events in the trace buffer before it is saved. A save rewrites the file's `transformations` list (adding it when the file has none) and leaves everything else as written. With the AI on, "Ask the AI" writes a rule from a request in words (below).

### Template permissions and checks

Built-in templates are trusted. A template loaded from a file holds only the permissions it declares, and the server refuses to start when a template doesn't line up:

- it uses a name it never declared (`console`, `Date`, `window`, a typo). In Google's sandbox these don't exist either, so load them with `require` (`JSON`, `Math`, `Object` and `Promise` included);
- it `require`s an API that doesn't exist, or with a name that isn't a literal string;
- it `require`s an API whose permission it doesn't declare.

Permissions, with the APIs they cover. Parameters are checked on every call, and a call outside them throws:

| Permission | Parameters | APIs |
| --- | --- | --- |
| `send_http` | `urls`: patterns, `*` matches anything | `sendHttpRequest`, `sendHttpGet` |
| `read_event_data` | `keys`: patterns; `getAllEventData` needs `["*"]` | `getEventData`, `getAllEventData` |
| `get_cookies` | `names`: patterns | `getCookieValues` |
| `set_cookies` | `names`: patterns | `setCookie` |
| `read_request` | none | `getRequestMethod`, `getRequestPath`, `getRequestQueryString`, `getRequestQueryParameter(s)`, `getRequestHeader`, `getRequestBody`, `getRemoteAddress` |
| `access_response` | none | `setResponseStatus`, `setResponseHeader`, `setResponseBody`, `setPixelResponse` |
| `return_response`, `run_container`, `read_container_data`, `access_template_storage`, `logging` | none | `returnResponse`, `runContainer`, `getContainerVersion`, `templateDataStorage`, `logToConsole` |

Cookie domain, path and header restrictions from a `.tpl` are not enforced beyond the names. Permissions for APIs this server doesn't have yet (BigQuery, Firestore and the like) are ignored when a `.tpl` is read.

Upgrading: a custom template in a container file used to run with every API. Add its permissions as above, or the server won't load it.

## Ad platforms (Meta, TikTok, Pinterest, Google Ads)

Set `INTEGRATIONS_FILE` and `ADMIN_TOKEN`, then open `/admin`: under Integrations each platform has a card for its ids and access token, a switch, and a test mode. A save is written to the file and every instance applies it within a few seconds, so pixel ids and tokens change without a deploy. Secrets are never shown again after saving; leave a secret field empty to keep it.

| Platform | Needs | Tag |
| --- | --- | --- |
| Meta Conversions API | pixel id, access token, optional test event code | `meta_capi` |
| TikTok Events API | pixel code, access token, optional test event code | `tiktok_events` |
| Pinterest Conversions API | ad account id, access token, test mode | `pinterest_capi` |
| Google Ads conversions | account id, conversion action id, OAuth client id and secret, refresh token, optional event, manager account and test mode | `google_ads` |
| Snapchat, Reddit, LinkedIn and X Conversions API | see the [setup guide](docs/ad-platforms.md) | `snapchat_capi`, `reddit_capi`, `linkedin_capi`, `x_capi` |

GA4 events are mapped to each platform's own names (`purchase` to Meta `Purchase`, TikTok `CompletePayment` and Pinterest `checkout`, and so on). Events with no match are skipped. Each event carries the visitor's IP and user agent, the platform's cookies and click ids when present (`_fbp`, `_fbc` or `?fbclid`, `_ttp`, `?ttclid`, `?epik`), `value`, `currency` and `transaction_id`, and the `user_id` as a hashed `external_id`. To deduplicate against a platform's browser pixel, send the same `event_id` event parameter from both.

A tag with a required setting missing fails with a log line naming it, and a TikTok answer of 200 with a non-zero `code` counts as a failure, logged with TikTok's message.

**Google Ads** goes through Google's [Data Manager API](https://developers.google.com/data-manager/api/devguides/events/google-ads/online/send-events) (`events:ingest`), which Google recommends for conversion imports since the Google Ads API's `UploadClickConversions` stopped accepting new developer tokens in June 2026. The tag does two things:

- On every event whose page URL has `?gclid`, `?gbraid` or `?wbraid`, it keeps the click in a first-party cookie, `_sst_ads`, for 90 days, as sGTM's Conversion Linker does. Pages then don't need Google's own conversion linker.
- For its conversion event (`purchase` unless set otherwise), it sends the click id (from the page, `_sst_ads`, or Google's own `_gcl_aw` and `_gcl_gb` cookies) with `value`, `currency` and `transaction_id` (or `event_id`, or the client id and time, since Google requires one), and, as enhanced conversions, the hashed email (Gmail dots removed), phone (E.164) and a whole address when the user data has all four parts. A conversion with neither a click nor user data isn't sent, and isn't counted as a failure.

It needs a conversion action in the account, an OAuth client and a refresh token for a Google user with access to the account and the `https://www.googleapis.com/auth/datamanager` scope; the access token is fetched with it and kept until a minute before it expires. Test mode sends `validateOnly`, so Google checks each conversion without importing it. One integration sends one conversion action; a container file can add a `google_ads` tag per action, each with its own `conversion_event`. Conversions take up to a few hours to show in Google Ads.

Consent Mode v2: a tag sends only when the hit grants both `ad_storage` and `ad_user_data`. The server reads them from the hit's `gcd` parameter and falls back to `gcs` for ad storage. A denied signal always skips the event. So does a signal the hit doesn't carry (for example a hit with `gcs` but no `gcd`, from an older tag setup), unless the platform's "Send when the hit has no consent signals" switch is on, which is meant for sites without Consent Mode.

A skipped event is reported as `skipped`, not as a success, with its reason in the request's log line, for example `tag[Meta Conversions API]=skipped(consent:ad_user_data=denied)`. Reasons are `unmapped_event`, `consent:<signal>=denied` and `consent:<signal>=missing`. `/system` and the admin counters count skipped tags apart from successes and failures.

Snapchat, Reddit, LinkedIn and X have their own [setup guide](docs/ad-platforms.md). The tags are tested against stand-ins for the platforms, not against real accounts, so use each platform's test mode first where it has one (LinkedIn and X don't).

The file holds the tokens in plain text. Keep the bucket private, and give only the service account access to it.

## Ad cookie keeper

The GA4 client re-sets `_fbp`, `_fbc` and `_ttp` from the server with a 390-day lifetime, so Safari's 7-day cap on script-set cookies doesn't drop the Meta and TikTok visitor. It runs only when ad storage is granted, and `keepAdCookies: false` turns it off. The [guide](docs/ad-cookie-keeper.md) covers what it sets, the consent rules and how to check the cookies.

## BigQuery export

The `bigquery_export` tag writes each consented event as one row to a BigQuery table, with an admin card next to the ad platforms. The [setup guide](docs/bigquery-export.md) covers the table, the service account, the server variables and how to check that rows arrive.

### Enrichment from your own server

With an enrichment endpoint set, each tag calls it for every conversion: `GET <url>?user_id=<id>` (or `client_id=`), with an optional secret header. It should answer JSON such as:

```json
{ "email": "jane@example.com", "phone": "+491701234567", "first_name": "Jane", "last_name": "Doe",
  "city": "Berlin", "state": "be", "zip": "10115", "country": "de", "external_id": "cust-7" }
```

Every field is optional and may be plain or already a SHA-256 hex digest. Plain values are normalised (trimmed and lower-cased, phone numbers reduced to digits) and hashed before they leave the server, and each platform gets the fields it accepts (TikTok: email, phone, external id). Answers are kept for five minutes per visitor, so a busy visitor doesn't hit your server on every event; two tags running for the same first event may each ask once. A slow (2 s limit) or failing endpoint never stops a conversion: it is sent without the extra fields. The endpoint must be `https://`, and addresses inside your own network are refused like any other outgoing send.

**User data sent with the event.** Turn on "Use user data sent with the event" (`event_user_data` in the enrichment settings) and the tags also use the user-provided data the page sends with GA4 events, with or without an endpoint. The GA4 client puts it in event data as Google's sGTM does, under `user_data`: `email_address`, `phone_number`, `address.first_name`, `last_name`, `street`, `city`, `region`, `postal_code`, `country`, and `sha256_email_address`, `sha256_phone_number`, `address.sha256_first_name`, `address.sha256_last_name`. It reads `ep.user_data.*` parameters (`email` and `phone` count as `email_address` and `phone_number`) and the Google tag's `em` envelope (`tv.1~em.<hash>~ph.<hash>…`), whose base64 digests become hex. Plain values are normalised and hashed like the endpoint's, digests are sent as they are, and where the endpoint answers a field too, its answer wins. A container file's own `meta_capi`, `tiktok_events` or `pinterest_capi` tag turns it on with `"eventUserData": true` in its `data`. Events the server makes itself, such as the Shopify order webhook's (marked with `x-sst-source`), carry the buyer's own details: the tags use those without the switch, and they win over the endpoint's answer.

The Google tag hashes phone numbers in E.164 form (with `+`), which Meta and Pinterest may not match against their digits-only hashes; send the plain number (or a hash of the digits) for those.

## Shopify orders

Paid Shopify orders can be sent to the ad tags and the BigQuery export from the server, as purchases. The [setup guide](docs/shopify-webhook.md) covers the webhook in Shopify admin, the signing secret, the admin settings and how to check that orders arrive.

## Built-in templates

**`ga4_client`** claims GA4 Measurement Protocol v2 hits (`/g/collect`, `/mp/collect`, `/j/collect` with `v=2`), runs the container once per event (a batched `POST` holds several) and answers `204` when every tag has finished.

- `extendGaCookie` (default `true`): re-sets the `_ga` cookie, and the property's `_ga_<id>` session cookie, from the server with a two-year lifetime, so browsers that cap script-set cookies (Safari) keep the same client id. It is skipped when the hit's Consent Mode state says analytics storage is denied. Set it to `false` to turn it off.
- `allowedMeasurementIds`: GA4 ids (`G-…`) hits are accepted for. Hits for other ids are left unclaimed, and a batched line that switches to another id is dropped, so nobody can send their own traffic through your server. `["*"]` accepts every id, and so does a missing list, which the server warns about when it loads a container file.
- `siteDomains`: domains whose pages may send hits, subdomains included. Default: the server's own host.
- `originCheck` (default `log`): a browser hit from your site carries an `Origin` or `Referer` on one of the site domains. With `enforce`, other hits get `403` and aren't forwarded. With `log` they're forwarded, and the request's log line shows `origin_check=would_reject` with the hosts, so you can see what enforcing would drop before turning it on. `off` skips the check.
- `keepCookies`: cookie groups to re-set from the server with each platform's own lifetime, so Safari's 7-day cap on cookies set by scripts doesn't cut them short: `google_analytics`, `google_ads` (`_gcl_aw`, `_gcl_gb`, `_gcl_dc`, `_gcl_au`, 90 days), `meta` (`_fbp`, `_fbc`) and `tiktok` (`_ttp`), both kept by default with `keepAdCookies` (below), `pinterest` (`_epik`, `_pin_unauth`), `microsoft` (`_uetmsclkid`, `_uetvid`), `snapchat` (`_scid`), `linkedin` (`li_fat_id`), or any cookie's own name (kept a year). Ad cookies are re-set only when the hit says ad storage is granted (Consent Mode `gcd`, or `gcs` without it); a hit with no consent signal leaves them alone, and a named cookie also needs analytics storage not denied. The Google Analytics cookies follow `extendGaCookie`.
- `keepAdCookies` (default `true`): keeps Meta's `_fbp` and `_fbc` and TikTok's `_ttp` for 390 days, and makes `_fbc` from `?fbclid` on the landing page, under the same consent rule. `false` turns it off; `keepCookies` can still name them.
- `clickIdAliases`: click ids to restore from another parameter, e.g. `{ "gclid": "gc_id" }`. Safari and Brave strip known click ids from links in some modes; put the alias in the ad's URL too (in Google Ads, the final URL suffix `gc_id={gclid}`), and a `page_location` that has the alias but no click id gets the click id added for every tag. A click id that's already there wins.
- `visitorInfo` (default `false`): adds what the request says about the visitor to every event, under keys tags and transformations can use but that nothing forwards: `x-sst-country`, `x-sst-region`, `x-sst-city` and `x-sst-postal-code` from the edge proxy's location headers (the Cloudflare Worker sends `x-sst-geo-*`; Cloudflare's own `cf-ipcountry`, Vercel's `x-vercel-ip-*`, CloudFront's `cloudfront-viewer-*` and App Engine's `x-appengine-*` work too), `x-sst-device` (`mobile`, `tablet` or `desktop`), `x-sst-browser`, `x-sst-os`, `x-sst-ai-source` when the visitor came from an AI assistant (`chatgpt`, `perplexity`, `claude`, `gemini`, `copilot`, `deepseek`, `meta_ai`, `grok`, `mistral`, `you`, `poe`; by referrer or `utm_source`) and `x-sst-ai-agent` when an AI agent browses for someone (by user agent, or ChatGPT's signed `Signature-Agent` header). To report on them in GA4, copy them with a transformation, e.g. `{ "op": "copy", "from": "x-sst-ai-source", "to": "ai_source" }`.
- `botHandling` (default `drop`): hits from bots, crawlers, headless browsers, automation tools and HTTP libraries (curl, wget, Python, Go, Java, Node and the like), or with no user agent, are answered `204` and not forwarded. With `flag`, every hit is forwarded and its events get `x-sst-bot` (true at a score of 50 or more), `x-sst-bot-score` (0 to 100: 100 for a known bot, points for a missing `Accept-Language`, a Chrome without fetch metadata headers, a headless browser) and `x-sst-bot-reason`, so a tag's trigger can leave bots out. AI agents whose user agent says "bot" (such as ChatGPT-User) are bots here too. Send Measurement Protocol calls from your backend straight to Google.
- CORS headers are only returned to your site's own origins.

The client always uses the address the request came from as the visitor IP. A `_uip` sent with the hit is ignored. So are event parameters whose names start with `x-` (`ep.x-…`, `epn.x-…`): those keys belong to the server, for consent, visitor info and the mark of a server source, and a hit can't set them.

**`google_scripts_client`** serves `/gtm.js` and `/gtag/js` from Google for the allowed ids, so the page can load them from its own domain (`<script src="https://your-site/k7q2/gtm.js?id=GTM-…">`). It also serves `/gtag/destination`, `/debug/bootstrap` and the service worker helper GTM opens when it runs first-party (`/_/service_worker/<version>/sw_iframe.html` and `sw.js`), and points the scripts' own loader host at this server, so the GA4 tag inside a container loads from your domain too. Browsers and blockers that refuse `www.googletagmanager.com`, such as Safari private browsing, then still load the tags. Scripts are cached in memory, and a cached copy is served if Google can't be reached.

- `allowedIds`: ids the server will serve. Requests for any other id are left unclaimed, so the server isn't an open proxy.
- `cacheSeconds`: how long a fetched script is reused. Default `900`.

Only the query parameters Google's script URLs use (`id`, `l`, `cx`, `gtm`, `gtm_auth`, `gtm_preview`, `gtm_cookies_win`, `src`, `cond`) with short plain values are passed on, sorted, so extra parameters can't bypass the cache.

**`google_ads`** sends conversions to Google Ads and keeps ad clicks in a first-party cookie, as described under [Ad platforms](#ad-platforms-meta-tiktok-pinterest-google-ads). Settings: `customer_id`, `conversion_action_id`, `client_id`, `client_secret`, `refresh_token`, and optionally `conversion_event`, `login_customer_id`, `test_mode` and `eventUserData`.

**`ga4_tag`** forwards each event's original hit to Google, adding the visitor's IP (`_uip`) and user agent (`_ua`). Forwarding the hit as sent means no parameter is lost in a mapping.

- `endpoint`: collection URL. Default `https://www.google-analytics.com/g/collect`.
- `stripUserData` (default `false`): drops plain user-provided data before forwarding: `ep.user_data.*` and `epn.user_data.*` parameters whose last key isn't `sha256_…` (or the tag's own `_tag_mode`). SHA-256 digests and the Google tag's `em` envelope are kept. GA4 doesn't accept personal data in event parameters, so turn it on when pages send plain user data for the ad platforms. In the console it is a switch on the GA4 tag and on each site; for the built-in container, `STRIP_USER_DATA=1`.

## Writing templates

A template is the body of a function that receives `data`, in strict mode. Load every API with `require`:

```js
const getEventData = require('getEventData');
const sendHttpRequest = require('sendHttpRequest');

sendHttpRequest('https://example.com/collect', { method: 'POST' }, JSON.stringify({ e: getEventData('event_name') }))
  .then(data.gtmOnSuccess, data.gtmOnFailure);
```

Tags must call `data.gtmOnSuccess()` or `data.gtmOnFailure()`. A tag that decides not to send can call `data.gtmOnSkipped(reason)` instead (not in sGTM): it ends the tag like a success, but the log line and counters show it as skipped with the reason. A tag that calls neither is marked failed when nothing else is left to run.

## API coverage

| Group | APIs |
| --- | --- |
| Request | `getRequestMethod`, `getRequestPath`, `getRequestQueryString`, `getRequestQueryParameter`, `getRequestQueryParameters`, `getRequestHeader`, `getRequestBody`, `getRemoteAddress`, `getCookieValues`, `isRequestMpv1`, `extractEventsFromMpv1`, `isRequestMpv2`, `extractEventsFromMpv2` |
| Response | `claimRequest`, `setResponseStatus`, `setResponseHeader`, `setResponseBody`, `setPixelResponse`, `setCookie`, `returnResponse` |
| Events and container | `runContainer`, `addEventCallback`, `getAllEventData`, `getEventData`, `getClientName`, `getContainerVersion` |
| Messages between templates | `sendMessage`, `addMessageListener`, `hasMessageListener` |
| Outgoing HTTP | `sendHttpRequest`, `sendHttpGet` (Promise and callback forms) |
| Strings, URLs, regex | `parseUrl`, `computeEffectiveTldPlusOne`, `encodeUri`, `encodeUriComponent`, `decodeUri`, `decodeUriComponent`, `createRegex`, `testRegex` |
| Types | `getType`, `makeString`, `makeNumber`, `makeInteger`, `makeTableMap` |
| Encoding and hashing | `toBase64`, `fromBase64`, `sha256`, `sha256Sync`, `hmacSha256` |
| Other | `getTimestampMillis`, `getTimestamp`, `generateRandom`, `callLater`, `logToConsole`, `templateDataStorage`, `JSON`, `Math`, `Object`, `Promise` |

Notes on the newer APIs:

- `hmacSha256(input, key, onSuccess, onFailure, options)` takes the key as a plain string and returns base64, or hex with `{ outputEncoding: 'hex' }`.
- `extractEventsFromMpv1` turns a Universal Analytics `/collect` hit (`v=1`, from the query or a form-encoded body) into one event: `pageview` becomes `page_view`, `screenview` becomes `screen_view`, and an `event` hit is named after its action (`ea`). Custom dimensions and metrics stay as `cd1`, `cm1`.
- `addEventCallback(callback)` is for tags. The callback runs once every tag of the event has reported, as `callback(containerId, { tags: [{ id, status, executionTime }] })`. `status` is `success`, `failure`, `exception` or `timeout` (a skipped tag reports `success`, as sGTM has no such status); `id` is the tag's name and `executionTime` is always `0`.
- `sendMessage(type, message)` reaches every listener of that type from `addMessageListener(type, callback)`, which is called as `callback(type, message)`. Delivery is not instant: listeners run after the sending code finishes. A run delivers at most 1000 messages.

Registered but not built yet (calling them throws): `sendEventToGoogleAnalytics`, `sendPixelFromBrowser`, `getGoogleScript`, `getGoogleAuth`, `BigQuery`, `Firestore`.

Not yet: loading a container exported from Google Tag Manager.

## Deploy: Cloud Run behind Cloudflare

1. `PROJECT_ID=<your project> ./deploy/cloud-run.sh` builds the image with Cloud Build and deploys it to Cloud Run in `europe-west3` (Frankfurt), scaling to zero when idle. It keeps `PROXY_SECRET` and `DEBUG_TOKEN` in Secret Manager, and runs the service as its own service account (`<service>-run`), which can read those two secrets and nothing else.
2. Put the printed service URL in [`deploy/cloudflare/wrangler.toml`](deploy/cloudflare/wrangler.toml) as `ORIGIN_URL`, run `npx wrangler deploy` there, and set the same `PROXY_SECRET` on the Worker. The Worker serves the server at `https://your-site/k7q2/*`: change `/k7q2` (the route and `PATH_PREFIX`) to a short path of your own. Ad blockers match words like `analytics`, `metrics`, `tracking`, `gtm`, `gtag` or `ad` in hostnames and paths, so a meaningless one lasts longer; the Setup check warns about such words. The Worker also passes the visitor's location from Cloudflare (`x-sst-geo-country`, `-region`, `-city`, `-postal-code`) for the GA4 client's `visitorInfo`, after dropping any a visitor sent.
   The script sets `ORIGIN_CHECK=enforce`, so hits that don't come from your site's pages get `403`. To watch first, deploy with `ORIGIN_CHECK=log` and look for `origin_check=would_reject` in the service's logs: `gcloud logging read 'resource.type="cloud_run_revision" AND resource.labels.service_name="tagging-server" AND textPayload:"origin_check=would_reject"' --project <your project> --freshness 7d --limit 50`.
3. Optional: `INTEGRATIONS_BUCKET=<name> PROJECT_ID=... ./deploy/cloud-run.sh` also creates a Cloud Storage bucket for the integrations file and the shared state, mounts it at `/data` (`STATE_DIR=/data/state`), creates `ADMIN_TOKEN` and `CHECK_TOKEN` in Secret Manager, sets `ADMIN_URL`, creates a Cloud Scheduler job that runs the alert checks every 5 minutes, and prints the admin page address (`<service url>/admin`, opened directly, not through the Worker). Add `SLACK_WEBHOOK_URL=https://hooks.slack.com/...` to post alerts to Slack; it is kept in Secret Manager, so later deploys don't need it again. The volume, scheduler and Slack steps have not been run against a real project yet. For [multi-tenant hosting](#multi-tenant-hosting) use `TENANTS_BUCKET=<name>` instead: the bucket then holds `tenants.json` and the shared state, edited from the same admin page, and `SCRIPT_IDS`, `MEASUREMENT_IDS`, `SITE_DOMAINS` and `ORIGIN_CHECK` are set per tenant there. The two can't be combined.
4. In your web GTM container, set the Google tag's `server_container_url` to `https://your-site/k7q2`.
5. The Worker limits each visitor IP to 100 requests per 10 seconds (`[[ratelimits]]` in `wrangler.toml`) and answers the rest with `429` at the edge, so floods never reach Cloud Run. Bot Fight Mode (Cloudflare dashboard, Security > Bots) can stop simple bots too, but it applies to the whole site, not only your path, and on the Free plan no rule can exempt a path from it, so a server-to-server caller (a webhook, an uptime check) may be challenged. The server already drops hits from bots and HTTP libraries by their user agent.

Cloud Run's own request logs keep each full request URL, including hit parameters, for 30 days. The server's log line drops the query. To keep the platform log from storing hit parameters, exclude `run.googleapis.com/requests` for this service in Cloud Logging, or mention it in your privacy notice.

### Upgrading

Deploy the Worker from the same release before the server, and keep its `PATH_PREFIX`. The server builds the address of the Google scripts it serves (`gtm.js` points the Google tag at it) from the forwarded host and the Worker's `x-forwarded-prefix` header. Without that header it assumes the server runs at the root. An older Worker that doesn't send the header leaves the Google tag loading from `https://your-site/gtag/js`, which returns `404`, and GA4 stops.

To check a new server version before it takes traffic, deploy it with `--no-traffic --tag next` and request `gtm.js` from the tag's address with the headers the Worker sends (`x-proxy-secret`, `x-forwarded-host`, `x-forwarded-prefix`). The Google tag's address in the answer (`"3":"your-site/k7q2"`) must include your path. `gtm.js` is cached for 4 hours, by Cloudflare and by browsers, so a wrong copy outlives a rollback. Cloudflare keeps the Worker's fetches under the Cloud Run URL, so purge that URL, or switch `ORIGIN_URL` to the service's other address, to drop it.

## Behind Cloudflare

`getRemoteAddress()` and the GA4 events use the first of `CF-Connecting-IP`, `X-Real-IP`, the left-most `X-Forwarded-For` entry and the socket address. The location headers `visitorInfo` reads are trusted the same way. Make sure only your proxy can reach the server (`PROXY_SECRET`), so visitors can't set these headers themselves.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## About

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="brand/assets/master/analitik-isler-dark.svg">
  <img alt="Analitik İşler" src="brand/assets/master/analitik-isler-light.svg" height="22">
</picture>

ai tagging server is made by Analitik İşler, one of its ai tools. “ai” stands for two things: AI, and the initials of analitik işler. The logos and colours are in [`brand/`](brand/BRAND.md).

## License

[MIT](LICENSE)
