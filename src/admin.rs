//! The management API and the page that drives it. Everything except the page
//! itself needs the admin token, which the page asks for and keeps only for the
//! length of the browser tab.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Digest;

use crate::ai::{Ai, AiError, audit, diagnose, explain, review, rules, setup, todo};
use crate::alerts::{Alerts, Config as AlertConfig, Context as AlertContext};
use crate::checkup;
use crate::container::{Condition, Container, Tag, compile_conditions};
use crate::container_file::{Change, ContainerFile, EditError};
use crate::eventlog::{CAPACITY, EventLog};
use crate::history::{self, Before, History};
use crate::integrations::{Enrichment, Integrations, TestOverrides, test_event};
use crate::metrics::{self, METRICS};
use crate::replay::{self, Hit};
use crate::request::{IncomingRequest, remote_address, same};
use crate::server::{Pool, Target, summarize};
use crate::state::State as StateStore;
use crate::tenants::{Tenant, Tenants};
use crate::traces::{self, Filter, Traces};
use crate::transform::{self, Transformation};

const CONSOLE: &str = include_str!("ui/console.html");

/// Fonts and logos the console loads: name, content type, bytes.
const ASSETS: &[(&str, &str, &[u8])] = &[
    (
        "favicon-ai-16.svg",
        "image/svg+xml",
        include_bytes!("ui/logos/favicon-ai-16.svg"),
    ),
    (
        "favicon-ai-16-ink.svg",
        "image/svg+xml",
        include_bytes!("ui/logos/favicon-ai-16-ink.svg"),
    ),
    (
        "google-sans-code.woff2",
        "font/woff2",
        include_bytes!("ui/fonts/google-sans-code.woff2"),
    ),
    (
        "google-sans-flex-display.woff2",
        "font/woff2",
        include_bytes!("ui/fonts/google-sans-flex-display.woff2"),
    ),
    (
        "google-sans-text.woff2",
        "font/woff2",
        include_bytes!("ui/fonts/google-sans-text.woff2"),
    ),
];

#[derive(Clone)]
pub struct Admin {
    /// Set when the server hosts several sites.
    pub tenants: Option<Arc<Tenants>>,
    /// Set when ad-platform integrations are edited from the page.
    pub integrations: Option<Arc<Integrations>>,
    pub token: String,
    /// Recent hits for the debugger.
    pub traces: Arc<Traces>,
    /// Who hits run for, so the console can show the container in use.
    pub target: Target,
    pub workers: usize,
    /// Set when ANTHROPIC_API_KEY turns the AI on.
    pub ai: Option<Arc<Ai>>,
    /// Shared across instances with STATE_DIR, else this process's memory.
    pub state: Arc<StateStore>,
    pub history: Arc<History>,
    pub alerts: Arc<Alerts>,
    /// Lets a scheduler run the alert checks, and nothing else.
    pub check_token: Option<String>,
    /// With a container file: the draft console edits go to.
    pub drafts: Option<Arc<crate::drafts::Drafts>>,
    /// Runs test events.
    pub pool: Arc<Pool>,
    /// Recent hits; the server records into it in integrations mode.
    pub log: Arc<EventLog>,
}

pub fn router(admin: Admin) -> Router {
    let mut router = Router::new()
        .route("/admin", get(console))
        .route("/admin/integrations", get(console))
        .route("/admin/assets/{file}", get(asset))
        .route("/admin/api/overview", get(overview))
        .route("/admin/api/traces", get(traces).delete(clear_traces))
        .route("/admin/api/traces/{id}", get(trace))
        .route("/admin/api/container", get(container))
        .route("/admin/api/container/tags", axum::routing::post(add_tag))
        .route(
            "/admin/api/container/tags/order",
            axum::routing::post(reorder_tags)
                .put(update_tag_named_order)
                .delete(delete_tag_named_order),
        )
        .route(
            "/admin/api/container/tags/{name}",
            axum::routing::put(update_tag).delete(delete_tag),
        )
        .route(
            "/admin/api/container/clients/{name}/features",
            axum::routing::put(update_client_features),
        )
        .route("/admin/api/container/test", axum::routing::post(test_tag))
        .route("/admin/api/history", get(history_list))
        .route("/admin/api/container/publish", axum::routing::post(publish))
        .route(
            "/admin/api/container/draft",
            axum::routing::delete(discard_draft),
        )
        .route("/admin/api/checkup", get(checkup_view))
        .route("/admin/api/checkup/live", axum::routing::post(live_start))
        .route("/admin/api/checkup/live/{code}", get(live_view))
        .route(
            "/admin/api/checkup/config",
            axum::routing::put(checkup_config),
        )
        .route("/admin/api/container/setup", axum::routing::put(save_setup))
        .route("/admin/api/pinned", get(pinned_view).post(pin))
        .route("/admin/api/pinned/{id}", axum::routing::delete(unpin))
        .route("/admin/api/alerts", get(alerts_view))
        .route("/admin/api/alerts/check", axum::routing::post(alerts_check))
        .route(
            "/admin/api/alerts/config",
            axum::routing::put(alerts_config),
        )
        .route("/admin/api/alerts/test", axum::routing::post(alerts_test))
        .route("/admin/api/history/{id}/undo", axum::routing::post(undo))
        .route(
            "/admin/api/container/transformations",
            axum::routing::put(save_transformations),
        )
        .route(
            "/admin/api/container/transformations/preview",
            axum::routing::post(preview_transformation),
        );
    if admin.tenants.is_some() {
        router = router
            .route("/admin/api/tenants", get(list).post(create))
            .route(
                "/admin/api/tenants/{id}",
                axum::routing::put(update).delete(remove),
            );
    }
    if admin.integrations.is_some() {
        router = router
            .route("/admin/api/integrations", get(integrations))
            .route(
                "/admin/api/integrations/{id}",
                axum::routing::put(save_provider),
            )
            .route(
                "/admin/api/integrations/{id}/test",
                axum::routing::post(send_test),
            )
            .route("/admin/api/enrichment", axum::routing::put(save_enrichment))
            .route("/admin/api/events", get(events));
    }
    if admin.ai.is_some() {
        router = router
            .route(
                "/admin/api/ai/explain/{id}",
                axum::routing::post(ai_explain),
            )
            .route("/admin/api/ai/audit", axum::routing::post(ai_audit))
            .route("/admin/api/ai/review", axum::routing::post(ai_review))
            .route("/admin/api/ai/setup", axum::routing::post(ai_setup))
            .route("/admin/api/ai/checkup", axum::routing::post(ai_checkup))
            .route(
                "/admin/api/ai/transformation",
                axum::routing::post(ai_transformation),
            );
    }
    router.with_state(admin)
}

/// The console with each asset's URL carrying a hash of its content
/// (`favicon.svg?v=1a2b3c4d`), so a changed logo or font is fetched again
/// instead of coming from a browser's 30-day cache.
static VERSIONED_CONSOLE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    ASSETS
        .iter()
        .fold(CONSOLE.to_string(), |html, (name, _, bytes)| {
            let v: String = sha2::Sha256::digest(bytes)
                .iter()
                .take(4)
                .map(|b| format!("{b:02x}"))
                .collect();
            let url = format!("/admin/assets/{name}");
            html.replace(&format!("{url}\""), &format!("{url}?v={v}\""))
                .replace(&format!("{url}'"), &format!("{url}?v={v}'"))
        })
});

async fn console() -> Response {
    page(VERSIONED_CONSOLE.as_str())
}

/// Public like any font or logo, so the page can load them before the token
/// is entered.
async fn asset(Path(file): Path<String>) -> Response {
    let Some((_, kind, bytes)) = ASSETS.iter().find(|(name, _, _)| *name == file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, *kind),
            (header::CACHE_CONTROL, "public, max-age=2592000"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'",
            ),
        ],
        *bytes,
    )
        .into_response()
}

fn page(html: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self'; frame-ancestors 'none'",
            ),
        ],
        Html(html),
    )
        .into_response()
}

/// Checks the bearer token. A wrong guess holds one of a few permits while it
/// waits to be answered, so guesses sent in parallel queue up too: about
/// eight a second per instance. The right token still gets through, only
/// later, so nobody can lock the admin out on purpose.
async fn authorize(admin: &Admin, headers: &HeaderMap) -> Result<(), Box<Response>> {
    static CHECKS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim();
    let _permit = CHECKS.acquire().await.expect("never closed");
    if same(given, &admin.token) {
        return Ok(());
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    Err(Box::new(error(
        StatusCode::UNAUTHORIZED,
        "wrong or missing admin token",
    )))
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "error": message })),
    )
        .into_response()
}

fn ok(body: serde_json::Value) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

async fn overview(State(a): State<Admin>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let mode = match a.target {
        Target::Single(_) => "single",
        Target::Multi(_) => "multi",
        Target::Integrated(_) => "integrated",
    };
    let tenants = a.tenants.as_ref().map(|t| t.stats());
    ok(json!({
        "server": {
            "version": env!("CARGO_PKG_VERSION"),
            "mode": mode,
            "uptime_s": metrics::uptime().as_secs(),
            "workers": a.workers,
            "trace_capacity": a.traces.capacity(),
        },
        "metrics": METRICS.snapshot(),
        "tenants": tenants,
        "ai": a.ai.as_ref().map(|ai| ai.status()),
        // What the Features page shows as set up or not.
        "services": {
            "state_shared": a.state.is_shared(),
            "slack": a.alerts.slack_on(),
            "drafts": a.drafts.is_some(),
            "check_token": a.check_token.is_some(),
        },
    }))
}

#[derive(Deserialize)]
struct AiQuery {
    tenant: Option<String>,
    lang: Option<String>,
}

fn ai_error(e: AiError) -> Response {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::BAD_GATEWAY);
    if status != StatusCode::BAD_REQUEST {
        tracing::warn!("admin: AI call failed: {e:?}");
    }
    error(status, &e.message())
}

/// The console's view of the container a tenant's hits run on, or of the
/// one container outside multi-site mode.
fn container_for(a: &Admin, tenant: Option<&str>) -> Result<Value, Box<Response>> {
    let Target::Multi(tenants) = &a.target else {
        return Ok(current_view(a));
    };
    let Some(id) = tenant.filter(|t| !t.is_empty()) else {
        return Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "name a tenant with ?tenant=",
        )));
    };
    tenants.reload_if_changed();
    match tenants.get(id) {
        Some(r) => Ok(container_view(&r.container, None, Some(MULTI_READ_ONLY))),
        None => Err(Box::new(error(StatusCode::NOT_FOUND, "no such tenant"))),
    }
}

async fn ai_explain(
    State(a): State<Admin>,
    headers: HeaderMap,
    Path(id): Path<u64>,
    Query(q): Query<AiQuery>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let Some(ai) = &a.ai else {
        return error(StatusCode::NOT_FOUND, "the AI is off");
    };
    let Some(t) = a.traces.get(id) else {
        return error(StatusCode::NOT_FOUND, "no such trace, or no longer kept");
    };
    let container = match container_for(&a, t["tenant"].as_str()) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    match explain::explain(ai, id, &traces::for_ai(&t), &container, q.lang.as_deref()).await {
        Ok(v) => ok(v),
        Err(e) => ai_error(e),
    }
}

#[derive(Deserialize)]
struct RequestBody {
    request: String,
}

/// Most characters of a request in words.
const MAX_REQUEST: usize = 2000;

async fn ai_transformation(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<AiQuery>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let Some(ai) = &a.ai else {
        return error(StatusCode::NOT_FOUND, "the AI is off");
    };
    let b: RequestBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let request = b.request.trim();
    if request.is_empty() || request.chars().count() > MAX_REQUEST {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("describe the change in 1 to {MAX_REQUEST} characters"),
        );
    }
    let tenant = q.tenant.as_deref().filter(|t| !t.is_empty());
    let container = match container_for(&a, tenant) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let events: Vec<Value> = buffer_events(&a, tenant)
        .into_iter()
        .map(|(_, e)| e)
        .take(500)
        .collect();
    let catalog = rules::catalog(&events);
    let taken: Vec<String> = container["transformations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    let check = |rule: Value| {
        let (rule, preview) = preview_rule(&a, tenant, rule)?;
        if taken.contains(&rule.name) {
            return Err(format!(
                "a transformation called {:?} already exists",
                rule.name
            ));
        }
        Ok((serde_json::to_value(&rule).unwrap_or_default(), preview))
    };
    match rules::write(ai, request, &container, &catalog, q.lang.as_deref(), check).await {
        Ok(v) => ok(v),
        Err(e) => ai_error(e),
    }
}

async fn ai_audit(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<AiQuery>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let Some(ai) = &a.ai else {
        return error(StatusCode::NOT_FOUND, "the AI is off");
    };
    let tenant = q.tenant.as_deref().filter(|t| !t.is_empty());
    let container = match container_for(&a, tenant) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let hits: Vec<(u64, Value)> = a
        .traces
        .details(tenant)
        .iter()
        .map(|(id, t)| (*id, traces::for_ai(t)))
        .collect();
    let retries = METRICS
        .send_retries
        .load(std::sync::atomic::Ordering::Relaxed);
    match audit::audit(ai, &hits, &container, retries, q.lang.as_deref()).await {
        Ok(v) => {
            tracing::info!("admin: AI audit of {} hits", hits.len());
            ok(v)
        }
        Err(e) => ai_error(e),
    }
}

async fn traces(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(filter): Query<Filter>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let (traces, latest) = a.traces.list(&filter);
    ok(json!({ "traces": traces, "latest": latest, "oldest": a.traces.oldest() }))
}

async fn trace(State(a): State<Admin>, headers: HeaderMap, Path(id): Path<u64>) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    match a.traces.get(id) {
        Some(t) => ok(t),
        None => error(StatusCode::NOT_FOUND, "no such trace, or no longer kept"),
    }
}

async fn clear_traces(State(a): State<Admin>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    a.traces.clear();
    ok(json!({ "cleared": true }))
}

#[derive(Deserialize)]
struct TenantQuery {
    tenant: Option<String>,
}

async fn container(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<TenantQuery>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    if let Target::Multi(tenants) = &a.target {
        let Some(id) = q.tenant.as_deref().filter(|t| !t.is_empty()) else {
            return error(StatusCode::BAD_REQUEST, "name a tenant with ?tenant=");
        };
        tenants.reload_if_changed();
        return match tenants.get(id) {
            Some(r) => ok(container_view(&r.container, None, Some(MULTI_READ_ONLY))),
            None => error(StatusCode::NOT_FOUND, "no such tenant"),
        };
    }
    ok(current_view(&a))
}

const MULTI_READ_ONLY: &str =
    "Each site runs the built-in container, so tags can't be edited with TENANTS_FILE.";
const BUILTIN_READ_ONLY: &str = "The server runs the built-in container. Start it with CONTAINER_FILE=<path to a container file> to edit tags here.";

/// The container file edits go to, outside multi-site mode.
fn editable(a: &Admin) -> Option<&Arc<ContainerFile>> {
    match &a.target {
        Target::Single(f) => Some(f),
        Target::Integrated(i) => Some(i.base()),
        Target::Multi(_) => None,
    }
}

/// The view of the container running now, outside multi-site mode.
fn current_view(a: &Admin) -> Value {
    view_of(a, true)
}

/// The container that runs, or with `with_draft` the draft when there is one.
fn view_of(a: &Admin, with_draft: bool) -> Value {
    let Some(file) = editable(a) else {
        return Value::Null;
    };
    file.reload_if_changed();
    let live = file.resolved().container;
    // With a draft, the console shows and edits the draft.
    let draft = a
        .drafts
        .as_ref()
        .filter(|_| with_draft)
        .and_then(|d| Some((d.get()?, d.container()?)));
    let (base, revision) = match &draft {
        Some((d, c)) => (c.clone(), Some(d.revision.clone())),
        None => (live.clone(), file.revision()),
    };
    let running = match &a.target {
        Target::Integrated(i) => Arc::new(i.with_base(&base)),
        _ => base.clone(),
    };
    let own = base.tags.len();
    let reason = (!file.is_file()).then_some(BUILTIN_READ_ONLY);
    let mut view = container_view(&running, revision, reason);
    view["drafts"] = json!(a.drafts.is_some() && file.is_file());
    view["draft"] = match &draft {
        Some((d, c)) => {
            let live_rev = file.revision().unwrap_or_default();
            json!({
                "revision": d.revision,
                "base": d.base,
                "live_revision": live_rev,
                "stale": d.base != live_rev,
                "updated_at": d.updated_at,
                "edits": history::describe(&live, c),
            })
        }
        None => Value::Null,
    };
    // Platform tags come after the file's own, and are set up under Integrations.
    if let Some(tags) = view["tags"].as_array_mut() {
        for (i, t) in tags.iter_mut().enumerate() {
            let own_tag = i < own && file.is_file();
            t["source"] = json!(if i < own { "file" } else { "integration" });
            t["editable"] = json!(own_tag);
        }
    }
    view
}

/// The container as the console shows it, with secret-looking settings
/// replaced by `[redacted]`. `revision` is set when it can be edited;
/// `reason` says why it can't.
fn container_view(c: &Container, revision: Option<String>, reason: Option<&str>) -> Value {
    let clients: Vec<Value> = c
        .clients
        .iter()
        .map(|e| json!({ "name": e.name, "template": e.template, "data": redact(&e.data) }))
        .collect();
    let tags: Vec<Value> = c
        .tags
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "template": t.template,
                "data": redact(&t.data),
                "fire_when": t.fire_when,
                "enabled": t.enabled,
                "source": "file",
                "editable": false,
            })
        })
        .collect();
    let mut names: Vec<&String> = c.templates.keys().collect();
    names.sort();
    let templates: Vec<Value> = names
        .into_iter()
        .map(|n| {
            let held = c.permissions.get(n);
            let used_by: Vec<&String> = c
                .clients
                .iter()
                .map(|e| (&e.name, &e.template))
                .chain(c.tags.iter().map(|t| (&t.name, &t.template)))
                .filter(|(_, t)| *t == n)
                .map(|(name, _)| name)
                .collect();
            json!({ "name": n, "builtin": held.is_none(), "permissions": held, "used_by": used_by })
        })
        .collect();
    json!({
        "version": c.version,
        "editable": reason.is_none(),
        "reason": reason,
        "revision": if reason.is_none() { revision } else { None },
        "clients": clients,
        "tags": tags,
        "transformations": c.transformations,
        "templates": templates,
    })
}

#[derive(Deserialize)]
struct TransformationsBody {
    #[serde(default)]
    revision: String,
    #[serde(default)]
    check: bool,
    transformations: Vec<Value>,
}

async fn save_transformations(State(a): State<Admin>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let b: TransformationsBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    for (i, r) in b.transformations.iter().enumerate() {
        if let Err(e) = serde_json::from_value::<Transformation>(r.clone()) {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("transformation {}: {e}", i + 1),
            );
        }
    }
    let what = format!("saved {} transformations", b.transformations.len());
    let c = Change::Transformations(b.transformations);
    change(&a, &what, &b.revision, c, b.check, None, To::Draft).await
}

/// The events in the trace buffer, newest first, with their trace ids.
fn buffer_events(a: &Admin, tenant: Option<&str>) -> Vec<(u64, Value)> {
    a.traces
        .events(tenant)
        .into_iter()
        .flat_map(|(id, evs)| evs.into_iter().map(move |e| (id, e)))
        .collect()
}

/// Checks a rule against the container and shows what it would do to the
/// events in the trace buffer, without saving anything.
fn preview_rule(
    a: &Admin,
    tenant: Option<&str>,
    rule: Value,
) -> Result<(Transformation, Value), String> {
    let mut rule: Vec<Transformation> =
        vec![serde_json::from_value(rule).map_err(|e| e.to_string())?];
    let container = container_for(a, tenant).map_err(|_| "no container".to_string())?;
    let names: Vec<&str> = container["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str())
        .collect();
    transform::compile(&mut rule, &names)?;
    let rule = rule.remove(0);
    let preview = transform::preview_on(&rule, &buffer_events(a, tenant));
    Ok((rule, preview))
}

#[derive(Deserialize)]
struct PreviewBody {
    rule: Value,
}

async fn preview_transformation(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<TenantQuery>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let b: PreviewBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    match preview_rule(&a, q.tenant.as_deref().filter(|t| !t.is_empty()), b.rule) {
        Ok((_, p)) => ok(p),
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

/// A tag as the console sends it, with the revision it was edited from.
#[derive(Deserialize)]
struct TagBody {
    #[serde(default)]
    revision: String,
    /// Only work out what the change would do; save nothing.
    #[serde(default)]
    check: bool,
    tag: Tag,
}

/// A client's features as the console sends them.
#[derive(Deserialize)]
struct FeaturesBody {
    #[serde(default)]
    revision: String,
    #[serde(default)]
    check: bool,
    features: crate::features::Features,
}

#[derive(Deserialize)]
struct RevisionQuery {
    #[serde(default)]
    revision: String,
    #[serde(default)]
    check: bool,
}

#[derive(Deserialize)]
struct OrderBody {
    #[serde(default)]
    revision: String,
    #[serde(default)]
    check: bool,
    names: Vec<String>,
}

/// Reads a JSON body, answering 400 with serde's message when it doesn't fit.
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Box<Response>> {
    serde_json::from_slice(body)
        .map_err(|e| Box::new(error(StatusCode::BAD_REQUEST, &format!("body: {e}"))))
}

fn parse_tag(body: &[u8]) -> Result<TagBody, Box<Response>> {
    let b: TagBody = parse(body)?;
    if !b.tag.data.is_object() {
        return Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "tag.data must be an object",
        )));
    }
    Ok(b)
}

/// Runs an edit and answers with the container as it is afterwards.
/// Where a change goes: the draft (the console's edits) or live (an undo,
/// which rolls live back).
#[derive(Clone, Copy, PartialEq)]
pub enum To {
    Draft,
    Live,
}

/// Saves a change, or with `check` only works out what it would change,
/// without saving. With drafts on, console edits go to the draft and live
/// changes only when it is published. `first` leads the edits the history
/// keeps, for an undo.
async fn change(
    a: &Admin,
    what: &str,
    revision: &str,
    change: Change,
    check: bool,
    first: Option<Value>,
    to: To,
) -> Response {
    let Some(file) = editable(a) else {
        return error(StatusCode::BAD_REQUEST, MULTI_READ_ONLY);
    };
    if !file.is_file() {
        return error(StatusCode::BAD_REQUEST, BUILTIN_READ_ONLY);
    }
    file.reload_if_changed();
    let live = file.resolved().container;
    let drafts = a.drafts.as_ref().filter(|_| to == To::Draft);
    let (old, result) = match drafts {
        Some(d) => (
            d.container().unwrap_or_else(|| live.clone()),
            d.edit(revision, change, check),
        ),
        None => (live.clone(), file.apply(revision, change, check)),
    };
    match result {
        Ok(applied) => {
            let mut edits: Vec<Value> = first.into_iter().collect();
            edits.extend(history::describe(&old, &applied.container));
            if check {
                return ok(
                    json!({ "check": { "edits": edits, "replay": compare_with(a, old, applied.container).await } }),
                );
            }
            if drafts.is_none() {
                a.history.record(
                    "console",
                    edits,
                    Some(Before {
                        revision: &applied.before_revision,
                        text: &applied.before,
                    }),
                    Some(&applied.after_revision),
                );
            }
            tracing::info!(
                "admin: {what}{}",
                if drafts.is_some() { " (draft)" } else { "" }
            );
            ok(json!({ "container": current_view(a) }))
        }
        Err(e) => edit_error(e),
    }
}

/// Replays recent and pinned hits through `old` and `new` as they would
/// run, platform tags included.
async fn compare_with(a: &Admin, old: Arc<Container>, new: Container) -> Value {
    let (now, next) = match &a.target {
        Target::Integrated(i) => (Arc::new(i.with_base(&old)), Arc::new(i.with_base(&new))),
        _ => (old, Arc::new(new)),
    };
    let hits = replay_hits(a, None);
    tokio::task::spawn_blocking(move || replay::compare(now, next, &hits))
        .await
        .unwrap_or_else(|e| json!({ "error": e.to_string() }))
}

fn edit_error(e: EditError) -> Response {
    match e {
        EditError::Conflict(revision) => (
            StatusCode::CONFLICT,
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "error": "the container changed since it was loaded", "revision": revision })),
        )
            .into_response(),
        EditError::NotFound => error(StatusCode::NOT_FOUND, "no such tag"),
        EditError::ReadOnly(m) => error(StatusCode::BAD_REQUEST, m),
        EditError::Invalid(m) => error(StatusCode::BAD_REQUEST, &m),
        EditError::Io(m) => {
            tracing::warn!("admin: container not saved: {m}");
            error(StatusCode::INTERNAL_SERVER_ERROR, &m)
        }
    }
}

#[derive(Deserialize)]
struct PublishBody {
    #[serde(default)]
    revision: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    check: bool,
    /// Publish even though live changed since the draft was started.
    #[serde(default)]
    force: bool,
}

/// Puts the draft live as a named version, or with check only works out
/// what publishing would change against live.
async fn publish(State(a): State<Admin>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let b: PublishBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let (Some(d), Some(file)) = (&a.drafts, editable(&a)) else {
        return error(StatusCode::BAD_REQUEST, BUILTIN_READ_ONLY);
    };
    let name = b.name.trim().chars().take(100).collect::<String>();
    let note = b.note.trim().chars().take(1000).collect::<String>();
    file.reload_if_changed();
    let live = file.resolved().container;
    match d.publish(&b.revision, b.force, b.check) {
        Ok(applied) => {
            let edits = history::describe(&live, &applied.container);
            if b.check {
                return ok(
                    json!({ "check": { "edits": edits, "replay": compare_with(&a, live, applied.container).await } }),
                );
            }
            a.history.record_version(
                &name,
                &note,
                edits,
                Before {
                    revision: &applied.before_revision,
                    text: &applied.before,
                },
                &applied.after_revision,
            );
            tracing::info!("admin: published {name:?}");
            ok(json!({ "container": current_view(&a) }))
        }
        Err(e) => edit_error(e),
    }
}

async fn discard_draft(State(a): State<Admin>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let Some(d) = &a.drafts else {
        return error(StatusCode::BAD_REQUEST, BUILTIN_READ_ONLY);
    };
    d.discard();
    tracing::info!("admin: discarded the draft");
    ok(json!({ "container": current_view(&a) }))
}

/// Runs the alert checks, with the AI explaining new alerts when it is on.
pub async fn run_alert_check(a: &Admin) -> Value {
    type Explain =
        dyn Fn(Value) -> futures_util::future::BoxFuture<'static, Option<Value>> + Send + Sync;
    let explain: Option<Box<Explain>> = a.ai.clone().map(|ai| {
        let admin = a.clone();
        Box::new(move |alert: Value| {
            let (ai, admin) = (ai.clone(), admin.clone());
            Box::pin(async move { explain_alert(&admin, &ai, &alert).await })
                as futures_util::future::BoxFuture<'static, Option<Value>>
        }) as Box<Explain>
    });
    a.alerts
        .check(AlertContext {
            explain: explain.as_deref(),
        })
        .await
}

/// The AI's diagnosis of a new alert, with a few recent hits that show it.
async fn explain_alert(a: &Admin, ai: &Ai, alert: &Value) -> Option<Value> {
    let site = alert["site"].as_str().filter(|s| !s.is_empty());
    let subject = alert["subject"].as_str().unwrap_or_default();
    let event = subject.split(['.', '→']).next().unwrap_or(subject);
    let shows = |t: &Value| -> bool {
        let r = &t["result"];
        let sends = r["sends"].as_array().into_iter().flatten();
        match alert["kind"].as_str().unwrap_or_default() {
            "vendor_failing" => sends
                .filter(|s| s["url"].as_str().is_some_and(|u| u.contains(subject)))
                .any(|s| s["result"]["Ok"].as_u64().is_none_or(|c| c >= 400)),
            "tag_failing" => r["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|x| x["tag"] == subject && x["outcome"] != "success"),
            "unclaimed" => r["claimed_by"].is_null(),
            _ => r["events"].as_array().into_iter().flatten().any(|e| {
                e["event_name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(event))
            }),
        }
    };
    let hits: Vec<Value> = a
        .traces
        .details(site)
        .iter()
        .filter(|(_, t)| shows(t))
        .take(3)
        .map(|(_, t)| traces::for_ai(t))
        .collect();
    let container = container_for(a, site).unwrap_or(Value::Null);
    let lang = a.alerts.config().language;
    match diagnose::diagnose(ai, alert, &hits, &container, Some(&lang)).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("alerts: no diagnosis: {}", e.message());
            None
        }
    }
}

async fn alerts_view(State(a): State<Admin>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let mut v = a.alerts.view();
    v["check_token"] = json!(a.check_token.is_some());
    ok(v)
}

/// The admin token or the check token, which only runs the checks.
async fn alerts_check(State(a): State<Admin>, headers: HeaderMap) -> Response {
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim();
    if !a.check_token.as_deref().is_some_and(|t| same(given, t))
        && let Err(r) = authorize(&a, &headers).await
    {
        return *r;
    }
    ok(run_alert_check(&a).await)
}

async fn alerts_config(State(a): State<Admin>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let c: AlertConfig = match parse(&body) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    match a.alerts.set_config(c) {
        Ok(c) => ok(json!({ "config": c })),
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

async fn alerts_test(State(a): State<Admin>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let text = if a.alerts.config().language == "tr" {
        "ai tagging server bu kanala uyarı gönderebiliyor. Bu bir deneme mesajı."
    } else {
        "ai tagging server can post alerts to this channel. This is a test message."
    };
    match a.alerts.post(text).await {
        Ok(()) => ok(json!({ "sent": true })),
        Err(e) => error(StatusCode::BAD_GATEWAY, &e),
    }
}

/// The hits a change is checked against: the trace buffer's, newest first,
/// and every pinned hit.
fn replay_hits(a: &Admin, tenant: Option<&str>) -> Vec<Hit> {
    let mut hits: Vec<Hit> = a
        .traces
        .details(tenant)
        .into_iter()
        .filter(|(_, t)| !t["result"].is_null())
        .take(replay::MAX_HITS)
        .filter_map(|(id, t)| {
            Some(Hit {
                trace: Some(id),
                pinned: None,
                at: t["at"].as_u64().unwrap_or(0),
                request: replay::request(&t["request"])?,
            })
        })
        .collect();
    for p in pinned_list(a) {
        if p["tenant"].as_str() != tenant {
            continue;
        }
        if let Some(request) = replay::request(&p["request"]) {
            hits.push(Hit {
                trace: None,
                pinned: p["label"].as_str().map(str::to_string),
                at: p["at"].as_u64().unwrap_or(0),
                request,
            });
        }
    }
    hits
}

/// Most pinned hits kept.
const MAX_PINNED: usize = 50;

fn pinned_list(a: &Admin) -> Vec<Value> {
    a.state
        .list("pinned")
        .iter()
        .filter_map(|n| a.state.read(&format!("pinned/{n}")))
        .collect()
}

async fn pinned_view(State(a): State<Admin>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let list: Vec<Value> = pinned_list(&a)
        .into_iter()
        .map(|mut p| {
            if let Some(m) = p.as_object_mut() {
                m.remove("request");
            }
            p
        })
        .collect();
    ok(json!({ "pinned": list }))
}

#[derive(Deserialize)]
struct PinBody {
    trace: u64,
    label: String,
}

async fn pin(State(a): State<Admin>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let b: PinBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let label = b.label.trim();
    if label.is_empty() || label.chars().count() > 100 {
        return error(StatusCode::BAD_REQUEST, "a label of 1 to 100 characters");
    }
    if a.state.list("pinned").len() >= MAX_PINNED {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("at most {MAX_PINNED} pinned hits"),
        );
    }
    let Some(t) = a.traces.get(b.trace) else {
        return error(StatusCode::NOT_FOUND, "no such trace, or no longer kept");
    };
    let at = t["at"].as_u64().unwrap_or(0);
    let id = format!("{}-{}", crate::state::now_ms(), b.trace);
    let mut entry = replay::pinned_entry(&id, label, t["tenant"].as_str(), at, &t["request"]);
    entry["events"] = json!(
        t["result"]["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["event_name"].as_str())
            .collect::<Vec<_>>()
    );
    if let Err(e) = a.state.write(&format!("pinned/{id}.json"), &entry) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    tracing::info!("admin: pinned trace {} as {label:?}", b.trace);
    pinned_view(State(a), headers).await
}

async fn unpin(State(a): State<Admin>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    if !id.chars().all(|c| c.is_ascii_digit() || c == '-')
        || a.state.read(&format!("pinned/{id}.json")).is_none()
    {
        return error(StatusCode::NOT_FOUND, "no such pinned hit");
    }
    a.state.remove(&format!("pinned/{id}.json"));
    pinned_view(State(a), headers).await
}

#[derive(Deserialize)]
struct TodoBody {
    items: Value,
    #[serde(default)]
    web: Value,
}

async fn ai_checkup(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<AiQuery>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let Some(ai) = &a.ai else {
        return error(StatusCode::NOT_FOUND, "the AI is off");
    };
    let b: TodoBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let container =
        container_for(&a, q.tenant.as_deref().filter(|t| !t.is_empty())).unwrap_or(Value::Null);
    match todo::todo(ai, &b.items, &b.web, &container, q.lang.as_deref()).await {
        Ok(v) => ok(v),
        Err(e) => ai_error(e),
    }
}

/// Starts a live check: a code to open the site with.
#[derive(Deserialize, Default)]
struct LiveBody {
    /// The browser runs the draft instead of live.
    #[serde(default)]
    preview: bool,
    /// A preview's sends really go out; by default they don't.
    #[serde(default)]
    send: bool,
}

async fn live_start(State(a): State<Admin>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let b: LiveBody = if body.is_empty() {
        LiveBody::default()
    } else {
        match parse(&body) {
            Ok(b) => b,
            Err(r) => return *r,
        }
    };
    let now = crate::state::now_ms();
    let digest = sha2::Sha256::digest(format!("{now}-{}", a.state.instance).as_bytes());
    let code: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    let _ = a.state.write(
        &format!("checkup/live-{code}.json"),
        &json!({ "code": code, "since": now, "preview": b.preview, "send": b.send }),
    );
    ok(json!({ "code": code, "since": now, "preview": b.preview, "send": b.send }))
}

async fn live_view(
    State(a): State<Admin>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    if code.len() != 8 || !code.chars().all(|c| c.is_ascii_hexdigit()) {
        return error(StatusCode::NOT_FOUND, "no such live check");
    }
    let Some(s) = a.state.read(&format!("checkup/live-{code}.json")) else {
        return error(StatusCode::NOT_FOUND, "no such live check");
    };
    let since = s["since"].as_u64().unwrap_or(0);
    // A preview is judged against the draft; any other check against live.
    let container = if matches!(a.target, Target::Multi(_)) {
        container_for(&a, None).unwrap_or(Value::Null)
    } else {
        view_of(&a, s["preview"] == json!(true))
    };
    // Browser pixels, from the last time the site was read.
    let pixels: std::collections::BTreeSet<String> = a
        .state
        .read(&format!("checkup/{}-web.json", checkup_key(None)))
        .and_then(|w| w["pixels"].as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect();
    ok(checkup::live(
        &code,
        since,
        &a.traces.details(None),
        &container,
        &pixels,
    ))
}

#[derive(Deserialize, serde::Serialize, Default)]
struct CheckupConfig {
    /// The site's address, e.g. https://example.com.
    #[serde(default)]
    site_url: String,
    /// The web GTM container, when the page doesn't show it.
    #[serde(default)]
    gtm_id: String,
    /// This server's address as the site reaches it, e.g. https://example.com/k7q2.
    #[serde(default)]
    public_url: String,
}

fn checkup_key(tenant: Option<&str>) -> String {
    tenant.map_or_else(
        || "default".into(),
        |t| {
            format!(
                "t-{}",
                t.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_")
            )
        },
    )
}

#[derive(Deserialize)]
struct CheckupQuery {
    tenant: Option<String>,
    #[serde(default)]
    refresh: bool,
}

/// The setup check: what the server knows from the last 24 hours, and the
/// site and its web container when a site is set (read again with refresh).
async fn checkup_view(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<CheckupQuery>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let tenant = q.tenant.as_deref().filter(|t| !t.is_empty());
    let container = match container_for(&a, tenant) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let key = checkup_key(tenant);
    let cfg: CheckupConfig = a
        .state
        .read(&format!("checkup/{key}.json"))
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let mut web = a.state.read(&format!("checkup/{key}-web.json"));
    if !cfg.site_url.is_empty() && (q.refresh || web.is_none()) {
        let allow = std::env::var("OUTGOING_ALLOW_PRIVATE").is_ok_and(|v| v == "1");
        let client = checkup::client(allow);
        let gtm = Some(cfg.gtm_id.as_str()).filter(|g| !g.is_empty());
        let public = Some(cfg.public_url.as_str()).filter(|u| !u.is_empty());
        let w = checkup::analyse(&client, &cfg.site_url, gtm, public).await;
        let _ = a.state.write(&format!("checkup/{key}-web.json"), &w);
        web = Some(w);
    }
    if cfg.site_url.is_empty() {
        web = None;
    }
    a.alerts.flush_counts().await;
    let state = a.state.clone();
    let site = tenant.unwrap_or("").to_string();
    let counts = tokio::task::spawn_blocking(move || {
        let now = crate::counts::quarter_now();
        let mut r = crate::counts::Reader::new(&state);
        r.quarters(now.saturating_sub(crate::counts::QUARTERS_PER_DAY)..=now)
            .remove(&site)
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    let hits: Vec<Value> = a
        .traces
        .details(tenant)
        .into_iter()
        .take(200)
        .map(|(_, t)| t)
        .collect();
    let items = checkup::check(&container, &counts, &hits, web.as_ref());
    ok(
        json!({ "items": items, "web": web, "config": cfg, "at": crate::state::now_ms(), "hits_sampled": hits.len() }),
    )
}

async fn checkup_config(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<TenantQuery>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let mut c: CheckupConfig = match parse(&body) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    c.site_url = c.site_url.trim().to_string();
    c.public_url = c.public_url.trim().trim_end_matches('/').to_string();
    c.gtm_id = c.gtm_id.trim().to_uppercase();
    for u in [&c.site_url, &c.public_url] {
        if !u.is_empty() && !(u.starts_with("https://") || u.starts_with("http://")) {
            return error(StatusCode::BAD_REQUEST, "addresses start with https://");
        }
    }
    if !c.gtm_id.is_empty() && !c.gtm_id.starts_with("GTM-") {
        return error(
            StatusCode::BAD_REQUEST,
            "a web container id starts with GTM-",
        );
    }
    let key = checkup_key(q.tenant.as_deref().filter(|t| !t.is_empty()));
    if let Err(e) = a.state.write(&format!("checkup/{key}.json"), &json!(c)) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    // The site is read again on the next check.
    a.state.remove(&format!("checkup/{key}-web.json"));
    ok(json!({ "config": c }))
}

#[derive(Deserialize)]
struct SetupBody {
    #[serde(default)]
    revision: String,
    #[serde(default)]
    check: bool,
    tags: Vec<Tag>,
    #[serde(default)]
    transformations: Vec<Value>,
}

/// Replaces every tag and transformation at once, as a setup proposes.
async fn save_setup(State(a): State<Admin>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let b: SetupBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    if b.tags.iter().any(|t| !t.data.is_object()) {
        return error(
            StatusCode::BAD_REQUEST,
            "every tag's data must be an object",
        );
    }
    let what = format!("applied a setup of {} tags", b.tags.len());
    let c = Change::Setup {
        tags: b.tags,
        transformations: b.transformations,
    };
    change(&a, &what, &b.revision, c, b.check, None, To::Draft).await
}

async fn ai_setup(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<AiQuery>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let Some(ai) = &a.ai else {
        return error(StatusCode::NOT_FOUND, "the AI is off");
    };
    let b: RequestBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let request = b.request.trim();
    if request.is_empty() || request.chars().count() > MAX_REQUEST {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("describe the setup in 1 to {MAX_REQUEST} characters"),
        );
    }
    if setup::looks_secret(request) {
        return error(
            StatusCode::BAD_REQUEST,
            "this looks like it holds a token or key; leave secrets out, they are filled in after the setup is applied",
        );
    }
    let Some(file) = editable(&a).filter(|f| f.is_file()) else {
        return error(StatusCode::BAD_REQUEST, BUILTIN_READ_ONLY);
    };
    let container = current_view(&a);
    let revision = file.revision().unwrap_or_default();
    let events: Vec<Value> = buffer_events(&a, None)
        .into_iter()
        .map(|(_, e)| e)
        .take(500)
        .collect();
    let catalog = rules::catalog(&events);
    let check = |tags: &Value, rules: &Value| -> Result<(), String> {
        let tags: Vec<Tag> =
            serde_json::from_value(tags.clone()).map_err(|e| format!("tags: {e}"))?;
        if tags.iter().any(|t| !t.data.is_object()) {
            return Err("every tag's data must be an object".into());
        }
        let rules: Vec<Value> =
            serde_json::from_value(rules.clone()).map_err(|e| format!("transformations: {e}"))?;
        file.apply(
            &revision,
            Change::Setup {
                tags,
                transformations: rules,
            },
            true,
        )
        .map(|_| ())
        .map_err(|e| match e {
            EditError::Invalid(m) | EditError::Io(m) => m,
            EditError::Conflict(_) => "the container changed meanwhile; ask again".into(),
            EditError::NotFound => "a tag it names doesn't exist".into(),
            EditError::ReadOnly(m) => m.into(),
        })
    };
    match setup::propose(ai, request, &container, &catalog, q.lang.as_deref(), check).await {
        Ok(v) => ok(v),
        Err(e) => ai_error(e),
    }
}

#[derive(Deserialize)]
struct ReviewBody {
    edits: Value,
    replay: Value,
}

async fn ai_review(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<AiQuery>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let Some(ai) = &a.ai else {
        return error(StatusCode::NOT_FOUND, "the AI is off");
    };
    let b: ReviewBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    match review::review(ai, &b.edits, &b.replay, q.lang.as_deref()).await {
        Ok(v) => ok(v),
        Err(e) => ai_error(e),
    }
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
}

async fn history_list(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let editable = editable(&a).is_some_and(|f| f.is_file());
    ok(json!({
        "entries": a.history.list(q.limit.unwrap_or(50).min(200)),
        "editable": editable,
        "shared": a.state.is_shared(),
    }))
}

#[derive(Deserialize)]
struct UndoBody {
    #[serde(default)]
    revision: String,
    #[serde(default)]
    check: bool,
}

async fn undo(
    State(a): State<Admin>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let b: UndoBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let Some(text) = a.history.before(&id) else {
        return error(
            StatusCode::NOT_FOUND,
            "no such change, or it can't be undone",
        );
    };
    let first = json!({ "kind": "undo", "of": id });
    change(
        &a,
        &format!("undid change {id}"),
        &b.revision,
        Change::Restore(text),
        b.check,
        Some(first),
        To::Live,
    )
    .await
}

async fn add_tag(State(a): State<Admin>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let b = match parse_tag(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let what = format!("added tag {:?}", b.tag.name.trim());
    change(
        &a,
        &what,
        &b.revision,
        Change::Add(b.tag),
        b.check,
        None,
        To::Draft,
    )
    .await
}

async fn update_tag(
    State(a): State<Admin>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    update_named(a, headers, name, body).await
}

async fn update_tag_named_order(
    State(a): State<Admin>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    update_named(a, headers, "order".into(), body).await
}

async fn update_named(a: Admin, headers: HeaderMap, name: String, body: Bytes) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let b = match parse_tag(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let what = format!("saved tag {name:?}");
    let c = Change::Update(name, b.tag);
    change(&a, &what, &b.revision, c, b.check, None, To::Draft).await
}

async fn update_client_features(
    State(a): State<Admin>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let mut b: FeaturesBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    if let Err(e) = b.features.validate() {
        return error(StatusCode::BAD_REQUEST, &e);
    }
    let what = format!("saved the features of client {name:?}");
    let c = Change::ClientFeatures(name, b.features);
    change(&a, &what, &b.revision, c, b.check, None, To::Draft).await
}

async fn delete_tag(
    State(a): State<Admin>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<RevisionQuery>,
) -> Response {
    delete_named(a, headers, name, q).await
}

async fn delete_tag_named_order(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<RevisionQuery>,
) -> Response {
    delete_named(a, headers, "order".into(), q).await
}

async fn delete_named(a: Admin, headers: HeaderMap, name: String, q: RevisionQuery) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let what = format!("deleted tag {name:?}");
    change(
        &a,
        &what,
        &q.revision,
        Change::Delete(name),
        q.check,
        None,
        To::Draft,
    )
    .await
}

async fn reorder_tags(State(a): State<Admin>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let b: OrderBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let c = Change::Reorder(b.names);
    change(
        &a,
        "reordered tags",
        &b.revision,
        c,
        b.check,
        None,
        To::Draft,
    )
    .await
}

#[derive(Deserialize)]
struct TestBody {
    tag: TestTag,
}

/// Only the conditions matter to a test; the rest of the tag may come along.
#[derive(Deserialize)]
struct TestTag {
    #[serde(default)]
    fire_when: Vec<Condition>,
}

/// Most hit ids a test lists.
const TEST_HITS: usize = 50;

/// Checks a tag's conditions against every event in the trace buffer, without
/// running anything: how many events it would have fired for, on which hits,
/// and how often each condition held.
async fn test_tag(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<TenantQuery>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let mut b: TestBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    if let Err(e) = compile_conditions(&mut b.tag.fire_when) {
        return error(StatusCode::BAD_REQUEST, &format!("fire_when: {e}"));
    }
    let conditions = &b.tag.fire_when;
    let tenant = q.tenant.as_deref().filter(|t| !t.is_empty());
    let hits = a.traces.events(tenant);
    let (mut events, mut matched) = (0, 0);
    let mut per_condition = vec![0usize; conditions.len()];
    let mut matched_hits = Vec::new();
    let mut matched_hit_count = 0;
    let mut by_name: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (id, evs) in &hits {
        let mut hit = false;
        for ev in evs {
            events += 1;
            let held: Vec<bool> = conditions.iter().map(|c| c.matches(ev)).collect();
            for (n, h) in per_condition.iter_mut().zip(&held) {
                *n += usize::from(*h);
            }
            let fired = held.iter().all(|h| *h);
            let name = ev["event_name"].as_str().unwrap_or_default().to_string();
            let entry = by_name.entry(name).or_default();
            entry.0 += 1;
            if fired {
                entry.1 += 1;
                matched += 1;
                hit = true;
            }
        }
        matched_hit_count += usize::from(hit);
        if hit && matched_hits.len() < TEST_HITS {
            matched_hits.push(*id);
        }
    }
    let by_event_name: serde_json::Map<String, Value> = by_name
        .into_iter()
        .map(|(k, (e, m))| (k, json!({ "events": e, "matched": m })))
        .collect();
    let conditions: Vec<Value> = conditions
        .iter()
        .zip(per_condition)
        .map(|(c, n)| json!({ "key": c.key, "op": c.op, "value": c.value, "matched": n }))
        .collect();
    ok(json!({
        "hits": hits.len(),
        "events": events,
        "matched_events": matched,
        "matched_hits": matched_hits,
        "matched_hit_count": matched_hit_count,
        "by_event_name": by_event_name,
        "conditions": conditions,
    }))
}

/// Whether a setting's name says it holds a secret: tokens, secrets,
/// passwords, credentials, auth values, header values and keys (`api_key`,
/// `apiKey`). Ids (`pixel_id`, `allowedMeasurementIds`) and a bare `key`
/// (the enrichment's lookup field) are not secrets.
/// Whether a setting's name says it holds a secret (`*token*`, `*secret*`,
/// `*key`…). Ids never count.
pub(crate) fn secret_name(name: &str) -> bool {
    let n: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if n.ends_with("id") || n.ends_with("ids") || n == "key" {
        return false;
    }
    [
        "token",
        "secret",
        "password",
        "passwd",
        "credential",
        "auth",
        "headervalue",
    ]
    .iter()
    .any(|w| n.contains(w))
        || n.ends_with("key")
}

fn redact(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let empty = v.is_null() || v.as_str().is_some_and(str::is_empty);
                    let v = if secret_name(k) && !empty {
                        json!("[redacted]")
                    } else {
                        redact(v)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
        other => other.clone(),
    }
}

async fn list(State(a): State<Admin>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let tenants = a.tenants.as_ref().expect("tenant routes need tenants");
    tenants.reload_if_changed();
    ok(json!({ "tenants": tenants.list(), "stats": tenants.stats() }))
}

async fn create(State(a): State<Admin>, headers: HeaderMap, Json(t): Json<Tenant>) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let tenants = a.tenants.as_ref().expect("tenant routes need tenants");
    match tenants.put(t, true) {
        Ok(t) => {
            tracing::info!("admin: created tenant {}", t.id);
            ok(json!({ "tenant": t }))
        }
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

async fn update(
    State(a): State<Admin>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(mut t): Json<Tenant>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    t.id = id;
    let tenants = a.tenants.as_ref().expect("tenant routes need tenants");
    if !tenants
        .list()
        .iter()
        .any(|x| x.id == t.id.trim().to_ascii_lowercase())
    {
        return error(StatusCode::NOT_FOUND, "no such tenant");
    }
    match tenants.put(t, false) {
        Ok(t) => {
            tracing::info!("admin: updated tenant {}", t.id);
            ok(json!({ "tenant": t }))
        }
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

async fn remove(State(a): State<Admin>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let tenants = a.tenants.as_ref().expect("tenant routes need tenants");
    match tenants.delete(&id) {
        Ok(true) => {
            tracing::info!("admin: deleted tenant {id}");
            ok(json!({ "deleted": id }))
        }
        Ok(false) => error(StatusCode::NOT_FOUND, "no such tenant"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

async fn integrations(State(a): State<Admin>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let i = a.integrations.as_ref().expect("routes need integrations");
    i.reload_if_changed();
    ok(i.view())
}

#[derive(Deserialize)]
struct ProviderBody {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    fields: BTreeMap<String, String>,
}

async fn save_provider(
    State(a): State<Admin>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ProviderBody>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let i = a.integrations.as_ref().expect("routes need integrations");
    match i.set_provider(&id, body.enabled, body.fields) {
        Ok(()) => {
            tracing::info!(
                "admin: saved integration {id} ({})",
                if body.enabled { "on" } else { "off" }
            );
            let name = crate::integrations::PROVIDERS
                .iter()
                .find(|p| p.id == id)
                .map_or(id.as_str(), |p| p.name);
            a.history.record(
                "integrations",
                vec![json!({ "kind": "integration_saved", "name": name, "enabled": body.enabled })],
                None,
                None,
            );
            ok(i.view())
        }
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

async fn save_enrichment(
    State(a): State<Admin>,
    headers: HeaderMap,
    Json(body): Json<Enrichment>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let i = a.integrations.as_ref().expect("routes need integrations");
    match i.set_enrichment(body) {
        Ok(()) => {
            tracing::info!("admin: saved enrichment endpoint");
            a.history.record(
                "integrations",
                vec![json!({ "kind": "integration_saved", "name": "the enrichment endpoint" })],
                None,
                None,
            );
            ok(i.view())
        }
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    limit: Option<usize>,
    /// Only hits where a tag whose name contains this ran.
    tag: Option<String>,
}

async fn events(
    State(a): State<Admin>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let limit = q.limit.unwrap_or(50).min(CAPACITY);
    let tag = q.tag.as_deref().filter(|t| !t.is_empty());
    ok(json!({ "events": a.log.recent(limit, tag), "capacity": CAPACITY }))
}

#[derive(Deserialize)]
struct SendTestBody {
    #[serde(default = "purchase")]
    event: String,
    #[serde(flatten)]
    overrides: TestOverrides,
}

fn purchase() -> String {
    "purchase".into()
}

/// Sends one made-up event through one provider's tag, with the caller's IP
/// and user agent as the visitor's, and answers what happened, as the event
/// log shows it.
async fn send_test(
    State(a): State<Admin>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SendTestBody>,
) -> Response {
    if let Err(r) = authorize(&a, &headers).await {
        return *r;
    }
    let i = a.integrations.as_ref().expect("routes need integrations");
    i.reload_if_changed();
    let target = match i.test_target(&id, &body.overrides) {
        Ok(t) => t,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
    let pairs: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let ip = remote_address(&pairs, &peer.ip().to_string());
    let header = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let user_agent = header("user-agent");
    let page = i
        .site_url()
        .unwrap_or_else(|| format!("https://{}/", header("host")));
    let event = match test_event(&body.event, &ip, &user_agent, &page) {
        Ok(e) => e,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
    let req = IncomingRequest {
        method: "POST".into(),
        path: "/admin/test".into(),
        query: String::new(),
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("user-agent".into(), user_agent),
        ],
        body: event.to_string(),
        remote_address: ip,
    };
    let started = Instant::now();
    let Some(result) = a.pool.run(req, target).await else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the server is busy, try again",
        );
    };
    tracing::info!(
        "admin: test {} for {id} -> {}",
        body.event,
        summarize(&result)
    );
    let entry = a.log.record("admin test", &result, started.elapsed(), true);
    ok(json!({ "entry": entry }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::Tag;
    use crate::request::IncomingRequest;
    use crate::sandbox::RunResult;
    use crate::traces::{Outcome, capture};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_console_links_assets_by_content() {
        let html = VERSIONED_CONSOLE.as_str();
        for (name, _, _) in ASSETS {
            assert!(
                html.contains(&format!("/admin/assets/{name}?v=")),
                "{name} isn't versioned"
            );
            assert!(!html.contains(&format!("/admin/assets/{name}\"")));
            assert!(!html.contains(&format!("/admin/assets/{name}'")));
        }
    }

    #[test]
    fn secret_looking_settings_are_redacted() {
        let v = redact(&json!({
            "pixel_id": "123",
            "access_token": "EAAB",
            "apiKey": "k",
            "test_event_code": "TEST1",
            "allowedMeasurementIds": ["G-1"],
            "enrich": { "url": "https://x", "key": "user_id", "headerValue": "s", "headerName": "x-k" },
            "client_secret": "",
        }));
        assert_eq!(v["pixel_id"], "123");
        assert_eq!(v["access_token"], "[redacted]");
        assert_eq!(v["apiKey"], "[redacted]");
        assert_eq!(v["test_event_code"], "TEST1");
        assert_eq!(v["allowedMeasurementIds"][0], "G-1");
        assert_eq!(v["enrich"]["key"], "user_id");
        assert_eq!(v["enrich"]["headerValue"], "[redacted]");
        assert_eq!(v["enrich"]["headerName"], "x-k");
        assert_eq!(v["client_secret"], "");
    }

    #[test]
    fn api_serves_traces_and_the_container() {
        let mut container = Container::builtin();
        container.tags.push(Tag {
            name: "Meta Conversions API".into(),
            template: "meta_capi".into(),
            data: json!({ "pixel_id": "123", "access_token": "EAAB" }),
            fire_when: vec![],
            enabled: true,
        });
        let traces = Arc::new(Traces::new(10));
        let req = IncomingRequest {
            method: "GET".into(),
            path: "/g/collect".into(),
            headers: vec![("cookie".into(), "_ga=1; _fbp=2".into())],
            ..Default::default()
        };
        let result = RunResult {
            events: vec![json!({ "event_name": "page_view" })],
            triggers: container.triggers(&[json!({ "event_name": "page_view" })]),
            ..Default::default()
        };
        traces.record(
            None,
            capture(&req),
            Duration::from_millis(3),
            Outcome::Ran(&result),
        );
        let admin = Admin {
            pool: crate::server::Pool::for_tests(),
            log: Default::default(),
            tenants: None,
            integrations: None,
            token: TOKEN.into(),
            traces,
            target: Target::Single(ContainerFile::fixed(
                container,
                Default::default(),
                Default::default(),
            )),
            workers: 2,
            ai: None,
            state: Arc::new(crate::state::State::memory()),
            history: Arc::new(crate::history::History::new(Arc::new(
                crate::state::State::memory(),
            ))),
            alerts: Arc::new(crate::alerts::Alerts::new(
                Arc::new(crate::state::State::memory()),
                Arc::new(crate::history::History::new(Arc::new(
                    crate::state::State::memory(),
                ))),
                None,
                None,
                None,
            )),
            check_token: None,
            drafts: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, router(admin)).await });
            let http = reqwest::Client::new();
            let call = |method: reqwest::Method, path: &str, token: &str| {
                http.request(method, format!("{base}{path}"))
                    .header("authorization", format!("Bearer {token}"))
                    .send()
            };
            let get = |path: &str| {
                let res = call(reqwest::Method::GET, path, TOKEN);
                async move {
                    let res = res.await.unwrap();
                    assert_eq!(res.status(), 200);
                    serde_json::from_str::<Value>(&res.text().await.unwrap()).unwrap()
                }
            };

            let denied = call(reqwest::Method::GET, "/admin/api/overview", "wrong")
                .await
                .unwrap();
            assert_eq!(denied.status(), 401);

            let o = get("/admin/api/overview").await;
            assert_eq!(o["server"]["mode"], "single");
            assert_eq!(o["server"]["trace_capacity"], 10);
            assert!(o["tenants"].is_null());

            let list = get("/admin/api/traces?limit=5&q=page_view").await;
            assert_eq!(list["latest"], 1);
            assert_eq!(list["traces"][0]["events"][0], "page_view");
            assert!(
                get("/admin/api/traces?after=1").await["traces"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );

            let t = get("/admin/api/traces/1").await;
            assert_eq!(t["request"]["headers"][0][1], "_ga, _fbp");
            assert_eq!(t["result"]["triggers"][1]["tag"], "Meta Conversions API");
            assert_eq!(t["result"]["triggers"][1]["fired"], true);

            let c = get("/admin/api/container").await;
            assert_eq!(c["tags"][1]["data"]["access_token"], "[redacted]");
            assert_eq!(c["tags"][1]["data"]["pixel_id"], "123");
            assert!(
                c["templates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == "meta_capi" && t["builtin"] == true)
            );

            let cleared = call(reqwest::Method::DELETE, "/admin/api/traces", TOKEN)
                .await
                .unwrap();
            assert_eq!(cleared.status(), 200);
            let gone = call(reqwest::Method::GET, "/admin/api/traces/1", TOKEN)
                .await
                .unwrap();
            assert_eq!(gone.status(), 404);

            let page = http.get(format!("{base}/admin")).send().await.unwrap();
            let csp = page.headers()["content-security-policy"].to_str().unwrap();
            assert!(csp.contains("font-src 'self'"), "{csp}");
            let font = http
                .get(format!("{base}/admin/assets/google-sans-text.woff2"))
                .send()
                .await
                .unwrap();
            assert_eq!(font.headers()["content-type"], "font/woff2");
            let missing = http
                .get(format!("{base}/admin/assets/nope.svg"))
                .send()
                .await
                .unwrap();
            assert_eq!(missing.status(), 404);
        });
    }

    #[test]
    fn tags_are_edited_in_the_container_file_and_tested_against_recent_hits() {
        let dir = std::env::temp_dir().join(format!("admin-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("container.json");
        let original = r#"{
  "version": "1",
  "clients": [{ "name": "GA4", "template": "ga4_client" }],
  "tags": [
    { "name": "GA4", "template": "ga4_tag" },
    { "name": "Meta", "template": "meta_capi", "data": { "pixel_id": "1", "access_token": "EAAB" } }
  ]
}
"#;
        std::fs::write(&path, original).unwrap();
        let file = ContainerFile::open(&path, Default::default(), Default::default()).unwrap();
        let traces = Arc::new(Traces::new(10));
        let ran = |events: Vec<Value>| {
            let result = RunResult {
                events,
                ..Default::default()
            };
            traces.record(
                None,
                capture(&IncomingRequest::default()),
                Duration::ZERO,
                Outcome::Ran(&result),
            );
        };
        ran(vec![json!({ "event_name": "page_view" })]);
        ran(vec![
            json!({ "event_name": "purchase", "value": 5 }),
            json!({ "event_name": "purchase", "value": 50 }),
        ]);
        ran(vec![json!({ "event_name": "purchase", "value": 7 })]);
        let admin = Admin {
            pool: crate::server::Pool::for_tests(),
            log: Default::default(),
            tenants: None,
            integrations: None,
            token: TOKEN.into(),
            traces: traces.clone(),
            target: Target::Single(file.clone()),
            workers: 2,
            ai: None,
            state: Arc::new(crate::state::State::memory()),
            history: Arc::new(crate::history::History::new(Arc::new(
                crate::state::State::memory(),
            ))),
            alerts: Arc::new(crate::alerts::Alerts::new(
                Arc::new(crate::state::State::memory()),
                Arc::new(crate::history::History::new(Arc::new(
                    crate::state::State::memory(),
                ))),
                None,
                None,
                None,
            )),
            check_token: None,
            drafts: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, router(admin)).await });
            let http = reqwest::Client::new();
            let send = |method: reqwest::Method, path: &str, body: Option<Value>| {
                let mut rb = http
                    .request(method, format!("{base}{path}"))
                    .header("authorization", format!("Bearer {TOKEN}"));
                if let Some(b) = body {
                    rb = rb.body(b.to_string());
                }
                async move {
                    let res = rb.send().await.unwrap();
                    let status = res.status().as_u16();
                    let v: Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
                    (status, v)
                }
            };
            use reqwest::Method;

            let (_, c) = send(Method::GET, "/admin/api/container", None).await;
            assert_eq!(c["editable"], true);
            assert!(c["reason"].is_null());
            let rev = c["revision"].as_str().unwrap().to_string();
            assert_eq!(c["tags"][1]["enabled"], true);
            assert_eq!(c["tags"][1]["source"], "file");
            assert_eq!(c["tags"][1]["editable"], true);
            assert_eq!(c["tags"][1]["data"]["access_token"], "[redacted]");
            let meta = c["templates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == "meta_capi")
                .unwrap();
            assert_eq!(meta["used_by"], json!(["Meta"]));

            // A broken tag is refused and nothing is written.
            let (status, e) = send(
                Method::POST,
                "/admin/api/container/tags",
                Some(json!({ "revision": rev, "tag": { "name": "X", "template": "nope" } })),
            )
            .await;
            assert_eq!(status, 400, "{e}");
            assert!(e["error"].as_str().unwrap().contains("unknown template nope"), "{e}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            let (status, _) = send(Method::POST, "/admin/api/container/tags", Some(json!({ "x": 1 }))).await;
            assert_eq!(status, 400);

            let (status, c) = send(
                Method::POST,
                "/admin/api/container/tags",
                Some(json!({ "revision": rev, "tag": { "name": "Big purchases", "template": "ga4_tag",
                    "fire_when": [{ "key": "event_name", "op": "equals", "value": "purchase" }] } })),
            )
            .await;
            assert_eq!(status, 200, "{c}");
            let c = &c["container"];
            assert_eq!(c["tags"][2]["name"], "Big purchases");
            let new_rev = c["revision"].as_str().unwrap().to_string();
            assert_ne!(new_rev, rev);
            assert_eq!(file.resolved().container.tags.len(), 3);

            // The old revision is stale now.
            let (status, e) = send(Method::DELETE, &format!("/admin/api/container/tags/GA4?revision={rev}"), None).await;
            assert_eq!(status, 409);
            assert_eq!(e["error"], "the container changed since it was loaded");
            assert_eq!(e["revision"], new_rev.as_str());

            // Rename, turn off and keep the hidden token.
            let (status, c) = send(
                Method::PUT,
                "/admin/api/container/tags/Meta",
                Some(json!({ "revision": new_rev, "tag": { "name": "Meta CAPI", "template": "meta_capi",
                    "enabled": false, "data": { "pixel_id": "2", "access_token": "[redacted]" } } })),
            )
            .await;
            assert_eq!(status, 200, "{c}");
            let c = &c["container"];
            assert_eq!(c["tags"][1]["name"], "Meta CAPI");
            assert_eq!(c["tags"][1]["enabled"], false);
            assert_eq!(file.resolved().container.tags[1].data["access_token"], "EAAB");
            let rev = c["revision"].as_str().unwrap().to_string();
            let (status, _) = send(
                Method::PUT,
                "/admin/api/container/tags/Nope",
                Some(json!({ "revision": rev, "tag": { "name": "Nope", "template": "ga4_tag" } })),
            )
            .await;
            assert_eq!(status, 404);

            let (status, c) = send(
                Method::POST,
                "/admin/api/container/tags/order",
                Some(json!({ "revision": rev, "names": ["Big purchases", "GA4", "Meta CAPI"] })),
            )
            .await;
            assert_eq!(status, 200, "{c}");
            assert_eq!(c["container"]["tags"][0]["name"], "Big purchases");
            let rev = c["container"]["revision"].as_str().unwrap().to_string();
            let (status, c) = send(Method::DELETE, &format!("/admin/api/container/tags/GA4?revision={rev}"), None).await;
            assert_eq!(status, 200, "{c}");
            assert_eq!(c["container"]["tags"].as_array().unwrap().len(), 2);

            let (status, t) = send(
                Method::POST,
                "/admin/api/container/test",
                Some(json!({ "tag": { "name": "T", "template": "ga4_tag", "fire_when": [
                    { "key": "event_name", "op": "equals", "value": "purchase" },
                    { "key": "value", "op": "matches_regex", "value": "^[0-9]$" }
                ] } })),
            )
            .await;
            assert_eq!(status, 200, "{t}");
            assert_eq!(t["hits"], 3);
            assert_eq!(t["events"], 4);
            assert_eq!(t["matched_events"], 2);
            assert_eq!(t["matched_hits"], json!([3, 2]));
            assert_eq!(t["matched_hit_count"], 2);
            assert_eq!(t["by_event_name"]["purchase"], json!({ "events": 3, "matched": 2 }));
            assert_eq!(t["by_event_name"]["page_view"], json!({ "events": 1, "matched": 0 }));
            assert_eq!(t["conditions"][0]["matched"], 3);
            assert_eq!(t["conditions"][1]["matched"], 2);
            let (status, _) = send(
                Method::POST,
                "/admin/api/container/test",
                Some(json!({ "tag": { "fire_when": [{ "key": "a", "op": "matches_regex", "value": "(" }] } })),
            )
            .await;
            assert_eq!(status, 400);
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn saves_are_kept_in_history_checked_without_saving_and_undone() {
        let dir = std::env::temp_dir().join(format!("undo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("container.json");
        let file_text = r#"{ "clients": [ { "name": "GA4", "template": "ga4_client" } ],
  "tags": [ { "name": "GA4", "template": "ga4_tag" } ] }"#;
        std::fs::write(&path, file_text).unwrap();
        let f = ContainerFile::open(&path, Default::default(), Default::default()).unwrap();
        let state = Arc::new(crate::state::State::memory());
        let traces = Arc::new(Traces::new(5));
        let req = IncomingRequest {
            method: "GET".into(),
            path: "/g/collect".into(),
            query: "v=2&tid=G-1&cid=1.2&en=purchase".into(),
            headers: vec![("user-agent".into(), "Mozilla/5.0 Test".into())],
            remote_address: "203.0.113.9".into(),
            ..Default::default()
        };
        let ran = RunResult {
            claimed_by: Some("GA4".into()),
            events: vec![json!({ "event_name": "purchase" })],
            ..Default::default()
        };
        traces.record(
            None,
            capture(&req),
            Duration::from_millis(1),
            Outcome::Ran(&ran),
        );
        let admin = Admin {
            pool: crate::server::Pool::for_tests(),
            log: Default::default(),
            tenants: None,
            integrations: None,
            token: TOKEN.into(),
            traces,
            target: Target::Single(f.clone()),
            workers: 1,
            ai: None,
            state: state.clone(),
            history: Arc::new(crate::history::History::new(state)),
            alerts: Arc::new(crate::alerts::Alerts::new(
                Arc::new(crate::state::State::memory()),
                Arc::new(crate::history::History::new(Arc::new(
                    crate::state::State::memory(),
                ))),
                None,
                None,
                None,
            )),
            check_token: None,
            drafts: None,
        };
        let tag: Tag = serde_json::from_value(
            json!({ "name": "GA4", "template": "ga4_tag", "enabled": false }),
        )
        .unwrap();
        let rev = f.revision().unwrap();

        // A check works the change out and writes nothing.
        let r = change(
            &admin,
            "x",
            &rev,
            Change::Update("GA4".into(), tag.clone()),
            true,
            None,
            To::Live,
        )
        .await;
        assert_eq!(r.status(), 200);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        assert_eq!(body["check"]["edits"][0]["fields"][0], "turned off");
        // Replayed: the hit in the buffer, and the same hit pinned.
        let replay = &body["check"]["replay"];
        assert_eq!(replay["hits"], 1, "{replay}");
        assert_eq!(replay["changed_hits"], 1, "{replay}");
        assert_eq!(replay["groups"][0]["kind"], "stops");
        assert_eq!(replay["groups"][0]["tag"], "GA4");
        let mut auth = HeaderMap::new();
        auth.insert(
            header::AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().unwrap(),
        );
        let r = pin(
            State(admin.clone()),
            auth.clone(),
            Bytes::from(r#"{"trace":1,"label":"A purchase"}"#),
        )
        .await;
        assert_eq!(r.status(), 200);
        let r = change(
            &admin,
            "x",
            &rev,
            Change::Delete("GA4".into()),
            true,
            None,
            To::Live,
        )
        .await;
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        assert_eq!(body["check"]["replay"]["hits"], 2);
        assert_eq!(
            body["check"]["replay"]["pinned_changed"],
            json!(["A purchase"])
        );
        assert_eq!(pinned_list(&admin)[0]["events"], json!(["purchase"]));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file_text);
        assert!(admin.history.list(10).is_empty());

        let r = change(
            &admin,
            "x",
            &rev,
            Change::Update("GA4".into(), tag),
            false,
            None,
            To::Live,
        )
        .await;
        assert_eq!(r.status(), 200);
        assert!(!f.resolved().container.tags[0].enabled);
        let entries = admin.history.list(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["undoable"], true);
        let id = entries[0]["id"].as_str().unwrap().to_string();

        let text = admin.history.before(&id).unwrap();
        let r = change(
            &admin,
            "undo",
            &f.revision().unwrap(),
            Change::Restore(text),
            false,
            Some(json!({ "kind": "undo", "of": id })),
            To::Live,
        )
        .await;
        assert_eq!(r.status(), 200);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file_text);
        assert!(f.resolved().container.tags[0].enabled);
        let entries = admin.history.list(10);
        assert_eq!(entries[0]["edits"][0]["kind"], "undo");
        assert_eq!(entries[0]["edits"][1]["fields"][0], "turned on");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn a_setup_replaces_tags_and_keeps_stored_secrets() {
        let dir = std::env::temp_dir().join(format!("setup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("container.json");
        std::fs::write(&path, r#"{ "clients": [ { "name": "GA4", "template": "ga4_client" } ],
  "tags": [ { "name": "GA4", "template": "ga4_tag" },
            { "name": "Meta", "template": "meta_capi", "data": { "pixel_id": "1", "access_token": "EAAsecret" } },
            { "name": "Old", "template": "ga4_tag" } ] }"#).unwrap();
        let f = ContainerFile::open(&path, Default::default(), Default::default()).unwrap();
        let state = Arc::new(crate::state::State::memory());
        let admin = Admin {
            pool: crate::server::Pool::for_tests(),
            log: Default::default(),
            tenants: None,
            integrations: None,
            token: TOKEN.into(),
            traces: Arc::new(Traces::new(1)),
            target: Target::Single(f.clone()),
            workers: 1,
            ai: None,
            state: state.clone(),
            history: Arc::new(crate::history::History::new(state.clone())),
            alerts: Arc::new(crate::alerts::Alerts::new(
                state.clone(),
                Arc::new(crate::history::History::new(state)),
                None,
                None,
                None,
            )),
            check_token: None,
            drafts: None,
        };
        let tags: Vec<Tag> = serde_json::from_value(json!([
            { "name": "GA4", "template": "ga4_tag" },
            { "name": "Meta", "template": "meta_capi", "data": { "pixel_id": "2", "access_token": "[redacted]" } },
            { "name": "Ads", "template": "google_ads", "data": { "customer_id": "1", "conversion_action_id": "2", "client_id": "", "client_secret": "", "refresh_token": "" } },
        ])).unwrap();
        let rules = vec![
            json!({ "name": "EUR", "steps": [{ "op": "set", "key": "currency", "value": "EUR", "only_if_missing": true }] }),
        ];
        let r = change(
            &admin,
            "setup",
            &f.revision().unwrap(),
            Change::Setup {
                tags,
                transformations: rules,
            },
            false,
            None,
            To::Live,
        )
        .await;
        assert_eq!(r.status(), 200);
        let c = f.resolved().container;
        let names: Vec<&str> = c.tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["GA4", "Meta", "Ads"]);
        assert_eq!(
            c.tags[1].data["access_token"], "EAAsecret",
            "a kept tag keeps its secret"
        );
        assert_eq!(c.tags[1].data["pixel_id"], "2");
        assert_eq!(c.transformations[0].name, "EUR");
        let kinds: Vec<String> = admin.history.list(1)[0]["edits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect();
        assert!(
            kinds.contains(&"tag_added".to_string())
                && kinds.contains(&"tag_deleted".to_string())
                && kinds.contains(&"transformation_added".to_string()),
            "{kinds:?}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn client_features_are_edited_in_the_draft() {
        let dir = std::env::temp_dir().join(format!("featuresapi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("container.json");
        std::fs::write(
            &path,
            r#"{ "clients": [ { "name": "GA4", "template": "ga4_client",
      "data": { "allowedMeasurementIds": ["G-1"], "botHandling": "flag" } } ],
  "tags": [ { "name": "GA4", "template": "ga4_tag" } ] }"#,
        )
        .unwrap();
        let f = ContainerFile::open(&path, Default::default(), Default::default()).unwrap();
        let state = Arc::new(crate::state::State::memory());
        let history = Arc::new(crate::history::History::new(state.clone()));
        let admin = Admin {
            pool: crate::server::Pool::for_tests(),
            log: Default::default(),
            tenants: None,
            integrations: None,
            token: TOKEN.into(),
            traces: Arc::new(Traces::new(1)),
            target: Target::Single(f.clone()),
            workers: 1,
            ai: None,
            state: state.clone(),
            history: history.clone(),
            alerts: Arc::new(crate::alerts::Alerts::new(
                state.clone(),
                history,
                None,
                None,
                None,
            )),
            check_token: None,
            drafts: Some(Arc::new(crate::drafts::Drafts::new(state, f.clone()))),
        };
        let mut auth = HeaderMap::new();
        auth.insert(
            header::AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().unwrap(),
        );
        let send = |features: Value| {
            let (admin, auth, rev) = (admin.clone(), auth.clone(), f.revision().unwrap());
            async move {
                let r = update_client_features(
                    State(admin),
                    auth,
                    Path("GA4".into()),
                    Bytes::from(json!({ "revision": rev, "features": features }).to_string()),
                )
                .await;
                let status = r.status();
                let v: Value = serde_json::from_slice(
                    &axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap(),
                )
                .unwrap();
                (status, v)
            }
        };

        let (status, _) = send(json!({ "bot_handling": "block" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, v) = send(json!({ "keep_cookies": ["meta"], "visitor_info": true })).await;
        assert_eq!(status, 200, "{v}");
        let client = &v["container"]["clients"][0]["data"];
        assert_eq!(
            client["allowedMeasurementIds"][0], "G-1",
            "other data stays"
        );
        assert_eq!(client["keepCookies"][0], "meta");
        assert_eq!(client["visitorInfo"], true);
        assert!(client.get("botHandling").is_none(), "replaced, not merged");
        let edit = &v["container"]["draft"]["edits"][0];
        assert_eq!(edit["kind"], "client_changed");
        assert_eq!(
            edit["fields"],
            json!(["data.botHandling", "data.keepCookies", "data.visitorInfo"])
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn edits_go_to_a_draft_that_is_published_as_a_version() {
        let dir = std::env::temp_dir().join(format!("draftapi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("container.json");
        let live_text = r#"{ "clients": [ { "name": "GA4", "template": "ga4_client" } ],
  "tags": [ { "name": "GA4", "template": "ga4_tag" } ] }"#;
        std::fs::write(&path, live_text).unwrap();
        let f = ContainerFile::open(&path, Default::default(), Default::default()).unwrap();
        let state = Arc::new(crate::state::State::memory());
        let admin = Admin {
            pool: crate::server::Pool::for_tests(),
            log: Default::default(),
            tenants: None,
            integrations: None,
            token: TOKEN.into(),
            traces: Arc::new(Traces::new(1)),
            target: Target::Single(f.clone()),
            workers: 1,
            ai: None,
            state: state.clone(),
            history: Arc::new(crate::history::History::new(state.clone())),
            alerts: Arc::new(crate::alerts::Alerts::new(
                state.clone(),
                Arc::new(crate::history::History::new(state.clone())),
                None,
                None,
                None,
            )),
            check_token: None,
            drafts: Some(Arc::new(crate::drafts::Drafts::new(state, f.clone()))),
        };
        let body = |r: Response| async move {
            serde_json::from_slice::<Value>(
                &axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap(),
            )
            .unwrap()
        };
        let off: Tag = serde_json::from_value(
            json!({ "name": "GA4", "template": "ga4_tag", "enabled": false }),
        )
        .unwrap();
        let r = change(
            &admin,
            "x",
            &f.revision().unwrap(),
            Change::Update("GA4".into(), off),
            false,
            None,
            To::Draft,
        )
        .await;
        let v = body(r).await;
        assert_eq!(
            v["container"]["tags"][0]["enabled"], false,
            "the console shows the draft"
        );
        assert_eq!(
            v["container"]["draft"]["edits"][0]["fields"][0],
            "turned off"
        );
        assert_eq!(v["container"]["draft"]["stale"], false);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            live_text,
            "live is untouched"
        );
        assert!(
            admin.history.list(5).is_empty(),
            "a draft edit isn't a version"
        );
        let draft_rev = v["container"]["revision"].as_str().unwrap().to_string();

        let mut auth = HeaderMap::new();
        auth.insert(
            header::AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().unwrap(),
        );
        let check = body(
            publish(
                State(admin.clone()),
                auth.clone(),
                Bytes::from(json!({ "revision": draft_rev, "check": true }).to_string()),
            )
            .await,
        )
        .await;
        assert_eq!(check["check"]["edits"][0]["fields"][0], "turned off");
        assert!(check["check"]["replay"].is_object());
        let r = publish(
            State(admin.clone()),
            auth.clone(),
            Bytes::from(
                json!({ "revision": draft_rev, "name": "GA4 off", "note": "testing" }).to_string(),
            ),
        )
        .await;
        assert_eq!(r.status(), 200);
        assert!(!f.resolved().container.tags[0].enabled, "published");
        let h = admin.history.list(5);
        assert_eq!(
            (
                h[0]["source"].as_str(),
                h[0]["name"].as_str(),
                h[0]["undoable"].as_bool()
            ),
            (Some("publish"), Some("GA4 off"), Some(true))
        );
        assert!(body(discard_draft(State(admin.clone()), auth.clone()).await).await["container"]["draft"].is_null());

        // Nothing to publish now.
        let r = publish(
            State(admin.clone()),
            auth,
            Bytes::from(json!({ "revision": "x" }).to_string()),
        )
        .await;
        assert_eq!(r.status(), 400);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_builtin_container_says_why_it_is_read_only() {
        let f = ContainerFile::fixed(Container::builtin(), Default::default(), Default::default());
        let admin = Admin {
            pool: crate::server::Pool::for_tests(),
            log: Default::default(),
            tenants: None,
            integrations: None,
            token: TOKEN.into(),
            traces: Arc::new(Traces::new(1)),
            target: Target::Single(f),
            workers: 1,
            ai: None,
            state: Arc::new(crate::state::State::memory()),
            history: Arc::new(crate::history::History::new(Arc::new(
                crate::state::State::memory(),
            ))),
            alerts: Arc::new(crate::alerts::Alerts::new(
                Arc::new(crate::state::State::memory()),
                Arc::new(crate::history::History::new(Arc::new(
                    crate::state::State::memory(),
                ))),
                None,
                None,
                None,
            )),
            check_token: None,
            drafts: None,
        };
        let v = current_view(&admin);
        assert_eq!(v["editable"], false);
        assert!(v["revision"].is_null());
        assert!(v["reason"].as_str().unwrap().contains("CONTAINER_FILE"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(change(
            &admin,
            "x",
            "",
            Change::Delete("GA4".into()),
            false,
            None,
            To::Live,
        ));
        assert_eq!(r.status(), 400);
    }
}
