//! Runs a request through the container: clients, then the tags that fire for
//! each event, in sandboxed QuickJS contexts.
//!
//! Every template run gets its own context, so templates can't see each
//! other's globals. Values cross between them only as JSON, through the host
//! bindings below. Promises, outgoing sends and container completions are
//! driven by [`Engine::run`]'s loop until nothing is left to do or the run's
//! time budget is spent.

mod host;

pub(crate) use host::bigquery_insert_url;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::module::WriteOptions;
use rquickjs::{Context, Ctx, Function, Module, Object, Persistent, Runtime, Value as JsValue};
use serde::Serialize;
use serde_json::Value;

use crate::container::{Container, Trigger};

/// An answer's body with the values of secret-looking keys hidden, before it
/// is kept with the hit: an OAuth answer's access token would otherwise reach
/// the debug report, traces, the event log and the AI. JSON string values and
/// form or query values are covered.
fn without_secrets(body: &str) -> String {
    use std::sync::LazyLock;
    const KEYS: &str = r"(?i:access_token|refresh_token|id_token|client_secret|api_key|apikey|password|secret|token)";
    static JSON: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(&format!(r#""({KEYS})"(\s*:\s*)"(?:[^"\\]|\\.)*""#)).expect("regex")
    });
    static FORM: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(&format!(r"(^|[&?])({KEYS})=[^&]*")).expect("regex"));
    let body = JSON.replace_all(body, r#""$1"$2"[redacted]""#);
    FORM.replace_all(&body, "$1$2=[redacted]").into_owned()
}
use crate::request::IncomingRequest;

/// Most messages delivered to listeners in one run, so two templates that
/// answer each other can't loop until the time budget is spent.
const MAX_MESSAGES: usize = 1000;

const PRELUDE: &str = include_str!("prelude.js");
use crate::permissions::API_PERMISSIONS;

/// An HTTP request a template asked to send.
#[derive(Debug, Clone, Serialize)]
pub struct Outgoing {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub timeout: Duration,
    /// Sent with the server's Google access token. Only `BigQuery.insert`
    /// sets it, for a URL it builds itself.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub google_auth: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Sends outgoing requests. The server uses HTTP; tests record them.
pub trait Transport: Send + Sync {
    fn send_all(&self, requests: Vec<Outgoing>) -> Vec<Result<SendResult, String>>;
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    #[serde(skip)]
    pub body: Vec<u8>,
}

/// What happened during a run: the response, plus what the debug view shows.
#[derive(Debug, Default, Serialize)]
pub struct RunResult {
    pub claimed_by: Option<String>,
    pub client_error: Option<String>,
    pub response: Response,
    pub events: Vec<Value>,
    pub tags: Vec<TagRun>,
    pub sends: Vec<SendRecord>,
    pub logs: Vec<String>,
    /// Why each tag did or didn't fire for each event. Filled by the server
    /// only for the debug report and the admin traces.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<Trigger>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagRun {
    pub tag: String,
    pub event: usize,
    pub outcome: &'static str,
    pub error: Option<String>,
    /// Why a tag chose not to send (`outcome` is then "skipped").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendRecord {
    pub from: String,
    pub method: String,
    pub url: String,
    pub result: Result<u16, String>,
    /// The start of the answer when the vendor refused the send, so a wrong
    /// id or token can be told apart from a network problem, and of every
    /// answer to a POST (an event delivery, such as Meta's `events_received`).
    /// Accepted GETs are left out: the enrichment lookup answers with the
    /// visitor's own details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub type Storage = Arc<Mutex<HashMap<String, HashMap<String, String>>>>;

pub struct Engine {
    rt: Runtime,
    container: Arc<Container>,
    storage: Storage,
    transport: Arc<dyn Transport>,
    budget: Duration,
    deadline: Rc<Cell<Option<Instant>>>,
    /// Compiled prelude and templates, loaded instead of parsed on every hit.
    /// Declared after `rt` so it outlives the runtime's own teardown, and
    /// entries are never removed because loaded code points into them.
    code: RefCell<HashMap<String, &'static [u8]>>,
    /// For a replay: the time templates see, in milliseconds, with random
    /// numbers that repeat for the same time and template, so two runs of
    /// one hit can be compared.
    clock: Cell<Option<u64>>,
}

struct EventRun {
    data: Value,
    /// The event as transformations changed it, by tag name, for tags whose
    /// event differs from `data`.
    views: HashMap<String, Value>,
    client_ctx: usize,
    started: bool,
    tag_ctxs: Vec<usize>,
    notified: bool,
}

struct CtxMeta {
    name: String,
    /// The `templateDataStorage` map this context reads and writes.
    storage: String,
    event: Option<usize>,
    is_tag: bool,
}

/// Per-run state the host bindings read and write.
pub(crate) struct State {
    req: IncomingRequest,
    req_json: String,
    container_version: String,
    claimed: Option<usize>,
    response: Response,
    returned: bool,
    events: Vec<EventRun>,
    sends: Vec<(u32, usize, Outgoing)>,
    next_send: u32,
    logs: Vec<String>,
    tag_done: HashMap<usize, bool>,
    /// Tags that finished without sending, with their reason.
    tag_skipped: HashMap<usize, String>,
    meta: Vec<CtxMeta>,
    send_log: Vec<SendRecord>,
    /// `addEventCallback` registrations: (event, context, callback id).
    callbacks: Vec<(usize, usize, u32)>,
    /// `addMessageListener` registrations: (message type, context, listener id).
    listeners: Vec<(String, usize, u32)>,
    /// Messages sent but not yet delivered: (type, message as JSON).
    messages: Vec<(String, String)>,
    delivered: usize,
    deadline: Instant,
}

struct Slot {
    // Declared first so it is dropped before its context.
    dispatch: Option<Persistent<Function<'static>>>,
    ctx: Context,
}

impl Engine {
    pub fn new(
        container: Arc<Container>,
        storage: Storage,
        transport: Arc<dyn Transport>,
        budget: Duration,
        memory_limit: usize,
    ) -> rquickjs::Result<Self> {
        let rt = Runtime::new()?;
        rt.set_memory_limit(memory_limit);
        rt.set_max_stack_size(512 * 1024);
        let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let d = deadline.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            d.get().is_some_and(|t| Instant::now() > t)
        })));
        Ok(Engine {
            rt,
            container,
            storage,
            transport,
            budget,
            deadline,
            code: RefCell::new(HashMap::new()),
            clock: Cell::new(None),
        })
    }

    /// Fixes the time templates see, and their random numbers, for the runs
    /// that follow; None goes back to the real clock.
    pub fn set_clock(&self, at: Option<u64>) {
        self.clock.set(at);
    }

    /// Evaluates `expr` (a function expression) to a function. The first call
    /// per key compiles it to bytecode; later calls load that bytecode, which
    /// skips parsing. Every call still gets a fresh function in its own
    /// context, so nothing a template does carries over between hits.
    fn function<'js>(
        &self,
        ctx: &Ctx<'js>,
        key: &str,
        expr: &str,
    ) -> rquickjs::Result<Function<'js>> {
        let cached = self.code.borrow().get(key).copied();
        let module = match cached {
            // SAFETY: the bytes were produced by `Module::write` below and
            // live for the rest of the process.
            Some(bytes) => unsafe { Module::load(ctx.clone(), bytes)? },
            None => {
                let m = Module::declare(ctx.clone(), key, format!("export default {expr};"))?;
                let bytes = m.write(WriteOptions {
                    strip_debug: true,
                    ..Default::default()
                })?;
                self.code
                    .borrow_mut()
                    .insert(key.to_string(), Box::leak(bytes.into_boxed_slice()));
                m
            }
        };
        let (module, promise) = module.eval()?;
        promise.finish::<()>()?;
        module.get("default")
    }

    pub fn run(&self, req: IncomingRequest) -> RunResult {
        let started = Instant::now();
        let deadline = started + self.budget;
        self.deadline.set(Some(deadline));

        let state = Rc::new(RefCell::new(State {
            req_json: serde_json::to_string(&req).unwrap_or_else(|_| "{}".into()),
            req,
            container_version: self.container.version.clone(),
            claimed: None,
            response: Response {
                status: 200,
                ..Default::default()
            },
            returned: false,
            events: vec![],
            sends: vec![],
            next_send: 0,
            logs: vec![],
            tag_done: HashMap::new(),
            tag_skipped: HashMap::new(),
            meta: vec![],
            send_log: vec![],
            callbacks: vec![],
            listeners: vec![],
            messages: vec![],
            delivered: 0,
            deadline,
        }));
        let mut slots: Vec<Slot> = vec![];
        let mut errors: HashMap<usize, String> = HashMap::new();
        let mut result = RunResult::default();

        for client in &self.container.clients {
            let src = &self.container.templates[&client.template];
            let (idx, err) = self.spawn(
                &state,
                &mut slots,
                &client.name,
                &client.template,
                src,
                &client.data,
                None,
                false,
            );
            if let Some(e) = err {
                errors.insert(idx, e);
            }
            if state.borrow().claimed == Some(idx) {
                break;
            }
            // Unclaimed: let its jobs run, then move on to the next client.
            self.drain_jobs(&state);
            if state.borrow().claimed == Some(idx) {
                break;
            }
        }

        let claimed = state.borrow().claimed;
        match claimed {
            None => {
                result.response = Response {
                    status: 400,
                    headers: vec![("content-type".into(), "text/plain".into())],
                    body: b"no client claimed the request".to_vec(),
                };
            }
            Some(idx) => {
                result.claimed_by = Some(state.borrow().meta[idx].name.clone());
                if let Some(e) = errors.remove(&idx) {
                    result.client_error = Some(e);
                    result.response = Response {
                        status: 500,
                        headers: vec![("content-type".into(), "text/plain".into())],
                        body: b"client error".to_vec(),
                    };
                } else {
                    self.drive(&state, &mut slots, &mut errors);
                    result.response = state.borrow().response.clone();
                }
            }
        }

        // Collect the record before the contexts go away.
        let mut st = state.borrow_mut();
        for (i, m) in st.meta.iter().enumerate() {
            if !m.is_tag {
                continue;
            }
            let skip = st.tag_skipped.get(&i).cloned();
            let outcome = match st.tag_done.get(&i) {
                Some(true) if skip.is_some() => "skipped",
                Some(true) => "success",
                Some(false) => "failure",
                None => "timeout",
            };
            result.tags.push(TagRun {
                tag: m.name.clone(),
                event: m.event.unwrap_or(0),
                outcome,
                error: errors.get(&i).cloned(),
                skip,
            });
        }
        result.events = st.events.iter().map(|e| e.data.clone()).collect();
        result.sends = std::mem::take(&mut st.send_log);
        result.logs = std::mem::take(&mut st.logs);
        drop(st);

        drop(slots);
        self.deadline.set(None);
        self.rt.run_gc();
        result
    }

    /// Runs jobs, fires tags for new events, performs sends and reports
    /// finished events back to the client, until nothing is left to do.
    fn drive(
        &self,
        state: &Rc<RefCell<State>>,
        slots: &mut Vec<Slot>,
        errors: &mut HashMap<usize, String>,
    ) {
        loop {
            let mut progress = self.drain_jobs(state);
            let timed_out = Instant::now() > state.borrow().deadline;

            // Start the tags for events that haven't run yet.
            let new_events: Vec<usize> = state
                .borrow()
                .events
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.started)
                .map(|(i, _)| i)
                .collect();
            for ev in new_events {
                state.borrow_mut().events[ev].started = true;
                progress = true;
                if timed_out {
                    continue;
                }
                let data = state.borrow().events[ev].data.clone();
                for tag in &self.container.tags {
                    let (view, _) = self.container.event_for(tag, &data);
                    if !tag.fires_for(&view) {
                        continue;
                    }
                    if let std::borrow::Cow::Owned(v) = view {
                        state.borrow_mut().events[ev]
                            .views
                            .insert(tag.name.clone(), v);
                    }
                    let src = &self.container.templates[&tag.template];
                    let (idx, err) = self.spawn(
                        state,
                        slots,
                        &tag.name,
                        &tag.template,
                        src,
                        &tag.data,
                        Some(ev),
                        true,
                    );
                    state.borrow_mut().events[ev].tag_ctxs.push(idx);
                    if let Some(e) = err {
                        errors.insert(idx, e);
                        state.borrow_mut().tag_done.entry(idx).or_insert(false);
                    }
                }
            }

            // Deliver messages between templates. Listeners run on the next
            // turn of this loop, not inside the sender's call.
            let (messages, listeners) = {
                let mut st = state.borrow_mut();
                (std::mem::take(&mut st.messages), st.listeners.clone())
            };
            for (kind, message) in messages {
                for (_, ctx_idx, id) in listeners.iter().filter(|(t, _, _)| *t == kind) {
                    if timed_out || state.borrow().delivered >= MAX_MESSAGES {
                        break;
                    }
                    state.borrow_mut().delivered += 1;
                    progress = true;
                    let payload = serde_json::json!({ "type": kind, "message": message });
                    self.dispatch(
                        state,
                        slots,
                        *ctx_idx,
                        "message",
                        *id as i32,
                        &payload.to_string(),
                    );
                }
            }

            // Send everything that is queued, all at once.
            let batch: Vec<(u32, usize, Outgoing)> = std::mem::take(&mut state.borrow_mut().sends);
            if !batch.is_empty() {
                progress = true;
                let outgoing: Vec<Outgoing> = batch.iter().map(|(_, _, o)| o.clone()).collect();
                let results = if timed_out {
                    outgoing
                        .iter()
                        .map(|_| Err("timed_out".to_string()))
                        .collect()
                } else {
                    let events: Vec<Option<usize>> = {
                        let st = state.borrow();
                        batch.iter().map(|(_, c, _)| st.meta[*c].event).collect()
                    };
                    send_deduplicated(self.transport.as_ref(), outgoing, &events)
                };
                for ((id, ctx_idx, out), res) in batch.into_iter().zip(results) {
                    let from = state.borrow().meta[ctx_idx].name.clone();
                    state.borrow_mut().send_log.push(SendRecord {
                        from,
                        method: out.method.clone(),
                        url: out.url.clone(),
                        result: res.as_ref().map(|r| r.status).map_err(Clone::clone),
                        detail: res
                            .as_ref()
                            .ok()
                            .filter(|r| {
                                (r.status >= 400 || out.method.eq_ignore_ascii_case("POST"))
                                    && !r.body.trim().is_empty()
                            })
                            .map(|r| without_secrets(r.body.trim()).chars().take(300).collect()),
                    });
                    let payload = match res {
                        Ok(r) => serde_json::json!({
                            "statusCode": r.status,
                            "headers": r.headers.into_iter().collect::<HashMap<_, _>>(),
                            "body": r.body,
                        }),
                        Err(e) => {
                            serde_json::json!({ "error": if e == "timed_out" { "timed_out" } else { "failed" } })
                        }
                    };
                    self.dispatch(
                        state,
                        slots,
                        ctx_idx,
                        "send",
                        id as i32,
                        &payload.to_string(),
                    );
                }
            }

            // Tell the client about events whose tags have all reported.
            let finished: Vec<(usize, usize)> = {
                let st = state.borrow();
                st.events
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.started && !e.notified)
                    .filter(|(_, e)| e.tag_ctxs.iter().all(|t| st.tag_done.contains_key(t)))
                    .map(|(i, e)| (i, e.client_ctx))
                    .collect()
            };
            for (ev, client) in finished {
                state.borrow_mut().events[ev].notified = true;
                progress = true;
                self.run_event_callbacks(state, slots, errors, ev);
                self.dispatch(state, slots, client, "container", ev as i32, "null");
            }

            if state.borrow().returned {
                // The response is decided; let pending jobs finish quietly.
                self.drain_jobs(state);
                break;
            }
            if !progress {
                // Tags that never reported are treated as failed, once.
                let stuck: Vec<usize> = {
                    let st = state.borrow();
                    st.events
                        .iter()
                        .filter(|e| e.started && !e.notified)
                        .flat_map(|e| e.tag_ctxs.iter().copied())
                        .filter(|t| !st.tag_done.contains_key(t))
                        .collect()
                };
                if stuck.is_empty() {
                    break;
                }
                let mut st = state.borrow_mut();
                for t in stuck {
                    st.tag_done.insert(t, false);
                    let line = format!(
                        "[{}] did not call gtmOnSuccess or gtmOnFailure",
                        st.meta[t].name
                    );
                    st.logs.push(line);
                }
            }
        }
    }

    /// Calls the `addEventCallback` callbacks of an event whose tags have all
    /// reported, with the outcome of each tag.
    fn run_event_callbacks(
        &self,
        state: &Rc<RefCell<State>>,
        slots: &[Slot],
        errors: &HashMap<usize, String>,
        ev: usize,
    ) {
        let (callbacks, payload) = {
            let st = state.borrow();
            let callbacks: Vec<(usize, u32)> = st
                .callbacks
                .iter()
                .filter(|(e, _, _)| *e == ev)
                .map(|(_, ctx, id)| (*ctx, *id))
                .collect();
            let tags: Vec<Value> = st.events[ev]
                .tag_ctxs
                .iter()
                .map(|t| {
                    let status = match st.tag_done.get(t) {
                        _ if errors.contains_key(t) => "exception",
                        Some(true) => "success",
                        Some(false) => "failure",
                        None => "timeout",
                    };
                    serde_json::json!({ "id": st.meta[*t].name, "status": status, "executionTime": 0 })
                })
                .collect();
            (callbacks, serde_json::json!({ "tags": tags }).to_string())
        };
        for (ctx_idx, id) in callbacks {
            self.dispatch(state, slots, ctx_idx, "eventCallback", id as i32, &payload);
        }
    }

    fn drain_jobs(&self, state: &Rc<RefCell<State>>) -> bool {
        let mut ran = false;
        loop {
            match self.rt.execute_pending_job() {
                Ok(true) => ran = true,
                Ok(false) => break,
                Err(e) => {
                    ran = true;
                    let msg = e.0.with(|ctx| describe(&ctx, rquickjs::Error::Exception));
                    state
                        .borrow_mut()
                        .logs
                        .push(format!("uncaught error in async code: {msg}"));
                }
            }
        }
        ran
    }

    /// Settles a send or container run inside the context that started it.
    /// An error thrown by the template's callback is logged against it.
    fn dispatch(
        &self,
        state: &Rc<RefCell<State>>,
        slots: &[Slot],
        idx: usize,
        kind: &str,
        id: i32,
        json: &str,
    ) {
        let Some(p) = slots[idx].dispatch.clone() else {
            return;
        };
        let err = slots[idx].ctx.with(|ctx| {
            let f = p.restore(&ctx).map_err(|e| describe(&ctx, e))?;
            f.call::<_, ()>((kind, id, json))
                .map_err(|e| describe(&ctx, e))
        });
        if let Err(e) = err {
            let mut st = state.borrow_mut();
            let line = format!("[{}] {e}", st.meta[idx].name);
            st.logs.push(line);
        }
    }

    /// Creates a context for one template run and runs the template's
    /// synchronous code. Returns the context index and any error it threw.
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &self,
        state: &Rc<RefCell<State>>,
        slots: &mut Vec<Slot>,
        name: &str,
        template: &str,
        src: &str,
        data: &Value,
        event: Option<usize>,
        is_tag: bool,
    ) -> (usize, Option<String>) {
        let idx = {
            let mut st = state.borrow_mut();
            st.meta.push(CtxMeta {
                name: name.into(),
                storage: self.container.storage_scope(template),
                event,
                is_tag,
            });
            st.meta.len() - 1
        };
        let ctx = match Context::full(&self.rt) {
            Ok(c) => c,
            Err(e) => return (idx, Some(format!("could not create context: {e}"))),
        };

        if let Some(at) = self.clock.get() {
            let seed = name.bytes().fold(at % 2_147_483_648, |s, b| {
                (s * 31 + b as u64) % 2_147_483_648
            });
            let fix = format!(
                "Date.now = () => {at}; (() => {{ let s = {seed}; Math.random = () => {{ s = (s * 1103515245 + 12345) % 2147483648; return s / 2147483648; }}; }})();"
            );
            if let Err(e) = ctx.with(|ctx| ctx.eval::<(), _>(fix)) {
                return (idx, Some(format!("could not fix the clock: {e}")));
            }
        }
        let data_json = data.to_string();
        let grants_json = self
            .container
            .permissions
            .get(template)
            .map_or_else(|| "null".to_string(), |p| serde_json::json!(p).to_string());
        let storage = self.storage.clone();
        let (dispatch, err) = ctx.with(|ctx| {
            let env = (|| -> rquickjs::Result<(Function, JsValue, Function)> {
                let host = host::bindings(&ctx, state.clone(), storage, idx)?;
                let prelude = self.function(&ctx, "prelude", PRELUDE)?;
                let env: Object =
                    prelude.call((host, data_json, is_tag, grants_json, API_PERMISSIONS))?;
                Ok((env.get("require")?, env.get("data")?, env.get("dispatch")?))
            })();
            let (require, data, dispatch) = match env {
                Ok(v) => v,
                Err(e) => return (None, Some(describe(&ctx, e))),
            };
            let dispatch = Persistent::save(&ctx, dispatch);
            let wrapped = format!("(function (require, data) {{ 'use strict';\n{src}\n}})");
            let run = (|| -> rquickjs::Result<()> {
                let f = self.function(&ctx, &format!("template:{template}"), &wrapped)?;
                f.call::<_, ()>((require, data))
            })();
            (Some(dispatch), run.err().map(|e| describe(&ctx, e)))
        });
        if let Some(e) = &err {
            state.borrow_mut().logs.push(format!("[{name}] {e}"));
        }
        slots.push(Slot { dispatch, ctx });
        (idx, err)
    }
}

/// Sends a batch, making identical GET requests asked for the same event
/// (such as one visitor lookup from several ad tags) only once; each asker
/// gets the same answer. Anything else goes out as asked, so two identical
/// events still send twice. `events` holds each request's event.
fn send_deduplicated(
    transport: &dyn Transport,
    outgoing: Vec<Outgoing>,
    events: &[Option<usize>],
) -> Vec<Result<SendResult, String>> {
    let same = |(a, ea): (&Outgoing, Option<usize>), (b, eb): (&Outgoing, Option<usize>)| {
        ea.is_some()
            && ea == eb
            && a.method.eq_ignore_ascii_case("GET")
            && a.method == b.method
            && a.url == b.url
            && a.headers == b.headers
            && a.body == b.body
    };
    let mut unique: Vec<Outgoing> = Vec::new();
    let mut unique_events: Vec<Option<usize>> = Vec::new();
    let mut slot = Vec::with_capacity(outgoing.len());
    for (o, &ev) in outgoing.into_iter().zip(events) {
        let found = (0..unique.len()).find(|&i| same((&unique[i], unique_events[i]), (&o, ev)));
        match found {
            Some(at) => {
                // Timeouts differ by the time left when each was asked.
                unique[at].timeout = unique[at].timeout.max(o.timeout);
                slot.push(at);
            }
            None => {
                slot.push(unique.len());
                unique.push(o);
                unique_events.push(ev);
            }
        }
    }
    let results = transport.send_all(unique);
    slot.into_iter()
        .map(|at| {
            results
                .get(at)
                .cloned()
                .unwrap_or_else(|| Err("failed".into()))
        })
        .collect()
}

/// Turns an rquickjs error into a readable message, taking the pending
/// exception off the context when there is one.
fn describe(ctx: &Ctx<'_>, err: rquickjs::Error) -> String {
    if let rquickjs::Error::Exception = err {
        let ex = ctx.catch();
        if let Some(obj) = ex.as_object() {
            let msg: Option<String> = obj.get("message").ok();
            if let Some(m) = msg {
                return m;
            }
        }
        if let Some(s) = ex.as_string().and_then(|s| s.to_string().ok()) {
            return s;
        }
        if ex.is_null() || ex.is_undefined() {
            return "interrupted: the run's time budget was spent".into();
        }
        return format!("{ex:?}");
    }
    err.to_string()
}

#[cfg(test)]
mod tests;
