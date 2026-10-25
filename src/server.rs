//! HTTP front end, the worker pool that owns the JavaScript runtimes, and the
//! transport that sends tags' outgoing requests.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderName, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::future::join_all;
use tokio::sync::oneshot;

use crate::admin::{self, Admin};
use crate::container_file::ContainerFile;
use crate::eventlog::{EventLog, without_queries};
use crate::google::GoogleAuth;
use crate::integrations::Integrations;
use crate::metrics::METRICS;
use crate::request::{IncomingRequest, from_proxy, same};
use crate::sandbox::{Engine, Outgoing, RunResult, SendResult, Transport};
use crate::tenants::{Resolved, Tenants};
use crate::traces::{self, Outcome};

const MAX_BODY: usize = 1024 * 1024;

/// The only address requests carrying the server's Google token may go to.
const BIGQUERY_API: &str = "https://bigquery.googleapis.com/";

type Job = (IncomingRequest, Resolved, oneshot::Sender<RunResult>);

/// Runtimes a worker keeps compiled at once; the least recently built is
/// dropped past this.
const ENGINES_PER_WORKER: usize = 16;

/// Fixed set of threads, each with its own QuickJS runtime.
pub struct Pool {
    tx: SyncSender<Job>,
}

impl Pool {
    /// One worker whose sends go nowhere, for tests that need an admin.
    #[cfg(test)]
    pub fn for_tests() -> Arc<Self> {
        Arc::new(Pool::new(1, 8, |t: &Resolved| {
            Engine::new(
                t.container.clone(),
                t.storage.clone(),
                Arc::new(crate::replay::Dry),
                Duration::from_millis(500),
                32 * 1024 * 1024,
            )
            .expect("javascript runtime")
        }))
    }

    pub fn new<F>(workers: usize, queue: usize, make_engine: F) -> Self
    where
        F: Fn(&Resolved) -> Engine + Send + Sync + 'static,
    {
        let (tx, rx) = sync_channel::<Job>(queue);
        let rx: Arc<Mutex<Receiver<Job>>> = Arc::new(Mutex::new(rx));
        let make_engine = Arc::new(make_engine);
        for i in 0..workers {
            let rx = rx.clone();
            let make_engine = make_engine.clone();
            std::thread::Builder::new()
                .name(format!("js-{i}"))
                .spawn(move || {
                    // One runtime per tenant, rebuilt when the tenant's settings change.
                    let mut engines: Vec<((String, u64), Engine)> = Vec::new();
                    loop {
                        let job = rx.lock().expect("job queue poisoned").recv();
                        let Ok((req, tenant, reply)) = job else { break };
                        let key = (tenant.id.clone(), tenant.generation);
                        let at = match engines.iter().position(|(k, _)| *k == key) {
                            Some(at) => at,
                            None => {
                                engines.retain(|((id, _), _)| *id != tenant.id);
                                if engines.len() >= ENGINES_PER_WORKER {
                                    engines.remove(0);
                                }
                                engines.push((key, make_engine(&tenant)));
                                engines.len() - 1
                            }
                        };
                        let _ = reply.send(engines[at].1.run(req));
                    }
                })
                .expect("spawn worker");
        }
        Pool { tx }
    }

    /// None when the queue is full or the workers are gone.
    pub async fn run(&self, req: IncomingRequest, tenant: Resolved) -> Option<RunResult> {
        let (reply, rx) = oneshot::channel();
        match self.tx.try_send((req, tenant, reply)) {
            Ok(()) => rx.await.ok(),
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => None,
        }
    }
}

/// Resolves names like the system resolver but fails when any address is
/// private or internal. Checking at connect time means a DNS answer that
/// changes after an earlier lookup can't reach internal hosts.
pub(crate) struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if addrs.is_empty() || addrs.iter().any(|a| is_private(a.ip())) {
                return Err("destination is private or internal".into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Sends over HTTPS, refusing private and internal destinations unless
/// `allow_private` is set. Redirects aren't followed, so a public URL can't
/// bounce a request to an internal one.
pub struct HttpTransport {
    client: reqwest::Client,
    handle: tokio::runtime::Handle,
    allow_private: bool,
    /// Extra attempts after a connection failure or a 502, 503 or 504.
    retries: u32,
    /// Adds a token to BigQuery inserts, when credentials are configured.
    google: Option<Arc<GoogleAuth>>,
}

impl HttpTransport {
    pub fn new(handle: tokio::runtime::Handle, allow_private: bool, retries: u32) -> Self {
        let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
        if !allow_private {
            // A proxy from HTTP(S)_PROXY would resolve the destination itself,
            // out of the resolver's sight.
            builder = builder
                .dns_resolver(Arc::new(PublicOnlyResolver))
                .no_proxy();
        }
        let client = builder.build().expect("http client");
        HttpTransport {
            client,
            handle,
            allow_private,
            retries,
            google: None,
        }
    }

    pub fn with_google(mut self, google: Option<Arc<GoogleAuth>>) -> Self {
        self.google = google;
        self
    }

    /// Sends once, then again after a short pause while the failure looks
    /// transient: the connection never opened, or a gateway answered 502, 503
    /// or 504. Timeouts are not retried, since the vendor may already have the
    /// hit, and all attempts together stay within the request's timeout.
    async fn send_one(&self, o: Outgoing) -> Result<SendResult, String> {
        let started = Instant::now();
        let mut attempt = 0;
        loop {
            let mut this = o.clone();
            this.timeout = o.timeout.saturating_sub(started.elapsed());
            let (result, transient) = self.attempt(this).await;
            let pause = Duration::from_millis(50 << attempt);
            if !transient || attempt >= self.retries || started.elapsed() + pause >= o.timeout {
                return result;
            }
            attempt += 1;
            METRICS.send_retries.fetch_add(1, Relaxed);
            tokio::time::sleep(pause).await;
        }
    }

    /// One try; the flag says whether a retry could help.
    async fn attempt(&self, o: Outgoing) -> (Result<SendResult, String>, bool) {
        let mut transient = false;
        let r = self.try_once(o, &mut transient).await;
        (r, transient)
    }

    async fn try_once(&self, o: Outgoing, transient: &mut bool) -> Result<SendResult, String> {
        let url = reqwest::Url::parse(&o.url).map_err(|e| e.to_string())?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("unsupported scheme".into());
        }
        // Names are checked by the resolver; IP literals never reach it.
        if !self.allow_private {
            let host = url.host_str().ok_or("missing host")?;
            let ip = host.trim_matches(['[', ']']).parse::<IpAddr>().ok();
            if ip.is_some_and(is_private) {
                return Err("destination is private or internal".into());
            }
        }
        let method = reqwest::Method::from_bytes(o.method.as_bytes()).map_err(|e| e.to_string())?;
        let mut rb = self
            .client
            .request(method, url)
            .timeout(o.timeout.max(Duration::from_millis(1)));
        for (k, v) in &o.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if o.google_auth {
            if !o.url.starts_with(BIGQUERY_API) {
                return Err("Google credentials only go to BigQuery".into());
            }
            let auth = self
                .google
                .as_ref()
                .ok_or("no Google credentials: see GOOGLE_AUTH")?;
            let token = tokio::time::timeout(o.timeout, auth.token())
                .await
                .map_err(|_| "timed_out".to_string())??;
            rb = rb.bearer_auth(token);
        }
        if !o.body.is_empty() {
            rb = rb.body(o.body);
        } else if !o.method.eq_ignore_ascii_case("GET") && !o.method.eq_ignore_ascii_case("HEAD") {
            // Without a body hyper sends no Content-Length, and some endpoints
            // (Google's /g/collect among them) answer 411 Length Required.
            rb = rb.header("content-length", "0");
        }
        let res = rb.send().await.map_err(|e| {
            if e.is_timeout() {
                return "timed_out".into();
            }
            // The resolver's refusal sits a few levels down the error chain.
            let mut msg = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(s) = source {
                msg = format!("{msg}: {s}");
                source = s.source();
            }
            *transient = e.is_connect() && !msg.contains("private or internal");
            msg
        })?;
        let status = res.status().as_u16();
        *transient = matches!(status, 502..=504);
        let headers = res
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
            .collect();
        let body = res.text().await.unwrap_or_default();
        Ok(SendResult {
            status,
            headers,
            body,
        })
    }
}

impl Transport for HttpTransport {
    fn send_all(&self, requests: Vec<Outgoing>) -> Vec<Result<SendResult, String>> {
        self.handle
            .block_on(join_all(requests.into_iter().map(|o| self.send_one(o))))
    }
}

pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || o[0] == 0
                || o[0] >= 240 // reserved
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // carrier-grade NAT
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4() {
                return is_private(IpAddr::V4(v4));
            }
            let s = v6.segments();
            let embedded = |hi: u16, lo: u16| {
                let [a, b] = hi.to_be_bytes();
                let [c, d] = lo.to_be_bytes();
                is_private(IpAddr::V4(std::net::Ipv4Addr::new(a, b, c, d)))
            };
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) carry an IPv4 address.
                || (s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4] && embedded(s[6], s[7]))
                || (s[0] == 0x2002 && embedded(s[1], s[2]))
        }
    }
}

/// Who the server runs hits for.
#[derive(Clone)]
pub enum Target {
    /// One container for every host (the default), from `CONTAINER_FILE` or
    /// built in.
    Single(Arc<ContainerFile>),
    /// A tenant per hostname.
    Multi(Arc<Tenants>),
    /// One container for every host, with ad-platform tags the admin page
    /// turns on and off.
    Integrated(Arc<Integrations>),
}

#[derive(Clone)]
pub struct App {
    pub pool: Arc<Pool>,
    pub target: Target,
    /// The management page and API, when an admin token is configured.
    pub admin: Option<Admin>,
    /// Debug output is off unless a token is configured.
    pub debug_token: Option<String>,
    /// When set, only requests from the edge proxy (carrying this secret) are served.
    pub proxy_secret: Option<String>,
    /// Counts per quarter hour for the alerts, with the admin console on.
    pub counts: Option<Arc<crate::counts::Recorder>>,
    /// Recent hits for the admin page, when it shows them.
    pub log: Option<Arc<EventLog>>,
}

/// The cookie that keeps a browser on the draft while it previews.
pub const PREVIEW_COOKIE: &str = "_sst_check";
/// How long a preview session lasts.
const PREVIEW_MS: u64 = 4 * 3_600_000;

/// The preview session a request belongs to, by the code on the page's
/// address (`sst_check=<code>` in the hit's `dl`) or the preview cookie,
/// and whether its sends are dry. Only a live check started as a preview,
/// and not older than 4 hours, counts.
fn preview_of(a: &Admin, req: &IncomingRequest) -> Option<(String, bool)> {
    let code_after = |text: &str, marker: &str| -> Option<String> {
        let at = text.find(marker)? + marker.len();
        let code: String = text[at..].chars().take(8).collect();
        (code.len() == 8 && code.chars().all(|c| c.is_ascii_hexdigit())).then_some(code)
    };
    let code = req
        .header("cookie")
        .and_then(|c| code_after(c, &format!("{PREVIEW_COOKIE}=")))
        .or_else(|| code_after(&req.query, "sst_check%3D"))
        .or_else(|| code_after(&req.body, "sst_check%3D"))?;
    let s = a.state.read(&format!("checkup/live-{code}.json"))?;
    let fresh = s["since"]
        .as_u64()
        .is_some_and(|t| t + PREVIEW_MS > crate::state::now_ms());
    (s["preview"] == serde_json::json!(true) && fresh)
        .then(|| (code, s["send"] != serde_json::json!(true)))
}

pub fn router(app: App) -> Router {
    let admin = app.admin.clone();
    let mut router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/system", get(system))
        .fallback(handle)
        .with_state(app);
    // The admin routes answer on any host and skip the proxy secret: they have
    // their own token, and the edge proxy only forwards the tracking path.
    if let Some(admin) = admin {
        router = router.merge(admin::router(admin));
    }
    router
}

/// Counters as JSON. Needs the debug token (and the proxy secret, when one is
/// set); without a configured token the route doesn't exist.
async fn system(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let Some(token) = app.debug_token.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((headers, _)) =
        from_proxy(headers, app.proxy_secret.as_deref(), &peer.ip().to_string())
    else {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    };
    if !headers
        .iter()
        .any(|(k, v)| k == "x-debug-token" && same(v.trim(), token))
    {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let mut snapshot = METRICS.snapshot();
    if let (Target::Multi(tenants), Some(obj)) = (&app.target, snapshot.as_object_mut()) {
        obj.insert("tenants".into(), tenants.stats());
    }
    (
        [("content-type", "application/json")],
        serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
    )
        .into_response()
}

async fn handle(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(body) = to_bytes(body, MAX_BODY).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response();
    };

    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();

    let Some((headers, remote)) =
        from_proxy(headers, app.proxy_secret.as_deref(), &peer.ip().to_string())
    else {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    };

    let raw_query = parts.uri.query().unwrap_or("");
    // `__debug` asks for the report; the token itself only counts in the
    // x-debug-token header, so it never lands in URL logs.
    let (query, debug) = take_debug_param(raw_query);
    let debug = debug.is_some()
        && app.debug_token.as_deref().is_some_and(|t| {
            headers
                .iter()
                .any(|(k, v)| k == "x-debug-token" && same(v.trim(), t))
        });

    let method = parts.method.as_str().to_string();
    let path = parts.uri.path().to_string();
    let incoming = IncomingRequest {
        method: method.clone(),
        path: path.clone(),
        query,
        remote_address: remote,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    // Kept for the admin console only while its trace buffer is on.
    let traces = app
        .admin
        .as_ref()
        .map(|a| a.traces.clone())
        .filter(|t| t.is_on());
    let captured = traces.as_ref().map(|_| traces::capture(&incoming));
    let received = Instant::now();

    // A browser previewing the draft runs it instead of the live container.
    let preview = app.admin.as_ref().and_then(|a| preview_of(a, &incoming));
    let draft = preview.as_ref().and_then(|(_, dry)| {
        let d = app.admin.as_ref()?.drafts.as_ref()?;
        let c = d.container()?;
        let c = match &app.target {
            Target::Integrated(i) => Arc::new(i.with_base(&c)),
            _ => c,
        };
        Some(d.resolved(c, *dry))
    });
    let tenant = match &app.target {
        _ if draft.is_some() => draft.expect("draft"),
        Target::Single(f) => f.resolved(),
        Target::Integrated(i) => i.resolved(),
        Target::Multi(tenants) => {
            let host = incoming.header("host").unwrap_or("");
            match tenants.resolve(host) {
                Some(t) => t,
                None => {
                    METRICS.unknown_host.fetch_add(1, Relaxed);
                    tracing::info!("{method} {path} -> 404 unknown host {host:?}");
                    if let (Some(t), Some(req)) = (&traces, captured) {
                        let error = format!("unknown host {host:?}");
                        let outcome = Outcome::Refused {
                            status: 404,
                            error: &error,
                        };
                        t.record(None, req, received.elapsed(), outcome);
                    }
                    return (StatusCode::NOT_FOUND, "unknown host").into_response();
                }
            }
        }
    };
    let tenant_id = if preview.is_some() {
        Some("preview".to_string())
    } else {
        matches!(app.target, Target::Multi(_)).then(|| tenant.id.clone())
    };

    METRICS.in_flight.fetch_add(1, Relaxed);
    let started = Instant::now();
    let result = app.pool.run(incoming, tenant.clone()).await;
    METRICS.in_flight.fetch_sub(1, Relaxed);
    let Some(mut result) = result else {
        METRICS.rejected_busy.fetch_add(1, Relaxed);
        tracing::warn!("{method} {path} -> 503 busy");
        if let (Some(t), Some(req)) = (&traces, captured) {
            let outcome = Outcome::Refused {
                status: 503,
                error: "busy: every worker was busy and the queue full",
            };
            t.record(tenant_id.as_deref(), req, received.elapsed(), outcome);
        }
        return (StatusCode::SERVICE_UNAVAILABLE, "busy").into_response();
    };
    let elapsed = started.elapsed();
    METRICS.record(&result, elapsed);
    tenant.stats.record(&result);
    // Preview hits stay out of the counts that alerts and checks read.
    if let Some(c) = app.counts.as_ref().filter(|_| preview.is_none()) {
        c.record(tenant_id.as_deref().unwrap_or(""), &result);
    }
    if let Some(log) = &app.log {
        log.record(&path, &result, elapsed, false);
    }
    let mode = if preview.is_some() { "preview " } else { "" };
    tracing::info!("{mode}{method} {path} -> {}", summarize(&result));

    if debug || captured.is_some() {
        result.triggers = tenant.container.triggers(&result.events);
    }
    if let (Some(t), Some(req)) = (&traces, captured) {
        t.record(
            tenant_id.as_deref(),
            req,
            received.elapsed(),
            Outcome::Ran(&result),
        );
    }

    if debug {
        // Plain user data is masked here as in traces.
        let mut report = serde_json::to_value(&result).unwrap_or_default();
        traces::mask_result(&mut report);
        return (
            [("content-type", "application/json")],
            serde_json::to_string_pretty(&report).unwrap_or_default(),
        )
            .into_response();
    }

    let mut r = result.response;
    // Keeps the preview on for the browser's next pages, for 4 hours.
    if let Some((code, _)) = &preview {
        r.headers.push((
            "set-cookie".into(),
            format!("{PREVIEW_COOKIE}={code}; Path=/; Max-Age=14400; SameSite=Lax"),
        ));
    }
    let mut res = Response::new(Body::from(r.body));
    *res.status_mut() = StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK);
    for (k, v) in r.headers {
        if let (Ok(k), Ok(v)) = (HeaderName::try_from(k), HeaderValue::try_from(v)) {
            res.headers_mut().append(k, v);
        }
    }
    res
}

/// One log line per request: who claimed it, how each tag ended, what each
/// outgoing request got back and the templates' own log lines. URLs are logged
/// without their query, which can carry personal data, so templates shouldn't
/// log it either.
pub fn summarize(r: &RunResult) -> String {
    let mut out = format!(
        "{} client={}",
        r.response.status,
        r.claimed_by.as_deref().unwrap_or("none")
    );
    if let Some(e) = &r.client_error {
        out.push_str(&format!(" client_error={e:?}"));
    }
    if !r.events.is_empty() {
        out.push_str(&format!(" events={}", r.events.len()));
    }
    for t in &r.tags {
        out.push_str(&format!(" tag[{}]={}", t.tag, t.outcome));
        if let Some(why) = &t.skip {
            out.push_str(&format!("({why})"));
        }
        if let Some(e) = &t.error {
            out.push_str(&format!("({e:?})"));
        }
    }
    for s in &r.sends {
        let target = s.url.split(['?', '#']).next().unwrap_or_default();
        match &s.result {
            Ok(status) => out.push_str(&format!(" send[{target}]={status}")),
            Err(e) => out.push_str(&format!(" send[{target}]=error({:?})", without_queries(e))),
        }
    }
    for l in &r.logs {
        out.push_str(&format!(" log={l:?}"));
    }
    out
}

/// Removes `__debug` (with or without a value) from the query string.
fn take_debug_param(query: &str) -> (String, Option<String>) {
    let mut found = None;
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            if k == "__debug" {
                found = Some(v.to_string());
                false
            } else {
                !p.is_empty()
            }
        })
        .collect();
    (kept.join("&"), found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "192.0.0.8",
            "198.18.0.1",
            "ff02::1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
        ] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "142.250.0.1",
            "2a00:1450::1",
            "100.128.0.1",
            "64:ff9b::8efa:1",
            "2002:8efa:1::",
        ] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn refuses_internal_destinations() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let transport = HttpTransport::new(tokio::runtime::Handle::current(), false, 0);
            for url in [
                "http://127.0.0.1:9/x",
                "http://[::1]:9/x",
                "http://169.254.169.254/computeMetadata/v1/",
                // A name is checked by the resolver at connect time.
                "http://localhost:9/x",
            ] {
                let err = transport
                    .send_one(Outgoing {
                        method: "GET".into(),
                        url: url.into(),
                        headers: vec![],
                        body: String::new(),
                        timeout: Duration::from_secs(5),
                        google_auth: false,
                    })
                    .await
                    .unwrap_err();
                assert!(err.contains("private or internal"), "{url}: {err}");
            }
        });
    }

    #[test]
    fn redirects_are_not_followed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/a", listener.local_addr().unwrap());
            tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 4096];
                let _ = sock.read(&mut buf).await.unwrap();
                sock.write_all(
                    b"HTTP/1.1 302 Found\r\nlocation: http://169.254.169.254/\r\ncontent-length: 0\r\n\r\n",
                )
                .await
                .unwrap();
            });
            let transport = HttpTransport::new(tokio::runtime::Handle::current(), true, 0);
            let res = transport
                .send_one(Outgoing {
                    method: "GET".into(),
                    url,
                    headers: vec![],
                    body: String::new(),
                    timeout: Duration::from_secs(5),
                    google_auth: false,
                })
                .await
                .unwrap();
            assert_eq!(res.status, 302);
        });
    }

    #[test]
    fn empty_post_carries_content_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let rt = tokio::runtime::Runtime::new().unwrap();
        let head = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/g/collect?v=2", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 4096];
                let n = sock.read(&mut buf).await.unwrap();
                sock.write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                    .await
                    .unwrap();
                String::from_utf8_lossy(&buf[..n]).to_lowercase()
            });
            let transport = HttpTransport::new(tokio::runtime::Handle::current(), true, 0);
            let res = transport
                .send_one(Outgoing {
                    method: "POST".into(),
                    url,
                    headers: vec![],
                    body: String::new(),
                    timeout: Duration::from_secs(5),
                    google_auth: false,
                })
                .await
                .unwrap();
            assert_eq!(res.status, 204);
            server.await.unwrap()
        });
        assert!(head.contains("content-length: 0"), "{head}");
    }

    #[test]
    fn retries_a_gateway_error_once_and_no_more() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (status, served) = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/g/collect", listener.local_addr().unwrap());
            // Answers 503 twice, then 204: with one retry the second answer is final.
            let server = tokio::spawn(async move {
                let mut served = 0;
                while let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = vec![0; 4096];
                    let _ = sock.read(&mut buf).await.unwrap();
                    served += 1;
                    let line = if served < 3 {
                        "503 Service Unavailable"
                    } else {
                        "204 No Content"
                    };
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 {line}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        )
                        .await;
                    if served == 2 {
                        break;
                    }
                }
                served
            });
            let transport = HttpTransport::new(tokio::runtime::Handle::current(), true, 1);
            let res = transport
                .send_one(Outgoing {
                    method: "GET".into(),
                    url,
                    headers: vec![],
                    body: String::new(),
                    timeout: Duration::from_secs(5),
                    google_auth: false,
                })
                .await
                .unwrap();
            (res.status, server.await.unwrap())
        });
        assert_eq!((status, served), (503, 2));
    }

    #[test]
    fn google_token_only_goes_to_bigquery_and_needs_credentials() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let transport = HttpTransport::new(tokio::runtime::Handle::current(), true, 0);
            let send = |url: &str| Outgoing {
                method: "POST".into(),
                url: url.into(),
                headers: vec![],
                body: "{}".into(),
                timeout: Duration::from_secs(5),
                google_auth: true,
            };
            let err = transport
                .send_one(send("https://evil.test/insertAll"))
                .await
                .unwrap_err();
            assert!(err.contains("only go to BigQuery"), "{err}");
            let err = transport
                .send_one(send(
                    "https://bigquery.googleapis.com/bigquery/v2/projects/p/datasets/d/tables/t/insertAll",
                ))
                .await
                .unwrap_err();
            assert!(err.contains("no Google credentials"), "{err}");
        });
    }

    #[test]
    fn strips_debug_param() {
        assert_eq!(
            take_debug_param("v=2&__debug=abc&en=x"),
            ("v=2&en=x".into(), Some("abc".into()))
        );
        assert_eq!(take_debug_param("v=2"), ("v=2".into(), None));
    }

    /// Records what live hits send.
    struct Sent(std::sync::Mutex<Vec<String>>);

    impl crate::sandbox::Transport for Sent {
        fn send_all(
            &self,
            requests: Vec<crate::sandbox::Outgoing>,
        ) -> Vec<Result<crate::sandbox::SendResult, String>> {
            let mut s = self.0.lock().unwrap();
            requests
                .into_iter()
                .map(|r| {
                    s.push(r.url);
                    Ok(crate::sandbox::SendResult {
                        status: 204,
                        headers: vec![],
                        body: String::new(),
                    })
                })
                .collect()
        }
    }

    #[tokio::test]
    async fn a_preview_browser_runs_the_draft_and_sends_nothing() {
        use crate::container::Tag;
        use crate::container_file::{Change, ContainerFile};
        let dir = std::env::temp_dir().join(format!("preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("container.json");
        std::fs::write(
            &path,
            r#"{ "clients": [ { "name": "GA4", "template": "ga4_client" } ],
  "tags": [ { "name": "GA4", "template": "ga4_tag" } ] }"#,
        )
        .unwrap();
        let file = ContainerFile::open(&path, Default::default(), Default::default()).unwrap();
        let state = Arc::new(crate::state::State::memory());
        let drafts = Arc::new(crate::drafts::Drafts::new(state.clone(), file.clone()));
        // The draft adds a second GA4 tag.
        let extra: Tag = serde_json::from_value(
            serde_json::json!({ "name": "GA4 copy", "template": "ga4_tag" }),
        )
        .unwrap();
        drafts
            .edit(&file.revision().unwrap(), Change::Add(extra), false)
            .unwrap();
        state.write("checkup/live-ab12cd34.json", &serde_json::json!({ "code": "ab12cd34", "since": crate::state::now_ms(), "preview": true })).unwrap();

        let sent = Arc::new(Sent(Default::default()));
        let live_transport = sent.clone();
        let pool = Pool::new(1, 8, move |t: &Resolved| {
            let tr: Arc<dyn crate::sandbox::Transport> = if t.id == "preview-dry" {
                Arc::new(crate::replay::Dry)
            } else {
                live_transport.clone()
            };
            crate::sandbox::Engine::new(
                t.container.clone(),
                t.storage.clone(),
                tr,
                Duration::from_secs(2),
                32 << 20,
            )
            .unwrap()
        });
        let history = Arc::new(crate::history::History::new(state.clone()));
        let counts = Arc::new(crate::counts::Recorder::new(state.clone()));
        let pool = Arc::new(pool);
        let admin = Admin {
            pool: pool.clone(),
            log: Default::default(),
            tenants: None,
            integrations: None,
            token: "t".repeat(32),
            traces: Arc::new(crate::traces::Traces::new(10)),
            target: Target::Single(file.clone()),
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
            drafts: Some(drafts),
        };
        let app = App {
            pool,
            log: None,
            target: Target::Single(file),
            admin: Some(admin.clone()),
            debug_token: None,
            proxy_secret: None,
            counts: Some(counts.clone()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(
                listener,
                router(app).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        });
        let http = reqwest::Client::new();
        let hit = |dl: &str, cookie: Option<&str>| {
            let mut r = http
                .get(format!(
                    "{base}/g/collect?v=2&tid=G-1&cid=1.2&en=page_view&dl={dl}"
                ))
                .header("user-agent", "Mozilla/5.0 Chrome/141.0");
            if let Some(c) = cookie {
                r = r.header("cookie", c);
            }
            r.send()
        };

        // An ordinary visitor: live, one GA4 send.
        hit("https%3A%2F%2Fshop%2F", None).await.unwrap();
        assert_eq!(sent.0.lock().unwrap().len(), 1);

        // The preview link: the draft runs (two tags), nothing is sent, and a cookie keeps it on.
        let r = hit("https%3A%2F%2Fshop%2F%3Fsst_check%3Dab12cd34", None)
            .await
            .unwrap();
        let cookie = r
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.starts_with("_sst_check=ab12cd34;"), "{cookie}");
        assert_eq!(
            sent.0.lock().unwrap().len(),
            1,
            "a dry preview sends nothing"
        );
        let t = admin.traces.details(None);
        assert_eq!(t[0].1["tenant"], "preview");
        assert_eq!(
            t[0].1["result"]["tags"].as_array().unwrap().len(),
            2,
            "the draft ran"
        );
        // The next page carries only the cookie.
        hit("https%3A%2F%2Fshop%2Fp2", Some("_sst_check=ab12cd34"))
            .await
            .unwrap();
        assert_eq!(admin.traces.details(None)[0].1["tenant"], "preview");
        assert_eq!(sent.0.lock().unwrap().len(), 1);
        // An unknown code is an ordinary visitor.
        hit("https%3A%2F%2Fshop%2F", Some("_sst_check=00000000"))
            .await
            .unwrap();
        assert_eq!(sent.0.lock().unwrap().len(), 2);

        // Only the two live hits are counted.
        counts.flush();
        let q = crate::counts::quarter_now();
        assert_eq!(crate::counts::Reader::new(&state).quarter(q)[""].hits, 2);
        std::fs::remove_dir_all(dir).ok();
    }
}
