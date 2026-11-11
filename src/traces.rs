//! Recent hits for the admin console's debugger: the request, how long it
//! took and the full run record, kept in memory only. The buffer is bounded,
//! oldest out first, and with no capacity nothing is recorded at all.
//!
//! Plain user-provided data (an email, a phone number, a name) never reaches
//! a trace: in events it shows as `[hashed before sending]`, and so do the
//! parameters that carried it in the request and in forwarded URLs. SHA-256
//! digests are kept.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::mpv2::RAW_HIT_KEY;
use crate::request::IncomingRequest;
use crate::sandbox::{RunKind, RunResult};

pub const DEFAULT_CAPACITY: usize = 200;
pub const MAX_CAPACITY: usize = 2000;
/// Most of a request body a trace keeps.
const MAX_BODY: usize = 16 * 1024;
/// Most traces one listing returns.
pub const MAX_LIMIT: usize = 500;

/// What a trace shows instead of a plain user data value.
pub const MASKED: &str = "[hashed before sending]";

/// Headers whose values never reach a trace. Cookies keep their names.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-debug-token",
    "x-proxy-secret",
];

pub struct Traces {
    capacity: usize,
    ring: Mutex<Ring>,
}

#[derive(Default)]
struct Ring {
    /// Last id handed out; ids start at 1 and never repeat.
    last: u64,
    items: VecDeque<Trace>,
}

struct Trace {
    id: u64,
    tenant: Option<String>,
    /// Lower-case text `q` is matched against.
    search: String,
    summary: Summary,
    /// Shared with whoever reads it, so nothing is copied while the ring is
    /// locked.
    record: Arc<Record>,
}

/// The request as a trace keeps it: see [`capture`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Captured {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub body_truncated: bool,
    pub remote_address: String,
}

/// One hit as the debugger keeps it. The setup check, the AI audit and the
/// change check read hits through this, by field, not by name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    /// When it was recorded, in milliseconds since 1970.
    pub at: u64,
    pub tenant: Option<String>,
    pub duration_ms: f64,
    pub status: u16,
    /// Why it was answered before any template ran.
    pub error: Option<String>,
    /// What the run did, with plain user data masked. None for a hit
    /// answered before any template ran.
    pub result: Option<RunResult>,
    pub request: Captured,
}

impl Captured {
    /// The request again, as far as a trace kept it, to run it once more.
    pub fn to_request(&self) -> IncomingRequest {
        IncomingRequest {
            method: self.method.clone(),
            path: self.path.clone(),
            query: self.query.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
            remote_address: self.remote_address.clone(),
        }
    }
}

impl Record {
    /// A record from the JSON a trace is shown as, for tests that describe a
    /// hit by what matters to them.
    #[cfg(test)]
    pub fn of(v: Value) -> Arc<Record> {
        Arc::new(serde_json::from_value(v).expect("a trace record"))
    }

    /// The events of the run, none when no template ran.
    pub fn events(&self) -> &[Value] {
        self.result.as_ref().map_or(&[], |r| &r.events)
    }
}

/// One row of the hit list.
#[derive(Debug, Serialize)]
struct Summary {
    at: u64,
    tenant: Option<String>,
    method: String,
    path: String,
    duration_ms: f64,
    status: u16,
    client: Option<String>,
    client_error: Option<String>,
    error: Option<String>,
    events: Vec<String>,
    tags: Tally,
    tag_runs: Vec<RunSummary>,
    sends: SendTally,
    has_error: bool,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    tag: String,
    event: usize,
    outcome: RunKind,
}

#[derive(Debug, Default, Serialize)]
struct SendTally {
    ok: usize,
    failed: usize,
}

/// How a hit ended.
pub enum Outcome<'a> {
    Ran(&'a RunResult),
    /// Answered before any template ran, e.g. 503 busy or 404 unknown host.
    Refused {
        status: u16,
        error: &'a str,
    },
}

/// Which traces a listing returns, newest first.
#[derive(Debug, Default, serde::Deserialize)]
pub struct Filter {
    /// Only traces newer than this id.
    pub after: Option<u64>,
    /// Only traces older than this id.
    pub before: Option<u64>,
    pub limit: Option<usize>,
    pub tenant: Option<String>,
    pub q: Option<String>,
}

impl Traces {
    pub fn new(capacity: usize) -> Self {
        Traces {
            capacity: capacity.min(MAX_CAPACITY),
            ring: Mutex::default(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn is_on(&self) -> bool {
        self.capacity > 0
    }

    /// Keeps one hit. `request` comes from [`capture`].
    pub fn record(
        &self,
        tenant: Option<&str>,
        request: Captured,
        elapsed: Duration,
        outcome: Outcome<'_>,
    ) {
        if !self.is_on() {
            return;
        }
        let at = crate::state::now_ms();
        let duration_ms = (elapsed.as_secs_f64() * 1e6).round() / 1e3;
        let tenant = tenant.map(str::to_string);
        let mut search = vec![request.path.clone()];
        let mut summary = Summary {
            at,
            tenant: tenant.clone(),
            method: request.method.clone(),
            path: request.path.clone(),
            duration_ms,
            status: 0,
            client: None,
            client_error: None,
            error: None,
            events: Vec::new(),
            tags: Tally::default(),
            tag_runs: Vec::new(),
            sends: SendTally::default(),
            has_error: true,
        };
        let mut record = Record {
            at,
            tenant: tenant.clone(),
            duration_ms,
            status: 0,
            error: None,
            result: None,
            request,
        };

        match outcome {
            Outcome::Refused { status, error } => {
                search.push(error.to_string());
                summary.status = status;
                summary.error = Some(error.to_string());
                record.status = status;
                record.error = Some(error.to_string());
            }
            Outcome::Ran(r) => {
                summary.events = r
                    .events
                    .iter()
                    .map(|e| e["event_name"].as_str().unwrap_or_default().to_string())
                    .collect();
                search.extend(summary.events.iter().cloned());
                search.extend(r.claimed_by.iter().cloned());
                search.extend(r.client_error.iter().cloned());
                for t in &r.tags {
                    // A tag that threw keeps the error; one that failed on its own has none.
                    let kind = t.kind();
                    summary.tags.add(kind);
                    summary.tag_runs.push(RunSummary {
                        tag: t.tag.clone(),
                        event: t.event,
                        outcome: kind,
                    });
                    search.push(t.tag.clone());
                    search.extend(t.error.iter().cloned());
                }
                let mut ok = 0;
                for s in &r.sends {
                    if s.accepted() {
                        ok += 1;
                    } else if let Err(e) = &s.result {
                        search.push(e.clone());
                    }
                    search.push(host_of(&s.url).to_string());
                }
                let failed = r.sends.len() - ok;
                summary.sends = SendTally { ok, failed };
                summary.has_error = r.client_error.is_some()
                    || summary.tags.went_wrong() > 0
                    || failed > 0
                    || r.response.status >= 500;
                summary.status = r.response.status;
                summary.client = r.claimed_by.clone();
                summary.client_error = r.client_error.clone();
                record.status = r.response.status;
                let mut run = r.clone();
                mask_run(&mut run);
                record.result = Some(run);
            }
        }

        let mut trace = Trace {
            id: 0,
            tenant,
            search: search.join("\n").to_lowercase(),
            summary,
            record: Arc::new(record),
        };
        let mut ring = self.ring.lock().expect("traces");
        ring.last += 1;
        trace.id = ring.last;
        if ring.items.len() >= self.capacity {
            ring.items.pop_front();
        }
        ring.items.push_back(trace);
    }

    /// Matching summaries, newest first, and the last id recorded.
    pub fn list(&self, f: &Filter) -> (Vec<Value>, u64) {
        let limit = f.limit.unwrap_or(100).min(MAX_LIMIT);
        let q =
            f.q.as_deref()
                .map(|q| q.trim().to_lowercase())
                .filter(|q| !q.is_empty());
        let tenant = f.tenant.as_deref().filter(|t| !t.is_empty());
        let ring = self.ring.lock().expect("traces");
        let out = ring
            .items
            .iter()
            .rev()
            .filter(|t| f.after.is_none_or(|a| t.id > a))
            .filter(|t| f.before.is_none_or(|b| t.id < b))
            .filter(|t| tenant.is_none() || t.tenant.as_deref() == tenant)
            .filter(|t| q.as_ref().is_none_or(|q| t.search.contains(q.as_str())))
            .take(limit)
            .map(|t| with_id(t.id, &t.summary))
            .collect();
        (out, ring.last)
    }

    pub fn get(&self, id: u64) -> Option<Arc<Record>> {
        let ring = self.ring.lock().expect("traces");
        let at = ring.items.binary_search_by_key(&id, |t| t.id).ok()?;
        Some(ring.items[at].record.clone())
    }

    /// The events of every hit that ran, newest first, with their trace ids.
    /// `tenant` keeps one tenant's hits only.
    pub fn events(&self, tenant: Option<&str>) -> Vec<(u64, Vec<Value>)> {
        let ring = self.ring.lock().expect("traces");
        ring.items
            .iter()
            .rev()
            .filter(|t| tenant.is_none() || t.tenant.as_deref() == tenant)
            .filter_map(|t| Some((t.id, t.record.result.as_ref()?.events.clone())))
            .collect()
    }

    /// Every hit's id and full record, newest first. `tenant` keeps one
    /// tenant's hits only.
    pub fn details(&self, tenant: Option<&str>) -> Vec<(u64, Arc<Record>)> {
        self.details_where(tenant, usize::MAX, |_| true)
    }

    /// The same, but only the hits `keep` wants and `limit` of them at
    /// most.
    pub fn details_where(
        &self,
        tenant: Option<&str>,
        limit: usize,
        keep: impl Fn(&Record) -> bool,
    ) -> Vec<(u64, Arc<Record>)> {
        let ring = self.ring.lock().expect("traces");
        ring.items
            .iter()
            .rev()
            .filter(|t| tenant.is_none() || t.tenant.as_deref() == tenant)
            .filter(|t| keep(&t.record))
            .take(limit)
            .map(|t| (t.id, t.record.clone()))
            .collect()
    }

    pub fn clear(&self) {
        self.ring.lock().expect("traces").items.clear();
    }

    /// Id of the oldest trace still kept.
    pub fn oldest(&self) -> Option<u64> {
        self.ring
            .lock()
            .expect("traces")
            .items
            .front()
            .map(|t| t.id)
    }
}

/// A row or a record as the API gives it: its fields and its id.
pub fn with_id(id: u64, fields: &impl Serialize) -> Value {
    let mut v = serde_json::to_value(fields).unwrap_or_default();
    v["id"] = json!(id);
    v
}

/// The host of a URL, or the URL itself when it has none.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// The request as a trace keeps it: secret headers redacted, cookies reduced
/// to their names, and the body cut to 16 KB.
pub fn capture(req: &IncomingRequest) -> Captured {
    let headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| {
            let v = if k == "cookie" {
                cookie_names(v)
            } else if SECRET_HEADERS.contains(&k.as_str()) {
                "[redacted]".to_string()
            } else {
                v.clone()
            };
            (k.clone(), v)
        })
        .collect();
    // A JSON body (an order webhook) can hold a name, an address or an
    // e-mail under any key, so only its shape is kept. A form body keeps
    // everything but the plain user data.
    let whole = match json_shape(&req.body) {
        Some(shape) => shape,
        None => req
            .body
            .split('\n')
            .map(mask_params)
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let mut cut = whole.len().min(MAX_BODY);
    while !whole.is_char_boundary(cut) {
        cut -= 1;
    }
    Captured {
        method: req.method.clone(),
        path: req.path.clone(),
        query: mask_params(&req.query),
        headers,
        body: whole[..cut].to_string(),
        body_truncated: cut < whole.len(),
        remote_address: req.remote_address.clone(),
    }
}

/// A JSON body with every value taken out: the keys and the kinds of value
/// stay, so the debugger still shows what arrived. None for a body that
/// isn't JSON. One that starts like JSON but doesn't parse is not kept at
/// all, since nothing in it can be told apart.
fn json_shape(body: &str) -> Option<String> {
    let text = body.trim_start();
    if !text.starts_with('{') && !text.starts_with('[') {
        return None;
    }
    fn shape(v: &mut Value) {
        match v {
            Value::Object(m) => m.values_mut().for_each(shape),
            Value::Array(a) => a.iter_mut().for_each(shape),
            Value::String(_) => *v = json!("[text]"),
            Value::Number(_) => *v = json!("[number]"),
            Value::Bool(_) | Value::Null => {}
        }
    }
    match serde_json::from_str::<Value>(text) {
        Ok(mut v) => {
            shape(&mut v);
            Some(v.to_string())
        }
        Err(_) => Some(format!("[{} bytes, not kept]", body.len())),
    }
}

/// How many of a hit's tag runs ended each way, as the hit list shows them.
#[derive(Debug, Default, Serialize)]
struct Tally {
    success: u32,
    failure: u32,
    timeout: u32,
    exception: u32,
    skipped: u32,
}

impl Tally {
    fn add(&mut self, kind: RunKind) {
        *match kind {
            RunKind::Success => &mut self.success,
            RunKind::Failure => &mut self.failure,
            RunKind::Timeout => &mut self.timeout,
            RunKind::Exception => &mut self.exception,
            RunKind::Skipped => &mut self.skipped,
        } += 1;
    }

    /// Runs that failed, threw or timed out.
    fn went_wrong(&self) -> u32 {
        self.failure + self.timeout + self.exception
    }
}

/// Headers that carry the visitor's IP address.
const IP_HEADERS: &[&str] = &[
    "x-forwarded-for",
    "x-real-ip",
    "x-sst-client-ip",
    "forwarded",
    "cf-connecting-ip",
    "true-client-ip",
    "x-client-ip",
    "fastly-client-ip",
];
/// Event data keys and hit parameters that carry the visitor's IP address.
pub const IP_KEYS: &[&str] = &["ip_override", "ip", "client_ip", "uip", "_uip"];
/// Most of a request body the AI is given.
const AI_BODY: usize = 8 * 1024;
/// Most template log lines the AI is given, and their longest length.
const AI_LOGS: usize = 100;
const AI_LOG_LINE: usize = 500;

/// A trace as the AI is given it: on top of what every trace already hides,
/// the visitor's IP address is removed from the headers, the event data,
/// the query and the sends' addresses, the body is cut to 8 KB and the logs
/// to the last 100 lines.
pub fn for_ai(record: &Record) -> Value {
    let mut t = serde_json::to_value(record).unwrap_or_default();
    let req = &mut t["request"];
    if let Some(m) = req.as_object_mut() {
        m.remove("remote_address");
    }
    if let Some(headers) = req["headers"].as_array_mut() {
        headers.retain(|h| {
            let name = h[0].as_str().unwrap_or_default().to_ascii_lowercase();
            !IP_HEADERS.contains(&name.as_str())
        });
    }
    if let Some(q) = req["query"].as_str() {
        req["query"] = json!(drop_params(q, IP_KEYS));
    }
    if let Some(body) = req["body"].as_str() {
        let lines: Vec<String> = body.split('\n').map(|l| drop_params(l, IP_KEYS)).collect();
        let body = lines.join("\n");
        let mut cut = body.len().min(AI_BODY);
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut < body.len() {
            req["body_truncated"] = json!(true);
        }
        req["body"] = json!(&body[..cut]);
    }
    if let Some(logs) = t["result"]["logs"].as_array_mut() {
        let skip = logs.len().saturating_sub(AI_LOGS);
        *logs = logs
            .drain(skip..)
            .map(|l| {
                json!(
                    l.as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(AI_LOG_LINE)
                        .collect::<String>()
                )
            })
            .collect();
    }
    if let Some(events) = t["result"].get_mut("events") {
        drop_keys(events, IP_KEYS);
        // The hit as the browser sent it rides along with each event.
        for e in events.as_array_mut().into_iter().flatten() {
            if let Some(Value::String(raw)) = e.get_mut(crate::mpv2::RAW_HIT_KEY) {
                *raw = drop_params(raw, IP_KEYS);
            }
        }
    }
    // The GA4 tag forwards the visitor's IP to Google as `_uip`.
    for s in t["result"]["sends"].as_array_mut().into_iter().flatten() {
        if let Some(Value::String(url)) = s.get_mut("url")
            && let Some((base, query)) = url.split_once('?')
        {
            *url = format!("{base}?{}", drop_params(query, IP_KEYS));
        }
    }
    t
}

/// Removes `names` from a query string (or a form body line), keeping the
/// other parameters exactly as they were.
fn drop_params(query: &str, names: &[&str]) -> String {
    query
        .split('&')
        .filter(|p| {
            let key = p.split_once('=').map_or(*p, |(k, _)| k);
            !names.contains(&key)
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Replaces the values of `keys`, wherever they sit, with `[removed]`.
fn drop_keys(v: &mut Value, keys: &[&str]) {
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                if keys.contains(&k.as_str()) {
                    *x = json!("[removed]");
                } else {
                    drop_keys(x, keys);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| drop_keys(x, keys)),
        _ => {}
    }
}

/// Whether a user data field holds a plain value: anything but a `sha256_`
/// digest and the tag's own `_` markers (`_tag_mode`).
fn plain_user_field(name: &str) -> bool {
    !name.starts_with("sha256_") && !name.starts_with('_')
}

fn is_digest(v: &str) -> bool {
    let v = v.trim().trim_end_matches('=');
    (v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        || (v.len() == 43
            && v.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'+' | b'/' | b' ')
            }))
}

/// Masks every plain value inside an event's `user_data`.
fn mask_user_data(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, v) in m.iter_mut() {
                match v {
                    Value::Object(_) | Value::Array(_) => mask_user_data(v),
                    Value::String(s) if s.is_empty() => {}
                    Value::Null => {}
                    _ if plain_user_field(k) => *v = json!(MASKED),
                    _ => {}
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(mask_user_data),
        _ => {}
    }
}

/// Masks the plain user data a form-encoded hit carries: `ep.user_data.*` and
/// `epn.user_data.*` values, and the plain parts of the Google tag's `em`
/// envelope. Everything else is kept exactly as sent.
pub fn mask_params(query: &str) -> String {
    if !query.contains("user_data") && !query.contains("em=") {
        return query.to_string();
    }
    let masked: String = form_urlencoded::byte_serialize(MASKED.as_bytes()).collect();
    query
        .split('&')
        .map(|pair| {
            let Some((raw_key, raw_value)) = pair.split_once('=') else {
                return pair.to_string();
            };
            let Some((key, value)) = form_urlencoded::parse(pair.as_bytes()).next() else {
                return pair.to_string();
            };
            let field = key
                .strip_prefix("ep.user_data.")
                .or_else(|| key.strip_prefix("epn.user_data."));
            if let Some(path) = field {
                let last = path.rsplit('.').next().unwrap_or(path);
                if plain_user_field(last) && !raw_value.is_empty() {
                    return format!("{raw_key}={masked}");
                }
            } else if key == "em" && value.split('~').any(|p| !envelope_part_ok(p)) {
                let parts: Vec<String> = value
                    .split('~')
                    .map(|p| match p.split_once('.') {
                        Some((code, _)) if !envelope_part_ok(p) => format!("{code}.{MASKED}"),
                        _ => p.to_string(),
                    })
                    .collect();
                let v: String =
                    form_urlencoded::byte_serialize(parts.join("~").as_bytes()).collect();
                return format!("{raw_key}={v}");
            }
            pair.to_string()
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// An `em` envelope part that can be shown: the version, an empty field or a digest.
fn envelope_part_ok(part: &str) -> bool {
    match part.split_once('.') {
        Some(("tv", _)) | None => true,
        Some((_, v)) => v.is_empty() || is_digest(v),
    }
}

/// Masks plain user data in a run record: the events (and the raw hit each
/// keeps), the values firing conditions were compared with, and the query
/// of forwarded URLs. Used for traces and the `__debug` report.
pub fn mask_run(run: &mut RunResult) {
    for ev in &mut run.events {
        if let Some(u) = ev.get_mut("user_data") {
            // `ep.user_data=jane@example.com` makes it a plain string.
            if u.is_string() || u.is_number() {
                *u = json!(MASKED);
            } else {
                mask_user_data(u);
            }
        }
        if let Some(Value::String(raw)) = ev.get_mut(RAW_HIT_KEY) {
            *raw = mask_params(raw);
        }
    }
    for c in run.triggers.iter_mut().flat_map(|t| &mut t.conditions) {
        let user = c.key == "user_data" || c.key.starts_with("user_data.");
        let last = c.key.rsplit('.').next().unwrap_or(&c.key);
        if user && (c.key == "user_data" || plain_user_field(last)) && c.actual.is_some() {
            c.actual = Some(MASKED.to_string());
        }
    }
    for s in &mut run.sends {
        if let Some((base, query)) = s.url.split_once('?') {
            s.url = format!("{base}?{}", hide_secret_params(&mask_params(query)));
        }
    }
}

/// Hides the value of a send's query parameter whose name says it holds a
/// secret (Snapchat's `access_token`, an API `key`), so no trace, debug
/// report or AI prompt carries it. The hit's own event and user parameters
/// are left as sent.
fn hide_secret_params(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _))
                if !["ep.", "epn.", "up.", "upn."]
                    .iter()
                    .any(|p| k.starts_with(p))
                    && (k == "key" || crate::admin::secret_name(k)) =>
            {
                format!("{k}=[redacted]")
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// `_ga=GA1.1.1; _fbp=fb.1.2` becomes `_ga, _fbp`.
fn cookie_names(header: &str) -> String {
    header
        .split(';')
        .filter_map(|c| {
            let name = c.split('=').next().unwrap_or_default().trim();
            (!name.is_empty()).then_some(name)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{SendRecord, TagOutcome, TagRun};

    /// A kept hit as the API gives it.
    fn shown(t: &Traces, id: u64) -> Value {
        with_id(id, &*t.get(id).unwrap())
    }

    fn hit(path: &str) -> Captured {
        capture(&IncomingRequest {
            method: "GET".into(),
            path: path.into(),
            ..Default::default()
        })
    }

    fn ran(t: &Traces, path: &str, tenant: Option<&str>) {
        t.record(
            tenant,
            hit(path),
            Duration::from_millis(2),
            Outcome::Ran(&RunResult::default()),
        );
    }

    #[test]
    fn keeps_the_newest_up_to_capacity() {
        let t = Traces::new(3);
        for i in 0..5 {
            ran(&t, &format!("/p{i}"), None);
        }
        let (all, latest) = t.list(&Filter::default());
        assert_eq!(latest, 5);
        let ids: Vec<u64> = all.iter().map(|s| s["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, [5, 4, 3]);
        assert_eq!(all[0]["path"], "/p4");
        assert!(t.get(2).is_none());
        assert_eq!(shown(&t, 4)["request"]["path"], "/p3");
        t.clear();
        assert!(t.list(&Filter::default()).0.is_empty());
        ran(&t, "/again", None);
        assert_eq!(t.list(&Filter::default()).1, 6);
    }

    #[test]
    fn off_records_nothing_and_capacity_is_capped() {
        let t = Traces::new(0);
        ran(&t, "/x", None);
        assert_eq!(t.list(&Filter::default()), (vec![], 0));
        assert_eq!(Traces::new(1_000_000).capacity(), MAX_CAPACITY);
    }

    #[test]
    fn pages_by_id_tenant_and_text() {
        let t = Traces::new(50);
        for i in 0..10 {
            ran(
                &t,
                &format!("/p{i}"),
                Some(if i % 2 == 0 { "a" } else { "b" }),
            );
        }
        let ids = |f: Filter| -> Vec<u64> {
            t.list(&f)
                .0
                .iter()
                .map(|s| s["id"].as_u64().unwrap())
                .collect()
        };
        assert_eq!(
            ids(Filter {
                after: Some(7),
                ..Default::default()
            }),
            [10, 9, 8]
        );
        assert_eq!(
            ids(Filter {
                before: Some(4),
                limit: Some(2),
                ..Default::default()
            }),
            [3, 2]
        );
        assert_eq!(
            ids(Filter {
                tenant: Some("b".into()),
                after: Some(5),
                ..Default::default()
            }),
            [10, 8, 6]
        );
        assert_eq!(
            ids(Filter {
                q: Some("/P3".into()),
                ..Default::default()
            }),
            [4]
        );
    }

    #[test]
    fn summary_counts_and_search() {
        let t = Traces::new(5);
        let tag = |name: &str, outcome, error: Option<&str>| TagRun {
            tag: name.into(),
            event: 0,
            outcome,
            error: error.map(Into::into),
            skip: None,
        };
        let r = RunResult {
            claimed_by: Some("GA4".into()),
            events: vec![json!({"event_name": "purchase"})],
            tags: vec![
                tag("GA4", TagOutcome::Success, None),
                tag("Meta Conversions API", TagOutcome::Failure, None),
                tag("Broken", TagOutcome::Failure, Some("x is not defined")),
            ],
            sends: vec![SendRecord {
                from: "GA4".into(),
                method: "POST".into(),
                url: "https://graph.facebook.com/v21.0/1/events".into(),
                result: Ok(400),
                detail: None,
            }],
            ..Default::default()
        };
        t.record(None, hit("/g/collect"), Duration::ZERO, Outcome::Ran(&r));
        t.record(
            None,
            hit("/g/collect"),
            Duration::ZERO,
            Outcome::Refused {
                status: 503,
                error: "busy",
            },
        );
        let (all, _) = t.list(&Filter::default());
        assert_eq!(all[0]["status"], 503);
        assert_eq!(all[0]["has_error"], true);
        let s = &all[1];
        assert_eq!(
            s["tags"],
            json!({"success": 1, "failure": 1, "timeout": 0, "exception": 1, "skipped": 0})
        );
        assert_eq!(s["tag_runs"][2]["outcome"], "exception");

        // A tag that skipped its event (no consent) is neither a success nor
        // an error.
        let skipped = RunResult {
            claimed_by: Some("GA4".into()),
            events: vec![json!({"event_name": "page_view"})],
            tags: vec![
                tag("GA4", TagOutcome::Success, None),
                tag("TikTok Events API", TagOutcome::Skipped, None),
            ],
            ..Default::default()
        };
        t.record(
            None,
            hit("/g/collect"),
            Duration::ZERO,
            Outcome::Ran(&skipped),
        );
        let (newest, _) = t.list(&Filter::default());
        assert_eq!(
            newest[0]["tags"],
            json!({"success": 1, "failure": 0, "timeout": 0, "exception": 0, "skipped": 1})
        );
        assert_eq!(newest[0]["has_error"], false);
        assert_eq!(s["sends"], json!({"ok": 0, "failed": 1}));
        assert_eq!(s["events"], json!(["purchase"]));
        assert_eq!(s["has_error"], true);
        for q in ["purchase", "meta conv", "NOT DEFINED", "graph.facebook.com"] {
            let f = Filter {
                q: Some(q.into()),
                ..Default::default()
            };
            assert_eq!(t.list(&f).0.len(), 1, "{q}");
        }
        assert_eq!(shown(&t, 1)["result"]["claimed_by"], "GA4");
    }

    #[test]
    fn plain_user_data_never_reaches_a_trace() {
        let hex = "b4c9a289323b21a01c3e940f150eb9b8c542587f1abfd8f0e1cc1ffc5e475514";
        let query = format!(
            "v=2&en=purchase&em=tv.1~em.{hex}~ph.%2B4917012~ct.berlin&ep.user_data.email_address=jane%40example.com&ep.user_data.sha256_phone_number={hex}&ep.user_data._tag_mode=MANUAL&ep.other=kept"
        );
        let req = IncomingRequest {
            method: "POST".into(),
            path: "/g/collect".into(),
            query: query.clone(),
            body: "en=sign_up&ep.user_data.address.first_name=Jane\nen=x".into(),
            ..Default::default()
        };
        let events = crate::mpv2::extract_events(&req);
        let mut c = crate::container::Container::builtin();
        c.tags[0].fire_when = serde_json::from_value(json!([
            { "key": "user_data.email_address", "op": "contains", "value": "@" },
            { "key": "user_data.sha256_phone_number", "op": "equals", "value": hex }
        ]))
        .unwrap();
        let r = RunResult {
            triggers: c.triggers(&events),
            sends: vec![SendRecord {
                from: "GA4".into(),
                method: "POST".into(),
                url: format!("https://www.google-analytics.com/g/collect?{query}"),
                result: Ok(204),
                detail: None,
            }],
            events,
            ..Default::default()
        };
        let t = Traces::new(5);
        t.record(None, capture(&req), Duration::ZERO, Outcome::Ran(&r));
        let d = shown(&t, 1);
        let text = d.to_string();
        for plain in ["jane", "Jane", "berlin", "4917012"] {
            assert!(!text.contains(plain), "{plain} in {text}");
        }
        let u = &d["result"]["events"][0]["user_data"];
        assert_eq!(u["email_address"], MASKED);
        assert_eq!(u["phone_number"], MASKED);
        assert_eq!(u["address"]["city"], MASKED);
        assert_eq!(u["sha256_email_address"], hex);
        assert_eq!(u["sha256_phone_number"], hex);
        assert_eq!(u["_tag_mode"], "MANUAL");
        assert_eq!(
            d["result"]["events"][0]["user_data"]["address"]["first_name"],
            MASKED
        );
        let q = d["request"]["query"].as_str().unwrap();
        assert!(
            q.contains("ep.user_data.email_address=%5Bhashed+before+sending%5D"),
            "{q}"
        );
        assert!(
            q.contains(&format!("ep.user_data.sha256_phone_number={hex}")),
            "{q}"
        );
        assert!(
            q.contains("ep.other=kept") && q.contains(&format!("em.{hex}")),
            "{q}"
        );
        let conds = &d["result"]["triggers"][0]["conditions"];
        assert_eq!(conds[0]["actual"], MASKED);
        assert_eq!(conds[1]["actual"], hex);
        assert!(d["request"]["body"].as_str().unwrap().ends_with("\nen=x"));
        assert!(
            d["result"]["events"][0][RAW_HIT_KEY]
                .as_str()
                .unwrap()
                .contains("ep.other=kept")
        );
    }

    #[test]
    fn redacts_secrets_and_cuts_the_body() {
        let req = IncomingRequest {
            method: "POST".into(),
            path: "/g/collect".into(),
            headers: vec![
                (
                    "cookie".into(),
                    "_ga=GA1.1.1.2; _fbp=fb.1.2.3;  other=x".into(),
                ),
                ("authorization".into(), "Bearer abc".into()),
                ("x-debug-token".into(), "debugdebugdebug1".into()),
                ("x-proxy-secret".into(), "s".into()),
                ("user-agent".into(), "Mozilla".into()),
            ],
            body: "é".repeat(MAX_BODY),
            ..Default::default()
        };
        let v = json!(capture(&req));
        let h = &v["headers"];
        assert_eq!(h[0], json!(["cookie", "_ga, _fbp, other"]));
        for i in 1..4 {
            assert_eq!(h[i][1], "[redacted]");
        }
        assert_eq!(h[4][1], "Mozilla");
        assert_eq!(v["body"].as_str().unwrap().len(), MAX_BODY);
        assert_eq!(v["body_truncated"], true);
    }

    #[test]
    fn a_json_body_keeps_its_shape_and_none_of_its_values() {
        let req = IncomingRequest {
            method: "POST".into(),
            path: "/shopify/orders".into(),
            body: r#"{"id":820982911946154508,"email":"jane@example.com","test":false,
                "customer":{"first_name":"Jane","phone":"+491701234567"},
                "line_items":[{"title":"Shirt","price":"19.90"}],"note":null}"#
                .into(),
            ..Default::default()
        };
        let v = json!(capture(&req));
        let body = v["body"].as_str().unwrap();
        for plain in [
            "jane@example.com",
            "Jane",
            "+491701234567",
            "820982911946154508",
        ] {
            assert!(!body.contains(plain), "{plain}: {body}");
        }
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({"id": "[number]", "email": "[text]", "test": false,
                "customer": {"first_name": "[text]", "phone": "[text]"},
                "line_items": [{"title": "[text]", "price": "[text]"}], "note": null})
        );
        // What only looks like JSON is not kept at all.
        let broken = IncomingRequest {
            body: r#"{"email":"jane@example.com""#.into(),
            ..Default::default()
        };
        assert_eq!(json!(capture(&broken))["body"], "[27 bytes, not kept]");

        // A plain string where the user data object should be is masked too.
        let mut run = RunResult {
            events: vec![json!({ "user_data": "jane@example.com" })],
            ..Default::default()
        };
        mask_run(&mut run);
        assert_eq!(run.events[0]["user_data"], MASKED);
    }
}
