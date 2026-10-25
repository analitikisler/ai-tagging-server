# BigQuery export

The `bigquery_export` tag writes each event as one row of a BigQuery table, using streaming inserts (`tabledata.insertAll`). It appears on `/admin` as a fourth card, **BigQuery export**, next to Meta, TikTok and Pinterest, with the same Save, on/off and "Send a test event" controls.

This guide covers what to set up in Google Cloud, which settings the server needs, and how to check that rows arrive.

## What gets written

- **Consent.** A row is written only when the hit grants `analytics_storage` (read from `gcd`, falling back to `gcs`). A denied signal skips the event with `consent:analytics_storage=denied`. A hit with no consent signals is skipped with `consent:analytics_storage=missing`, unless "Export when the hit has no consent signals" is on. Ad consent signals don't matter for this tag.
- **IP addresses.** The visitor's IP address is never written. `ip_override` and the raw hit are dropped as well.
- **Event filter.** Events not in "Events to export" are skipped with `event_not_selected`. An empty list exports every event.
- **Enrichment.** The enrichment endpoint is not called for this tag.
- **Duplicates.** Each row gets an `insertId` (a hash of the row), so a retried send doesn't add the same row twice.
- **Errors.** Insert errors fail the tag and appear in the log line, for example `BigQuery insert failed: notFound Not found: Table ...`.

Costs: streaming inserts are billed per row, with a 1 KB minimum per row. Each event is one row.

## 1. Create the table

The table needs the columns the server writes. The schema is in [`deploy/bigquery-schema.json`](../deploy/bigquery-schema.json).

The easiest route is the deploy script. Set `BIGQUERY_TABLE` to `dataset.table` and `cloud-run.sh` does the rest:

```sh
PROJECT_ID=my-project INTEGRATIONS_BUCKET=my-bucket BIGQUERY_TABLE=analytics.events ./deploy/cloud-run.sh
```

The script enables the BigQuery API, creates the dataset and table if they are missing (day-partitioned on `event_timestamp`), and grants the runtime service account `roles/bigquery.dataEditor` on that table only. `BIGQUERY_LOCATION` sets the location of a newly created dataset and defaults to `REGION` (europe-west3).

To create the table by hand instead:

```sh
bq mk --dataset --location=europe-west3 my-project:analytics
bq mk --table --time_partitioning_field event_timestamp \
  --schema deploy/bigquery-schema.json my-project:analytics.events
```

Columns:

| Column | Type | Filled from |
| --- | --- | --- |
| `event_timestamp` | TIMESTAMP (required) | Server time of the hit |
| `event_name` | STRING | `event_name` |
| `event_id` | STRING | `event_id` |
| `measurement_id` | STRING | GA4 `tid` |
| `client_id` | STRING | `client_id` |
| `user_id` | STRING | `user_id` |
| `session_id` | STRING | `ga_session_id` |
| `page_location` | STRING | |
| `page_referrer` | STRING | |
| `page_title` | STRING | |
| `value` | FLOAT64 | |
| `currency` | STRING | |
| `transaction_id` | STRING | |
| `items` | STRING (JSON text) | GA4 items array |
| `user_agent` | STRING | |
| `consent` | STRING (JSON text) | For example `{"ad_storage":"granted","analytics_storage":"granted",...}` |
| `event_data` | STRING (JSON text) | Every event field except the IP and the raw hit |

Query the JSON-text columns with `JSON_VALUE`, for example `JSON_VALUE(event_data, '$.page_title')`. A table can keep only some of these columns: unknown columns are ignored, so the server doesn't fail when a column is missing from the table.

## 2. Give the service account access

The server writes with a Google access token for a service account. That account needs `roles/bigquery.dataEditor` on the table, or on the dataset.

- **On Cloud Run**, the account is the service's runtime account, `tagging-server-run@<project>.iam.gserviceaccount.com`. `cloud-run.sh` grants it access to `BIGQUERY_TABLE` when that variable is set.
- **Outside Google Cloud** (for example a VPS), create a service account, grant it `roles/bigquery.dataEditor` on the table, and create a JSON key for it. Keep the key file private, and give the server its path with `GOOGLE_APPLICATION_CREDENTIALS`. Only `type: service_account` keys are accepted.

Grant access on the table where you can, not on the whole project. The server requests only the `https://www.googleapis.com/auth/bigquery.insertdata` scope.

## 3. Server settings

| Variable | Values | Default | Meaning |
| --- | --- | --- | --- |
| `GOOGLE_AUTH` | `auto`, `metadata`, `key`, `off` | `auto` | Where the Google access token comes from. `auto` uses the key file when `GOOGLE_APPLICATION_CREDENTIALS` is set, else the metadata server when running on Cloud Run (`K_SERVICE` is set), else no credentials. `metadata` uses the metadata server (Cloud Run, GCE). `key` uses the key file and requires it. `off` sends no credentials, so inserts fail with "no Google credentials". |
| `GOOGLE_APPLICATION_CREDENTIALS` | path | unset | Service account key JSON file, for hosts outside Google Cloud. |

A bad key file or an unknown `GOOGLE_AUTH` value stops the server at startup, with a `GOOGLE_AUTH: ...` message. On a successful start the log says either `BigQuery inserts use the metadata server's service account` or `BigQuery inserts use the key of <email>`.

## 4. Turn the export on

1. Open `/admin` (set `ADMIN_TOKEN` and `INTEGRATIONS_FILE` first, as described in [ad platforms](../README.md#ad-platforms-meta-tiktok-pinterest)).
2. In the **BigQuery export** card, enter:
   - **Project ID**: the Google Cloud project that owns the table.
   - **Dataset ID** and **Table ID**: letters, digits, `_` and `-`. A partition decorator such as `events$20261015` is accepted.
   - **Events to export** (optional): comma-separated GA4 event names, for example `purchase, generate_lead`. Leave it empty to export every event.
   - **Export when the hit has no consent signals** (optional, off by default): only for sites without Consent Mode.
3. Save, then switch the card on.

The server refuses an ID that doesn't look valid when you save, with the message `BigQuery export: odd project, dataset or table ID`.

## 5. Check that rows arrive

Send a test event with **Send a test event** on the card, or load a page on your site that fires one of the events you export. Then query the table:

```sh
bq query --nouse_legacy_sql \
  'SELECT event_timestamp, event_name, measurement_id, client_id
   FROM `my-project.analytics.events`
   WHERE event_timestamp > TIMESTAMP_SUB(CURRENT_TIMESTAMP(), INTERVAL 1 HOUR)
   ORDER BY event_timestamp DESC
   LIMIT 10'
```

Rows written by streaming inserts can take a short while to show in queries. If nothing appears:

- **`BigQuery insert failed: notFound`**: the project, dataset or table ID is wrong, or the table doesn't exist yet.
- **A permission error in the `BigQuery insert failed` message (403)**: the service account lacks `roles/bigquery.dataEditor` on the table.
- **`no Google credentials`**: `GOOGLE_AUTH` is `off`, or the server has no credentials for this environment.
- **`skipped(consent:analytics_storage=…)`** in the log line: the hit didn't grant analytics storage. Check the Consent Mode settings on the site, or turn on "Export when the hit has no consent signals" if the site has no Consent Mode.
- **`skipped(event_not_selected)`**: the event isn't in "Events to export".

The request's log line shows each tag's outcome. `/system` counts skipped tags apart from successes and failures.

## Custom templates

A container template can write to BigQuery too: `require('BigQuery').insert({projectId, datasetId, tableId}, rows, {ignoreUnknownValues, skipInvalidRows})`, as in sGTM. `projectId` is required here. The call resolves with nothing, or rejects with a list of `{reason, message, row?}` entries.

A template loaded from a file needs the `access_bigquery` permission, which lists the tables it may write to:

```json
{ "access_bigquery": { "tables": ["my-project.analytics.events"] } }
```

Use `project.dataset.*` to allow every table in a dataset. A `.tpl` export works too, through its `allowedTables`. Templates never see the Google token: `getGoogleAuth` is unavailable on purpose.
