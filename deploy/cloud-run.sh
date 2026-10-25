#!/usr/bin/env bash
# Builds the image with Cloud Build and deploys it to Cloud Run.
#
#   PROJECT_ID=my-project ./deploy/cloud-run.sh
#
# Scales to zero when idle (request-based billing, so a low-traffic site
# stays within the free tier). PROXY_SECRET and DEBUG_TOKEN are kept in
# Secret Manager; the script creates them on the first run.
#
# Optional: INTEGRATIONS_BUCKET=my-bucket-name turns on the admin console at
# <service url>/admin, where ad platforms (Meta, TikTok, Pinterest) are set up
# without a deploy. The bucket holds their settings and the shared state
# (counts, history, alerts, pinned hits) and is created if missing;
# ADMIN_TOKEN and CHECK_TOKEN are created in Secret Manager like the other
# secrets. A Cloud Scheduler job runs the alert checks every 5 minutes with
# CHECK_TOKEN.
#
# Optional, with a bucket: SLACK_WEBHOOK_URL=https://hooks.slack.com/...
# posts alerts to Slack. It is stored in Secret Manager; later deploys keep
# it without passing it again.
#
# Optional: BIGQUERY_TABLE=dataset.table creates that table, if missing, with
# the BigQuery export's columns (deploy/bigquery-schema.json, partitioned by
# day) and lets the service write rows to it and nothing else. The export is
# then turned on at /admin. BIGQUERY_LOCATION sets where a new dataset lives
# (REGION by default).
#
# Or TENANTS_BUCKET=my-bucket-name hosts several sites from one service: the
# admin page then edits the tenants file (a container per hostname) kept in
# that bucket. SCRIPT_IDS, MEASUREMENT_IDS, SITE_DOMAINS and ORIGIN_CHECK are
# set per tenant there instead, and the two buckets can't be combined.
set -euo pipefail

PROJECT_ID="${PROJECT_ID:?set PROJECT_ID}"
REGION="${REGION:-europe-west3}"
SERVICE="${SERVICE:-tagging-server}"
REPO="${REPO:-tagging-server}"
# GTM container and Google tag ids whose scripts the server serves first-party,
# e.g. SCRIPT_IDS=GTM-ABC123,G-XYZ789.
SCRIPT_IDS="${SCRIPT_IDS:-}"
# GA4 properties hits are accepted for. Empty means the G- ids in SCRIPT_IDS.
MEASUREMENT_IDS="${MEASUREMENT_IDS:-}"
# Domains whose pages may send hits (subdomains included). Empty means the
# site's own host.
SITE_DOMAINS="${SITE_DOMAINS:-}"
# What happens to hits that don't come from your site: enforce (refused with
# 403, the default), log (forwarded, logged as origin_check=would_reject) or
# off.
ORIGIN_CHECK="${ORIGIN_CHECK:-enforce}"
INTEGRATIONS_BUCKET="${INTEGRATIONS_BUCKET:-}"
TENANTS_BUCKET="${TENANTS_BUCKET:-}"
BIGQUERY_TABLE="${BIGQUERY_TABLE:-}"
BIGQUERY_LOCATION="${BIGQUERY_LOCATION:-$REGION}"
# Hits one instance runs at once. Each needs a JavaScript worker until the
# platforms answer, so Cloud Run's concurrency is set to the same number:
# extra hits then wait at Cloud Run, which starts another instance, instead of
# queueing unseen inside a busy one.
WORKERS="${WORKERS:-16}"
IMAGE="${REGION}-docker.pkg.dev/${PROJECT_ID}/${REPO}/${SERVICE}:$(git rev-parse --short HEAD)"

if [ -n "$INTEGRATIONS_BUCKET" ] && [ -n "$TENANTS_BUCKET" ]; then
  echo "set INTEGRATIONS_BUCKET or TENANTS_BUCKET, not both" >&2
  exit 1
fi
if [ -n "$TENANTS_BUCKET" ] && [ -n "${SCRIPT_IDS}${MEASUREMENT_IDS}${SITE_DOMAINS}" ]; then
  echo "SCRIPT_IDS, MEASUREMENT_IDS and SITE_DOMAINS are set per tenant with TENANTS_BUCKET" >&2
  exit 1
fi
if [ -n "$BIGQUERY_TABLE" ] && ! [[ "$BIGQUERY_TABLE" =~ ^[A-Za-z0-9_]+\.[A-Za-z0-9_-]+$ ]]; then
  echo "BIGQUERY_TABLE must be dataset.table" >&2
  exit 1
fi
case "$ORIGIN_CHECK" in
  enforce | log | off) ;;
  *) echo "ORIGIN_CHECK must be enforce, log or off" >&2; exit 1 ;;
esac

cd "$(dirname "$0")/.."

gcloud services enable run.googleapis.com cloudbuild.googleapis.com \
  artifactregistry.googleapis.com secretmanager.googleapis.com iam.googleapis.com \
  cloudscheduler.googleapis.com \
  --project "$PROJECT_ID"

gcloud artifacts repositories describe "$REPO" --location "$REGION" --project "$PROJECT_ID" >/dev/null 2>&1 \
  || gcloud artifacts repositories create "$REPO" --repository-format docker \
       --location "$REGION" --project "$PROJECT_ID"

# The service runs as its own service account, which can read the two secrets
# and nothing else. (The default compute account has Editor on the project.)
RUNTIME_SA="${SERVICE}-run@${PROJECT_ID}.iam.gserviceaccount.com"
gcloud iam service-accounts describe "$RUNTIME_SA" --project "$PROJECT_ID" >/dev/null 2>&1 \
  || gcloud iam service-accounts create "${SERVICE}-run" --project "$PROJECT_ID" \
       --display-name "${SERVICE} runtime"

SECRET_NAMES="PROXY_SECRET DEBUG_TOKEN"
SECRET_ARGS="PROXY_SECRET=${SERVICE}-proxy-secret:latest,DEBUG_TOKEN=${SERVICE}-debug-token:latest"
EXTRA_ARGS=()
ENV_VARS="WORKERS=${WORKERS}@SCRIPT_IDS=${SCRIPT_IDS}@MEASUREMENT_IDS=${MEASUREMENT_IDS}@SITE_DOMAINS=${SITE_DOMAINS}@ORIGIN_CHECK=${ORIGIN_CHECK}"
SETTINGS_BUCKET=""
if [ -n "$INTEGRATIONS_BUCKET" ]; then
  SETTINGS_BUCKET="$INTEGRATIONS_BUCKET"
  VOLUME="integrations"
  ENV_VARS="${ENV_VARS}@INTEGRATIONS_FILE=/data/integrations.json"
elif [ -n "$TENANTS_BUCKET" ]; then
  SETTINGS_BUCKET="$TENANTS_BUCKET"
  VOLUME="tenants"
  ENV_VARS="WORKERS=${WORKERS}@TENANTS_FILE=/data/tenants.json"
fi
if [ -n "$SETTINGS_BUCKET" ]; then
  SECRET_NAMES="$SECRET_NAMES ADMIN_TOKEN CHECK_TOKEN"
  SECRET_ARGS="$SECRET_ARGS,ADMIN_TOKEN=${SERVICE}-admin-token:latest,CHECK_TOKEN=${SERVICE}-check-token:latest"
  # Shared state on the same bucket. Idle instances are paused on Cloud Run,
  # so the scheduler runs the alert checks.
  ENV_VARS="${ENV_VARS}@STATE_DIR=/data/state@ALERT_INTERVAL_S=0"
  SLACK_SECRET="${SERVICE}-slack-webhook-url"
  if [ -n "${SLACK_WEBHOOK_URL:-}" ]; then
    gcloud secrets describe "$SLACK_SECRET" --project "$PROJECT_ID" >/dev/null 2>&1 \
      || gcloud secrets create "$SLACK_SECRET" --replication-policy automatic --project "$PROJECT_ID"
    printf '%s' "$SLACK_WEBHOOK_URL" | gcloud secrets versions add "$SLACK_SECRET" --data-file=- --project "$PROJECT_ID" >/dev/null
  fi
  if gcloud secrets describe "$SLACK_SECRET" --project "$PROJECT_ID" >/dev/null 2>&1; then
    gcloud secrets add-iam-policy-binding "$SLACK_SECRET" --project "$PROJECT_ID" \
      --member "serviceAccount:${RUNTIME_SA}" --role roles/secretmanager.secretAccessor >/dev/null
    SECRET_ARGS="$SECRET_ARGS,SLACK_WEBHOOK_URL=${SLACK_SECRET}:latest"
  fi
  # The settings file lives on a mounted bucket shared by all instances.
  gcloud storage buckets describe "gs://${SETTINGS_BUCKET}" --project "$PROJECT_ID" >/dev/null 2>&1 \
    || gcloud storage buckets create "gs://${SETTINGS_BUCKET}" --location "$REGION" \
         --uniform-bucket-level-access --project "$PROJECT_ID"
  gcloud storage buckets add-iam-policy-binding "gs://${SETTINGS_BUCKET}" \
    --member "serviceAccount:${RUNTIME_SA}" --role roles/storage.objectUser >/dev/null
  EXTRA_ARGS=(--execution-environment gen2
    --add-volume "name=${VOLUME},type=cloud-storage,bucket=${SETTINGS_BUCKET}"
    --add-volume-mount "volume=${VOLUME},mount-path=/data")
fi

if [ -n "$BIGQUERY_TABLE" ]; then
  BQ_DATASET="${BIGQUERY_TABLE%%.*}"
  gcloud services enable bigquery.googleapis.com --project "$PROJECT_ID"
  bq --project_id "$PROJECT_ID" show "${PROJECT_ID}:${BQ_DATASET}" >/dev/null 2>&1 \
    || bq --project_id "$PROJECT_ID" --location "$BIGQUERY_LOCATION" mk --dataset "${PROJECT_ID}:${BQ_DATASET}"
  bq --project_id "$PROJECT_ID" show "${PROJECT_ID}:${BIGQUERY_TABLE}" >/dev/null 2>&1 \
    || bq --project_id "$PROJECT_ID" mk --table --time_partitioning_field event_timestamp \
         --time_partitioning_type DAY "${PROJECT_ID}:${BIGQUERY_TABLE}" deploy/bigquery-schema.json
  # Write access to this one table only.
  bq --project_id "$PROJECT_ID" add-iam-policy-binding --member "serviceAccount:${RUNTIME_SA}" \
    --role roles/bigquery.dataEditor --table "${PROJECT_ID}:${BIGQUERY_TABLE}" >/dev/null
fi

for name in $SECRET_NAMES; do
  secret="${SERVICE}-$(echo "$name" | tr '[:upper:]_' '[:lower:]-')"
  if ! gcloud secrets describe "$secret" --project "$PROJECT_ID" >/dev/null 2>&1; then
    openssl rand -hex 24 | tr -d '\n' | gcloud secrets create "$secret" --data-file=- --project "$PROJECT_ID"
    echo "created secret $secret"
  fi
  gcloud secrets add-iam-policy-binding "$secret" --project "$PROJECT_ID" \
    --member "serviceAccount:${RUNTIME_SA}" --role roles/secretmanager.secretAccessor >/dev/null
done

gcloud builds submit --tag "$IMAGE" --project "$PROJECT_ID" .

gcloud run deploy "$SERVICE" \
  --image "$IMAGE" \
  --project "$PROJECT_ID" \
  --region "$REGION" \
  --allow-unauthenticated \
  --service-account "$RUNTIME_SA" \
  --cpu 1 --memory 512Mi \
  --min-instances 0 --max-instances 3 \
  --concurrency "$WORKERS" \
  --set-env-vars "^@^${ENV_VARS}" \
  --set-secrets "$SECRET_ARGS" \
  ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}

URL="$(gcloud run services describe "$SERVICE" --region "$REGION" --project "$PROJECT_ID" --format 'value(status.url)')"
echo
echo "Service URL: $URL"
echo "Put it in deploy/cloudflare/wrangler.toml as ORIGIN_URL, then deploy the Worker and run:"
echo "  gcloud secrets versions access latest --secret ${SERVICE}-proxy-secret --project $PROJECT_ID | npx wrangler secret put PROXY_SECRET"
if [ -n "$SETTINGS_BUCKET" ]; then
  # Links in Slack messages lead to the console.
  gcloud run services update "$SERVICE" --region "$REGION" --project "$PROJECT_ID" \
    --update-env-vars "ADMIN_URL=$URL/admin" >/dev/null
  # Runs the alert checks every 5 minutes. The job holds the check token,
  # which can run the checks and nothing else.
  CHECK_TOKEN_VALUE="$(gcloud secrets versions access latest --secret "${SERVICE}-check-token" --project "$PROJECT_ID")"
  JOB_ARGS=(--location "$REGION" --project "$PROJECT_ID" --schedule "*/5 * * * *"
    --uri "$URL/admin/api/alerts/check" --http-method POST
    --update-headers "Authorization=Bearer ${CHECK_TOKEN_VALUE}" --attempt-deadline 180s)
  if gcloud scheduler jobs describe "${SERVICE}-alerts" --location "$REGION" --project "$PROJECT_ID" >/dev/null 2>&1; then
    gcloud scheduler jobs update http "${SERVICE}-alerts" "${JOB_ARGS[@]}" >/dev/null
  else
    gcloud scheduler jobs create http "${SERVICE}-alerts" "${JOB_ARGS[@]/--update-headers/--headers}" >/dev/null
  fi
  echo
  echo "Alert checks: Cloud Scheduler job ${SERVICE}-alerts, every 5 minutes"
  echo
  echo "Admin page: $URL/admin  (open it on this address, not through the Worker)"
  echo "Admin token:"
  echo "  gcloud secrets versions access latest --secret ${SERVICE}-admin-token --project $PROJECT_ID"
fi
if [ -n "$BIGQUERY_TABLE" ]; then
  echo
  echo "BigQuery export: at /admin, set project $PROJECT_ID, dataset ${BIGQUERY_TABLE%%.*} and table ${BIGQUERY_TABLE#*.}."
fi
